//! cell_run — `shuttle cell-run`, the fail-closed cell entrypoint.
//!
//! Unlike `shuttle run` (human interactive), `cell-run` is a fail-closed
//! subcommand: it consumes a shuttle-LOCAL `cell_binding/v1` envelope (see
//! [`shuttle_protocol::cell_binding`]) and stands up a **mandatory,
//! non-disableable** [`ScopedPathResolver`] from `workspace_root` +
//! `allowed_paths`. There is no `--no-scope` escape hatch — an empty
//! allowlist forbids every write by design (shuttle's own write-boundary
//! rule).
//!
//! ## Directly launchable as an agent
//!
//! This entrypoint is the way shuttle runs *as an agent*. The binding can be a
//! minimal hand-written `cell_binding.json` (workspace_root + allowed_paths +
//! allowed_commands + ids) — no external orchestration is required. When an
//! external orchestrator *does* drive the run, the envelope is instead
//! produced by `CellBinding::from_external(...)`; either way this code path is
//! identical, so a human can `shuttle cell-run --binding local.json
//! --prompt "…"` to bring shuttle up as a caged agent directly.
//!
//! This slice wires both the *input envelope* and the *caged spawn*: parse +
//! validate the binding, self-fill `shuttle_binary_digest`, construct the mandatory
//! resolver, emit a structured preflight, and — when a `prompt` is supplied —
//! drive a pi planner **inside** that cage. Convergence form (b):
//! cell-run reuses `run::supervise` rather than duplicating the spawn loop, but
//! layers three fail-closed guarantees `shuttle run` does not give:
//!   1. `--exclude-tools bash` — the cell can never spawn a shell / arbitrary
//!      command directly. Read-only tools and the native `write` tool stay
//!      enabled (a planning cell such as the `splitter` emits its spec files
//!      through `write`); shell is the only capability the cage removes.
//!   2. the extension's `shuttle apply` is forced to run with
//!      `--enforce-scope --allowed-path <binding.allowed_paths>` (propagated via
//!      `SHUTTLE_ENFORCE_SCOPE` / `SHUTTLE_ALLOWED_PATHS`), so the resolver=None escape
//!      hatch of a bare `shuttle run` is closed.
//!   3. the workspace root is the binding's canonical `workspace_root`, so
//!      writes are confined to that subtree.
//!      With no `prompt` the command stays preflight-only (validate + describe).

use serde::Serialize;
use shuttle_protocol::{
    orchestrate, reason, AttemptOutput, AttemptRequest, AttemptRunner, CellBinding,
    OrchestrationOptions, OrchestrationTask, ScopedPathResolver, ToolOutcome, TypedDetail,
};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cell_registry::{self, CellRegistry, CellState};
use crate::run::{self, RunConfig, RunTelemetry, WrapMode};
use crate::sha256;
use crate::upstream::{ExternalUpstream, LocalUpstream, UpstreamOrchestrator};

/// Machine-readable preflight emitted on stdout after a successful parse. The
/// caller uses it to confirm the enforced boundary before shuttle does any work.
#[derive(Debug, Serialize)]
pub struct CellPreflight {
    /// Always `cell_binding/v1` — echoes the consumed schema.
    pub contract: String,
    /// Canonicalized workspace root shuttle will confine every write under.
    pub workspace_root: String,
    /// The glob allowlist in force (verbatim from the binding).
    pub allowed_paths: Vec<String>,
    /// Number of whitelisted command entries (0 = no command may spawn).
    pub allowed_command_count: usize,
    /// Read-only base OID shuttle executes on and references in the receipt.
    pub base_oid: String,
    /// Assignment digest copied verbatim from the binding.
    pub assignment_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_digest: Option<String>,
    /// Explicit project state; old direct bindings report `legacy_unbound`.
    pub project_mode: String,
    /// Explicit project id for project-bound cells; absent for legacy-unbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The immutable project binding digest for project-bound cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_binding_digest: Option<String>,
    /// shuttle's self-computed binary digest (shuttle-local; not an external field).
    pub shuttle_binary_digest: String,
    /// Opaque environment id, passed through untouched.
    pub environment_id: String,
    /// True when `allowed_paths` is empty — writes are fully fail-closed.
    pub write_fail_closed: bool,
}

/// Compute the running shuttle binary's SHA-256 (shuttle-local). Best-effort: if the current
/// exe path cannot be read we surface an error rather than fabricating a digest.
fn self_binary_digest() -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let bytes = std::fs::read(&exe)?;
    Ok(sha256::sha256_hex_prefixed(&bytes))
}

/// Read the raw input envelope from `path` (file) or stdin (`None`).
fn read_envelope(path: Option<String>) -> anyhow::Result<String> {
    match path {
        Some(p) => {
            std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("failed to read {p}: {e}"))
        }
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

/// The upstream-agnostic cell driver. Every `shuttle cell-run` — direct or
/// orchestrated — flows through here: read the raw envelope, let the
/// [`UpstreamOrchestrator`] lower it to a binding, stand up the mandatory
/// resolver, emit the preflight, then (with a prompt) drive the caged pi loop
/// and report the settled outcome back to the upstream. The `orch` owns the two
/// launcher-specific ends (ingest + observation); the cage in the middle never
/// names any upstream.
///
/// Fail-closed: on any post-ingest setup failure the driver calls
/// [`UpstreamOrchestrator::note_failure`] so an orchestrating upstream still
/// hands back a legal refusal observation before the error propagates.
fn drive_cell<O: UpstreamOrchestrator>(
    mut orch: O,
    input_path: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    // 1. Read the raw envelope (file or stdin).
    let raw = read_envelope(input_path)?;

    // 2. Ingest: the upstream lowers the envelope into a fail-closed binding.
    //    (A external upstream emits its own refusal receipt on ingest failure.)
    let binding = orch.ingest(&raw)?;

    // 3-4. Fill digest + construct the MANDATORY resolver. A setup failure here
    //      is post-ingest, so the upstream must be told (external emits a refusal).
    let (mut binding, resolver) = match prepare_cell(binding) {
        Ok(pair) => pair,
        Err(e) => {
            orch.note_failure();
            return Err(e);
        }
    };

    // 5. Emit the structured preflight (identical for both upstreams).
    emit_preflight(&binding, &resolver)?;

    // 6. Resolve the effective prompt (external falls back to the bundle's own).
    //    With no prompt this is preflight-only: nothing lands, so the upstream
    //    records a non-terminal observation and we return cleanly.
    let Some(prompt) = orch.resolve_prompt(prompt).filter(|p| !p.trim().is_empty()) else {
        let _ = orch.on_preflight_only()?;
        return Ok(());
    };
    // Backward compatibility: existing direct callers (including `shuttle goal`)
    // did not opt into a durable provider identity. Keep their synchronous,
    // ephemeral behavior unchanged. external lowering and new direct lifecycle calls
    // provide `provider_run_id` explicitly and enter the registry path below.
    if binding.provider_run_id.is_none() {
        let tel = run_caged(&resolver, &binding, prompt, model, None)?;
        eprintln!("{}", serde_json::to_string(&tel)?);
        let _ = orch.on_settled(&resolver, &tel)?;
        std::process::exit(tel.exit_code);
    }

    let run_id = orch.provider_run_id(&binding);
    let attempt_id = orch.attempt_id(&binding);
    let registry = CellRegistry::open_default()?;
    if binding.session_file.is_none() {
        let session_dir = registry.session_dir(&run_id);
        std::fs::create_dir_all(&session_dir)?;
        binding.session_file = Some(session_dir.to_string_lossy().into_owned());
    }
    let binding_json = serde_json::to_string_pretty(&binding)?;
    let mut record = registry.initialize(&run_id, attempt_id, &binding, &binding_json)?;
    if record.state == CellState::Active && !cell_registry::process_alive(record.pgid) {
        record = registry.mutate(&run_id, "process_missing", |record| {
            record.state = CellState::Recoverable;
            record.pid = None;
            record.pgid = None;
            Ok(serde_json::json!({"reason": "process_group_missing"}))
        })?;
    }
    if record.state.terminal() || matches!(record.state, CellState::Active | CellState::Recoverable)
    {
        orch.emit_existing(&cell_registry::provider_receipt(&record))?;
        return Ok(());
    }

    // 7. Drive the caged pi loop inside the resolver boundary.
    let tel = match run_caged(
        &resolver,
        &binding,
        prompt,
        model,
        Some((&registry, &run_id)),
    ) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            let telemetry = RunTelemetry::errored(0);
            let receipt = crate::provider_receipt::build_receipt(&telemetry, &run_id, None);
            let (record, committed) = registry.fail_if_nonterminal(
                &run_id,
                receipt,
                "shuttle_cell_spawn_failed".into(),
                telemetry.exit_code,
            )?;
            let record = if record.state.terminal() {
                registry.finalize_usage(&run_id, None)?
            } else {
                record
            };
            orch.emit_existing(&cell_registry::provider_receipt(&record))?;
            if committed {
                return Err(error);
            }
            return Ok(());
        }
    };
    // Telemetry mirrors `shuttle run`: one JSON line on stderr for any reader.
    eprintln!("{}", serde_json::to_string(&tel)?);

    // 8. Build the candidate receipt without emitting it. `settle_if_active`
    //    then performs the decisive state transition under the per-run lock; a
    //    concurrent cancel that won first remains authoritative.
    let upstream_receipt = orch.on_settled(&resolver, &tel)?;
    let receipt = upstream_receipt.unwrap_or_else(|| {
        let result_ref = crate::provider_receipt::land_result_artifact(
            resolver.root(),
            &run_id,
            &tel,
            Some(&binding),
        )
        .ok();
        crate::provider_receipt::build_receipt(&tel, &run_id, result_ref)
    });
    let (record, committed) = registry.settle_if_active(
        &run_id,
        receipt,
        (tel.exit_code != 0).then(|| "shuttle_cell_gate_failed".into()),
        tel.exit_code,
    )?;
    let record = if record.state.terminal() {
        registry.finalize_usage(&run_id, Some(&tel))?
    } else {
        record
    };
    orch.emit_existing(&cell_registry::provider_receipt(&record))?;
    if !committed {
        return Ok(());
    }
    std::process::exit(tel.exit_code);
}

