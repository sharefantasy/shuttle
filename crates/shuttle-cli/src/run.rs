//! `shuttle run` — the supervisor front controller.
//!
//! Architecture (see ): `shuttle` is the host and
//! `pi` is a replaceable LLM planning loop spawned as a child. This module owns
//! the **argument layer**: it turns an `shuttle run` invocation into a concrete `pi`
//! command line (form B = json supervisor by default, form C = rpc `--deep`),
//! disabling pi's built-in edit/write and injecting the shuttle extension so every
//! disk write and code lookup flows through shuttle's fast path.
//!
//! Form B (json supervisor, W1/W2): spawn `pi -p --mode json`, parse the JSONL
//! event stream into telemetry, forward it verbatim, apply a soft timeout.
//!
//! Form C (rpc deep-drive, W6): spawn `pi --mode rpc`, drive the single turn
//! over JSON-RPC — feed the prompt as a `{"type":"prompt"}` command on pi's
//! stdin, fold the same event stream off pi's stdout, and (crucially) act as the
//! supervisor by auto-answering pi's `extension_ui_request` dialogs so a headless
//! run never deadlocks on a confirm/select no human is there to click. The turn
//! ends on `agent_settled`; closing stdin then shuts pi down gracefully. This is
//! the control seam where shuttle can later inject repo-map context mid-stream,
//! rewrite tool results, or retry multiple candidates.
//!
//! No LLM is embedded in shuttle — planning is always the single model inside pi
//! (keeps the eval A/B fair).
//!
//! Credential handling: shuttle never reads provider secrets itself. It only helps
//! the caller layer model/provider flags and leaves the actual secret
//! resolution to the spawned pi process and its provider configuration.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Bound the supervisor's in-memory JSONL handoff. A slow stdout consumer now
// backpressures the reader after 32 events, and an unexpectedly large event is
// rejected before `String` allocation can grow without limit.
const PI_EVENT_QUEUE_CAPACITY: usize = 32;
const MAX_PI_EVENT_LINE_BYTES: usize = 1024 * 1024;
const DEFAULT_SOFT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const DEFAULT_STALL_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const DEFAULT_MAX_PI_EVENTS: u64 = 100_000;
const DEFAULT_MAX_PI_STDOUT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_PI_TOKENS: u64 = 2_000_000;
const DEFAULT_MAX_PROVIDER_COST: f64 = 25.0;

#[cfg(unix)]
static TERMINATION_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn record_termination_signal(signal: libc::c_int) {
    TERMINATION_SIGNAL.store(signal, Ordering::SeqCst);
}

#[cfg(unix)]
struct TerminationSignalGuard {
    previous_term: libc::sigaction,
    previous_int: libc::sigaction,
}

#[cfg(unix)]
impl TerminationSignalGuard {
    fn install() -> anyhow::Result<Self> {
        TERMINATION_SIGNAL.store(0, Ordering::SeqCst);
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_termination_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            let mut previous_term: libc::sigaction = std::mem::zeroed();
            let mut previous_int: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &mut previous_term) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if libc::sigaction(libc::SIGINT, &action, &mut previous_int) != 0 {
                libc::sigaction(libc::SIGTERM, &previous_term, std::ptr::null_mut());
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                previous_term,
                previous_int,
            })
        }
    }
}

#[cfg(unix)]
impl Drop for TerminationSignalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
        }
        TERMINATION_SIGNAL.store(0, Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn install_termination_signal_guard() -> anyhow::Result<TerminationSignalGuard> {
    TerminationSignalGuard::install()
}

#[cfg(not(unix))]
struct TerminationSignalGuard;

#[cfg(not(unix))]
fn install_termination_signal_guard() -> anyhow::Result<TerminationSignalGuard> {
    Ok(TerminationSignalGuard)
}

fn termination_signal() -> Option<i32> {
    #[cfg(unix)]
    {
        let signal = TERMINATION_SIGNAL.load(Ordering::SeqCst);
        (signal != 0).then_some(signal)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Wrapper mode = which pi transport shuttle drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    /// Form B (default): `pi -p --mode json`, shuttle parses the JSON event stream.
    JsonSupervisor,
    /// Form C: `pi --mode rpc`, shuttle drives pi turn-by-turn over JSON-RPC.
    RpcDeep,
}

impl WrapMode {
    fn pi_mode(self) -> &'static str {
        match self {
            WrapMode::JsonSupervisor => "json",
            WrapMode::RpcDeep => "rpc",
        }
    }
}

/// Everything needed to render a `pi` invocation and supervise it.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The user's requirement / prompt (last positional).
    pub prompt: String,
    /// Optional model pattern; when absent, pi uses its own configured default.
    pub model: Option<String>,
    /// pi provider to select. None = don't emit `--provider`.
    pub provider: Option<String>,
    /// Thinking level: off/minimal/low/medium/high/xhigh/max.
    pub thinking: String,
    /// Built-in pi tools to disable — shuttle takes these over.
    pub exclude_tools: Vec<String>,
    /// Positive tool allowlist (`--tools`). When non-empty, pi enables ONLY
    /// these tool names (across built-in + extension + custom) — the
    /// exact-allowlist seam the fail-closed cell cage relies on (SP-10 K1).
    /// Empty = omit the flag (bare `shuttle run` keeps its legacy behaviour).
    pub tools: Vec<String>,
    /// Disable pi's built-in tools by default (`--no-builtin-tools`). The cage
    /// sets this so no built-in shell/edit/write leaks in outside `tools`.
    pub no_builtin_tools: bool,
    /// Disable ambient extension discovery (`--no-extensions`); an explicit
    /// `-e` path still loads. The cage sets this so ONLY the trusted shuttle
    /// extension is present.
    pub no_extensions: bool,
    /// Path to the shuttle extension file to inject via `-e` (exposes apply_edit_plan…).
    pub extension: Option<String>,
    /// Exact provider executable selected by a digest-bound runtime
    /// descriptor. `None` preserves the legacy `PI_BIN`/PATH behavior.
    pub provider_binary: Option<String>,
    /// Repo root passed to shuttle-side tools.
    pub root: String,
    /// Wrapper transport.
    pub mode: WrapMode,
    /// Don't persist a pi session (ephemeral eval runs).
    pub no_session: bool,
    /// Stable session id for a durable cell run. Mutually exclusive with
    /// `no_session`; callers pair it with `session_dir`.
    pub session_id: Option<String>,
    /// Provider-owned directory used for durable pi session lookup.
    pub session_dir: Option<String>,
    /// Raw pi args after `--` (escape hatch: never lose a pi capability).
    pub passthrough: Vec<String>,
    /// Extra environment variables exported to the pi child on top of `SHUTTLE_BIN`.
    /// `cell-run` uses this to hand the injected extension the enforced write
    /// boundary (`SHUTTLE_ENFORCE_SCOPE` / `SHUTTLE_ALLOWED_PATHS`) so every
    /// `apply_edit_plan` shells `shuttle apply --enforce-scope --allowed-path …`.
    /// `shuttle run` leaves it empty (legacy raw-path behaviour).
    pub child_env: Vec<(String, String)>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            prompt: String::new(),
            model: None,
            provider: None,
            thinking: "off".into(),
            exclude_tools: vec!["edit".into(), "write".into()],
            tools: Vec::new(),
            no_builtin_tools: false,
            no_extensions: false,
            extension: None,
            provider_binary: None,
            root: ".".into(),
            mode: WrapMode::JsonSupervisor,
            no_session: false,
            session_id: None,
            session_dir: None,
            passthrough: Vec::new(),
            child_env: Vec::new(),
        }
    }
}

impl RunConfig {
    /// Render the full pi argument vector (excluding argv[0] = the pi binary).
    ///
    /// Order is deterministic so it can be unit-tested byte-for-byte without
    /// spawning pi or touching the network.
    pub fn render_pi_args(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();

        // Non-interactive print mode is implied for json; rpc drives its own loop.
        if self.mode == WrapMode::JsonSupervisor {
            a.push("-p".into());
        }
        a.push("--mode".into());
        a.push(self.mode.pi_mode().into());

        if let Some(model) = &self.model {
            a.push("--model".into());
            a.push(model.clone());
        }

        a.push("--thinking".into());
        a.push(self.thinking.clone());

        if let Some(p) = &self.provider {
            a.push("--provider".into());
            a.push(p.clone());
        }

        if !self.exclude_tools.is_empty() {
            a.push("--exclude-tools".into());
            a.push(self.exclude_tools.join(","));
        }

        // Positive allowlist + built-in/extension lockdown (the cell cage seam).
        // Rendered after the exclude list; a bare `shuttle run` leaves all three
        // empty/false so its arg vector is byte-for-byte unchanged.
        if !self.tools.is_empty() {
            a.push("--tools".into());
            a.push(self.tools.join(","));
        }
        if self.no_builtin_tools {
            a.push("--no-builtin-tools".into());
        }
        if self.no_extensions {
            a.push("--no-extensions".into());
        }

        if let Some(ext) = &self.extension {
            a.push("-e".into());
            a.push(ext.clone());
        }

        if self.no_session {
            a.push("--no-session".into());
        } else {
            if let Some(dir) = &self.session_dir {
                a.push("--session-dir".into());
                a.push(dir.clone());
            }
            if let Some(id) = &self.session_id {
                a.push("--session-id".into());
                a.push(id.clone());
            }
        }

        // Passthrough escape hatch: raw pi flags win by coming last (before prompt).
        a.extend(self.passthrough.iter().cloned());

        // Prompt is the final positional; rpc mode feeds it over the wire instead.
        if self.mode == WrapMode::JsonSupervisor && !self.prompt.is_empty() {
            a.push(self.prompt.clone());
        }

        a
    }

