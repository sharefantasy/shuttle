//! Binary-level CLI integration tests for `shuttle verify` — the G5 axis-2
//! **execution-truth** loop .
//! Spawns the REAL `shuttle` binary and drives all three faces of the same T2
//! selection end-to-end:
//!
//!   * **compute-only** (no `--run`/`--dry-run`) — a pure alias of
//!     `shuttle impact --base/--head`: emits the Reverify set, runs nothing.
//!   * **`--dry-run`** — the identical select → fold → downgrade loop driven by
//!     the hermetic zero-side-effect runner (every unit "builds" + "passes"),
//!     so every §7 downgrade branch is exercisable with NO toolchain present.
//!   * **`--run`** — the real caged runner actually builds + tests each affected
//!     unit inside the `cell-run` cage. Gated on the toolchain (pytest) being
//!     present; the allowlist-refusal path needs no toolchain at all.
//!
//! The iron contract under test (§7 「绝不瞎猜」 / red line #20): a unit that
//! *did not run* is NEVER laundered into a pass. A command outside the cage
//! allowlist is a blocking `needs-run`; a stale index blocks `stale-index`; an
//! UNKNOWN frontier (`--partial`) caps an all-green run at `Green_low`. Exit is
//! ALWAYS 0 — the state/reason envelope is the machine contract a caller routes
//! on, matching split/impact/goal.
//!
//! Hermetic: scratch repos live under the OS temp root (honours TMPDIR). The
//! git-backed cases skip when a scratch `.git/config` cannot be created; the
//! real-`--run` cases additionally skip without a discoverable `pytest`.

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn can_init_git() -> bool {
    let probe = scratch_dir("initprobe");
    let ok = Command::new("git")
        .args(["init", "-q", "--template="])
        .current_dir(&probe)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && probe.join(".git").join("config").exists();
    let _ = std::fs::remove_dir_all(&probe);
    ok
}