pub fn observe_cell(run_id: &str) -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    let mut record = registry
        .read(run_id)?
        .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
    if record.state == CellState::Active && !cell_registry::process_alive(record.pgid) {
        record = registry.mutate(run_id, "process_missing", |record| {
            record.state = CellState::Recoverable;
            record.pid = None;
            record.pgid = None;
            Ok(serde_json::json!({"reason": "process_group_missing"}))
        })?;
    }
    println!("{}", serde_json::to_string(&record)?);
    Ok(())
}

pub fn collect_cell(run_id: &str) -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    let record = registry
        .read(run_id)?
        .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
    crate::provider_receipt::emit_receipt(&cell_registry::provider_receipt(&record), None)
}

pub fn usage_cell(run_id: &str) -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    let record = registry
        .read(run_id)?
        .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
    if !record.state.terminal() {
        anyhow::bail!("usage receipt is unavailable before terminal state");
    }
    if record.usage_ref.is_none() || record.usage_digest.is_none() {
        anyhow::bail!("terminal usage receipt is not finalized; reconcile the supervisor attempt");
    }
    let path = registry.usage_path(run_id);
    let body = std::fs::read(&path)?;
    let digest = sha256::sha256_hex_prefixed(&body);
    if record.usage_digest.as_deref() != Some(&digest) {
        anyhow::bail!("durable usage receipt digest drift");
    }
    print!("{}", String::from_utf8(body)?);
    Ok(())
}

pub fn cancel_cell(run_id: &str, grace_ms: u64) -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    let record = registry
        .read(run_id)?
        .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
    if !record.state.terminal() {
        let cancelling = registry.mutate(run_id, "cancellation_requested", |record| {
            if record.state.terminal() {
                return Ok(serde_json::json!({"replayed": true}));
            }
            record.state = CellState::Blocked;
            record.error_class = Some("shuttle_cell_cancellation_requested".into());
            Ok(serde_json::json!({"grace_ms": grace_ms}))
        })?;
        if cancelling.state.terminal() {
            return crate::provider_receipt::emit_receipt(
                &cell_registry::provider_receipt(&cancelling),
                None,
            );
        }
        if let Some(pgid) = cancelling.pgid {
            cell_registry::terminate_process_group(pgid, grace_ms)?;
        }
        let cancelled = crate::provider_receipt::cancelled_receipt(run_id);
        let record = registry.mutate(run_id, "cancelled", |record| {
            record.state = CellState::Cancelled;
            record.pid = None;
            record.pgid = None;
            record.receipt = Some(cancelled.clone());
            record.error_class = Some("shuttle_cell_cancelled".into());
            Ok(serde_json::json!({"grace_ms": grace_ms, "process_group_absent": true}))
        })?;
        return crate::provider_receipt::emit_receipt(
            &cell_registry::provider_receipt(&record),
            None,
        );
    }
    crate::provider_receipt::emit_receipt(&cell_registry::provider_receipt(&record), None)
}

pub fn recover_cell(run_id: &str, prompt: String, model: Option<String>) -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    let record = registry
        .read(run_id)?
        .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
    if record.state.terminal() {
        return crate::provider_receipt::emit_receipt(
            &cell_registry::provider_receipt(&record),
            None,
        );
    }
    if cell_registry::process_alive(record.pgid) {
        anyhow::bail!("cell run is still active; observe or cancel it instead");
    }
    registry.mutate(run_id, "recovery_started", |record| {
        record.state = CellState::Recoverable;
        record.pid = None;
        record.pgid = None;
        Ok(serde_json::json!({"reason": "cold_resume"}))
    })?;
    let binding_json = registry.verify_binding(&record)?;
    registry.verify_base_oid(&record)?;
    let mut binding = CellBinding::parse(&binding_json)
        .map_err(|error| anyhow::anyhow!("durable cell binding invalid: {error}"))?;
    let session_dir = registry.session_dir(run_id);
    std::fs::create_dir_all(&session_dir)?;
    binding.session_file = Some(session_dir.to_string_lossy().into_owned());
    let (_, resolver) = prepare_cell(binding.clone())?;
    let tel = run_caged(
        &resolver,
        &binding,
        prompt,
        model,
        Some((&registry, run_id)),
    )?;
    eprintln!("{}", serde_json::to_string(&tel)?);
    let result_ref = crate::provider_receipt::land_result_artifact(
        resolver.root(),
        run_id,
        &tel,
        Some(&binding),
    )
    .ok();
    let receipt = crate::provider_receipt::build_receipt(&tel, run_id, result_ref.clone());
    let (_record, _) = registry.settle_if_active(
        run_id,
        receipt,
        (tel.exit_code != 0).then(|| "shuttle_cell_gate_failed".into()),
        tel.exit_code,
    )?;
    let record = registry.finalize_usage(run_id, Some(&tel))?;
    crate::provider_receipt::emit_receipt(&cell_registry::provider_receipt(&record), None)
}

pub fn doctor_cell() -> anyhow::Result<()> {
    let registry = CellRegistry::open_default()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&cell_registry::doctor(&registry))?
    );
    Ok(())
}