    /// The provider binary to invoke. A runtime descriptor wins absolutely;
    /// descriptor-free launches preserve the legacy `PI_BIN`/PATH behavior.
    pub fn pi_bin(&self) -> String {
        self.provider_binary
            .clone()
            .unwrap_or_else(|| std::env::var("PI_BIN").unwrap_or_else(|_| "pi".into()))
    }

    /// Absolute path to the current `shuttle` executable, exported to the pi child
    /// as `SHUTTLE_BIN` so the injected extension (`apply_edit_plan`,
    /// `conflict_propose`, `shuttle_locate`, ...) can shell back into shuttle rather than
    /// silently falling back to a raw bash edit. Honors an explicit `SHUTTLE_BIN`
    /// override first, then `current_exe()`, then bare `"shuttle"`.
    pub fn shuttle_bin_for_child() -> String {
        std::env::var("SHUTTLE_BIN")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|pth| pth.to_str().map(String::from))
                    .unwrap_or_else(|| "shuttle".into())
            })
    }
}

/// Provider token usage observed from completed assistant messages. Missing
/// provider fields remain `None`; zero is reserved for an explicit provider
/// observation of zero.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct UsageBreakdown {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Cost values reported by the provider through pi. This is an observation,
/// not SHUTTLE's billing calculation; callers that need an auditable cost
/// derive it independently from their own rate card.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ProviderCostObservation {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub total: Option<f64>,
    pub currency: Option<String>,
}

/// Why the supervisor stopped the provider process. This is deliberately
/// independent from `exit_code`: a provider may itself return 124, which must
/// not be misreported as an SHUTTLE soft timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTermination {
    Settled,
    SoftTimeout,
    StallTimeout,
    ResourceLimit,
    StreamRejected,
    Interrupted,
    ChildExit,
    SpawnFailure,
}

impl RunTermination {
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::SoftTimeout => "soft_timeout",
            Self::StallTimeout => "stall_timeout",
            Self::ResourceLimit => "resource_limit",
            Self::StreamRejected => "stream_rejected",
            Self::Interrupted => "interrupted",
            Self::ChildExit => "child_exit",
            Self::SpawnFailure => "spawn_failure",
        }
    }
}

/// One-shot telemetry shuttle emits for every run (feeds eval B arm directly).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunTelemetry {
    /// Supervisor-owned terminal classification. Never infer this from the
    /// numeric exit code.
    pub termination: RunTermination,
    /// Machine-readable resource guard that ended the run, if any.
    pub limit_reason: Option<String>,
    pub wall_ms: u128,
    /// Total model tokens, parsed from the pi event stream.
    pub tokens: Option<u64>,
    /// Lossless field-level usage where the provider supplied it.
    pub usage: UsageBreakdown,
    /// Provider-reported cost, if present. Never treated as a rate-card truth.
    pub provider_cost: Option<ProviderCostObservation>,
    /// Effective provider/model selected for this invocation, when known.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Number of decoded pi events folded into this telemetry.
    pub event_count: u64,
    /// Raw JSONL bytes accepted and forwarded by the supervisor.
    pub stdout_bytes: u64,
    /// Tool-call count.
    pub tool_calls: u32,
    /// Fraction of apply ops accepted (from shuttle apply outcomes).
    pub accept_rate: Option<f64>,
    /// Raw apply-family attempts (denominator of `accept_rate`). Surfaced so a
    /// caller composing runs — e.g. the `shuttle goal` outer loop's per-task execute
    /// seam — can aggregate proposed/accepted counts honestly rather than
    /// re-deriving them from a rounded rate (§7 never-invent).
    pub apply_total: u32,
    /// Raw apply-family successes (numerator of `accept_rate`).
    pub apply_ok: u32,
    pub exit_code: i32,
    /// Time-to-first-token: spawn -> first token-bearing pi event (ms).
    /// None when no token ever arrived (cold-start stall / rc=125 tokens=0),
    /// which is exactly the A_cold provider-handshake failure signature.
    pub ttft_ms: Option<u128>,
    /// Best-effort host/process resource peaks sampled across the run. `None`
    /// when nothing landed and no guard tripped (honest absence, §7): the
    /// probes degrade to `None` whenever `ps`/`vm_stat`/`sysctl` are
    /// unavailable rather than fabricating a number.
    pub resources: Option<crate::resource::RunResources>,
}

impl RunTelemetry {
    pub(crate) fn errored(wall_ms: u128) -> Self {
        RunTelemetry {
            termination: RunTermination::SpawnFailure,
            limit_reason: None,
            wall_ms,
            tokens: None,
            usage: UsageBreakdown::default(),
            provider_cost: None,
            provider: None,
            model: None,
            event_count: 0,
            stdout_bytes: 0,
            tool_calls: 0,
            accept_rate: None,
            apply_total: 0,
            apply_ok: 0,
            exit_code: -1,
            ttft_ms: None,
            resources: None,
        }
    }
}

fn effective_provider(cfg: &RunConfig) -> Option<String> {
    cfg.provider.clone().or_else(|| {
        cfg.model
            .as_deref()
            .and_then(|model| model.split_once('/'))
            .map(|(provider, _)| provider.to_string())
    })
}

const HERDR_SOURCE: &str = "custom:shuttle";
const HERDR_AGENT: &str = "shuttle";
const HERDR_DISPLAY_AGENT: &str = "shuttle";
const HERDR_TITLE: &str = "shuttle";

/// Minimal Herdr bridge for `shuttle run`.
///
/// The built-in Herdr `pi` integration only reports TUI sessions, while
/// `shuttle`'s main path is `shuttle run -> pi -p --mode json|rpc`, i.e.
/// a headless child pi behind an `shuttle` supervisor. Report `shuttle run` itself as a
/// custom pane agent so Herdr can label the pane while the command is active.
#[derive(Debug)]
struct HerdrReporter {
    pane_id: String,
    session_id: String,
    seq: u64,
}

impl HerdrReporter {
    fn detect_pane_id_from_env() -> Option<String> {
        if let Ok(pane_id) = std::env::var("HERDR_PANE_ID") {
            if !pane_id.is_empty() {
                return Some(pane_id);
            }
        }
        None
    }

