//! Binary-level CLI integration tests for `shuttle goal --reconcile --diff` — the
//! G8/P3b reverse-reconcile surface . Spawns the REAL `shuttle` binary and drives the whole reverse path
//! end-to-end with ZERO model calls: it reads an intent DAG (`--plan`) and a
//! *landed* unified diff (`--diff`) off disk and re-derives, per intent task,
//! whether the diff satisfies / violates / leaves no trace of it, plus any diff
//! path that fits no task at all (`unattributed`).
//!
//! reconcile is the inverse of the forward goal drive: shuttle never lands (#11), so
//! the diff is an INPUT handed in by the caller. The three states each get a
//! dedicated assertion, and — the §7 red line — an `unattributed` path is
//! reported verbatim and NEVER hard-attributed to the nearest task.
//!
//! Exit is always 0; the ToolOutcome envelope (state/reason_code/detail) is the
//! machine contract. No ctags / no toolchain needed — the diff parser and the
//! footprint adjudication are pure.

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_goal_reconcile_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

fn write_intent(dir: &std::path::Path, json: serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("intent.json");
    std::fs::write(&p, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    p
}

/// Run `shuttle goal --reconcile --plan <plan> --diff <diff> --json`, parse stdout
/// as the ToolOutcome JSON, assert exit 0.
fn run_reconcile(plan: &std::path::Path, diff: &std::path::Path) -> serde_json::Value {
    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--reconcile",
            "--plan",
            plan.to_str().unwrap(),
            "--diff",
            diff.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle goal --reconcile");
    assert!(
        out.status.success(),
        "shuttle goal --reconcile exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("reconcile stdout is not the JSON ToolOutcome: {e}\n---\n{stdout}")
    })
}

/// The `verdict` string for a task id, or `None` if the task is absent.
fn verdict_of<'a>(r: &'a serde_json::Value, id: &str) -> Option<&'a str> {
    r["detail"]["tasks"]
        .as_array()?
        .iter()
        .find(|t| t["id"] == id)?["verdict"]
        .as_str()
}

fn unattributed(r: &serde_json::Value) -> Vec<String> {
    r["detail"]["unattributed"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// A git-style unified diff hunk that ADDS a line to `path` (post-image
/// authoritative). One helper keeps every fixture diff byte-identical in shape.
fn added(path: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n\
         index 1111111..2222222 100644\n\
         --- a/{path}\n\
         +++ b/{path}\n\
         @@ -1 +1,2 @@\n\
         \x20fn existing() {{}}\n\
         +fn added() {{}}\n"
    )
}

// ---------------------------------------------------------------------------
// E2E 5a — SATISFIED: a diff landing inside a task's declared footprint is
// mechanically attributed to it (the trace behind a `satisfied` verdict).
// ---------------------------------------------------------------------------

#[test]
fn reconcile_satisfied_when_diff_lands_in_task_footprint() {
    let ws = scratch_dir("satisfied");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — satisfied",
            "version": "intent/v2",
            "tasks": [
                {"id": "t1", "intent": "edit alpha",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": []}
            ]
        }),
    );
    let diff = write(&ws, "landed.diff", &added("src/alpha.rs"));

    let r = run_reconcile(&plan, &diff);

    // The task the diff falls inside is `satisfied`; nothing else fired.
    assert_eq!(
        verdict_of(&r, "t1"),
        Some("satisfied"),
        "a diff inside the footprint must be satisfied: {r}"
    );
    assert_eq!(r["detail"]["satisfied"], 1, "{r}");
    assert_eq!(r["detail"]["violated"], 0, "{r}");
    assert_eq!(r["detail"]["no_trace"], 0, "{r}");
    assert!(
        unattributed(&r).is_empty(),
        "every path was attributed: {r}"
    );
    // A fully-attributed, violation-free reconcile lowers to a green state.
    assert_eq!(r["state"], "green", "clean attribution is green: {r}");
    // The mechanical trace (which file matched) is surfaced, not invented.
    let matched: Vec<String> = r["detail"]["tasks"][0]["matched_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(matched, vec!["src/alpha.rs".to_string()], "{r}");
}