/// Direct-agent-launch entry point for `shuttle cell-run [--binding …]`. Consumes a
/// hand-written `cell_binding/v1` (file or stdin) through a [`LocalUpstream`]:
/// no external contracts, no provider receipt — the run telemetry is the only
/// observation. This is the zero-upstream path an OSS build ships with.
pub fn run_cell(
    binding_path: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    drive_cell(LocalUpstream, binding_path, prompt, model)
}

/// Orchestrated entry point for `shuttle cell-run --launch-bundle`. Consumes a
/// launch bundle (file or stdin) through a [`ExternalUpstream`], which lowers the
/// carried protocol anchors into the same shuttle-local binding and emits a
/// compliant provider receipt to `receipt_out` (file, or `-`/stdout) for every
/// terminal / refusal state. The caged run in between is byte-for-byte the
/// direct path — only ingest + observation differ.
pub fn run_cell_from_external(
    bundle_path: Option<String>,
    prompt_override: Option<String>,
    model: Option<String>,
    receipt_out: Option<String>,
) -> anyhow::Result<()> {
    drive_cell(
        ExternalUpstream::new(receipt_out),
        bundle_path,
        prompt_override,
        model,
    )
}

/// Execute one argv under a previously validated cell command allowlist.
///
/// This is deliberately not a shell: no token is reparsed, interpolated, or
/// expanded. The binding is re-read in the trusted Rust process, argv[0] must
/// match by basename, and every configured prefix item must match exactly.
pub fn run_allowed_commands<F>(
    binding: &CellBinding,
    timeout_ms: u64,
    heartbeat_every: Duration,
    mut heartbeat: F,
) -> anyhow::Result<Vec<serde_json::Value>>
where
    F: FnMut() -> anyhow::Result<()>,
{
    if timeout_ms == 0 {
        anyhow::bail!("timeout_ms must be positive");
    }
    if binding.allowed_commands.is_empty() {
        anyhow::bail!("verification requires at least one allowed command");
    }
    let root = std::fs::canonicalize(&binding.workspace_root)?;
    let snapshot = CommandSnapshot::new(&root)?;
    let started = Instant::now();
    let mut records = Vec::new();
    for allowed in &binding.allowed_commands {
        let prefix = allowed
            .argv_prefix
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("verification commands require exact argv prefixes"))?;
        let mut argv = vec![allowed.program.clone()];
        argv.extend(prefix.iter().cloned());
        let remaining = Duration::from_millis(timeout_ms).saturating_sub(started.elapsed());
        if remaining.is_zero() {
            records.push(command_evidence(binding, &argv, 0, 124, &[], &[]));
            break;
        }
        let record = run_allowed_command_in_snapshot(
            binding,
            &snapshot,
            &argv,
            remaining.as_millis().min(u64::MAX as u128) as u64,
            heartbeat_every,
            &mut heartbeat,
        )?;
        let failed = record["exit_code"] != 0;
        records.push(record);
        if failed {
            break;
        }
    }
    Ok(records)
}

fn run_allowed_command_in_snapshot<F>(
    binding: &CellBinding,
    snapshot: &CommandSnapshot,
    argv: &[String],
    timeout_ms: u64,
    heartbeat_every: Duration,
    heartbeat: &mut F,
) -> anyhow::Result<serde_json::Value>
where
    F: FnMut() -> anyhow::Result<()>,
{
    if !binding.command_exactly_allowed(argv) {
        anyhow::bail!("command is not allowed by cell binding");
    }
    if Path::new(&argv[0]).components().count() != 1 {
        anyhow::bail!("command argv[0] must be a bare basename");
    }
    let executable = resolve_command(&argv[0], Path::new(&binding.workspace_root))?;
    let mut command = std::process::Command::new(executable);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .args(&argv[1..])
        .current_dir(snapshot.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, _) in std::env::vars() {
        let upper = name.to_ascii_uppercase();
        if [
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "CREDENTIAL",
            "API_KEY",
            "PROVIDER_KEY",
        ]
        .iter()
        .any(|marker| upper.contains(marker))
        {
            command.env_remove(name);
        }
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("command stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("command stderr was not piped"))?;
    let stdout_reader = std::thread::spawn(move || read_bounded(stdout));
    let stderr_reader = std::thread::spawn(move || read_bounded(stderr));
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut next_heartbeat = Instant::now() + heartbeat_every.max(Duration::from_millis(250));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            // The approved command may spawn compilers/test workers. Kill the
            // whole isolated process group so descendants cannot keep running
            // or hold the bounded output pipes open after timeout.
            kill_process_tree(&mut child);
            let _ = child.wait()?;
            let (stdout, stderr) = collect_command_output(stdout_reader, stderr_reader)?;
            return Ok(command_evidence(
                binding,
                argv,
                started.elapsed().as_millis(),
                124,
                &stdout,
                &stderr,
            ));
        }
        if Instant::now() >= next_heartbeat {
            if let Err(error) = heartbeat() {
                kill_process_tree(&mut child);
                let _ = child.wait();
                let _ = collect_command_output(stdout_reader, stderr_reader);
                return Err(error);
            }
            next_heartbeat = Instant::now() + heartbeat_every.max(Duration::from_millis(250));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (stdout, stderr) = collect_command_output(stdout_reader, stderr_reader)?;
    Ok(command_evidence(
        binding,
        argv,
        started.elapsed().as_millis(),
        status.code().unwrap_or(-1),
        &stdout,
        &stderr,
    ))
}

fn resolve_command(program: &str, workspace_root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH is unavailable for verifier command"))?;
    for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
        let candidate = directory.join(program);
        if !candidate.is_file() {
            continue;
        }
        let resolved = std::fs::canonicalize(candidate)?;
        if resolved.starts_with(workspace_root) {
            continue;
        }
        return Ok(resolved);
    }
    anyhow::bail!("allowed command cannot be resolved outside the workspace: {program}")
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

fn command_evidence(
    binding: &CellBinding,
    argv: &[String],
    wall_ms: u128,
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "shuttle_goal_command_evidence/v1",
        "argv": argv,
        "wall_ms": wall_ms,
        "exit_code": exit_code,
        "assignment_digest": binding.assignment_digest,
        "base_oid": binding.base_oid,
        "workspace_mode": "disposable_snapshot",
        "stdout": String::from_utf8_lossy(stdout),
        "stderr": String::from_utf8_lossy(stderr),
    })
}

fn read_bounded(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    Ok(output)
}

fn collect_command_output(
    stdout_reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("command stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("command stderr reader panicked"))??;
    Ok((stdout, stderr))
}

static NEXT_COMMAND_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

struct CommandSnapshot {
    path: std::path::PathBuf,
}

impl CommandSnapshot {
    fn new(root: &Path) -> anyhow::Result<Self> {
        let nonce = NEXT_COMMAND_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "shuttle-cell-command-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        if let Err(error) = copy_command_snapshot(root, &path) {
            let _ = std::fs::remove_dir_all(&path);
            return Err(error);
        }
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CommandSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn copy_command_snapshot(root: &Path, destination: &Path) -> anyhow::Result<()> {
    const MAX_FILES: usize = 50_000;
    const MAX_BYTES: u64 = 512 * 1024 * 1024;
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
    let output = std::process::Command::new(git)
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!("workspace snapshot requires a Git worktree");
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let relative = std::str::from_utf8(raw)?;
        let relative = Path::new(relative);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            anyhow::bail!("unsafe path in Git snapshot: {}", relative.display());
        }
        let source = root.join(relative);
        let metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            let resolved = std::fs::canonicalize(&source)?;
            if !resolved.starts_with(root) || !resolved.is_file() {
                anyhow::bail!("unsafe symlink in command snapshot: {}", relative.display());
            }
            let target = destination.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(resolved, target)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        files += 1;
        bytes = bytes.saturating_add(metadata.len());
        if files > MAX_FILES || bytes > MAX_BYTES {
            anyhow::bail!("workspace snapshot exceeds file/byte budget");
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, target)?;
    }
    Ok(())
}