    fn detect_pane_id_from_current() -> Option<String> {
        let out = Command::new("herdr")
            .args(["pane", "current", "--current"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        json.pointer("/result/pane/pane_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn parent_pid() -> Option<u32> {
        let pid = std::process::id().to_string();
        let ps = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Ps).ok()?;
        let out = Command::new(ps)
            .args(["-o", "ppid=", "-p", &pid])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    }

    fn detect_pane_id_from_parent_shell() -> Option<String> {
        let parent_pid = Self::parent_pid()?;
        let out = Command::new("herdr")
            .args(["api", "snapshot"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let pane_ids: Vec<String> = json
            .pointer("/result/snapshot/panes")
            .and_then(serde_json::Value::as_array)?
            .iter()
            .filter_map(|pane| {
                pane.get("pane_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect();

        for pane_id in pane_ids {
            let out = Command::new("herdr")
                .args(["pane", "process-info", "--pane", &pane_id])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !out.status.success() {
                continue;
            }
            let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
            let shell_pid = json
                .pointer("/result/process_info/shell_pid")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as u32);
            if shell_pid == Some(parent_pid) {
                return Some(pane_id);
            }
        }
        None
    }

    fn detect_pane_id() -> Option<(String, &'static str)> {
        if let Some(pane_id) = Self::detect_pane_id_from_env() {
            return Some((pane_id, "env"));
        }
        if let Some(pane_id) = Self::detect_pane_id_from_current() {
            return Some((pane_id, "current"));
        }
        if let Some(pane_id) = Self::detect_pane_id_from_parent_shell() {
            return Some((pane_id, "parent_shell"));
        }
        None
    }

    fn maybe_new() -> Option<Self> {
        if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
            return None;
        }
        let (pane_id, source) = Self::detect_pane_id()?;
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        tracing::info!(pane_id = %pane_id, source, "shuttle run: herdr bridge attached");
        Some(Self {
            pane_id,
            session_id: format!("shuttle-{}-{started_ms}", std::process::id()),
            seq: started_ms.saturating_mul(1000),
        })
    }

    fn next_seq(&mut self) -> String {
        self.seq = self.seq.saturating_add(1);
        self.seq.to_string()
    }

    fn run_herdr(&self, args: &[String]) {
        for attempt in 1..=2 {
            let mut cmd = Command::new("herdr");
            cmd.args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            match cmd.status() {
                Ok(status) if status.success() => return,
                Ok(status) => {
                    tracing::warn!(
                        attempt,
                        code = status.code().unwrap_or(-1),
                        ?args,
                        "shuttle run: herdr report returned non-zero status"
                    );
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, ?args, "shuttle run: herdr report failed");
                }
            }
            if attempt == 1 {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    fn report_metadata(&mut self) {
        let args = vec![
            "pane".into(),
            "report-metadata".into(),
            self.pane_id.clone(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            HERDR_AGENT.into(),
            "--display-agent".into(),
            HERDR_DISPLAY_AGENT.into(),
            "--title".into(),
            HERDR_TITLE.into(),
            "--ttl-ms".into(),
            "30000".into(),
            "--seq".into(),
            self.next_seq(),
        ];
        self.run_herdr(&args);
    }

    fn report_session(&mut self) {
        let args = vec![
            "pane".into(),
            "report-agent-session".into(),
            self.pane_id.clone(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            HERDR_AGENT.into(),
            "--agent-session-id".into(),
            self.session_id.clone(),
            "--session-start-source".into(),
            "shuttle_run".into(),
            "--seq".into(),
            self.next_seq(),
        ];
        self.run_herdr(&args);
    }

    fn report_state(&mut self, state: &str, message: Option<&str>) {
        let mut args = vec![
            "pane".into(),
            "report-agent".into(),
            self.pane_id.clone(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            HERDR_AGENT.into(),
            "--state".into(),
            state.into(),
            "--agent-session-id".into(),
            self.session_id.clone(),
            "--seq".into(),
            self.next_seq(),
        ];
        if let Some(message) = message {
            args.push("--message".into());
            args.push(message.into());
        }
        self.run_herdr(&args);
    }

    fn start(&mut self) {
        self.report_metadata();
        self.report_session();
        self.report_state("working", None);
    }

    fn finish(&mut self, tel: Option<&RunTelemetry>, err: Option<&str>) {
        match tel {
            Some(tel) if tel.exit_code == 0 => self.report_state("idle", None),
            Some(tel) => self.report_state("unknown", Some(&format!("exit {}", tel.exit_code))),
            None => self.report_state("unknown", err),
        }

        let args = vec![
            "pane".into(),
            "release-agent".into(),
            self.pane_id.clone(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            HERDR_AGENT.into(),
            "--seq".into(),
            self.next_seq(),
        ];
        self.run_herdr(&args);
    }
}

/// Accumulator folded over the pi `--mode json` JSONL event stream.
///
/// Kept as a pure struct + `ingest_event` fn so the parser is unit-testable
/// against synthetic fixtures without spawning pi or touching the network.
#[derive(Debug, Default, PartialEq)]
struct StreamAcc {
    usage: UsageAcc,
    provider_cost: ProviderCostAcc,
    event_count: u64,
    tool_calls: u32,
    apply_total: u32,
    apply_ok: u32,
}

impl StreamAcc {
    fn tokens_opt(&self) -> Option<u64> {
        self.usage.total.value()
    }

    fn usage(&self) -> UsageBreakdown {
        UsageBreakdown {
            input_tokens: self.usage.input.value(),
            output_tokens: self.usage.output.value(),
            cache_read_tokens: self.usage.cache_read.value(),
            cache_write_tokens: self.usage.cache_write.value(),
            reasoning_tokens: self.usage.reasoning.value(),
            total_tokens: self.usage.total.value(),
        }
    }

    fn provider_cost(&self) -> Option<ProviderCostObservation> {
        let value = ProviderCostObservation {
            input: self.provider_cost.input.value(),
            output: self.provider_cost.output.value(),
            cache_read: self.provider_cost.cache_read.value(),
            cache_write: self.provider_cost.cache_write.value(),
            total: self.provider_cost.total.value(),
            currency: None,
        };
        if value.input.is_none()
            && value.output.is_none()
            && value.cache_read.is_none()
            && value.cache_write.is_none()
            && value.total.is_none()
        {
            None
        } else {
            Some(value)
        }
    }

    fn accept_rate(&self) -> Option<f64> {
        if self.apply_total > 0 {
            Some(f64::from(self.apply_ok) / f64::from(self.apply_total))
        } else {
            None
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct CountAcc {
    total: u64,
    seen: bool,
}

impl CountAcc {
    fn add(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.total = self.total.saturating_add(value);
            self.seen = true;
        }
    }

    fn value(&self) -> Option<u64> {
        self.seen.then_some(self.total)
    }
}

#[derive(Debug, Default, PartialEq)]
struct AmountAcc {
    total: f64,
    seen: bool,
}

impl AmountAcc {
    fn add(&mut self, value: Option<f64>) {
        if let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) {
            self.total += value;
            self.seen = true;
        }
    }

    fn value(&self) -> Option<f64> {
        self.seen.then_some(self.total)
    }
}

#[derive(Debug, Default, PartialEq)]
struct UsageAcc {
    input: CountAcc,
    output: CountAcc,
    cache_read: CountAcc,
    cache_write: CountAcc,
    reasoning: CountAcc,
    total: CountAcc,
}

#[derive(Debug, Default, PartialEq)]
struct ProviderCostAcc {
    input: AmountAcc,
    output: AmountAcc,
    cache_read: AmountAcc,
    cache_write: AmountAcc,
    total: AmountAcc,
}

/// Fold a single decoded pi event into the accumulator.
///
/// Event shapes (from `pi -p --mode json`, identical after `toJsonEvent` in
/// `--mode rpc`):
///   · `message_end`  { message: { role, usage: { totalTokens } } }
///        — assistant messages carry per-response token usage; sum them so the
///          total covers every LLM round (turn_end / agent_end re-report the
///          last message, so we ignore those to avoid double counting).
///   · `tool_execution_end` { toolName, isError }
///        — one per completed tool call; apply-family tools additionally feed
///          the accept rate (ok vs errored).
fn ingest_event(acc: &mut StreamAcc, v: &serde_json::Value) {
    acc.event_count = acc.event_count.saturating_add(1);
    match v.get("type").and_then(serde_json::Value::as_str) {
        Some("message_end") => {
            let Some(msg) = v.get("message") else { return };
            if msg.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                return;
            }
            let Some(usage) = msg.get("usage") else {
                return;
            };
            acc.usage
                .input
                .add(usage.get("input").and_then(serde_json::Value::as_u64));
            acc.usage
                .output
                .add(usage.get("output").and_then(serde_json::Value::as_u64));
            acc.usage
                .cache_read
                .add(usage.get("cacheRead").and_then(serde_json::Value::as_u64));
            acc.usage
                .cache_write
                .add(usage.get("cacheWrite").and_then(serde_json::Value::as_u64));
            acc.usage
                .reasoning
                .add(usage.get("reasoning").and_then(serde_json::Value::as_u64));
            acc.usage
                .total
                .add(usage.get("totalTokens").and_then(serde_json::Value::as_u64));
            if let Some(cost) = usage.get("cost") {
                acc.provider_cost
                    .input
                    .add(cost.get("input").and_then(serde_json::Value::as_f64));
                acc.provider_cost
                    .output
                    .add(cost.get("output").and_then(serde_json::Value::as_f64));
                acc.provider_cost
                    .cache_read
                    .add(cost.get("cacheRead").and_then(serde_json::Value::as_f64));
                acc.provider_cost
                    .cache_write
                    .add(cost.get("cacheWrite").and_then(serde_json::Value::as_f64));
                acc.provider_cost
                    .total
                    .add(cost.get("total").and_then(serde_json::Value::as_f64));
            }
        }
        Some("tool_execution_end") => {
            acc.tool_calls += 1;
            let name = v
                .get("toolName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if name.contains("apply") {
                acc.apply_total += 1;
                let is_err = v
                    .get("isError")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !is_err {
                    acc.apply_ok += 1;
                }
            }
        }
        _ => {}
    }
}

fn env_u64_budget(name: &str, default: u64) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .or(Some(default))
        .filter(|value| *value > 0)
}

fn env_f64_budget(name: &str, default: f64) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .or(Some(default))
        .filter(|value| value.is_finite() && *value > 0.0)
}

#[derive(Debug, Clone, Copy)]
struct RunBudgets {
    max_events: Option<u64>,
    max_stdout_bytes: Option<u64>,
    max_tokens: Option<u64>,
    max_provider_cost: Option<f64>,
}

impl RunBudgets {
    fn from_env() -> Self {
        Self {
            max_events: env_u64_budget("SHUTTLE_MAX_PI_EVENTS", DEFAULT_MAX_PI_EVENTS),
            max_stdout_bytes: env_u64_budget(
                "SHUTTLE_MAX_PI_STDOUT_BYTES",
                DEFAULT_MAX_PI_STDOUT_BYTES,
            ),
            max_tokens: env_u64_budget("SHUTTLE_MAX_PI_TOKENS", DEFAULT_MAX_PI_TOKENS),
            max_provider_cost: env_f64_budget(
                "SHUTTLE_MAX_PROVIDER_COST",
                DEFAULT_MAX_PROVIDER_COST,
            ),
        }
    }

    fn violation(
        self,
        acc: &StreamAcc,
        observed_events: u64,
        stdout_bytes: u64,
    ) -> Option<&'static str> {
        if self.max_events.is_some_and(|limit| observed_events > limit) {
            return Some("pi_event_count");
        }
        if self
            .max_stdout_bytes
            .is_some_and(|limit| stdout_bytes > limit)
        {
            return Some("pi_stdout_bytes");
        }
        if self
            .max_tokens
            .is_some_and(|limit| acc.tokens_opt().is_some_and(|value| value > limit))
        {
            return Some("pi_tokens");
        }
        if self.max_provider_cost.is_some_and(|limit| {
            acc.provider_cost
                .total
                .value()
                .is_some_and(|value| value > limit)
        }) {
            return Some("provider_cost");
        }
        None
    }
}

/// Soft wall-clock budget for a supervised run (`SHUTTLE_SOFT_TIMEOUT_MS`).
/// Defaults to 30 minutes; explicit `0` disables it. On expiry shuttle kills pi and
/// reports exit code 124.
fn soft_timeout() -> Option<Duration> {
    env_u64_budget("SHUTTLE_SOFT_TIMEOUT_MS", DEFAULT_SOFT_TIMEOUT_MS).map(Duration::from_millis)
}

/// Progress stall budget (`SHUTTLE_STALL_TIMEOUT_MS`). When set, shuttle kills pi and
/// reports exit code 125 if no *progress* event arrives within this window —
/// i.e. the model emits only empty (0-token) `message_update` heartbeats while
/// neither tokens nor tool activity advance. Distinct from the wall-clock soft
/// timeout (124); this catches provider empty-response livelocks where the stream
/// never falls silent but never advances either. Defaults to five minutes;
/// explicit `0` disables it.
fn stall_timeout() -> Option<Duration> {
    env_u64_budget("SHUTTLE_STALL_TIMEOUT_MS", DEFAULT_STALL_TIMEOUT_MS).map(Duration::from_millis)
}

/// Sample on elapsed wall time, independent of whether the stream was busy or
/// idle. Returning a stable reason lets both supervisor transports share the
/// same admission-control semantics.
fn poll_resource_guard<P: crate::resource::ResourceProbe>(
    sampler: &mut crate::resource::ResourceSampler<P>,
    sample_interval: Option<Duration>,
    last_sample: &mut Instant,
    now: Instant,
) -> Option<&'static str> {
    let interval = sample_interval?;
    if now.duration_since(*last_sample) < interval {
        return None;
    }
    *last_sample = now;
    sampler
        .sample()
        .map(|admission| match admission.reason.as_str() {
            "resource_host_pressure" => "resource_host_pressure",
            _ => "resource_proc_rss",
        })
}

/// Cap a blocking receive at the next resource-sampling deadline. This keeps
/// telemetry alive even when both wall and stall timeouts are explicitly off.
fn resource_sample_wait(
    sample_interval: Option<Duration>,
    last_sample: Instant,
    now: Instant,
) -> Option<Duration> {
    sample_interval.map(|interval| interval.saturating_sub(now.duration_since(last_sample)))
}

/// A pi stream line counts as *progress* unless it is a zero-token
/// `message_update` heartbeat. Any tool activity, message/turn boundary, or
/// token-bearing update resets the stall clock.
fn event_is_progress(v: &serde_json::Value) -> bool {
    match v.get("type").and_then(serde_json::Value::as_str) {
        Some("message_update") => v
            .pointer("/usage/totalTokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|t| t > 0),
        _ => true,
    }
}

/// pi's terminal turn-boundary event. Once seen, the run is complete and the
/// stall watchdog must not false-kill a finished process that merely went quiet.
fn event_is_settled(v: &serde_json::Value) -> bool {
    v.get("type").and_then(serde_json::Value::as_str) == Some("agent_settled")
}

fn read_pi_event_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        let take = buf
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buf.len(), |index| index + 1);
        let observed = bytes.len().saturating_add(take);
        if observed > MAX_PI_EVENT_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "pi JSONL event exceeds {MAX_PI_EVENT_LINE_BYTES} byte limit (observed at least {observed})"
                ),
            ));
        }
        bytes.extend_from_slice(&buf[..take]);
        let complete = buf[take - 1] == b'\n';
        reader.consume(take);
        if complete {
            break;
        }
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes).map(Some).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("pi JSONL event is not UTF-8: {error}"),
        )
    })
}