// ---------------------------------------------------------------------------
// E2E 5b — VIOLATED: a diff that trips a task's machine-checkable invariant
// (`no-touch:<path>`) is a violation, with safe evidence, and blocks review.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_violated_when_diff_breaches_a_no_touch_invariant() {
    let ws = scratch_dir("violated");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — violated",
            "version": "intent/v2",
            "tasks": [
                {"id": "t1", "intent": "edit alpha but never the frozen file",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": [],
                 "invariants": ["no-touch:src/frozen.rs"]}
            ]
        }),
    );
    // The landed diff touches BOTH the allowed file and the forbidden one.
    let diff = write(
        &ws,
        "landed.diff",
        &format!("{}{}", added("src/alpha.rs"), added("src/frozen.rs")),
    );

    let r = run_reconcile(&plan, &diff);

    // Invariant breach dominates the footprint match → the task is `violated`.
    assert_eq!(
        verdict_of(&r, "t1"),
        Some("violated"),
        "a no-touch breach must be violated: {r}"
    );
    assert_eq!(r["detail"]["violated"], 1, "{r}");
    // A violation blocks for human review — never silently green.
    assert_eq!(r["state"], "blocked", "violation must block: {r}");
    assert_eq!(r["reason_code"], "needs-review", "{r}");
    // The forbidden path shows up as machine-safe evidence, and the frozen file
    // was consumed by the invariant — it is NOT dangling as unattributed.
    let evidence = r["detail"]["tasks"][0]["evidence"].as_str().unwrap_or("");
    assert!(
        evidence.contains("src/frozen.rs") && evidence.contains("no-touch"),
        "evidence must name the breached path + invariant: {r}"
    );
    assert!(
        !unattributed(&r).contains(&"src/frozen.rs".to_string()),
        "the invariant consumed the frozen path; it is not unattributed: {r}"
    );
}

// ---------------------------------------------------------------------------
// E2E 5c — UNATTRIBUTED (the §7 red line): a landed path that fits NO task's
// footprint is reported verbatim and NEVER hard-attributed to any task.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_unattributed_path_is_never_hard_attributed() {
    let ws = scratch_dir("unattributed");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — unattributed",
            "version": "intent/v2",
            "tasks": [
                {"id": "t1", "intent": "edit alpha",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": []},
                {"id": "t2", "intent": "edit beta",
                 "touched_files_guess": ["src/beta.rs"], "depends_on_guess": []}
            ]
        }),
    );
    // Diff touches alpha (t1's) AND an orphan file no task claims.
    let diff = write(
        &ws,
        "landed.diff",
        &format!("{}{}", added("src/alpha.rs"), added("src/orphan.rs")),
    );

    let r = run_reconcile(&plan, &diff);

    // t1 is satisfied; t2 has no trace; the orphan path is unattributed —
    // and CRUCIALLY it is NOT folded into either task's matched_files.
    assert_eq!(verdict_of(&r, "t1"), Some("satisfied"), "{r}");
    assert_eq!(verdict_of(&r, "t2"), Some("no-trace"), "{r}");
    assert_eq!(
        unattributed(&r),
        vec!["src/orphan.rs".to_string()],
        "the orphan path must be reported verbatim as unattributed: {r}"
    );
    // The red line: the unattributed path never appears under ANY task.
    for t in r["detail"]["tasks"].as_array().unwrap() {
        let matched = t["matched_files"].as_array().cloned().unwrap_or_default();
        assert!(
            !matched.iter().any(|v| v == "src/orphan.rs"),
            "unattributed path must never be hard-attributed to task {}: {r}",
            t["id"]
        );
    }
    // An unattributed path (with no violation) still blocks for manual placement.
    assert_eq!(r["state"], "blocked", "unattributed must block: {r}");
    assert_eq!(r["reason_code"], "needs-review", "{r}");
}

// ---------------------------------------------------------------------------
// E2E 5d — all three states in ONE diff: satisfied + violated + no-trace, plus
// an unattributed path. Pins that the worst state (violation) wins the envelope
// while every per-task verdict is still reported honestly.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_reports_all_three_states_in_one_pass() {
    let ws = scratch_dir("three_states");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — three states",
            "version": "intent/v2",
            "tasks": [
                {"id": "sat", "intent": "edit alpha",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": []},
                {"id": "viol", "intent": "edit beta, never frozen",
                 "touched_files_guess": ["src/beta.rs"], "depends_on_guess": [],
                 "invariants": ["no-touch:src/frozen.rs"]},
                {"id": "none", "intent": "edit gamma (never landed)",
                 "touched_files_guess": ["src/gamma.rs"], "depends_on_guess": []}
            ]
        }),
    );
    // alpha → sat; beta + frozen → viol (breach); gamma untouched → no-trace;
    // orphan → unattributed.
    let diff = write(
        &ws,
        "landed.diff",
        &format!(
            "{}{}{}{}",
            added("src/alpha.rs"),
            added("src/beta.rs"),
            added("src/frozen.rs"),
            added("src/orphan.rs")
        ),
    );

    let r = run_reconcile(&plan, &diff);

    assert_eq!(verdict_of(&r, "sat"), Some("satisfied"), "{r}");
    assert_eq!(verdict_of(&r, "viol"), Some("violated"), "{r}");
    assert_eq!(verdict_of(&r, "none"), Some("no-trace"), "{r}");
    assert_eq!(r["detail"]["satisfied"], 1, "{r}");
    assert_eq!(r["detail"]["violated"], 1, "{r}");
    assert_eq!(r["detail"]["no_trace"], 1, "{r}");
    assert_eq!(unattributed(&r), vec!["src/orphan.rs".to_string()], "{r}");
    // Worst-first lowering: a violation present anywhere blocks review and its
    // reason wins over the (also-present) unattributed path.
    assert_eq!(r["state"], "blocked", "{r}");
    assert_eq!(r["reason_code"], "needs-review", "{r}");
    let next = r["next_action"].as_str().unwrap_or("");
    assert!(
        next.contains("violated") || next.contains("conflict"),
        "the violation reason must win the envelope: {r}"
    );
}