/// Fill `shuttle_binary_digest` (shuttle-local preflight value) and construct the
/// MANDATORY [`ScopedPathResolver`]. Shared by the direct and orchestrated
/// paths.
fn prepare_cell(mut binding: CellBinding) -> anyhow::Result<(CellBinding, ScopedPathResolver)> {
    // Revalidate even when a caller constructed the binding programmatically.
    // This happens before canonicalization or any workspace value is exposed.
    binding
        .validate()
        .map_err(|e| anyhow::anyhow!("cell_binding rejected before workspace exposure: {e}"))?;
    let shuttle_digest = self_binary_digest()?;
    let tools = shuttle_runtime_tools::ToolResolver::from_env()
        .map_err(|error| anyhow::anyhow!("runtime_dependency_mismatch: {error}"))?;
    if let Some(descriptor) = binding.runtime_descriptor.as_ref() {
        if descriptor.core.runtime_abi != shuttle_runtime_tools::RUNTIME_ABI {
            anyhow::bail!("runtime_dependency_mismatch: unsupported SHUTTLE runtime ABI");
        }
        if descriptor.core.digest.as_str() != shuttle_digest {
            anyhow::bail!("runtime_dependency_mismatch: SHUTTLE core digest drift");
        }
        tools
            .assert_identity(
                descriptor.tools.manifest_digest.as_str(),
                descriptor.tools.distribution_digest.as_str(),
                Path::new(&descriptor.tools.root),
                &descriptor.tools.profiles,
            )
            .map_err(|error| anyhow::anyhow!("runtime_dependency_mismatch: {error}"))?;
        validate_provider_runtime(descriptor, &binding.workspace_root)?;
    } else if tools.mode() == shuttle_runtime_tools::ToolMode::Strict {
        anyhow::bail!(
            "runtime_dependency_mismatch: strict tool mode requires a runtime descriptor"
        );
    }
    if binding.is_shuttle_binary_digest_placeholder() {
        binding.shuttle_binary_digest = Some(shuttle_digest);
    }
    // P0-1/P0-2: the resolver cannot be disabled; `allowed_paths` is the sole
    // write boundary and an empty list rejects every write. A missing root is a
    // hard failure (fail-closed), never a silent open scope.
    // For project-bound execution, the provider workspace is the sole root of
    // the resolver. `CellBinding::validate` already proved it equals the local
    // workspace root; selecting it here prevents future callers from silently
    // rooting the cage at another path.
    let resolver_root = binding
        .project
        .provider_workspace_root()
        .unwrap_or(&binding.workspace_root);
    let resolver =
        ScopedPathResolver::new(resolver_root, binding.allowed_paths.clone()).map_err(|e| {
            anyhow::anyhow!(
                "cannot canonicalize workspace_root {}: {e}",
                binding.workspace_root
            )
        })?;
    Ok((binding, resolver))
}

fn validate_provider_runtime(
    descriptor: &shuttle_protocol::EaRuntimeDescriptor,
    workspace_root: &str,
) -> anyhow::Result<()> {
    let provider = &descriptor.provider;
    if provider.kind != "pi" {
        anyhow::bail!("runtime_dependency_mismatch: unsupported provider kind");
    }
    if let Some(configured) = std::env::var_os("PI_BIN") {
        if Path::new(&configured) != Path::new(&provider.binary) {
            anyhow::bail!("runtime_dependency_mismatch: PI_BIN conflicts with provider binary");
        }
    }
    if let Some(configured) = crate::config::configured_extension_path(workspace_root) {
        if configured != provider.extension_path {
            anyhow::bail!(
                "runtime_dependency_mismatch: configured extension conflicts with provider extension"
            );
        }
    }
    verify_runtime_file(
        "provider binary",
        Path::new(&provider.binary),
        provider.binary_digest.as_str(),
    )?;
    verify_runtime_file(
        "provider extension",
        Path::new(&provider.extension_path),
        provider.extension_digest.as_str(),
    )?;
    verify_provider_version(Path::new(&provider.binary), &provider.version)?;
    Ok(())
}

fn verify_runtime_file(name: &str, path: &Path, expected_digest: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        anyhow::anyhow!(
            "runtime_dependency_mismatch: {name} unavailable at {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "runtime_dependency_mismatch: {name} must be a regular non-symlink file: {}",
            path.display()
        );
    }
    let observed = sha256::sha256_hex_prefixed(&std::fs::read(path)?);
    if observed != expected_digest {
        anyhow::bail!("runtime_dependency_mismatch: {name} digest drift");
    }
    Ok(())
}

fn verify_provider_version(binary: &Path, expected: &str) -> anyhow::Result<()> {
    let mut child = std::process::Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("runtime_dependency_mismatch: provider version probe failed: {error}")
        })?;
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if started.elapsed() >= Duration::from_secs(2) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("runtime_dependency_mismatch: provider version probe timed out");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let observed = if stdout.is_empty() { stderr } else { stdout };
    let expected_outputs = [
        expected.to_string(),
        format!("pi {expected}"),
        format!("pi v{expected}"),
    ];
    if !output.status.success() || !expected_outputs.iter().any(|value| value == &observed) {
        anyhow::bail!(
            "runtime_dependency_mismatch: provider version drift (expected {expected:?}, observed {observed:?})"
        );
    }
    Ok(())
}

/// Emit the structured [`CellPreflight`] JSON on stdout.
fn emit_preflight(binding: &CellBinding, resolver: &ScopedPathResolver) -> anyhow::Result<()> {
    let preflight = CellPreflight {
        contract: "cell_binding/v1".to_string(),
        workspace_root: resolver.root().display().to_string(),
        allowed_paths: binding.allowed_paths.clone(),
        allowed_command_count: binding.allowed_commands.len(),
        base_oid: binding.base_oid.clone(),
        assignment_digest: binding.assignment_digest.clone(),
        runtime_descriptor_ref: binding.runtime_descriptor_ref.clone(),
        runtime_descriptor_digest: binding
            .runtime_descriptor_digest
            .as_ref()
            .map(ToString::to_string),
        project_mode: binding.project.mode().to_string(),
        project_id: binding.project.project_id().map(ToString::to_string),
        project_binding_digest: binding.project.binding_digest().map(ToString::to_string),
        shuttle_binary_digest: binding.shuttle_binary_digest.clone().unwrap_or_default(),
        environment_id: binding.environment_id.clone(),
        write_fail_closed: binding.allowed_paths.is_empty(),
    };
    println!("{}", serde_json::to_string_pretty(&preflight)?);
    Ok(())
}

