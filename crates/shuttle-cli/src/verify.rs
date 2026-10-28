//! `shuttle verify` — the CLI wiring for the G5 execution-truth loop .
//!
//! The pure state machine lives in [`shuttle_nav::verify::drive_verify`] /
//! [`shuttle_nav::verify::fold_verify`]; this module is only the seams made concrete
//! against the real world, mirroring how [`crate::goal`] wraps `drive_goal`.
//! The command:
//!
//! ```text
//! shuttle verify --base <oid> --head <oid>
//!           [--run] [--dry-run] [--binding <cell_binding.json>]
//!           [--partial] [--max-depth N] [--telemetry] [--json]
//! ```
//!
//! # Three faces of the same T2 selection
//! * **compute-only** (the default, no `--run`/`--dry-run`) — a pure alias of
//!   `shuttle impact --base/--head`: [`shuttle_nav::reverify::reverify_impact`] computes
//!   the minimal re-verify *set* and shuttle prints it, running nothing. Zero
//!   behavior change vs `shuttle impact`; the `--telemetry` line is the same
//!   `reverify_size` projection. shuttle never lands and never runs unasked (#11/§7).
//! * **`--run`** — drive the full select → build → test → fold state machine
//!   ([`drive_verify`]) with the real caged runner
//!   ([`crate::cell_verify::CellVerifyRunner::confined`]): every test unit's
//!   language-native build+test is spawned **inside the cage**, authorized only
//!   by the binding's `allowed_commands` allowlist. A command not in the
//!   allowlist comes back as a blocking `not-allowed` skip (`needs-run`), never
//!   spawned; a stale index degrades to `stale-index`; an UNKNOWN frontier
//!   (`--partial`) caps an otherwise-green run at `Green_low`. "Did not run" is
//!   never laundered into "passed".
//! * **`--dry-run`** — drive the identical state machine with a hermetic
//!   zero-side-effect runner ([`DryRunner`]): every unit "builds" and "passes"
//!   with no subprocess ever spawned, so the whole loop (select → fold →
//!   downgrade → telemetry) is exercisable with no toolchain present. This is
//!   the analog of `shuttle goal --dry-run`'s no-op executor: an explicit,
//!   caller-requested preview, not a claim about the code.
//!
//! # The cage (#11)
//! shuttle owns no land权 and spawns nothing it was not authorized to. `--run`
//! needs a [`CellBinding`] cage: load one from `--binding <file>` (a external-/human-
//! /pre-run-produced binding, validated fail-closed) or, absent that, shuttle
//! synthesizes a **default local cage** rooted at the (canonicalized) `--root`
//! that authorizes exactly the language-native build/test toolchains shuttle knows
//! how to drive (`cargo`/`go`/`pytest`/`npm`). The synthesized cage authorizes
//! only those programs — anything else a runner might try is fail-closed.
//!
//! Exit is always 0 (like split/impact/goal): a Blocked/Error envelope is a
//! machine-readable result the caller routes on, not a process failure.

use shuttle_nav::verify::{drive_verify, BuildResult, TestResult, VerifyRunner};
use shuttle_protocol::{
    AllowedCommand, CellBinding, ReverifyUnit, ToolOutcome, TypedDetail, VerifyTelemetry,
    CELL_BINDING_VERSION,
};

/// The hermetic `--dry-run` runner: every unit "builds" and "passes" without a
/// single subprocess. It mirrors `shuttle goal --dry-run`'s no-op executor — an
/// explicit, caller-requested preview that exercises the full select → fold →
/// downgrade path with no toolchain present. It never spawns and never touches
/// the filesystem, so it is safe to run anywhere; the settled envelope reflects
/// only the *selection* (size, staleness, `--partial` frontier), not the code.
struct DryRunner;

impl VerifyRunner for DryRunner {
    fn build(&mut self, _unit: &ReverifyUnit, _cage: &CellBinding) -> BuildResult {
        BuildResult::Built
    }
    fn test(&mut self, _unit: &ReverifyUnit, _cage: &CellBinding) -> TestResult {
        TestResult::Passed
    }
}

/// Programs shuttle's [`crate::cell_verify::CellVerifyRunner`] knows how to drive for
/// a language-native build+test. The synthesized default cage authorizes
/// exactly these (no `argv_prefix`, so both the `build` and `test` argv of each
/// toolchain match); nothing else is spawnable. Keep in lockstep with
/// `cell_verify::commands_for`.
const DEFAULT_CAGE_PROGRAMS: &[&str] = &["cargo", "go", "pytest", "npm"];

/// Build the `--run` cage: an explicit `--binding <file>` (validated
/// fail-closed) or a synthesized local cage rooted at `root` authorizing only
/// the native build/test toolchains shuttle can drive.
fn build_cage(binding: Option<String>, root: &str) -> anyhow::Result<CellBinding> {
    // `--run` must build/test against the real tree, so the cage root is the
    // canonicalized repo root (the same path the runner's ScopedPathResolver
    // would guard). Fall back to the raw root if it does not yet exist on disk.
    let abs_root = std::fs::canonicalize(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string());

    match binding {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read cell binding {path}: {e}"))?;
            let cage = CellBinding::parse(&raw)
                .map_err(|e| anyhow::anyhow!("invalid cell binding {path}: {e}"))?;
            // parse() already validates; be explicit so a hand-authored binding
            // that skirts parse invariants still fails closed here.
            cage.validate()
                .map_err(|e| anyhow::anyhow!("cell binding {path} failed validation: {e}"))?;
            Ok(cage)
        }
        None => Ok(synth_default_cage(&abs_root)),
    }
}

