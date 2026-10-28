//! Binary-level CLI integration tests for the T5 `--telemetry` outlets
//! . Both `shuttle split
//! --correct` and `shuttle impact --base/--head` gain an opt-in `--telemetry` flag
//! that emits a single JSON telemetry line to **stderr** while stdout keeps the
//! machine `ToolOutcome` envelope untouched. The line is a pure projection of
//! the already-adjudicated outcome (§7 「never invent」): `SplitFeedback` for
//! split, `{"reverify_size": N}` for the land-time reverify set.
//!
//! Hermetic: scratch repos live under the OS temp root (honours TMPDIR). The
//! git-backed cases skip when a scratch `.git/config` cannot be created (some
//! sandboxes block it); the ctags-backed cases skip without universal-ctags.

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn ctags_present() -> bool {
    Command::new("ctags")
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
    let dir = std::env::temp_dir().join(format!("shuttle_telemetry_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

/// Two Python files where `wrapper` imports and calls into `core`; the symbol
/// graph therefore PROVES wrapper depends on core (direction core -> wrapper).
fn seed_wrapper_core(dir: &std::path::Path) {
    write(
        dir,
        "core.py",
        "def core_fn():\n    return 1\n\nclass Core:\n    pass\n",
    );
    write(
        dir,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
}

/// An `intent/v1` DAG where the planner CORRECTLY guessed `wrapper depends_on
/// core`. The corrector should return a single AGREE edge, provenance Both,
/// zero review surface — so the SplitFeedback line is fully determined.
fn write_agree_intent(dir: &std::path::Path) -> std::path::PathBuf {
    let plan = serde_json::json!({
        "goal": "demo",
        "version": "intent/v1",
        "tasks": [
            { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
            { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
        ]
    });
    let p = dir.join("intent.json");
    std::fs::write(&p, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
    p
}

// ---------------------------------------------------------------------------
// shuttle split --correct --telemetry
// ---------------------------------------------------------------------------

#[test]
fn split_telemetry_emits_feedback_to_stderr_and_keeps_envelope_on_stdout() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping split --telemetry E2E");
        return;
    }
    let ws = scratch_dir("split_on");
    seed_wrapper_core(&ws);
    let plan = write_agree_intent(&ws);

    let out = Command::new(shuttle_bin())
        .args([
            "split",
            "--correct",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--telemetry",
            "--json",
        ])
        .output()
        .expect("spawn shuttle split --correct --telemetry");
    assert!(
        out.status.success(),
        "shuttle split exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout is still the untouched ToolOutcome envelope.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("split stdout is not the JSON envelope: {e}\n---\n{stdout}"));
    assert!(
        env.get("state").and_then(|s| s.as_str()).is_some(),
        "envelope missing state: {stdout}"
    );
    assert!(
        env.get("detail").is_some(),
        "envelope missing detail: {stdout}"
    );

    // stderr carries exactly one SplitFeedback JSON object. tracing (info) may
    // also write to stderr, so scan for the single line that parses with our
    // schema fields rather than assuming stderr is pure JSON.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fb = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.get("total_edges").is_some() && v.get("review_surface").is_some())
        .unwrap_or_else(|| panic!("no SplitFeedback line on stderr:\n---\n{stderr}"));

    // The AGREE case: exactly one edge, decided as agree, zero review surface,
    // intent+mechanical both saw it (provenance Both counts on both sides).
    assert_eq!(fb["total_edges"], 1, "feedback: {fb}");
    assert_eq!(fb["agree"], 1, "feedback: {fb}");
    assert_eq!(fb["added_by_symbolgraph"], 0, "feedback: {fb}");
    assert_eq!(fb["dropped_redundant"], 0, "feedback: {fb}");
    assert_eq!(fb["conflict"], 0, "feedback: {fb}");
    assert_eq!(fb["unknown"], 0, "feedback: {fb}");
    assert_eq!(fb["review_surface"], 0, "feedback: {fb}");
    assert_eq!(fb["auto_decided"], 1, "feedback: {fb}");
    assert_eq!(fb["intent_edges"], 1, "feedback: {fb}");
    assert_eq!(fb["mechanical_edges"], 1, "feedback: {fb}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn split_without_telemetry_emits_no_feedback_line() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping split no-telemetry E2E");
        return;
    }
    let ws = scratch_dir("split_off");
    seed_wrapper_core(&ws);
    let plan = write_agree_intent(&ws);

    let out = Command::new(shuttle_bin())
        .args([
            "split",
            "--correct",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle split --correct (no telemetry)");
    assert!(out.status.success());

    // No stderr line should parse as a SplitFeedback object.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let leaked = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .any(|v| v.get("total_edges").is_some() && v.get("review_surface").is_some());
    assert!(!leaked, "telemetry leaked without --telemetry:\n{stderr}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// shuttle impact --base/--head --telemetry
// ---------------------------------------------------------------------------

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

#[test]
fn impact_land_telemetry_emits_reverify_size_to_stderr() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git-init unavailable; skipping impact --telemetry E2E");
        return;
    }
    let ws = scratch_dir("impact_on");
    init_repo(&ws);
    // Base commit: core only. Head commit: add wrapper that depends on core.
    write(&ws, "core.py", "def core_fn():\n    return 1\n");
    let base = commit_all(&ws, "base");
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let head = commit_all(&ws, "head");

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--base",
            &base,
            "--head",
            &head,
            "--root",
            ws.to_str().unwrap(),
            "--telemetry",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --base/--head --telemetry");
    assert!(
        out.status.success(),
        "shuttle impact land exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout stays the reverify envelope.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("impact stdout is not the JSON envelope: {e}\n---\n{stdout}"));
    // TypedDetail is internally tagged (`tag = "kind"`), so the reverify fields
    // are flattened onto `detail` alongside `"kind": "reverify"`.
    assert_eq!(
        env.get("detail")
            .and_then(|d| d.get("kind"))
            .and_then(|k| k.as_str()),
        Some("reverify"),
        "expected a reverify detail: {stdout}"
    );
    let detail_size = env
        .get("detail")
        .and_then(|d| d.get("reverify_size"))
        .and_then(|s| s.as_u64())
        .unwrap_or_else(|| panic!("envelope missing detail.reverify_size: {stdout}"));

    // stderr carries exactly the reverify_size projection, matching the envelope.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tel = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.get("reverify_size").is_some())
        .unwrap_or_else(|| panic!("no reverify_size line on stderr:\n---\n{stderr}"));
    let tel_size = tel["reverify_size"].as_u64().unwrap();
    assert_eq!(
        tel_size, detail_size,
        "stderr reverify_size must equal the envelope's (pure projection)"
    );
    assert!(tel_size >= 1, "at least the changed file is in the set");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn impact_land_without_telemetry_emits_no_reverify_size_line() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git-init unavailable; skipping impact no-telemetry E2E");
        return;
    }
    let ws = scratch_dir("impact_off");
    init_repo(&ws);
    write(&ws, "core.py", "def core_fn():\n    return 1\n");
    let base = commit_all(&ws, "base");
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let head = commit_all(&ws, "head");

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--base",
            &base,
            "--head",
            &head,
            "--root",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --base/--head (no telemetry)");
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    let leaked = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .any(|v| v.get("reverify_size").is_some());
    assert!(!leaked, "telemetry leaked without --telemetry:\n{stderr}");

    let _ = std::fs::remove_dir_all(&ws);
}