/// Drive a caged pi planner inside the resolver boundary and return telemetry.
///
/// Two modes:
///   * **default (single-shot)** — one `run::supervise` spawn, exactly as
///     before. This is the safe production default; no extra model spend, no
///     re-spawns.
///   * **orchestrated (opt-in, `SHUTTLE_ORCHESTRATE=1`)** — drive the cheap-first →
///     route → escalate loop (efficiency bet #3, the assembly line) via the
///     pure [`shuttle_protocol::orchestrate`] state machine. A [`CagedAttemptRunner`]
///     adapts one spawn into an `AttemptOutput`; the state machine owns every
///     decision (which tier, when to escalate, when to stop). The settling
///     attempt's telemetry is returned so the caller's receipt/exit path is
///     unchanged. The cheap-first economics land as a shuttle-local audit sidecar
///     (never the frozen protocol receipt).
fn run_caged(
    resolver: &ScopedPathResolver,
    binding: &CellBinding,
    prompt: String,
    model: Option<String>,
    durable: Option<(&CellRegistry, &str)>,
) -> anyhow::Result<RunTelemetry> {
    if durable.is_some() || !orchestration_enabled() {
        // Default: single-shot spawn (behavior unchanged).
        let cfg = build_caged_config(resolver, binding, prompt, model);
        return match durable {
            Some((registry, run_id)) => run::supervise_with_spawn(&cfg, |pid| {
                registry.mark_active(run_id, pid)?;
                Ok(())
            }),
            None => run::supervise(&cfg),
        };
    }

    // Opt-in: let the pure state machine run the loop. The runner is the only
    // outside-world dependency — it spawns pi once per attempt and maps the
    // telemetry to a uniform `ToolOutcome`; tier selection + retry/escalation
    // are the orchestrator's.
    let task = OrchestrationTask {
        op: cell_op(),
        // shuttle-local trace/idempotency roots derived from the binding. The external
        // `provider_run_id` is carried verbatim on the receipt in the external orchestrator path
        // (see `run_cell_from_external`); this only names the per-attempt trace ids
        // in the shuttle-local audit sidecar.
        base_run_id: binding.environment_id.clone(),
        idempotency_key: binding.assignment_digest.clone(),
    };
    let opts = OrchestrationOptions {
        max_attempts: orchestrate_max_attempts(),
        ..OrchestrationOptions::default()
    };
    let mut runner = CagedAttemptRunner {
        resolver,
        binding,
        prompt,
        model,
        last_telemetry: None,
    };
    let result = orchestrate(&task, &mut runner, &opts);

    // Land the cheap-first economics as a shuttle-local audit sidecar. This is NOT
    // the external orchestrator receipt: the frozen `external_provider_receipt/v1` snapshot has
    // `additionalProperties: false`, so tier/route/economics never go in it.
    emit_orchestration_audit(resolver.root(), &task, &result);

    // The settling (last) attempt's telemetry is what the caller reports and
    // the §2 external receipt gate maps over.
    runner
        .last_telemetry
        .ok_or_else(|| anyhow::anyhow!("orchestration produced no attempt telemetry"))
}

/// Whether the opt-in orchestration loop is enabled (`SHUTTLE_ORCHESTRATE=1`).
/// Absent / any other value keeps the safe single-shot default.
fn orchestration_enabled() -> bool {
    std::env::var("SHUTTLE_ORCHESTRATE")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

/// Hard cap on orchestrated attempts (`SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS`). Bounds
/// re-spawn cost; defaults to 2 (one cheap try + one escalation). Values `< 1`
/// or unparseable fall back to the default.
fn orchestrate_max_attempts() -> usize {
    std::env::var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(2)
}

/// The op name that drives the initial tier (`SHUTTLE_CELL_OP`). A mechanical op
/// (e.g. `rename`) starts cheap; anything else — including the default,
/// unknown `cell-run` — starts expensive (fail-safe, §7「绝不瞎猜」).
fn cell_op() -> String {
    std::env::var("SHUTTLE_CELL_OP")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "cell-run".to_string())
}

/// Map a completed run's telemetry into the uniform [`ToolOutcome`] envelope so
/// the router can classify it. The exit-code contract (see `run.rs`): 0 settled,
/// 124 soft timeout, 125 progress stall, `< 0` spawn failure, else child code.
fn outcome_from_telemetry(tel: &RunTelemetry) -> ToolOutcome {
    match tel.exit_code {
        0 => ToolOutcome::green("cell-run", TypedDetail::None),
        // Spawn failure — an unframeable hard error; a stronger tier cannot
        // repair a process that never started. Abort.
        code if code < 0 => {
            ToolOutcome::error("cell-run", reason::INTERNAL, "cell child failed to spawn")
        }
        // Soft timeout (124) / progress stall (125) / other non-zero: a real
        // but non-diagnosable failure. An unknown Blocked reason fails SAFE to
        // Escalate (§7「绝不瞎猜」), giving a stronger tier exactly one shot.
        _ => ToolOutcome::blocked(
            "cell-run",
            reason::INTERNAL,
            "run did not settle green",
            TypedDetail::None,
        ),
    }
}

/// Land the orchestration audit (cheap-first economics) as a shuttle-local sidecar
/// under `<root>/.shuttle/receipts/<base_run_id>.audit.json`. Best-effort: any I/O
/// failure is swallowed (audit is observability, never a run gate).
fn emit_orchestration_audit(
    workspace_root: &Path,
    task: &OrchestrationTask,
    result: &shuttle_protocol::OrchestrationResult,
) {
    let dir = workspace_root.join(".shuttle").join("receipts");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let safe: String = task
        .base_run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "cell-run".to_string()
    } else {
        safe
    };
    let path = dir.join(format!("{safe}.audit.json"));
    let audit = serde_json::json!({
        "schema": "shuttle_orchestration_audit/v1",
        "op": task.op,
        "escalated": result.escalated,
        "exhausted": result.exhausted,
        "final_tier": result.final_tier,
        "attempts": result.attempts,
        "dual_receipt": result.dual_receipt,
        "summary": result.summary(&task.op),
    });
    if let Ok(body) = serde_json::to_string_pretty(&audit) {
        let _ = std::fs::write(&path, body);
    }
}

/// The one outside-world dependency of the orchestrator: run a single caged
/// attempt. Each call rebuilds the fail-closed [`RunConfig`], injects the
/// tier's provider/model endpoint (dispatch layer), spawns pi once, and maps
/// the telemetry to a [`ToolOutcome`]. The last telemetry is stashed so
/// [`run_caged`] can return the settling attempt to the receipt/exit path.
struct CagedAttemptRunner<'a> {
    resolver: &'a ScopedPathResolver,
    binding: &'a CellBinding,
    prompt: String,
    model: Option<String>,
    last_telemetry: Option<RunTelemetry>,
}

impl AttemptRunner for CagedAttemptRunner<'_> {
    fn run(&mut self, req: &AttemptRequest) -> AttemptOutput {
        let mut cfg = build_caged_config(
            self.resolver,
            self.binding,
            self.prompt.clone(),
            self.model.clone(),
        );
        // Inject this tier's provider/model endpoint. Only present fields are
        // added, so an unconfigured tier leaves the child's env untouched.
        for (k, v) in shuttle_protocol::env_for_tier(req.tier) {
            cfg.child_env.push((k.to_string(), v));
        }
        let tel = run::supervise(&cfg).unwrap_or(RunTelemetry {
            termination: run::RunTermination::SpawnFailure,
            limit_reason: None,
            wall_ms: 0,
            tokens: None,
            usage: Default::default(),
            provider_cost: None,
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            event_count: 0,
            stdout_bytes: 0,
            tool_calls: 0,
            accept_rate: None,
            apply_total: 0,
            apply_ok: 0,
            exit_code: -1,
            ttft_ms: None,
            resources: None,
        });
        let outcome = outcome_from_telemetry(&tel);
        self.last_telemetry = Some(tel);
        // The deterministic per-attempt trace id doubles as this attempt's
        // provider_run_id so both legs stay independently traceable.
        AttemptOutput {
            outcome,
            provider_run_id: req.trace_run_id.clone(),
        }
    }
}