/// A minimal local cage authorizing only the native build/test toolchains shuttle
/// can drive, rooted at the (absolute) repo root. Everything else is
/// fail-closed. Used when `--run` is requested without an explicit `--binding`.
fn synth_default_cage(abs_root: &str) -> CellBinding {
    CellBinding {
        version: CELL_BINDING_VERSION,
        workspace_root: abs_root.to_string(),
        allowed_paths: vec!["**".to_string()],
        allowed_commands: DEFAULT_CAGE_PROGRAMS
            .iter()
            .map(|p| AllowedCommand {
                program: p.to_string(),
                argv_prefix: None,
            })
            .collect(),
        base_oid: String::new(),
        assignment_digest: String::new(),
        runtime_descriptor_ref: None,
        runtime_descriptor_digest: None,
        runtime_descriptor: None,
        project: shuttle_protocol::CellProjectBinding::LegacyUnbound,
        shuttle_binary_digest: None,
        environment_id: "shuttle-verify-local".to_string(),
        provider_run_id: None,
        attempt_id: None,
        runtime_id: None,
        package_id: None,
        profile_ref: None,
        session_file: None,
        repo_set: None,
    }
}

/// `shuttle verify` entrypoint. `--base/--head` are required (clap enforces the pair
/// upstream); the mode is chosen by `--run`/`--dry-run` (default: compute-only).
#[allow(clippy::too_many_arguments)]
pub fn run_verify(
    base: String,
    head: String,
    root: String,
    run: bool,
    dry_run: bool,
    binding: Option<String>,
    max_depth: usize,
    partial: bool,
    telemetry: bool,
    json: bool,
) -> anyhow::Result<()> {
    let opts = shuttle_nav::QueryOptions::new(&root);

    // Compute-only (the default): a pure alias of `shuttle impact --base/--head`.
    // Nothing runs; the envelope is the T2 Reverify set with its staleness
    // watermark, and the telemetry line is the same reverify_size projection.
    if !run && !dry_run {
        let outcome = shuttle_nav::reverify::reverify_impact(&base, &head, max_depth, &opts)?;
        if telemetry {
            if let Some(size) = shuttle_protocol::reverify_size_of(&outcome) {
                let line = serde_json::json!({ "reverify_size": size });
                eprintln!("{}", serde_json::to_string(&line)?);
            }
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        } else {
            crate::print_reverify_outcome(&outcome);
        }
        return Ok(());
    }

    // Run/dry-run: drive the full state machine. `--dry-run` takes precedence —
    // it swaps the caged runner for the hermetic no-op so the loop is
    // exercisable with no toolchain. The cage is still built (dry-run honors the
    // same shape) but never spawned into.
    let cage = build_cage(binding, &root)?;
    let outcome = if dry_run {
        drive_verify(
            &base,
            &head,
            max_depth,
            partial,
            &opts,
            &cage,
            &mut DryRunner,
        )?
    } else {
        let mut runner =
            crate::cell_verify::CellVerifyRunner::confined(cage.workspace_root.clone());
        drive_verify(&base, &head, max_depth, partial, &opts, &cage, &mut runner)?
    };

    // T5: one VerifyTelemetry JSON line to stderr (a pure projection of the
    // settled VerifyDetail — it counts what ran, never re-derives). stdout stays
    // the ToolOutcome envelope. A non-Verify detail (e.g. an early Error) yields
    // None and we simply skip the line.
    if telemetry {
        if let Some(vt) = VerifyTelemetry::from_outcome(&outcome) {
            eprintln!("{}", serde_json::to_string(&vt)?);
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        print_verify_outcome(&outcome);
    }

    // Exit 0 even on Blocked/Error: the state/reason fields are the machine
    // contract the caller routes on, matching split/impact/goal.
    Ok(())
}

/// Human-readable rendering of a `--run`/`--dry-run` verify [`ToolOutcome`]
/// (JSON mode is the machine contract; this is the eyeball view). A non-Verify
/// detail (an Error re-stamped under `verify`) falls back to state + reason.
fn print_verify_outcome(o: &ToolOutcome) {
    println!("state: {}  reason: {}", o.state.as_str(), o.reason_code);
    if let Some(s) = &o.summary {
        println!("{s}");
    }
    if let TypedDetail::Verify(d) = &o.detail {
        println!(
            "base: {}  head: {}  reverify_size: {}  ran: {}  passed: {}  partial: {}  degraded: {}",
            d.base_oid, d.head_oid, d.reverify_size, d.ran, d.passed, d.partial, d.degraded
        );
        for f in &d.failed {
            println!("  FAIL [{}] {}  {}", f.kind, f.unit, f.evidence);
        }
        for s in &d.skipped {
            let tag = if s.is_blocking() { "SKIP*" } else { "skip" };
            println!("  {tag} {}  ({})", s.unit, s.reason);
        }
    }
    if let Some(a) = &o.next_action {
        println!("next: {a}");
    }
}