// ---------------------------------------------------------------------------
// E2E 5e — clean landing: every path attributed, no violation, no orphan →
// full green. The honest happy path is not accidentally blocked.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_clean_landing_is_green() {
    let ws = scratch_dir("clean");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — clean",
            "version": "intent/v2",
            "tasks": [
                {"id": "t1", "intent": "edit alpha",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": []},
                {"id": "t2", "intent": "edit beta",
                 "touched_files_guess": ["src/beta.rs"], "depends_on_guess": []}
            ]
        }),
    );
    let diff = write(
        &ws,
        "landed.diff",
        &format!("{}{}", added("src/alpha.rs"), added("src/beta.rs")),
    );

    let r = run_reconcile(&plan, &diff);

    assert_eq!(verdict_of(&r, "t1"), Some("satisfied"), "{r}");
    assert_eq!(verdict_of(&r, "t2"), Some("satisfied"), "{r}");
    assert_eq!(r["detail"]["satisfied"], 2, "{r}");
    assert_eq!(r["detail"]["violated"], 0, "{r}");
    assert_eq!(r["detail"]["no_trace"], 0, "{r}");
    assert!(unattributed(&r).is_empty(), "{r}");
    assert_eq!(r["state"], "green", "a clean landing is fully green: {r}");
}

// ---------------------------------------------------------------------------
// E2E 5f — a claimed task the diff never touches is `no-trace` (green_low, not
// full green): honest "claimed but uncorroborated", never laundered into pass.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_untouched_claim_is_no_trace_green_low() {
    let ws = scratch_dir("no_trace");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "reconcile — no trace",
            "version": "intent/v2",
            "tasks": [
                {"id": "t1", "intent": "edit alpha",
                 "touched_files_guess": ["src/alpha.rs"], "depends_on_guess": []},
                {"id": "t2", "intent": "edit beta (never landed)",
                 "touched_files_guess": ["src/beta.rs"], "depends_on_guess": []}
            ]
        }),
    );
    // Only alpha lands; beta's claim has no diff trace.
    let diff = write(&ws, "landed.diff", &added("src/alpha.rs"));

    let r = run_reconcile(&plan, &diff);

    assert_eq!(verdict_of(&r, "t1"), Some("satisfied"), "{r}");
    assert_eq!(verdict_of(&r, "t2"), Some("no-trace"), "{r}");
    assert_eq!(r["detail"]["no_trace"], 1, "{r}");
    assert!(unattributed(&r).is_empty(), "{r}");
    // no-trace (but no violation, no orphan) caps at green_low — an honest
    // partial. `green_low` is not a wire state; it is `state:green` + the
    // explicit `ok-low-confidence` reason, never laundered to a bare OK.
    assert_eq!(
        r["state"], "green",
        "a no-trace claim stays green (low-confidence flavour): {r}"
    );
    assert_eq!(
        r["reason_code"], "ok-low-confidence",
        "a no-trace claim is explicitly low-confidence, never a bare OK: {r}"
    );
}

// ---------------------------------------------------------------------------
// E2E 5g — I/O contract: a missing plan yields a machine Error envelope (exit
// 0), never a panic — the CLI always hands back a routable ToolOutcome.
// ---------------------------------------------------------------------------

#[test]
fn reconcile_missing_plan_is_a_machine_error_not_a_crash() {
    let ws = scratch_dir("missing_plan");
    let diff = write(&ws, "landed.diff", &added("src/alpha.rs"));
    let missing = ws.join("does_not_exist.json");

    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--reconcile",
            "--plan",
            missing.to_str().unwrap(),
            "--diff",
            diff.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle goal --reconcile");
    assert!(
        out.status.success(),
        "exit must still be 0 (machine envelope is the contract): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("machine JSON on stdout");
    assert_eq!(r["tool"], "reconcile", "{r}");
    assert_eq!(r["state"], "error", "a missing plan is an Error state: {r}");
    assert_eq!(r["reason_code"], "io", "{r}");
}