/// Assemble the fail-closed [`RunConfig`] cell-run spawns pi under.
///
/// Three guarantees a bare `shuttle run` does not give (fail-closed cage):
///   1. `exclude_tools = [bash]` — the model cell can never spawn a shell or
///      arbitrary command. Read-only tools and the native `write` tool remain
///      available so a planning cell (e.g. `splitter`) can emit spec files.
///      Goal verification commands are instead run after Pi exits by the
///      trusted parent driver from exact `allowed_commands` in one snapshot.
///   2. `child_env` carries `SHUTTLE_ENFORCE_SCOPE=1` + `SHUTTLE_ALLOWED_PATHS=<globs>`
///      so the injected extension appends `--enforce-scope --allowed-path …`
///      to every `shuttle apply`, closing the resolver=None escape of `shuttle run`.
///   3. `root` is the binding's canonical workspace root — the same subtree the
///      resolver confines writes to.
///
/// Provider + extension are still auto-injected via [`crate::config::resolve`]
/// (configured provider + `shuttle-tools.ts`); we then override the security-
/// relevant fields the cage must own.
/// The POSITIVE tool allowlist the cage exposes to pi (P0-1 / SP-10 K1: 正向
/// allowlist, 不采用负向策略). pi sees EXACTLY these tool names and nothing
/// else — host shell, host git/land, and any ambient extension are unreachable
/// by construction (that is what makes `tool_allowlist_exact` hold), because
/// the cage also renders `--no-builtin-tools --no-extensions` and only loads
/// the trusted shuttle extension via `-e`.
///
/// Baseline (six narrow tools):
///   * `read`            — read-only file access (the non-removable floor),
///   * `shuttle_locate`       — ripgrep / ast-grep lookup (read-only),
///   * `shuttle_repomap`      — repo structure map (read-only),
///   * `shuttle_context_pack` — bounded context assembly (read-only),
///   * `apply_edit_plan` — the ONLY write path, forced through the
///     ScopedPathResolver-enforced `shuttle apply`,
///   * `write`           — native write, kept so a planning cell (the splitter)
///     can emit its spec files.
///
/// `SHUTTLE_CELL_EXCLUDE_TOOLS` (comma-separated) can only TIGHTEN the cage: any
/// listed name is dropped from the allowlist. `read` is never removable, so the
/// resulting allowlist is never empty (fail-closed: an empty `--tools` would
/// otherwise be ambiguous). A leaf implementation cell sets
/// `SHUTTLE_CELL_EXCLUDE_TOOLS=write` (and/or `edit`) so the sole remaining write
/// path is `apply_edit_plan` under `--enforce-scope`. `bash`/host git/land are
/// never in the baseline, so they can never be granted.
fn caged_tools() -> Vec<String> {
    // `read` is the non-removable floor; the rest may be tightened away.
    const REMOVABLE: [&str; 5] = [
        "shuttle_locate",
        "shuttle_repomap",
        "shuttle_context_pack",
        "apply_edit_plan",
        "write",
    ];
    let mut removed: Vec<String> = Vec::new();
    if let Ok(extra) = std::env::var("SHUTTLE_CELL_EXCLUDE_TOOLS") {
        for t in extra.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            removed.push(t.to_string());
        }
    }
    let mut tools = vec!["read".to_string()];
    for t in REMOVABLE {
        if !removed.iter().any(|r| r == t) {
            tools.push(t.to_string());
        }
    }
    tools
}

const CELL_EXECUTION_DISCIPLINE: &str = "You are a bounded implementation worker. Inspect only the files needed to resolve concrete interfaces. By your eighth tool call, either apply the smallest coherent in-scope change or write the required self-report with evidence that no source change is needed. Do not spend the response reproducing a full design before making progress. Prefer several small validated edits over one very large patch. After implementation, run available checks when permitted, then write any required machine-readable self-report.";

