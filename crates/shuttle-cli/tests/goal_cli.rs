//! Binary-level CLI integration tests for `shuttle goal` — the §4 GoalGraph outer
//! loop . Spawns the
//! REAL `shuttle` binary and exercises the whole outer loop end-to-end with ZERO
//! model calls via the two hermetic seams:
//!
//!   * `--plan <intent/v1 file>` supplies the intent DAG without spawning pi;
//!   * `--dry-run` swaps the per-task executor for a no-op.
//!
//! Together they let both the Green-lands path AND every escalation path
//! (unprovable edge, proven cycle) be tested against the real symbol graph
//! without an LLM. Exit is always 0 — the machine result (phase/escalated/
//! review) is the contract a caller routes on, matching split/impact.
//!
//! Hermetic: scratch repos live under the OS temp root (honours TMPDIR). The
//! ctags-backed cases skip without universal-ctags; the git-backed land case
//! also skips when a scratch `.git/config` cannot be created (some sandboxes
//! block it).

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
    let dir = std::env::temp_dir().join(format!("shuttle_goal_it_{tag}_{nanos}"));
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

/// Write an `intent/v1` DAG file and return its path.
fn write_intent(dir: &std::path::Path, json: serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("intent.json");
    std::fs::write(&p, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    p
}

/// Run `shuttle goal` with the given extra args, parsing stdout as the GoalResult
/// JSON. Asserts exit 0 (the machine contract). Returns (result, stderr).
fn run_goal_json(
    ws: &std::path::Path,
    plan: &std::path::Path,
    extra: &[&str],
) -> (serde_json::Value, String) {
    let mut args: Vec<String> = vec![
        "goal".into(),
        "--plan".into(),
        plan.to_str().unwrap().into(),
        "--root".into(),
        ws.to_str().unwrap().into(),
        "--dry-run".into(),
        "--json".into(),
    ];
    for e in extra {
        args.push((*e).into());
    }
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle goal");
    assert!(
        out.status.success(),
        "shuttle goal exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("goal stdout is not the JSON GoalResult: {e}\n---\n{stdout}"));
    (v, String::from_utf8_lossy(&out.stderr).into_owned())
}

// ---------------------------------------------------------------------------
// Green-lands: the planner's guess matches the symbol graph → AGREE, topo runs.
// ---------------------------------------------------------------------------

#[test]
fn goal_green_lands_in_topological_order() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal green-lands E2E");
        return;
    }
    let ws = scratch_dir("green");
    seed_wrapper_core(&ws);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "edit core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "edit wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let (r, _stderr) = run_goal_json(&ws, &plan, &[]);

    assert_eq!(r["phase"], "landed", "expected landed: {r}");
    assert_eq!(
        r["escalated"], false,
        "must not escalate on a clean AGREE: {r}"
    );
    // Prerequisite before dependent.
    let order: Vec<&str> = r["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(order, vec!["core", "wrapper"], "topo order wrong: {r}");
    // One exec record per task, carrying the (dry-run zero) apply counts.
    let exec = r["exec"].as_array().unwrap();
    assert_eq!(exec.len(), 2, "expected 2 exec records: {r}");
    assert_eq!(exec[0]["id"], "core");
    assert_eq!(exec[0]["order_index"], 0);
    assert_eq!(exec[1]["id"], "wrapper");
    assert_eq!(exec[1]["order_index"], 1);
    // Dry-run executor proposes/accepts nothing (§7 never-invent: honest zero).
    for rec in exec {
        assert_eq!(rec["patches"], 0, "dry-run must record zero patches: {r}");
        assert_eq!(rec["accepted"], 0);
        assert_eq!(rec["proposed"], 0);
    }
    // The adjudication saw exactly one AGREE edge, zero review surface.
    assert_eq!(r["feedback"]["agree"], 1, "feedback: {r}");
    assert_eq!(r["feedback"]["review_surface"], 0, "feedback: {r}");
    // No land window given → no reverify set.
    assert!(
        r.get("reverify").is_none(),
        "no --base/--head → no reverify: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Escalation A: an unprovable edge (in-universe but no import corroboration)
// stops the loop at the review gate — never guessed, never executed.
// ---------------------------------------------------------------------------

#[test]
fn goal_unprovable_edge_escalates_to_review() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal unprovable-edge E2E");
        return;
    }
    let ws = scratch_dir("unknown");
    // Two files with NO import relationship between them.
    write(&ws, "core.py", "def core_fn():\n    return 1\n");
    write(&ws, "other.py", "def other_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",  "intent": "edit core",  "touched_files_guess": ["core.py"],  "depends_on_guess": [] },
                // A GUESSED dependency the symbol graph can neither prove nor disprove.
                { "id": "other", "intent": "edit other", "touched_files_guess": ["other.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let (r, _stderr) = run_goal_json(&ws, &plan, &[]);

    assert_eq!(
        r["phase"], "escalated",
        "unprovable edge must escalate: {r}"
    );
    assert_eq!(r["escalated"], true, "{r}");
    // The review surface names the undecided edge; nothing executed.
    let review = r["review"].as_array().unwrap();
    assert!(!review.is_empty(), "review surface must be non-empty: {r}");
    assert!(
        review
            .iter()
            .any(|m| m.as_str().unwrap_or("").contains("core")
                && m.as_str().unwrap_or("").contains("other")),
        "review must name the undecided core→other edge: {r}"
    );
    assert!(
        r["exec"].as_array().map(|a| a.is_empty()).unwrap_or(true),
        "must not execute when escalated: {r}"
    );
    assert_eq!(r["feedback"]["unknown"], 1, "feedback: {r}");
    assert!(
        r["feedback"]["review_surface"].as_u64().unwrap() >= 1,
        "feedback: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Escalation B: a PROVEN cycle (mutual imports, both edges AGREE) is detected
// at topological ordering and escalated — the DAG cannot run.
// ---------------------------------------------------------------------------

#[test]
fn goal_proven_cycle_escalates() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal cycle E2E");
        return;
    }
    let ws = scratch_dir("cycle");
    // Mutual imports: core imports wrapper AND wrapper imports core. The symbol
    // graph proves BOTH directions, so both intent edges are AGREE, but the two
    // tasks form a dependency cycle that topological ordering must reject.
    write(
        &ws,
        "core.py",
        "import wrapper\n\ndef core_fn():\n    return wrapper.wrap()\n",
    );
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "cyclic",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "edit core",    "touched_files_guess": ["core.py"],    "depends_on_guess": ["wrapper"] },
                { "id": "wrapper", "intent": "edit wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let (r, _stderr) = run_goal_json(&ws, &plan, &[]);

    assert_eq!(r["phase"], "escalated", "proven cycle must escalate: {r}");
    assert_eq!(r["escalated"], true, "{r}");
    let review = r["review"].as_array().unwrap();
    assert!(
        review
            .iter()
            .any(|m| m.as_str().unwrap_or("").contains("cycle")),
        "review must report the dependency cycle: {r}"
    );
    assert!(
        r["exec"].as_array().map(|a| a.is_empty()).unwrap_or(true),
        "cycle must not execute: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// T5 telemetry outlet: --telemetry emits one GoalTelemetry JSON line on stderr;
// without the flag, nothing leaks. stdout stays the machine result either way.
// ---------------------------------------------------------------------------

#[test]
fn goal_telemetry_flag_emits_goal_telemetry_to_stderr() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal --telemetry E2E");
        return;
    }
    let ws = scratch_dir("tel_on");
    seed_wrapper_core(&ws);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let (_r, stderr) = run_goal_json(&ws, &plan, &["--telemetry"]);

    // The GoalTelemetry line is identified by its distinctive field set.
    let tel = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| {
            v.get("intent_edges").is_some()
                && v.get("reverify_size").is_some()
                && v.get("accept_rate").is_some()
        })
        .unwrap_or_else(|| panic!("no GoalTelemetry line on stderr:\n---\n{stderr}"));
    assert_eq!(tel["intent_edges"], 1, "telemetry: {tel}");
    assert_eq!(tel["mechanical_edges"], 1, "telemetry: {tel}");
    assert_eq!(tel["review_surface"], 0, "telemetry: {tel}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn goal_without_telemetry_emits_no_goal_telemetry_line() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal no-telemetry E2E");
        return;
    }
    let ws = scratch_dir("tel_off");
    seed_wrapper_core(&ws);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let (_r, stderr) = run_goal_json(&ws, &plan, &[]);
    let leaked = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .any(|v| {
            v.get("intent_edges").is_some()
                && v.get("reverify_size").is_some()
                && v.get("accept_rate").is_some()
        });
    assert!(
        !leaked,
        "GoalTelemetry leaked without --telemetry:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Human (non-JSON) rendering still exits 0 and prints the phase line.
// ---------------------------------------------------------------------------

#[test]
fn goal_human_output_prints_phase_and_exits_zero() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping goal human-output E2E");
        return;
    }
    let ws = scratch_dir("human");
    seed_wrapper_core(&ws);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .expect("spawn shuttle goal (human)");
    assert!(
        out.status.success(),
        "human goal exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("phase: landed"),
        "human output missing phase line:\n{stdout}"
    );
    assert!(
        stdout.contains("execution order: core -> wrapper"),
        "human output missing order:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Argument contract: neither --goal nor --plan is a clap error (non-zero),
// and --goal + --plan together is rejected (mutual exclusion).
// ---------------------------------------------------------------------------

#[test]
fn goal_requires_exactly_one_of_goal_or_plan() {
    // Neither → error.
    let neither = Command::new(shuttle_bin())
        .args(["goal", "--root", "."])
        .output()
        .expect("spawn shuttle goal (neither)");
    assert!(
        !neither.status.success(),
        "shuttle goal with neither --goal nor --plan must fail"
    );

    // Both → error (conflicts_with).
    let both = Command::new(shuttle_bin())
        .args(["goal", "--goal", "x", "--plan", "p.json", "--root", "."])
        .output()
        .expect("spawn shuttle goal (both)");
    assert!(
        !both.status.success(),
        "shuttle goal with both --goal and --plan must fail"
    );
}

// ---------------------------------------------------------------------------
// Land-time reverify (T2): with --base/--head over a real git diff, the loop
// lands AND computes a non-empty minimal re-verify set. git-gated.
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
fn goal_land_mode_computes_reverify_set() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git-init unavailable; skipping goal land-mode E2E");
        return;
    }
    let ws = scratch_dir("land");
    init_repo(&ws);
    // Base: core + wrapper. Head: mutate core's body (wrapper depends on it, so
    // the reverse-dependency closure must include wrapper too).
    seed_wrapper_core(&ws);
    let base = commit_all(&ws, "base");
    write(
        &ws,
        "core.py",
        "def core_fn():\n    return 42\n\nclass Core:\n    pass\n",
    );
    let head = commit_all(&ws, "head");

    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--dry-run",
            "--base",
            &base,
            "--head",
            &head,
            "--json",
        ])
        .output()
        .expect("spawn shuttle goal (land)");
    assert!(
        out.status.success(),
        "goal land exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let r: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("goal land stdout is not JSON: {e}\n---\n{stdout}"));

    assert_eq!(r["phase"], "landed", "land mode must land: {r}");
    // The reverify envelope is present and is a reverify detail.
    let rev = r
        .get("reverify")
        .unwrap_or_else(|| panic!("land mode must carry a reverify set: {r}"));
    assert_eq!(
        rev.get("detail")
            .and_then(|d| d.get("kind"))
            .and_then(|k| k.as_str()),
        Some("reverify"),
        "reverify must be a reverify detail: {r}"
    );
    let size = rev
        .get("detail")
        .and_then(|d| d.get("reverify_size"))
        .and_then(|s| s.as_u64())
        .unwrap_or_else(|| panic!("reverify missing reverify_size: {r}"));
    assert!(
        size >= 1,
        "at least the changed core.py is in the re-verify set: {r}"
    );
    // The telemetry mirrors the envelope's reverify_size (pure projection).
    assert_eq!(
        r["telemetry"]["reverify_size"].as_u64().unwrap(),
        size,
        "telemetry reverify_size must match envelope: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