fn spawn_pi_stream_reader(
    stdout: std::process::ChildStdout,
) -> (
    mpsc::Receiver<Result<String, String>>,
    thread::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::sync_channel(PI_EVENT_QUEUE_CAPACITY);
    let reader = thread::spawn(move || {
        let mut buf = BufReader::new(stdout);
        loop {
            match read_pi_event_line(&mut buf) {
                Ok(Some(line)) => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = tx.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    (rx, reader)
}

/// True when a pi stream line carries a positive token count. The earliest
/// such line marks the first real model token, so shuttle stamps TTFT off it.
/// Streaming `message_update` puts the count at `/usage/totalTokens`; the
/// final `message_end` nests it under `/message/usage/totalTokens`.
fn event_has_tokens(v: &serde_json::Value) -> bool {
    match v.get("type").and_then(serde_json::Value::as_str) {
        Some("message_update") => v
            .pointer("/usage/totalTokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|t| t > 0),
        Some("message_end") => v
            .pointer("/message/usage/totalTokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|t| t > 0),
        _ => false,
    }
}

/// Decide shuttle's automatic reply to a pi `extension_ui_request` during deep-drive.
///
/// Returns the JSONL command line to write back to pi's stdin, or `None` for
/// fire-and-forget requests (`notify` / `setStatus` / `setWidget` / `setTitle` /
/// `set_editor_text`) that expect no response.
///
/// shuttle is the supervisor: it auto-approves confirmations (so `apply_edit_plan`
/// proceeds unattended), picks the first offered option, and returns empty text
/// for free-form `input` / `editor`. This keeps a headless deep-drive from
/// deadlocking on a dialog no human is there to answer. The response shapes
/// mirror pi's `RpcExtensionUIResponse` union (`confirmed` / `value`).
fn rpc_ui_autoresponse(req: &serde_json::Value) -> Option<String> {
    let id = req.get("id").and_then(serde_json::Value::as_str)?;
    let method = req.get("method").and_then(serde_json::Value::as_str)?;
    let resp = match method {
        "confirm" => {
            serde_json::json!({"type":"extension_ui_response","id":id,"confirmed":true})
        }
        "select" => {
            let first = req
                .get("options")
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.first())
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            serde_json::json!({"type":"extension_ui_response","id":id,"value":first})
        }
        "input" | "editor" => {
            serde_json::json!({"type":"extension_ui_response","id":id,"value":""})
        }
        // notify / setStatus / setWidget / setTitle / set_editor_text are
        // fire-and-forget in pi's rpc mode — no response is expected.
        _ => return None,
    };
    Some(resp.to_string())
}

/// Spawn pi under shuttle supervision and wait.
///
/// Dispatches on transport: form B (json supervisor) parses pi's `-p --mode
/// json` JSONL off stdout; form C (rpc deep-drive) drives pi turn-by-turn over
/// `--mode rpc`. Both fold the same event stream into telemetry (tokens / tool
/// calls / apply accept rate) and forward every raw line to our stdout, and
/// both honor the soft timeout.
pub fn supervise(cfg: &RunConfig) -> anyhow::Result<RunTelemetry> {
    supervise_with_spawn(cfg, |_| Ok(()))
}

/// Supervise pi and report its process-group id immediately after spawn.
/// Durable cell runs use this hook to publish a cancellable handle before any
/// model or tool work can occur; ordinary `shuttle run` uses [`supervise`].
pub fn supervise_with_spawn<F>(cfg: &RunConfig, mut on_spawn: F) -> anyhow::Result<RunTelemetry>
where
    F: FnMut(u32) -> anyhow::Result<()>,
{
    let _signal_guard = install_termination_signal_guard()?;
    let bin = cfg.pi_bin();
    let args = cfg.render_pi_args();
    let mut herdr = HerdrReporter::maybe_new();

    if let Some(rep) = herdr.as_mut() {
        rep.start();
    }

    tracing::info!(pi = %bin, mode = ?cfg.mode, "shuttle run: spawning pi");
    tracing::debug!(?args, "rendered pi args");

    if cfg.mode == WrapMode::RpcDeep {
        let result = supervise_rpc(&bin, &args, cfg, &mut on_spawn);
        if let Some(rep) = herdr.as_mut() {
            match result.as_ref() {
                Ok(tel) => rep.finish(Some(tel), None),
                Err(e) => rep.finish(None, Some(&e.to_string())),
            }
        }
        return result;
    }

    let started = Instant::now();
    let mut cmd = Command::new(&bin);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.args(&args)
        .env("SHUTTLE_BIN", RunConfig::shuttle_bin_for_child())
        .current_dir(&cfg.root)
        // pi -p --mode json is non-interactive: hand it an empty stdin so it
        // never blocks waiting for an EOF we inherit but never close.
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    for (k, v) in &cfg.child_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| {
        tracing::error!(error = %e, "failed to spawn pi (is it on PATH? set PI_BIN)");
        anyhow::anyhow!("spawn pi failed: {e}")
    })?;
    if let Err(error) = on_spawn(child.id()) {
        kill_process_group(&mut child);
        let _ = child.wait();
        return Err(error);
    }

    let stdout = child.stdout.take().expect("stdout was piped");

    // The bounded queue applies backpressure when forwarding/parsing is slow;
    // the reader also rejects over-sized JSONL events before allocation grows
    // without limit.
    let (rx, reader) = spawn_pi_stream_reader(stdout);

    let deadline = soft_timeout().map(|d| started + d);
    let stall_budget = stall_timeout();
    let budgets = RunBudgets::from_env();
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let mut stalled = false;
    let mut settled = false;
    let mut stream_error = None;
    let mut resource_limit = None;
    let mut interrupted = false;
    let mut observed_event_lines = 0u64;
    let mut stdout_bytes = 0u64;
    let mut last_progress = Instant::now();
    let mut first_token_at: Option<Instant> = None;
    let out = std::io::stdout();
    // Best-effort resource sampling on the loop tick shuttle already owns
    // (daemonless: no extra thread, no listening port). Every probe is
    // Option-typed and degrades to `None` when `ps`/`vm_stat`/`sysctl`
    // are unavailable (§7 never-fabricate). Guards default disabled, so
    // admission control only fires when explicitly opted in via env.
    let sample_iv = crate::resource::sample_interval();
    let mut sampler = crate::resource::ResourceSampler::new(
        crate::resource::SystemProbe,
        std::process::id() as i32,
        child.id() as i32,
        crate::resource::ResourceGuards::from_env(),
    );
    let sample_clock = Instant::now();
    let mut last_sample = sample_iv
        .and_then(|interval| sample_clock.checked_sub(interval))
        .unwrap_or(sample_clock);

    loop {
        let recv = {
            let now = Instant::now();
            if termination_signal().is_some() {
                interrupted = true;
                break;
            }
            if let Some(dl) = deadline {
                if now >= dl {
                    timed_out = true;
                    break;
                }
            }
            if let Some(sb) = stall_budget {
                if now.duration_since(last_progress) >= sb {
                    stalled = true;
                    break;
                }
            }
            if let Some(reason) =
                poll_resource_guard(&mut sampler, sample_iv, &mut last_sample, now)
            {
                resource_limit = Some(reason);
                break;
            }
            let sample_wait = resource_sample_wait(sample_iv, last_sample, now);
            match (deadline, stall_budget, sample_wait) {
                (None, None, None) => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                _ => {
                    let mut wait = Duration::from_millis(500);
                    if let Some(dl) = deadline {
                        wait = wait.min(dl.saturating_duration_since(now));
                    }
                    if let Some(sample_wait) = sample_wait {
                        wait = wait.min(sample_wait);
                    }
                    rx.recv_timeout(wait)
                }
            }
        };
        match recv {
            Ok(Ok(line)) => {
                observed_event_lines = observed_event_lines.saturating_add(1);
                stdout_bytes = stdout_bytes.saturating_add(line.len() as u64 + 1);
                if budgets
                    .max_events
                    .is_some_and(|limit| observed_event_lines > limit)
                {
                    resource_limit = Some("pi_event_count");
                    break;
                }
                if budgets
                    .max_stdout_bytes
                    .is_some_and(|limit| stdout_bytes > limit)
                {
                    resource_limit = Some("pi_stdout_bytes");
                    break;
                }
                // Transparency: forward pi's raw JSONL unchanged.
                {
                    let mut w = out.lock();
                    let _ = writeln!(w, "{line}");
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                    if let Some(reason) =
                        budgets.violation(&acc, observed_event_lines, stdout_bytes)
                    {
                        resource_limit = Some(reason);
                        break;
                    }
                    if event_is_progress(&v) {
                        last_progress = Instant::now();
                    }
                    if first_token_at.is_none() && event_has_tokens(&v) {
                        first_token_at = Some(Instant::now());
                    }
                    // Turn boundary: pi emitted its terminal event. The work is
                    // done; do NOT let the stall watchdog false-kill a finished
                    // run that merely went quiet (mirrors the rpc loop).
                    if event_is_settled(&v) {
                        settled = true;
                        break;
                    }
                }
            }
            Ok(Err(error)) => {
                stream_error = Some(error);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // stream ended
        }
    }

    if interrupted {
        tracing::warn!("shuttle run: termination signal received, killing pi");
        kill_process_group(&mut child);
    } else if resource_limit.is_some() {
        tracing::warn!(
            reason = resource_limit,
            "shuttle run: resource budget exceeded, killing pi"
        );
        kill_process_group(&mut child);
    } else if stream_error.is_some() {
        tracing::warn!("shuttle run: PI stream limit/parse failure, killing pi");
        kill_process_group(&mut child);
    } else if settled {
        // Turn already complete and forwarded; reap pi promptly so a lingering
        // process cannot re-arm the watchdog on a subsequent run.
        kill_process_group(&mut child);
    } else if stalled {
        tracing::warn!("shuttle run: progress stall detected, killing pi");
        kill_process_group(&mut child);
    } else if timed_out {
        tracing::warn!("shuttle run: soft timeout hit, killing pi");
        kill_process_group(&mut child);
    }

    drop(rx);
    let status = child.wait();
    let _ = reader.join();
    let wall_ms = started.elapsed().as_millis();

    let exit_code = if interrupted {
        130
    } else if stream_error.is_some() {
        123
    } else if resource_limit.is_some() {
        122
    } else if settled {
        0
    } else if stalled {
        125
    } else if timed_out {
        124
    } else {
        match status {
            Ok(st) => st.code().unwrap_or(-1),
            Err(_) => -1,
        }
    };

    let tel = RunTelemetry {
        termination: if interrupted {
            RunTermination::Interrupted
        } else if stream_error.is_some() {
            RunTermination::StreamRejected
        } else if resource_limit.is_some() {
            RunTermination::ResourceLimit
        } else if settled {
            RunTermination::Settled
        } else if stalled {
            RunTermination::StallTimeout
        } else if timed_out {
            RunTermination::SoftTimeout
        } else {
            RunTermination::ChildExit
        },
        limit_reason: stream_error
            .as_ref()
            .map(|_| "pi_event_line".to_string())
            .or_else(|| resource_limit.map(str::to_owned)),
        wall_ms,
        tokens: acc.tokens_opt(),
        usage: acc.usage(),
        provider_cost: acc.provider_cost(),
        provider: effective_provider(cfg),
        model: cfg.model.clone(),
        event_count: acc.event_count,
        stdout_bytes,
        tool_calls: acc.tool_calls,
        accept_rate: acc.accept_rate(),
        apply_total: acc.apply_total,
        apply_ok: acc.apply_ok,
        exit_code,
        ttft_ms: first_token_at.map(|t| t.duration_since(started).as_millis()),
        resources: sampler.snapshot(),
    };

    if let Some(rep) = herdr.as_mut() {
        rep.finish(Some(&tel), None);
    }

    Ok(tel)
}

/// Form C — rpc deep-drive (W6).
///
/// shuttle drives a single pi turn over JSON-RPC:
///   1. spawn `pi --mode rpc` with piped stdin **and** stdout;
///   2. feed the requirement as one `{"type":"prompt"}` command on stdin
///      (rpc mode takes the prompt over the wire, not as an argv positional);
///   3. fold pi's stdout event stream into telemetry while forwarding it raw;
///   4. auto-answer every `extension_ui_request` (shuttle is the supervisor — a
///      headless run has no human to click a confirm/select), so pi never
///      blocks;
///   5. stop when the turn `agent_settled`, then close stdin so pi shuts down
///      gracefully (pi's `stdin.on("end")` → `shutdown(0)`).
///      The soft timeout still applies via the same deadline watchdog as form B.
fn supervise_rpc(
    bin: &str,
    args: &[String],
    cfg: &RunConfig,
    on_spawn: &mut dyn FnMut(u32) -> anyhow::Result<()>,
) -> anyhow::Result<RunTelemetry> {
    let started = Instant::now();
    let mut cmd = Command::new(bin);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.args(args)
        .env("SHUTTLE_BIN", RunConfig::shuttle_bin_for_child())
        .current_dir(&cfg.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    for (k, v) in &cfg.child_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| {
        tracing::error!(error = %e, "failed to spawn pi (rpc; is it on PATH? set PI_BIN)");
        let _ = RunTelemetry::errored(started.elapsed().as_millis()); // keep shape stable
        anyhow::anyhow!("spawn pi failed: {e}")
    })?;
    if let Err(error) = on_spawn(child.id()) {
        kill_process_group(&mut child);
        let _ = child.wait();
        return Err(error);
    }

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    // Kick off the single turn. rpc feeds the prompt here (not as a positional).
    let prompt_cmd = serde_json::json!({
        "type": "prompt",
        "id": "shuttle-run-1",
        "message": cfg.prompt,
    })
    .to_string();
    if let Err(e) = writeln!(stdin, "{prompt_cmd}").and_then(|()| stdin.flush()) {
        tracing::error!(error = %e, "shuttle run (rpc): failed to send prompt command");
    }

    // Same bounded/limited stream contract as form B.
    let (rx, reader) = spawn_pi_stream_reader(stdout);

    let deadline = soft_timeout().map(|d| started + d);
    let stall_budget = stall_timeout();
    let budgets = RunBudgets::from_env();
    let mut acc = StreamAcc::default();
    let mut timed_out = false;
    let mut stalled = false;
    let mut settled = false;
    let mut stream_error = None;
    let mut resource_limit = None;
    let mut interrupted = false;
    let mut observed_event_lines = 0u64;
    let mut stdout_bytes = 0u64;
    let mut last_progress = Instant::now();
    let mut first_token_at: Option<Instant> = None;
    let out = std::io::stdout();
    // Best-effort resource sampling on the loop tick shuttle already owns
    // (daemonless: no extra thread, no listening port). Every probe is
    // Option-typed and degrades to `None` when `ps`/`vm_stat`/`sysctl`
    // are unavailable (§7 never-fabricate). Guards default disabled, so
    // admission control only fires when explicitly opted in via env.
    let sample_iv = crate::resource::sample_interval();
    let mut sampler = crate::resource::ResourceSampler::new(
        crate::resource::SystemProbe,
        std::process::id() as i32,
        child.id() as i32,
        crate::resource::ResourceGuards::from_env(),
    );
    let sample_clock = Instant::now();
    let mut last_sample = sample_iv
        .and_then(|interval| sample_clock.checked_sub(interval))
        .unwrap_or(sample_clock);

    loop {
        let recv = {
            let now = Instant::now();
            if termination_signal().is_some() {
                interrupted = true;
                break;
            }
            if let Some(dl) = deadline {
                if now >= dl {
                    timed_out = true;
                    break;
                }
            }
            if let Some(sb) = stall_budget {
                if now.duration_since(last_progress) >= sb {
                    stalled = true;
                    break;
                }
            }
            if let Some(reason) =
                poll_resource_guard(&mut sampler, sample_iv, &mut last_sample, now)
            {
                resource_limit = Some(reason);
                break;
            }
            let sample_wait = resource_sample_wait(sample_iv, last_sample, now);
            match (deadline, stall_budget, sample_wait) {
                (None, None, None) => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                _ => {
                    let mut wait = Duration::from_millis(500);
                    if let Some(dl) = deadline {
                        wait = wait.min(dl.saturating_duration_since(now));
                    }
                    if let Some(sample_wait) = sample_wait {
                        wait = wait.min(sample_wait);
                    }
                    rx.recv_timeout(wait)
                }
            }
        };
        match recv {
            Ok(Ok(line)) => {
                observed_event_lines = observed_event_lines.saturating_add(1);
                stdout_bytes = stdout_bytes.saturating_add(line.len() as u64 + 1);
                if budgets
                    .max_events
                    .is_some_and(|limit| observed_event_lines > limit)
                {
                    resource_limit = Some("pi_event_count");
                    break;
                }
                if budgets
                    .max_stdout_bytes
                    .is_some_and(|limit| stdout_bytes > limit)
                {
                    resource_limit = Some("pi_stdout_bytes");
                    break;
                }
                // Transparency: forward pi's raw JSONL unchanged.
                {
                    let mut w = out.lock();
                    let _ = writeln!(w, "{line}");
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    ingest_event(&mut acc, &v);
                    if let Some(reason) =
                        budgets.violation(&acc, observed_event_lines, stdout_bytes)
                    {
                        resource_limit = Some(reason);
                        break;
                    }
                    if event_is_progress(&v) {
                        last_progress = Instant::now();
                    }
                    if first_token_at.is_none() && event_has_tokens(&v) {
                        first_token_at = Some(Instant::now());
                    }
                    match v.get("type").and_then(serde_json::Value::as_str) {
                        // Supervisor control seam: auto-answer UI dialogs so a
                        // headless deep-drive never deadlocks on a confirm/select.
                        Some("extension_ui_request") => {
                            if let Some(resp) = rpc_ui_autoresponse(&v) {
                                if let Err(e) =
                                    writeln!(stdin, "{resp}").and_then(|()| stdin.flush())
                                {
                                    tracing::warn!(error = %e, "shuttle run (rpc): failed to answer ui request");
                                }
                            }
                        }
                        // Turn boundary: the agent has quiesced.
                        Some("agent_settled") => {
                            settled = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Ok(Err(error)) => {
                stream_error = Some(error);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // stream ended
        }
    }

    // Turn done (or timed out): close stdin → pi's stdin `end` triggers a clean
    // shutdown(0). On timeout we additionally kill it.
    drop(stdin);
    if interrupted {
        tracing::warn!("shuttle run (rpc): termination signal received, killing pi");
        kill_process_group(&mut child);
    } else if resource_limit.is_some() {
        tracing::warn!(
            reason = resource_limit,
            "shuttle run (rpc): resource budget exceeded, killing pi"
        );
        kill_process_group(&mut child);
    } else if stream_error.is_some() {
        tracing::warn!("shuttle run (rpc): PI stream limit/parse failure, killing pi");
        kill_process_group(&mut child);
    } else if stalled {
        tracing::warn!("shuttle run (rpc): progress stall detected, killing pi");
        kill_process_group(&mut child);
    } else if timed_out {
        tracing::warn!("shuttle run (rpc): soft timeout hit, killing pi");
        kill_process_group(&mut child);
    }

    drop(rx);
    let status = child.wait();
    let _ = reader.join();
    let wall_ms = started.elapsed().as_millis();

    let exit_code = if interrupted {
        130
    } else if stream_error.is_some() {
        123
    } else if resource_limit.is_some() {
        122
    } else if stalled {
        125
    } else if timed_out {
        124
    } else {
        match status {
            Ok(st) => st.code().unwrap_or(-1),
            Err(_) => -1,
        }
    };

    tracing::info!(settled, exit_code, "shuttle run (rpc): turn complete");

    Ok(RunTelemetry {
        termination: if interrupted {
            RunTermination::Interrupted
        } else if stream_error.is_some() {
            RunTermination::StreamRejected
        } else if resource_limit.is_some() {
            RunTermination::ResourceLimit
        } else if settled {
            RunTermination::Settled
        } else if stalled {
            RunTermination::StallTimeout
        } else if timed_out {
            RunTermination::SoftTimeout
        } else {
            RunTermination::ChildExit
        },
        limit_reason: stream_error
            .as_ref()
            .map(|_| "pi_event_line".to_string())
            .or_else(|| resource_limit.map(str::to_owned)),
        wall_ms,
        tokens: acc.tokens_opt(),
        usage: acc.usage(),
        provider_cost: acc.provider_cost(),
        provider: effective_provider(cfg),
        model: cfg.model.clone(),
        event_count: acc.event_count,
        stdout_bytes,
        tool_calls: acc.tool_calls,
        accept_rate: acc.accept_rate(),
        apply_total: acc.apply_total,
        apply_ok: acc.apply_ok,
        exit_code,
        ttft_ms: first_token_at.map(|t| t.duration_since(started).as_millis()),
        resources: sampler.snapshot(),
    })
}

/// Kill the complete child process group. Cell tools may spawn descendants; a
/// direct-child kill would leave those descendants able to write after timeout.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_supervisor_default_render() {
        let cfg = RunConfig {
            prompt: "add a feature".into(),
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        assert_eq!(
            a,
            vec![
                "-p",
                "--mode",
                "json",
                "--thinking",
                "off",
                "--exclude-tools",
                "edit,write",
                "add a feature",
            ]
        );
    }

    #[test]
    fn explicit_model_and_provider_render() {
        let cfg = RunConfig {
            prompt: "add a feature".into(),
            model: Some("vendor/model".into()),
            provider: Some("vendor".into()),
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        assert_eq!(
            a,
            vec![
                "-p",
                "--mode",
                "json",
                "--model",
                "vendor/model",
                "--thinking",
                "off",
                "--provider",
                "vendor",
                "--exclude-tools",
                "edit,write",
                "add a feature",
            ]
        );
    }

    #[test]
    fn extension_and_no_session_render() {
        let cfg = RunConfig {
            prompt: "x".into(),
            extension: Some("/path/ext.ts".into()),
            no_session: true,
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        // -e injects the shuttle extension; --no-session for ephemeral eval runs.
        let joined = a.join(" ");
        assert!(joined.contains("-e /path/ext.ts"), "got: {joined}");
        assert!(joined.contains("--no-session"), "got: {joined}");
        // prompt stays last.
        assert_eq!(a.last().unwrap(), "x");
    }

    #[test]
    fn durable_session_renders_explicit_id_and_directory() {
        let cfg = RunConfig {
            prompt: "x".into(),
            no_session: false,
            session_id: Some("00000000-0000-4000-a000-000000000000".into()),
            session_dir: Some("/state/sessions/run".into()),
            ..Default::default()
        };
        let args = cfg.render_pi_args();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-id", "00000000-0000-4000-a000-000000000000"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--session-dir", "/state/sessions/run"]));
        assert!(!args.contains(&"--no-session".to_string()));
    }

    #[test]
    fn rpc_deep_has_no_print_and_no_trailing_prompt() {
        let cfg = RunConfig {
            prompt: "drive me".into(),
            mode: WrapMode::RpcDeep,
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        assert!(!a.contains(&"-p".to_string()), "rpc must not use -p");
        assert_eq!(a[a.iter().position(|x| x == "--mode").unwrap() + 1], "rpc");
        // rpc feeds prompt over the wire, not as a trailing positional.
        assert_ne!(a.last().unwrap(), "drive me");
    }

    #[test]
    fn passthrough_precedes_prompt() {
        let cfg = RunConfig {
            prompt: "P".into(),
            passthrough: vec!["--verbose".into(), "--approve".into()],
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        let vpos = a.iter().position(|x| x == "--verbose").unwrap();
        let ppos = a.iter().position(|x| x == "P").unwrap();
        assert!(vpos < ppos, "passthrough must come before prompt: {a:?}");
    }

    #[test]
    fn empty_exclude_tools_omits_flag() {
        let cfg = RunConfig {
            exclude_tools: vec![],
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        assert!(!a.contains(&"--exclude-tools".to_string()));
    }

    #[test]
    fn positive_allowlist_renders_tools_and_lockdown_flags() {
        // The cage seam: a positive `--tools` allowlist plus built-in and
        // ambient-extension lockdown. Order: after --exclude-tools, before -e.
        let cfg = RunConfig {
            prompt: "P".into(),
            exclude_tools: vec![],
            tools: vec!["read".into(), "apply_edit_plan".into()],
            no_builtin_tools: true,
            no_extensions: true,
            extension: Some("/ext.ts".into()),
            ..Default::default()
        };
        let a = cfg.render_pi_args();
        let joined = a.join(" ");
        assert!(
            joined.contains("--tools read,apply_edit_plan"),
            "got: {joined}"
        );
        assert!(joined.contains("--no-builtin-tools"), "got: {joined}");
        assert!(joined.contains("--no-extensions"), "got: {joined}");
        // Positive allowlist must precede the -e extension load.
        let tpos = a.iter().position(|x| x == "--tools").unwrap();
        let epos = a.iter().position(|x| x == "-e").unwrap();
        assert!(tpos < epos, "--tools must precede -e: {a:?}");
    }

    #[test]
    fn empty_tools_and_lockdown_off_by_default() {
        // Regression guard: a bare `shuttle run` never emits the cage-only flags.
        let a = RunConfig {
            prompt: "x".into(),
            ..Default::default()
        }
        .render_pi_args();
        assert!(!a.contains(&"--tools".to_string()));
        assert!(!a.contains(&"--no-builtin-tools".to_string()));
        assert!(!a.contains(&"--no-extensions".to_string()));
    }

    fn ingest(lines: &[&str]) -> StreamAcc {
        let mut acc = StreamAcc::default();
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            ingest_event(&mut acc, &v);
        }
        acc
    }

    #[test]
    fn tokens_sum_across_assistant_message_ends() {
        // Two assistant rounds; user message_end and non-assistant usage ignored.
        let acc = ingest(&[
            r#"{"type":"message_end","message":{"role":"user","content":[]}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"totalTokens":100}}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"totalTokens":250}}}"#,
        ]);
        assert_eq!(acc.tokens_opt(), Some(350));
    }

    #[test]
    fn detailed_usage_and_provider_cost_sum_across_assistant_rounds() {
        let acc = ingest(&[
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":10,"output":4,"cacheRead":3,"cacheWrite":2,"reasoning":1,"totalTokens":20,"cost":{"input":0.1,"output":0.2,"cacheRead":0.01,"cacheWrite":0.02,"total":0.33}}}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":5,"output":2,"totalTokens":7,"cost":{"input":0.05,"output":0.1,"total":0.15}}}}"#,
        ]);
        assert_eq!(
            acc.usage(),
            UsageBreakdown {
                input_tokens: Some(15),
                output_tokens: Some(6),
                cache_read_tokens: Some(3),
                cache_write_tokens: Some(2),
                reasoning_tokens: Some(1),
                total_tokens: Some(27),
            }
        );
        let cost = acc.provider_cost().unwrap();
        for (actual, expected) in [
            (cost.input, 0.15),
            (cost.output, 0.3),
            (cost.cache_read, 0.01),
            (cost.cache_write, 0.02),
            (cost.total, 0.48),
        ] {
            assert!((actual.unwrap() - expected).abs() < 1e-12);
        }
        assert_eq!(acc.event_count, 2);
    }

    #[test]
    fn missing_usage_components_remain_none_not_zero() {
        let acc = ingest(&[
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"totalTokens":7}}}"#,
        ]);
        let usage = acc.usage();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.total_tokens, Some(7));
        assert_eq!(acc.provider_cost(), None);
    }

    #[test]
    fn provider_is_derived_from_qualified_model_when_not_explicit() {
        let cfg = RunConfig {
            model: Some("opencode/model-one".into()),
            ..Default::default()
        };
        assert_eq!(effective_provider(&cfg).as_deref(), Some("opencode"));
    }

    #[test]
    fn no_usage_yields_none_tokens() {
        let acc = ingest(&[r#"{"type":"agent_start"}"#, r#"{"type":"turn_start"}"#]);
        assert_eq!(acc.tokens_opt(), None);
    }

    #[test]
    fn tool_execution_ends_are_counted() {
        let acc = ingest(&[
            r#"{"type":"tool_execution_end","toolName":"bash","isError":false}"#,
            r#"{"type":"tool_execution_start","toolName":"bash"}"#,
            r#"{"type":"tool_execution_end","toolName":"read","isError":false}"#,
        ]);
        assert_eq!(acc.tool_calls, 2);
        // No apply-family calls -> accept_rate is undefined.
        assert_eq!(acc.accept_rate(), None);
    }

    #[test]
    fn apply_accept_rate_from_ok_and_errored() {
        let acc = ingest(&[
            r#"{"type":"tool_execution_end","toolName":"apply_edit_plan","isError":false}"#,
            r#"{"type":"tool_execution_end","toolName":"apply_edit_plan","isError":true}"#,
            r#"{"type":"tool_execution_end","toolName":"apply_edit_plan","isError":false}"#,
        ]);
        assert_eq!(acc.tool_calls, 3);
        assert_eq!(acc.apply_total, 3);
        assert_eq!(acc.apply_ok, 2);
        assert_eq!(acc.accept_rate(), Some(2.0 / 3.0));
    }

    #[test]
    fn soft_timeout_env_parsing() {
        std::env::set_var("SHUTTLE_SOFT_TIMEOUT_MS", "1500");
        assert_eq!(soft_timeout(), Some(Duration::from_millis(1500)));
        std::env::set_var("SHUTTLE_SOFT_TIMEOUT_MS", "0");
        assert_eq!(soft_timeout(), None);
        std::env::remove_var("SHUTTLE_SOFT_TIMEOUT_MS");
        assert_eq!(
            soft_timeout(),
            Some(Duration::from_millis(DEFAULT_SOFT_TIMEOUT_MS))
        );
    }

    #[test]
    fn stall_timeout_env_parsing() {
        std::env::set_var("SHUTTLE_STALL_TIMEOUT_MS", "90000");
        assert_eq!(stall_timeout(), Some(Duration::from_millis(90000)));
        std::env::set_var("SHUTTLE_STALL_TIMEOUT_MS", "0");
        assert_eq!(stall_timeout(), None);
        std::env::remove_var("SHUTTLE_STALL_TIMEOUT_MS");
        assert_eq!(
            stall_timeout(),
            Some(Duration::from_millis(DEFAULT_STALL_TIMEOUT_MS))
        );
    }

    struct FixedResourceProbe;

    impl crate::resource::ResourceProbe for FixedResourceProbe {
        fn ps_pid_pgid_rss(&self) -> Option<String> {
            Some("1 1 16\n7 7 128\n".to_string())
        }

        fn vm_stat(&self) -> Option<String> {
            None
        }

        fn mem_total_bytes(&self) -> Option<u64> {
            None
        }

        fn swap_usage(&self) -> Option<String> {
            None
        }

        fn meminfo(&self) -> Option<String> {
            None
        }
    }

    #[test]
    fn resource_sampling_progresses_on_busy_stream_iterations() {
        let start = Instant::now();
        let mut last_sample = start;
        let mut sampler = crate::resource::ResourceSampler::new(
            FixedResourceProbe,
            1,
            7,
            crate::resource::ResourceGuards::default(),
        );

        // Model ten immediately-ready stream events. No recv timeout occurs;
        // the common loop path must still sample at elapsed wall intervals.
        for step in 1..=10 {
            let now = start + Duration::from_millis(step * 100);
            assert_eq!(
                poll_resource_guard(
                    &mut sampler,
                    Some(Duration::from_millis(250)),
                    &mut last_sample,
                    now,
                ),
                None
            );
        }

        let resources = sampler.snapshot().expect("busy stream was sampled");
        assert_eq!(resources.samples, 3);
        assert_eq!(resources.peak_managed_rss_bytes, Some(144 * 1024));
    }

    #[test]
    fn resource_deadline_caps_receive_when_other_timeouts_are_off() {
        let start = Instant::now();
        assert_eq!(
            resource_sample_wait(
                Some(Duration::from_millis(1000)),
                start,
                start + Duration::from_millis(400),
            ),
            Some(Duration::from_millis(600))
        );
        assert_eq!(resource_sample_wait(None, start, start), None);
    }

    #[test]
    fn run_budgets_detect_each_cumulative_limit() {
        let mut acc = StreamAcc {
            event_count: 3,
            ..Default::default()
        };
        acc.usage.total.add(Some(101));
        acc.provider_cost.total.add(Some(2.5));
        let base = RunBudgets {
            max_events: Some(2),
            max_stdout_bytes: Some(1000),
            max_tokens: Some(1000),
            max_provider_cost: Some(10.0),
        };
        assert_eq!(base.violation(&acc, 3, 10), Some("pi_event_count"));
        assert_eq!(
            RunBudgets {
                max_events: None,
                max_stdout_bytes: Some(9),
                ..base
            }
            .violation(&acc, 3, 10),
            Some("pi_stdout_bytes")
        );
        assert_eq!(
            RunBudgets {
                max_events: None,
                max_stdout_bytes: None,
                max_tokens: Some(100),
                ..base
            }
            .violation(&acc, 3, 10),
            Some("pi_tokens")
        );
        assert_eq!(
            RunBudgets {
                max_events: None,
                max_stdout_bytes: None,
                max_tokens: None,
                max_provider_cost: Some(2.0),
            }
            .violation(&acc, 3, 10),
            Some("provider_cost")
        );
    }

    #[test]
    fn empty_heartbeat_is_not_progress() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"type":"message_update","usage":{"totalTokens":0}}"#).unwrap();
        assert!(!event_is_progress(&v));
    }

    #[test]
    fn agent_settled_is_detected_and_not_progress() {
        let v: serde_json::Value = serde_json::from_str(r#"{"type":"agent_settled"}"#).unwrap();
        // It is a terminal boundary, not a progress heartbeat.
        assert!(event_is_settled(&v));
        // A finished run that goes quiet must break the loop before the stall
        // watchdog fires — regression guard for the false-positive rc=125 kill.
        let other: serde_json::Value = serde_json::from_str(r#"{"type":"turn_end"}"#).unwrap();
        assert!(!event_is_settled(&other));
    }

    #[test]
    fn token_update_and_tool_events_are_progress() {
        let tok: serde_json::Value =
            serde_json::from_str(r#"{"type":"message_update","usage":{"totalTokens":42}}"#)
                .unwrap();
        assert!(event_is_progress(&tok));
        let tool: serde_json::Value =
            serde_json::from_str(r#"{"type":"tool_execution_start","toolName":"bash"}"#).unwrap();
        assert!(event_is_progress(&tool));
    }

    #[test]
    fn event_has_tokens_marks_first_real_token() {
        let upd: serde_json::Value =
            serde_json::from_str(r#"{"type":"message_update","usage":{"totalTokens":7}}"#).unwrap();
        assert!(event_has_tokens(&upd));
        let end: serde_json::Value = serde_json::from_str(
            r#"{"type":"message_end","message":{"role":"assistant","usage":{"totalTokens":128}}}"#,
        )
        .unwrap();
        assert!(event_has_tokens(&end));
        // Zero-token heartbeat / non-token events must NOT stamp TTFT, so a
        // cold-start stall (rc=125 tokens=0) correctly yields ttft_ms=None.
        let hb: serde_json::Value =
            serde_json::from_str(r#"{"type":"message_update","usage":{"totalTokens":0}}"#).unwrap();
        assert!(!event_has_tokens(&hb));
        let tl: serde_json::Value =
            serde_json::from_str(r#"{"type":"tool_execution_start","toolName":"bash"}"#).unwrap();
        assert!(!event_has_tokens(&tl));
    }

    #[test]
    fn pi_event_reader_rejects_oversized_record() {
        let bytes = vec![b'x'; MAX_PI_EVENT_LINE_BYTES + 1];
        let mut reader = BufReader::new(bytes.as_slice());
        let error = read_pi_event_line(&mut reader).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("pi JSONL event exceeds"));
    }

    #[test]
    fn pi_event_reader_accepts_exact_limit_and_trims_newline() {
        let mut bytes = vec![b'x'; MAX_PI_EVENT_LINE_BYTES - 1];
        bytes.push(b'\n');
        let mut reader = BufReader::new(bytes.as_slice());
        let line = read_pi_event_line(&mut reader).unwrap().unwrap();
        assert_eq!(line.len(), MAX_PI_EVENT_LINE_BYTES - 1);
    }

    // --- W6: rpc deep-drive auto-response ---

    fn parse(line: &str) -> serde_json::Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn rpc_confirm_is_auto_approved() {
        let req = parse(
            r#"{"type":"extension_ui_request","id":"u1","method":"confirm","title":"Apply?","message":"proceed"}"#,
        );
        let resp = rpc_ui_autoresponse(&req).expect("confirm expects a response");
        let v = parse(&resp);
        assert_eq!(v["type"], "extension_ui_response");
        assert_eq!(v["id"], "u1");
        assert_eq!(v["confirmed"], true);
    }

    #[test]
    fn rpc_select_picks_first_option() {
        let req = parse(
            r#"{"type":"extension_ui_request","id":"u2","method":"select","title":"pick","options":["a","b","c"]}"#,
        );
        let v = parse(&rpc_ui_autoresponse(&req).unwrap());
        assert_eq!(v["id"], "u2");
        assert_eq!(v["value"], "a");
    }

    #[test]
    fn rpc_select_empty_options_yields_empty_value() {
        let req = parse(
            r#"{"type":"extension_ui_request","id":"u3","method":"select","title":"pick","options":[]}"#,
        );
        let v = parse(&rpc_ui_autoresponse(&req).unwrap());
        assert_eq!(v["value"], "");
    }

    #[test]
    fn rpc_input_and_editor_return_empty_value() {
        for method in ["input", "editor"] {
            let req = parse(&format!(
                r#"{{"type":"extension_ui_request","id":"u","method":"{method}","title":"t"}}"#
            ));
            let v = parse(&rpc_ui_autoresponse(&req).unwrap());
            assert_eq!(v["value"], "", "method {method} should return empty value");
        }
    }

    #[test]
    fn rpc_fire_and_forget_methods_need_no_response() {
        for method in [
            "notify",
            "setStatus",
            "setWidget",
            "setTitle",
            "set_editor_text",
        ] {
            let req = parse(&format!(
                r#"{{"type":"extension_ui_request","id":"u","method":"{method}","message":"m"}}"#
            ));
            assert!(
                rpc_ui_autoresponse(&req).is_none(),
                "method {method} must be fire-and-forget"
            );
        }
    }

    #[test]
    fn rpc_malformed_request_is_ignored() {
        // Missing id / method → no response, never panics.
        assert!(rpc_ui_autoresponse(&parse(r#"{"type":"extension_ui_request"}"#)).is_none());
        assert!(rpc_ui_autoresponse(&parse(r#"{"method":"confirm"}"#)).is_none());
    }
}