fn build_caged_config(
    resolver: &ScopedPathResolver,
    binding: &CellBinding,
    prompt: String,
    model: Option<String>,
) -> RunConfig {
    let root = resolver.root().display().to_string();
    let mut cfg = crate::config::resolve(
        prompt,
        // cli_model: cell-run's --model pins the caged child's model (the cage
        // owns tool/scope policy, not model selection). None falls through to
        // shuttle.toml / built-in default, matching `shuttle run`.
        model,
        None,
        None,
        // The cage uses a POSITIVE allowlist (set below), not an
        // exclude-list, so pass no cli_exclude_tools here and clear any that
        // leaked in from shuttle.toml.
        None,
        None,
        root,
        false,
        // Cells are ephemeral: never persist a pi session for a one-shot run.
        true,
        Vec::new(),
    );
    cfg.mode = WrapMode::JsonSupervisor;
    cfg.passthrough.push("--append-system-prompt".into());
    cfg.passthrough.push(CELL_EXECUTION_DISCIPLINE.into());
    if let Some(session_file) = binding
        .session_file
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        cfg.no_session = false;
        let session_identity = binding
            .provider_run_id
            .as_deref()
            .unwrap_or(&binding.environment_id);
        cfg.session_id = Some(cell_registry::pi_session_id(session_identity));
        cfg.session_dir = Some(session_file.clone());
    }

    // P0-1 exact-allowlist seam: pi sees EXACTLY the narrow cell tools and
    // nothing else. `--no-builtin-tools` kills the built-in shell/edit/write,
    // `--no-extensions` blocks ambient extension discovery (the trusted shuttle
    // extension still loads via the `-e` path config::resolve injected), and
    // the positive `--tools` list is the sole enabled set. Host shell, host
    // git/land, and passthrough are unreachable by construction.
    cfg.tools = caged_tools();
    cfg.exclude_tools = Vec::new();
    cfg.no_builtin_tools = true;
    cfg.no_extensions = true;
    if let Some(descriptor) = binding.runtime_descriptor.as_ref() {
        cfg.provider_binary = Some(descriptor.provider.binary.clone());
        cfg.extension = Some(descriptor.provider.extension_path.clone());
    }

    // Propagate the enforced write boundary to the extension's `shuttle apply`.
    cfg.child_env
        .push(("SHUTTLE_ENFORCE_SCOPE".into(), "1".into()));
    cfg.child_env.push((
        "SHUTTLE_ALLOWED_PATHS".into(),
        binding.allowed_paths.join(","),
    ));
    cfg.child_env.push((
        "SHUTTLE_WORKSPACE_ROOT".into(),
        resolver.root().display().to_string(),
    ));
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `caged_exclude_tools` reads the process-global `SHUTTLE_CELL_EXCLUDE_TOOLS`.
    // cargo runs tests as parallel threads in one process, so all env-mutating
    // assertions are serialized under this mutex and the var is always cleared
    // on exit — otherwise a leaked value would poison sibling tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn minimal_binding() -> CellBinding {
        let json = r#"{
            "version": 1,
            "workspace_root": "/tmp/cell/ws",
            "allowed_paths": ["src/**", "docs/**"],
            "allowed_commands": [{"program": "git", "argv_prefix": ["status"]}],
            "base_oid": "abc123",
            "assignment_digest": "sha256:deadbeef",
            "environment_id": "env-1"
        }"#;
        CellBinding::parse(json).expect("minimal binding must parse")
    }

    fn project_binding(workspace_root: &Path) -> CellBinding {
        let digest = |ch: char| format!("sha256:{}", ch.to_string().repeat(64));
        let project_digest = digest('2');
        let mut approval = serde_json::json!({
            "schema_version": "shuttle_external_project_approval/v1",
            "decision": "approved",
            "external_commit_oid": "a".repeat(40),
            "provider_capability_snapshot_ref": "capability://one",
            "provider_capability_snapshot_digest": digest('6'),
            "project_binding_digest": project_digest
        });
        let approval_digest = format!(
            "sha256:{}",
            shuttle_protocol::sha256_hex(&serde_json::to_vec(&approval).unwrap())
        );
        approval["approval_ref"] = serde_json::json!("approval://one");
        approval["approval_digest"] = serde_json::json!(approval_digest);
        let json = serde_json::json!({
            "version": 1,
            "workspace_root": workspace_root.display().to_string(),
            "allowed_paths": ["src/**"],
            "allowed_commands": [{"program": "git", "argv_prefix": ["status"]}],
            "base_oid": "a".repeat(40),
            "assignment_digest": digest('1'),
            "project": {
                "mode": "project_bound",
                "project_id": "project-a",
                "authorization_scope": "auth/project-a",
                "assignment_ref": "assignment://one",
                "execution_assignment_digest": digest('7'),
                "project_binding_ref": "project-binding://one",
                "project_binding_digest": project_digest,
                "workspace_binding_ref": "workspace-binding://one",
                "workspace_binding_digest": digest('3'),
                "repo_id": "shuttle",
                "provider_workspace_root": workspace_root.display().to_string(),
                "provider_allowed_write_scopes": ["src/**"],
                "provider_receipt_digest": digest('4'),
                "approved_external_inputs": approval
            },
            "environment_id": "env-project-a"
        });
        CellBinding::parse(&serde_json::to_string(&json).unwrap()).unwrap()
    }

    #[test]
    fn prepare_cell_validates_project_binding_before_workspace_exposure() {
        let nonexistent = std::env::temp_dir().join("shuttle_project_binding_must_not_be_opened");
        let mut binding = project_binding(&nonexistent);
        binding.workspace_root = std::env::temp_dir().display().to_string();

        let error = prepare_cell(binding).unwrap_err().to_string();
        assert!(error.contains("before workspace exposure"));
        assert!(error.contains("provider workspace binding"));
        assert!(
            !error.contains("canonicalize"),
            "binding drift must be rejected before resolving either root"
        );
    }

    fn verifier_binding(root: &Path, program: &str, prefix: Vec<&str>) -> CellBinding {
        CellBinding {
            version: 1,
            workspace_root: root.display().to_string(),
            allowed_paths: Vec::new(),
            allowed_commands: vec![shuttle_protocol::AllowedCommand {
                program: program.into(),
                argv_prefix: Some(prefix.into_iter().map(str::to_owned).collect()),
            }],
            base_oid: "oid".into(),
            assignment_digest: format!("sha256:{}", "a".repeat(64)),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            shuttle_binary_digest: None,
            environment_id: "verify-test".into(),
            provider_run_id: None,
            attempt_id: None,
            runtime_id: None,
            package_id: None,
            profile_ref: None,
            session_file: None,
            repo_set: None,
            project: Default::default(),
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("tracked.txt"), "original\n").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        dir
    }

    #[test]
    fn verifier_commands_run_in_one_snapshot_without_writing_back() {
        let dir = git_fixture();
        let binding = verifier_binding(dir.path(), "rm", vec!["tracked.txt"]);
        let records =
            run_allowed_commands(&binding, 10_000, Duration::from_secs(30), || Ok(())).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["exit_code"], 0);
        assert_eq!(records[0]["workspace_mode"], "disposable_snapshot");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn prepare_cell_roots_resolver_at_provider_workspace() {
        let root = std::env::temp_dir().join(format!(
            "shuttle_project_workspace_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let binding = project_binding(&root);

        let (prepared, resolver) = prepare_cell(binding).unwrap();
        assert_eq!(resolver.root(), std::fs::canonicalize(&root).unwrap());
        assert_eq!(prepared.project.mode(), "project_bound");
        assert_eq!(
            prepared.project.binding_digest().unwrap().as_str(),
            format!("sha256:{}", "2".repeat(64))
        );
        assert!(resolver
            .resolve("src/lib.rs")
            .unwrap()
            .starts_with(std::fs::canonicalize(&root).unwrap()));
        assert!(resolver.resolve("../sibling.txt").is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verifier_command_scrubs_secret_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = git_fixture();
        let binding = verifier_binding(dir.path(), "env", Vec::new());
        std::env::set_var("GOAL_TEST_API_KEY", "must-not-leak");
        let records =
            run_allowed_commands(&binding, 10_000, Duration::from_secs(30), || Ok(())).unwrap();
        std::env::remove_var("GOAL_TEST_API_KEY");
        assert_eq!(records[0]["exit_code"], 0);
        assert!(!records[0]["stdout"]
            .as_str()
            .unwrap()
            .contains("must-not-leak"));
    }

    #[test]
    fn verifier_command_output_and_timeout_are_bounded() {
        let dir = git_fixture();
        let binding = verifier_binding(dir.path(), "yes", vec!["x"]);
        let records =
            run_allowed_commands(&binding, 100, Duration::from_secs(30), || Ok(())).unwrap();
        assert_eq!(records[0]["exit_code"], 124);
        assert!(records[0]["stdout"].as_str().unwrap().len() <= 64 * 1024);
    }

    #[test]
    fn caged_tools_is_positive_allowlist_bash_never_grantable() {
        let _guard = ENV_LOCK.lock().unwrap();

        // 1. Baseline: exactly the six narrow tools, positively enumerated.
        //    `read` first (the non-removable floor); host shell / git / land
        //    are absent by construction — a positive allowlist cannot grant a
        //    name it does not list.
        std::env::remove_var("SHUTTLE_CELL_EXCLUDE_TOOLS");
        let tools = caged_tools();
        assert_eq!(
            tools,
            vec![
                "read".to_string(),
                "shuttle_locate".to_string(),
                "shuttle_repomap".to_string(),
                "shuttle_context_pack".to_string(),
                "apply_edit_plan".to_string(),
                "write".to_string(),
            ]
        );
        assert!(
            !tools.iter().any(|t| t == "bash"),
            "bash is never grantable"
        );

        // 2. A leaf implementation cell drops `write`; the sole write path left
        //    is `apply_edit_plan` (forced through --enforce-scope). The override
        //    can only TIGHTEN — it can never introduce bash or any host tool.
        std::env::set_var("SHUTTLE_CELL_EXCLUDE_TOOLS", "write");
        let tools = caged_tools();
        assert!(!tools.iter().any(|t| t == "write"), "write tightened away");
        assert!(tools.contains(&"apply_edit_plan".to_string()));
        assert!(tools.contains(&"read".to_string()));
        assert!(!tools.iter().any(|t| t == "bash"));

        // 3. `read` is the non-removable floor: even if a caller tries to strip
        //    everything, the allowlist is never empty (an empty `--tools` would
        //    be ambiguous / fail-open).
        std::env::set_var(
            "SHUTTLE_CELL_EXCLUDE_TOOLS",
            "read,shuttle_locate,shuttle_repomap,shuttle_context_pack,apply_edit_plan,write",
        );
        let tools = caged_tools();
        assert_eq!(tools, vec!["read".to_string()], "read is the floor");

        std::env::remove_var("SHUTTLE_CELL_EXCLUDE_TOOLS");
    }

    #[test]
    fn build_caged_config_enforces_scope_env_and_supervisor_mode() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SHUTTLE_CELL_EXCLUDE_TOOLS");

        // A canonicalizable root is required by ScopedPathResolver::new.
        let root = std::env::temp_dir();
        let binding = minimal_binding();
        let resolver = ScopedPathResolver::new(&root, binding.allowed_paths.clone())
            .expect("temp_dir must canonicalize");

        let cfg = build_caged_config(&resolver, &binding, "do the task".to_string(), None);

        // Cage guarantee 1 (P0-1 positive allowlist): pi sees EXACTLY the
        // narrow tool set; built-in tools and ambient extensions are off, so
        // bash / host git / land are unreachable by construction.
        assert_eq!(cfg.tools, caged_tools());
        assert!(
            !cfg.tools.iter().any(|t| t == "bash"),
            "bash never in allowlist"
        );
        assert!(cfg.no_builtin_tools, "built-in tools disabled");
        assert!(cfg.no_extensions, "ambient extension discovery disabled");
        assert!(
            cfg.exclude_tools.is_empty(),
            "cage uses allowlist, not exclude-list"
        );

        // Cage guarantee 2: the enforced write boundary is propagated to the
        // extension's `shuttle apply` via child_env.
        let enforce = cfg
            .child_env
            .iter()
            .find(|(k, _)| k == "SHUTTLE_ENFORCE_SCOPE")
            .map(|(_, v)| v.as_str());
        assert_eq!(enforce, Some("1"));

        let allowed = cfg
            .child_env
            .iter()
            .find(|(k, _)| k == "SHUTTLE_ALLOWED_PATHS")
            .map(|(_, v)| v.as_str());
        assert_eq!(allowed, Some("src/**,docs/**"));

        let workspace_root = cfg
            .child_env
            .iter()
            .find(|(k, _)| k == "SHUTTLE_WORKSPACE_ROOT")
            .map(|(_, v)| v.as_str());
        let canonical_root = root.canonicalize().expect("temp root must canonicalize");
        assert_eq!(
            workspace_root,
            Some(canonical_root.to_string_lossy().as_ref())
        );

        // Convergence form (b): cell-run drives pi under the JSON supervisor.
        assert!(matches!(cfg.mode, WrapMode::JsonSupervisor));

        // Bounded workers receive generic execution-first guidance. It is a
        // run policy, not case-specific prompt content.
        let guidance = cfg
            .passthrough
            .windows(2)
            .find(|pair| pair[0] == "--append-system-prompt")
            .map(|pair| pair[1].as_str());
        assert_eq!(guidance, Some(CELL_EXECUTION_DISCIPLINE));

        // Cage guarantee 3: writes are confined to the binding's canonical root.
        assert_eq!(
            cfg.root,
            std::fs::canonicalize(&root).unwrap().display().to_string()
        );
    }

    fn tel_exit(code: i32) -> RunTelemetry {
        RunTelemetry {
            termination: run::RunTermination::ChildExit,
            limit_reason: None,
            wall_ms: 1,
            tokens: None,
            usage: Default::default(),
            provider_cost: None,
            provider: None,
            model: None,
            event_count: 0,
            stdout_bytes: 0,
            tool_calls: 0,
            accept_rate: None,
            exit_code: code,
            ttft_ms: None,
            resources: None,
            apply_ok: 0,
            apply_total: 0,
        }
    }

    #[test]
    fn telemetry_maps_to_uniform_outcome_states() {
        use shuttle_protocol::OutcomeState;
        // green: settled.
        assert_eq!(
            outcome_from_telemetry(&tel_exit(0)).state,
            OutcomeState::Green
        );
        // soft timeout / stall / other non-zero: Blocked (fails safe → Escalate).
        assert_eq!(
            outcome_from_telemetry(&tel_exit(124)).state,
            OutcomeState::Blocked
        );
        assert_eq!(
            outcome_from_telemetry(&tel_exit(125)).state,
            OutcomeState::Blocked
        );
        assert_eq!(
            outcome_from_telemetry(&tel_exit(1)).state,
            OutcomeState::Blocked
        );
        // spawn failure: hard Error (→ Abort).
        assert_eq!(
            outcome_from_telemetry(&tel_exit(-1)).state,
            OutcomeState::Error
        );
    }

    #[test]
    fn telemetry_mapping_drives_orchestrate_escalation() {
        use shuttle_protocol::ModelTier;
        // A mechanical op starts cheap; the cheap attempt stalls (rc=125) →
        // Blocked/unknown → Escalate; the expensive attempt greens (rc=0) →
        // Accept. This exercises the full seam (telemetry→outcome→route→loop)
        // minus the real pi spawn.
        let mut n = 0;
        let mut runner = |req: &AttemptRequest| {
            n += 1;
            let tel = if n == 1 {
                assert_eq!(req.tier, ModelTier::Cheap);
                tel_exit(125)
            } else {
                assert_eq!(req.tier, ModelTier::Expensive);
                tel_exit(0)
            };
            AttemptOutput {
                outcome: outcome_from_telemetry(&tel),
                provider_run_id: req.trace_run_id.clone(),
            }
        };
        let task = OrchestrationTask {
            op: "rename".into(),
            base_run_id: "env-1".into(),
            idempotency_key: "dig".into(),
        };
        let opts = OrchestrationOptions {
            max_attempts: 3,
            ..OrchestrationOptions::default()
        };
        let r = orchestrate(&task, &mut runner, &opts);
        assert!(r.is_green());
        assert!(r.escalated);
        assert_eq!(r.attempt_count(), 2);
        assert_eq!(r.final_tier, ModelTier::Expensive);
        // The escalated attempt minted a fresh idempotency key (B4).
        assert_eq!(r.attempts[0].idempotency_key, "dig");
        assert_eq!(r.attempts[1].idempotency_key, "dig-esc1");
    }

    #[test]
    fn spawn_failure_aborts_without_escalation() {
        use shuttle_protocol::RouteAction;
        // rc<0 → Error → Abort: escalation cannot repair a process that never
        // started, so the loop stops on the first attempt.
        let mut runner = |req: &AttemptRequest| AttemptOutput {
            outcome: outcome_from_telemetry(&tel_exit(-1)),
            provider_run_id: req.trace_run_id.clone(),
        };
        let task = OrchestrationTask {
            op: "rename".into(),
            base_run_id: "env-1".into(),
            idempotency_key: "dig".into(),
        };
        let r = orchestrate(&task, &mut runner, &OrchestrationOptions::default());
        assert!(!r.is_green());
        assert_eq!(r.attempt_count(), 1);
        assert_eq!(r.attempts[0].action, RouteAction::Abort);
        assert!(!r.escalated);
    }

    #[test]
    fn orchestration_env_knobs_parse() {
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::remove_var("SHUTTLE_ORCHESTRATE");
        assert!(
            !orchestration_enabled(),
            "absent = safe single-shot default"
        );
        std::env::set_var("SHUTTLE_ORCHESTRATE", "1");
        assert!(orchestration_enabled());
        std::env::set_var("SHUTTLE_ORCHESTRATE", "true");
        assert!(orchestration_enabled());
        std::env::set_var("SHUTTLE_ORCHESTRATE", "0");
        assert!(!orchestration_enabled());
        std::env::remove_var("SHUTTLE_ORCHESTRATE");

        std::env::remove_var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS");
        assert_eq!(orchestrate_max_attempts(), 2, "default cap");
        std::env::set_var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS", "5");
        assert_eq!(orchestrate_max_attempts(), 5);
        std::env::set_var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS", "0");
        assert_eq!(orchestrate_max_attempts(), 2, "< 1 rejected → default");
        std::env::set_var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS", "notanumber");
        assert_eq!(orchestrate_max_attempts(), 2, "unparseable → default");
        std::env::remove_var("SHUTTLE_ORCHESTRATE_MAX_ATTEMPTS");

        std::env::remove_var("SHUTTLE_CELL_OP");
        assert_eq!(
            cell_op(),
            "cell-run",
            "default op (non-mechanical → expensive)"
        );
        std::env::set_var("SHUTTLE_CELL_OP", "rename");
        assert_eq!(cell_op(), "rename");
        std::env::remove_var("SHUTTLE_CELL_OP");
    }
}