/// A real `pytest` we can actually spawn (the `--run` execution-truth cases
/// need it; the compute-only / dry-run / allowlist-refusal cases do not).
fn pytest_present() -> bool {
    Command::new("pytest")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_verify_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &std::path::Path) {
    run_git(dir, &["init", "-q", "--template="]);
    run_git(dir, &["config", "user.email", "t@t"]);
    run_git(dir, &["config", "user.name", "t"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_all(dir: &std::path::Path, msg: &str) -> String {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", msg]);
    run_git(dir, &["rev-parse", "HEAD"])
}

/// Run `shuttle verify` with the given args, parsing stdout as the ToolOutcome JSON.
/// Asserts exit 0 (the machine contract). Returns (outcome_json, stderr).
fn run_verify_json(ws: &std::path::Path, extra: &[&str]) -> (serde_json::Value, String) {
    let mut args: Vec<String> = vec![
        "verify".into(),
        "--root".into(),
        ws.to_str().unwrap().into(),
        "--json".into(),
    ];
    for e in extra {
        args.push((*e).into());
    }
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle verify");
    assert!(
        out.status.success(),
        "shuttle verify exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("verify stdout is not JSON ToolOutcome: {e}\n---\n{stdout}"));
    (v, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// A repo with a module + its test. Returns (base_oid, head_oid) where `head`
/// mutates the TEST file's body, so the test unit is in the changed set at
/// distance 0 — no ctags/closure needed for it to be selected + run.
fn seed_and_commit(
    ws: &std::path::Path,
    test_name: &str,
    base_body: &str,
    head_body: &str,
) -> (String, String) {
    init_repo(ws);
    write(ws, "core.py", "def core():\n    return 1\n");
    write(ws, test_name, base_body);
    let base = commit_all(ws, "base");
    write(ws, test_name, head_body);
    let head = commit_all(ws, "head");
    (base, head)
}

// ---------------------------------------------------------------------------
// compute-only (the default): a pure alias of `shuttle impact --base/--head`.
// Emits the Reverify set, runs nothing. Zero behavior change vs `shuttle impact`.
// ---------------------------------------------------------------------------

#[test]
fn verify_compute_only_is_a_reverify_alias_of_impact() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify compute-only E2E");
        return;
    }
    let ws = scratch_dir("compute");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    let (r, _stderr) = run_verify_json(&ws, &["--base", &base, "--head", &head]);

    // Compute-only never runs: the detail is the T2 Reverify set, NOT a Verify
    // execution result. This is exactly what `shuttle impact --base/--head` emits.
    assert_eq!(
        r["state"], "green",
        "fresh index compute-only is green: {r}"
    );
    assert_eq!(
        r["detail"]["kind"], "reverify",
        "compute-only must be a reverify alias, not an execution result: {r}"
    );
    let size = r["detail"]["reverify_size"].as_u64().unwrap();
    assert!(size >= 1, "the changed test unit is in the set: {r}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn verify_compute_only_telemetry_emits_reverify_size_line() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify compute-only telemetry E2E");
        return;
    }
    let ws = scratch_dir("compute_tel");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    let (r, stderr) = run_verify_json(&ws, &["--base", &base, "--head", &head, "--telemetry"]);
    let size = r["detail"]["reverify_size"].as_u64().unwrap();

    // Same `{"reverify_size":N}` projection as `shuttle impact --telemetry`.
    let line = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.get("reverify_size").is_some())
        .unwrap_or_else(|| panic!("no reverify_size telemetry line on stderr:\n{stderr}"));
    assert_eq!(
        line["reverify_size"].as_u64().unwrap(),
        size,
        "compute-only telemetry must mirror the envelope reverify_size"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// --dry-run: the full select -> fold -> downgrade loop with the hermetic no-op
// runner. Every test unit "builds" + "passes"; non-test units are benign
// no-test skips. Exercises the whole state machine with NO toolchain.
// ---------------------------------------------------------------------------

#[test]
fn verify_dry_run_is_honest_green_and_a_verify_detail() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify --dry-run green E2E");
        return;
    }
    let ws = scratch_dir("dry_green");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    let (r, _stderr) = run_verify_json(&ws, &["--dry-run", "--base", &base, "--head", &head]);

    // The dry-run drives the fold: the payload is a Verify execution result.
    assert_eq!(r["state"], "green", "dry-run all-pass is green: {r}");
    assert_eq!(r["reason_code"], "ok", "full green, not low: {r}");
    assert_eq!(
        r["detail"]["kind"], "verify",
        "--dry-run drives the execution fold: {r}"
    );
    let ran = r["detail"]["ran"].as_u64().unwrap();
    let passed = r["detail"]["passed"].as_u64().unwrap();
    assert!(
        ran >= 1,
        "the changed test unit ran under the no-op runner: {r}"
    );
    assert_eq!(ran, passed, "dry-run passes everything it runs: {r}");
    assert_eq!(r["detail"]["partial"], false, "no frontier caveat: {r}");
    assert_eq!(r["detail"]["degraded"], false, "fresh index: {r}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn verify_dry_run_telemetry_toggles_verify_telemetry_line() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify --dry-run telemetry E2E");
        return;
    }
    let ws = scratch_dir("dry_tel");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    // ON: the VerifyTelemetry line (identified by its distinctive field set) is
    // on stderr; stdout stays the envelope.
    let (r_on, stderr_on) = run_verify_json(
        &ws,
        &["--dry-run", "--base", &base, "--head", &head, "--telemetry"],
    );
    assert_eq!(r_on["detail"]["kind"], "verify");
    let tel = stderr_on
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| {
            v.get("reverify_size").is_some()
                && v.get("ran").is_some()
                && v.get("pass_rate").is_some()
        })
        .unwrap_or_else(|| panic!("no VerifyTelemetry line on stderr:\n{stderr_on}"));
    assert_eq!(
        tel["ran"].as_u64().unwrap(),
        r_on["detail"]["ran"].as_u64().unwrap(),
        "telemetry ran must mirror the envelope"
    );
    assert_eq!(
        tel["passed"].as_u64().unwrap(),
        r_on["detail"]["passed"].as_u64().unwrap()
    );

    // OFF: no VerifyTelemetry line leaks.
    let (_r_off, stderr_off) =
        run_verify_json(&ws, &["--dry-run", "--base", &base, "--head", &head]);
    let leaked = stderr_off
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .any(|v| v.get("pass_rate").is_some() && v.get("blocking_skips").is_some());
    assert!(
        !leaked,
        "VerifyTelemetry leaked without --telemetry:\n{stderr_off}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn verify_partial_caps_an_all_green_run_at_green_low() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify --partial E2E");
        return;
    }
    let ws = scratch_dir("partial");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    // Same all-green dry-run, but an UNKNOWN frontier is asserted: even a fully
    // green run is capped at Green_low, never full green (§7 — we never claim to
    // have covered a frontier we cannot prove).
    let (r, _stderr) = run_verify_json(
        &ws,
        &["--dry-run", "--partial", "--base", &base, "--head", &head],
    );
    assert_eq!(r["state"], "green", "partial is still green, just low: {r}");
    assert_eq!(
        r["reason_code"], "ok-low-confidence",
        "--partial caps at Green_low: {r}"
    );
    assert_eq!(
        r["detail"]["partial"], true,
        "the frontier caveat is recorded: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// needs-run: a `--binding` cage that does NOT authorize the toolchain forces a
// blocking `not-allowed` skip -> `needs-run`. shuttle never spawns an unauthorized
// command, and never launders "did not run" into "passed". No toolchain needed
// — the gate refuses BEFORE the process boundary.
// ---------------------------------------------------------------------------

#[test]
fn verify_run_with_unauthorized_cage_is_needs_run_never_green() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify needs-run E2E");
        return;
    }
    let ws = scratch_dir("needs_run");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    // A valid cell binding rooted at the repo, authorizing ONLY `go` — the .py
    // test's `pytest` is not in the allowlist, so the test stage is a blocking
    // `not-allowed` skip. (Interpreted py has no build to gate.)
    let binding = serde_json::json!({
        "version": 1,
        "workspace_root": ws.to_str().unwrap(),
        "allowed_paths": ["**"],
        "allowed_commands": [ { "program": "go" } ],
        "base_oid": "",
        "assignment_digest": "",
        "environment_id": "shuttle-verify-test"
    });
    let binding_path = ws.join("cage.json");
    std::fs::write(
        &binding_path,
        serde_json::to_string_pretty(&binding).unwrap(),
    )
    .unwrap();

    let (r, _stderr) = run_verify_json(
        &ws,
        &[
            "--run",
            "--binding",
            binding_path.to_str().unwrap(),
            "--base",
            &base,
            "--head",
            &head,
        ],
    );

    assert_eq!(
        r["state"], "blocked",
        "unauthorized command must block: {r}"
    );
    assert_eq!(
        r["reason_code"], "needs-run",
        "an un-runnable unit is needs-run, never green: {r}"
    );
    assert_eq!(r["detail"]["kind"], "verify");
    assert_eq!(
        r["detail"]["ran"].as_u64().unwrap(),
        0,
        "nothing was run: {r}"
    );
    // The skip records the allowlist refusal against the test unit.
    let skipped = r["detail"]["skipped"].as_array().unwrap();
    assert!(
        skipped.iter().any(|s| s["reason"] == "not-allowed"
            && s["unit"].as_str().unwrap_or("").contains("test_core.py")),
        "skip must be a not-allowed refusal of the test unit: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// stale-index: when the worktree HEAD is NOT the requested `--head`, the symbol
// graph is stale relative to head, so even an all-green run blocks `stale-index`
// (C11 — shuttle never runs a "green" off a stale graph).
// ---------------------------------------------------------------------------

#[test]
fn verify_stale_index_blocks_even_when_units_would_pass() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify stale-index E2E");
        return;
    }
    let ws = scratch_dir("stale");
    init_repo(&ws);
    write(&ws, "core.py", "def core():\n    return 1\n");
    write(&ws, "test_core.py", "def test_core():\n    assert True\n");
    let base = commit_all(&ws, "base");
    write(&ws, "test_core.py", "def test_core():\n    assert 1 == 1\n");
    let head = commit_all(&ws, "head");
    // Move the worktree PAST `head` so the live index reflects a newer commit —
    // the graph is now stale relative to the requested `--head`.
    write(&ws, "extra.py", "def extra():\n    return 2\n");
    let _third = commit_all(&ws, "third");

    let (r, _stderr) = run_verify_json(&ws, &["--dry-run", "--base", &base, "--head", &head]);

    assert_eq!(r["state"], "blocked", "stale index must block: {r}");
    assert_eq!(
        r["reason_code"], "stale-index",
        "a stale graph is never silently trusted: {r}"
    );
    assert_eq!(r["detail"]["kind"], "verify");
    assert_eq!(
        r["detail"]["degraded"], true,
        "the staleness watermark is set: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// empty diff: base == head -> nothing changed -> nothing to run, nothing
// withheld -> honest Green with an empty run.
// ---------------------------------------------------------------------------

#[test]
fn verify_empty_diff_short_circuits_to_honest_green() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify empty-diff E2E");
        return;
    }
    let ws = scratch_dir("empty");
    init_repo(&ws);
    write(&ws, "core.py", "def core():\n    return 1\n");
    let head = commit_all(&ws, "only");

    // base == head: the diff is empty.
    let (r, _stderr) = run_verify_json(&ws, &["--dry-run", "--base", &head, "--head", &head]);
    assert_eq!(r["state"], "green", "empty diff is green: {r}");
    assert_eq!(r["detail"]["kind"], "verify");
    assert_eq!(r["detail"]["ran"].as_u64().unwrap(), 0, "nothing ran: {r}");
    assert_eq!(
        r["detail"]["reverify_size"].as_u64().unwrap(),
        0,
        "empty set: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Real execution truth (--run): the caged runner actually spawns pytest inside
// the synthesized default cage. Gated on a discoverable pytest.
// ---------------------------------------------------------------------------

#[test]
fn verify_run_real_pytest_green_is_proven_not_inferred() {
    if !can_init_git() || !pytest_present() {
        eprintln!("git-init/pytest unavailable; skipping verify --run green E2E");
        return;
    }
    let ws = scratch_dir("run_green");
    // A self-contained, dependency-free passing test so pytest is deterministic.
    let (base, head) = seed_and_commit(
        &ws,
        "test_ok.py",
        "def test_ok():\n    assert True\n",
        "def test_ok():\n    assert 2 == 2\n",
    );

    // No --binding: shuttle synthesizes a default local cage authorizing pytest.
    let (r, _stderr) = run_verify_json(&ws, &["--run", "--base", &base, "--head", &head]);

    assert_eq!(
        r["state"], "green",
        "a really-run passing test is green: {r}"
    );
    assert_eq!(r["reason_code"], "ok");
    assert_eq!(r["detail"]["kind"], "verify");
    assert!(
        r["detail"]["ran"].as_u64().unwrap() >= 1,
        "the test unit was actually run: {r}"
    );
    assert_eq!(
        r["detail"]["ran"].as_u64().unwrap(),
        r["detail"]["passed"].as_u64().unwrap(),
        "all run units passed: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn verify_run_real_pytest_failure_blocks_test_blocked() {
    if !can_init_git() || !pytest_present() {
        eprintln!("git-init/pytest unavailable; skipping verify --run test-blocked E2E");
        return;
    }
    let ws = scratch_dir("run_fail");
    // Head mutates the test into a red assertion — a real failing run.
    let (base, head) = seed_and_commit(
        &ws,
        "test_bad.py",
        "def test_bad():\n    assert True\n",
        "def test_bad():\n    assert 1 == 2\n",
    );

    let (r, _stderr) = run_verify_json(&ws, &["--run", "--base", &base, "--head", &head]);

    assert_eq!(r["state"], "blocked", "a red test blocks: {r}");
    assert_eq!(
        r["reason_code"], "test-blocked",
        "a really-failed test is test-blocked, never green: {r}"
    );
    let failed = r["detail"]["failed"].as_array().unwrap();
    assert!(
        failed.iter().any(|f| f["kind"] == "test"),
        "a test-stage failure is recorded with evidence: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Human (non-JSON) rendering still exits 0 and prints the state line.
// ---------------------------------------------------------------------------

#[test]
fn verify_human_output_prints_state_and_exits_zero() {
    if !can_init_git() {
        eprintln!("git-init unavailable; skipping verify human-output E2E");
        return;
    }
    let ws = scratch_dir("human");
    let (base, head) = seed_and_commit(
        &ws,
        "test_core.py",
        "def test_core():\n    assert True\n",
        "def test_core():\n    assert 1 == 1\n",
    );

    let out = Command::new(shuttle_bin())
        .args([
            "verify",
            "--dry-run",
            "--root",
            ws.to_str().unwrap(),
            "--base",
            &base,
            "--head",
            &head,
        ])
        .output()
        .expect("spawn shuttle verify (human)");
    assert!(
        out.status.success(),
        "human verify exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("state: green"),
        "human output missing state line:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Argument contract: --run and --dry-run are mutually exclusive, and --base
// requires --head (each half of the pair requires the other). These are clap
// errors (non-zero exit), NOT the always-0 machine envelope.
// ---------------------------------------------------------------------------

#[test]
fn verify_run_and_dry_run_are_mutually_exclusive() {
    let out = Command::new(shuttle_bin())
        .args([
            "verify",
            "--base",
            "x",
            "--head",
            "y",
            "--run",
            "--dry-run",
            "--root",
            ".",
        ])
        .output()
        .expect("spawn shuttle verify (run+dry-run)");
    assert!(
        !out.status.success(),
        "shuttle verify with both --run and --dry-run must fail"
    );
}

#[test]
fn verify_base_requires_head() {
    let out = Command::new(shuttle_bin())
        .args(["verify", "--base", "x", "--root", "."])
        .output()
        .expect("spawn shuttle verify (base without head)");
    assert!(
        !out.status.success(),
        "shuttle verify --base without --head must fail"
    );
}
