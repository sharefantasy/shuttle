//! Binary-level CLI integration tests for `shuttle goal`'s P2 re-plan loop — the
//! G8/P2 drift/resume surface .
//! Spawns the REAL `shuttle` binary and drives the whole outer loop end-to-end with
//! ZERO model calls via the hermetic seams (`--plan` + `--dry-run`) plus the two
//! P2 inputs shuttle does NOT own (#11): the launcher's observed footprint
//! (`--observe <map.json>`) and a caller-persisted checkpoint (`--resume
//! <checkpoint.json>`).
//!
//! The loop's contract (§3.2/§3.3):
//!   * a task whose OBSERVED footprint stays inside its attributed set is not
//!     drift — the goal lands single-pass, no checkpoint;
//!   * a task that drifts onto a file outside its attribution folds that file
//!     back in, RE-PLANS the remaining tasks, and (when the re-plan re-settles
//!     Green) lands with a `checkpoint` recording the re-plan count;
//!   * repeated drift is anti-thrash capped at MAX_REPLAN (=3): the 4th drift
//!     ESCALATES to review — and CRUCIALLY keeps the checkpoint of the landed
//!     prefix so a `--resume` never redoes finished work (§7 never-guess:
//!     shuttle stops at the gate rather than churn forever);
//!   * `--resume` replays only the UNfinished tail (completed tasks skipped,
//!     never redone) and the re-plan tally CONTINUES across resumes (the
//!     anti-thrash budget is cumulative — a caller cannot launder past the cap
//!     by resuming).
//!
//! Exit is always 0 — the machine result (phase/escalated/checkpoint/review) is
//! the contract a caller routes on, matching split/impact. ctags-gated: the
//! re-adjudication re-scans the real symbol graph, so these skip cleanly when
//! universal-ctags is unavailable.

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
    let dir = std::env::temp_dir().join(format!("shuttle_goal_replan_it_{tag}_{nanos}"));
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

/// Seed a self-contained, provably-disjoint Python module per id: each file
/// defines one uniquely-named function and imports nothing, so the symbol graph
/// proves every pair independent (AGREE / no edge). This keeps the adjudication
/// Green so the loop's *drift* behaviour — not an accidental dependency — is
/// what the assertions pin.
fn seed_disjoint(dir: &std::path::Path, ids: &[&str]) {
    for id in ids {
        write(
            dir,
            &format!("{id}.py"),
            &format!("def {id}_fn():\n    return \"{id}\"\n"),
        );
    }
}

/// A per-task observed-footprint map JSON handed in via `--observe`.
fn write_observe(dir: &std::path::Path, map: serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("observe.json");
    std::fs::write(&p, serde_json::to_string_pretty(&map).unwrap()).unwrap();
    p
}

/// A caller-persisted `GoalCheckpoint` JSON handed back via `--resume`.
fn write_checkpoint(
    dir: &std::path::Path,
    completed: &[&str],
    replans: usize,
) -> std::path::PathBuf {
    let p = dir.join("checkpoint.json");
    std::fs::write(
        &p,
        serde_json::to_string_pretty(&serde_json::json!({
            "completed": completed,
            "replans": replans,
        }))
        .unwrap(),
    )
    .unwrap();
    p
}

/// Run `shuttle goal --plan <plan> --root <ws> --dry-run --json` plus any extra args,
/// parse stdout as the GoalResult JSON, assert exit 0 (the machine contract).
fn run_goal(ws: &std::path::Path, plan: &std::path::Path, extra: &[&str]) -> serde_json::Value {
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
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("goal stdout is not the JSON GoalResult: {e}\n---\n{stdout}"))
}

fn ids_of(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn exec_ids(r: &serde_json::Value) -> Vec<String> {
    r["exec"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// E2E 3 — explainable drift → re-plan once → land, with a checkpoint. The
// observed footprint escapes a task's attribution; shuttle folds it back in,
// re-adjudicates the remainder Green, and lands (never redoing the landed
// prefix). The checkpoint proves exactly one re-plan happened.
// ---------------------------------------------------------------------------

#[test]
fn replan_on_explainable_drift_lands_with_a_checkpoint() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping replan drift-lands E2E");
        return;
    }
    let ws = scratch_dir("drift_lands");
    // Two provably-disjoint tasks, plus a real (also-disjoint) `shared.py` the
    // launcher will report `a` as having also touched.
    seed_disjoint(&ws, &["a", "b", "shared"]);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "replan — drift lands",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": [] }
            ]
        }),
    );
    // `a` drifted onto shared.py (outside its a.py attribution); `b` stayed put.
    let observe = write_observe(
        &ws,
        serde_json::json!({
            "a": ["a.py", "shared.py"],
            "b": ["b.py"]
        }),
    );

    let r = run_goal(&ws, &plan, &["--observe", observe.to_str().unwrap()]);

    // Drift re-settled Green → the goal lands, no escalation.
    assert_eq!(
        r["phase"], "landed",
        "explainable drift must still land: {r}"
    );
    assert_eq!(r["escalated"], false, "{r}");
    // Both tasks executed exactly once, in topo/id order; `a` is not redone.
    assert_eq!(exec_ids(&r), vec!["a", "b"], "{r}");
    assert_eq!(ids_of(&r["order"]), vec!["a", "b"], "{r}");
    // A re-plan happened → a checkpoint records it (single-pass would carry none).
    let cp = &r["checkpoint"];
    assert!(!cp.is_null(), "a re-plan must attach a checkpoint: {r}");
    assert_eq!(cp["replans"], 1, "exactly one re-plan: {r}");
    assert_eq!(ids_of(&cp["completed"]), vec!["a", "b"], "{r}");
}

// ---------------------------------------------------------------------------
// E2E 3b — no drift → single pass → NO checkpoint. The honest baseline: an
// observation that stays inside every attribution never triggers a re-plan.
// ---------------------------------------------------------------------------

#[test]
fn no_drift_lands_single_pass_without_a_checkpoint() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping no-drift E2E");
        return;
    }
    let ws = scratch_dir("no_drift");
    seed_disjoint(&ws, &["a", "b"]);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "replan — no drift",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": [] }
            ]
        }),
    );
    // Each task is observed touching exactly what it was attributed — no drift.
    // (One uses a `./`-prefixed path to pin drift-normalization parity.)
    let observe = write_observe(
        &ws,
        serde_json::json!({
            "a": ["a.py"],
            "b": ["./b.py"]
        }),
    );

    let r = run_goal(&ws, &plan, &["--observe", observe.to_str().unwrap()]);

    assert_eq!(r["phase"], "landed", "{r}");
    assert_eq!(r["escalated"], false, "{r}");
    assert!(
        r["checkpoint"].is_null(),
        "no drift ⇒ single pass ⇒ no checkpoint: {r}"
    );
}

// ---------------------------------------------------------------------------
// E2E 4 — repeated drift hits the anti-thrash cap (MAX_REPLAN=3) → escalate,
// keeping the checkpoint of the landed prefix. Four tasks each drift once: the
// loop admits three re-plans, then the fourth drift trips the guard. shuttle stops
// at the review gate rather than churn forever (§3.3 / §7).
// ---------------------------------------------------------------------------

#[test]
fn repeated_drift_hits_the_replan_cap_and_escalates_keeping_checkpoint() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping replan-cap E2E");
        return;
    }
    let ws = scratch_dir("cap");
    seed_disjoint(&ws, &["a", "b", "c", "e"]);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "replan — cap",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": [] },
                { "id": "c", "intent": "edit c", "touched_files_guess": ["c.py"], "depends_on_guess": [] },
                { "id": "e", "intent": "edit e", "touched_files_guess": ["e.py"], "depends_on_guess": [] }
            ]
        }),
    );
    // Every task drifts onto a distinct orphan path outside its attribution.
    let observe = write_observe(
        &ws,
        serde_json::json!({
            "a": ["a.py", "d1.py"],
            "b": ["b.py", "d2.py"],
            "c": ["c.py", "d3.py"],
            "e": ["e.py", "d4.py"]
        }),
    );

    let r = run_goal(&ws, &plan, &["--observe", observe.to_str().unwrap()]);

    // The 4th drift trips the cap → escalate to the review gate, never guess on.
    assert_eq!(r["phase"], "escalated", "cap must escalate: {r}");
    assert_eq!(r["escalated"], true, "{r}");
    let review = ids_of(&r["review"]);
    assert_eq!(review.len(), 1, "one cap review line: {r}");
    assert!(
        review[0].contains("re-plan cap (3)"),
        "the review must name the anti-thrash limit: {r}"
    );
    // The checkpoint is retained — the landed prefix is never redone on resume.
    let cp = &r["checkpoint"];
    assert!(
        !cp.is_null(),
        "a mid-exec escalation keeps a checkpoint: {r}"
    );
    assert_eq!(
        cp["replans"], 3,
        "three re-plans were admitted before the cap: {r}"
    );
    assert_eq!(
        ids_of(&cp["completed"]),
        vec!["a", "b", "c", "e"],
        "every task that ran before the gate is in the checkpoint: {r}"
    );
}

// ---------------------------------------------------------------------------
// E2E 4b — `--resume` replays only the unfinished tail. A caller hands back a
// prior checkpoint (completed=[a]); shuttle skips `a` (never redoing landed work)
// and runs only `b`, then lands with the full order.
// ---------------------------------------------------------------------------

#[test]
fn resume_skips_completed_and_runs_only_the_tail() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping resume-tail E2E");
        return;
    }
    let ws = scratch_dir("resume_tail");
    seed_disjoint(&ws, &["a", "b"]);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "replan — resume tail",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": [] }
            ]
        }),
    );
    // A prior session already landed `a`.
    let cp = write_checkpoint(&ws, &["a"], 0);

    let r = run_goal(&ws, &plan, &["--resume", cp.to_str().unwrap()]);

    assert_eq!(r["phase"], "landed", "{r}");
    assert_eq!(r["escalated"], false, "{r}");
    // Only `b` was executed this session — `a` is never redone.
    assert_eq!(exec_ids(&r), vec!["b"], "resume runs only the tail: {r}");
    // The completed order still carries the resumed prefix + the tail.
    assert_eq!(ids_of(&r["order"]), vec!["a", "b"], "{r}");
    // A resumed finish still records the final landed checkpoint.
    let out = &r["checkpoint"];
    assert!(!out.is_null(), "a resumed run records its checkpoint: {r}");
    assert_eq!(ids_of(&out["completed"]), vec!["a", "b"], "{r}");
}

// ---------------------------------------------------------------------------
// E2E 4c — the anti-thrash budget is cumulative across resumes: a caller
// cannot launder past the cap by resuming. A prior session already spent
// MAX_REPLAN-1 (=2) re-plans; on resume the first fresh drift admits the last
// re-plan (tally hits 3) and the second drift trips the cap → escalate.
// ---------------------------------------------------------------------------

#[test]
fn resume_replan_tally_is_cumulative_across_resumes() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping resume-tally E2E");
        return;
    }
    let ws = scratch_dir("resume_tally");
    seed_disjoint(&ws, &["a", "b"]);
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "replan — resume tally",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": [] }
            ]
        }),
    );
    // Both tasks drift onto distinct orphan paths.
    let observe = write_observe(
        &ws,
        serde_json::json!({
            "a": ["a.py", "x.py"],
            "b": ["b.py", "y.py"]
        }),
    );
    // The prior session already spent 2 of the 3-re-plan budget, nothing landed.
    let cp = write_checkpoint(&ws, &[], 2);

    let r = run_goal(
        &ws,
        &plan,
        &[
            "--observe",
            observe.to_str().unwrap(),
            "--resume",
            cp.to_str().unwrap(),
        ],
    );

    // `a` drifts → the last admissible re-plan (tally hits 3); `b` drifts → cap.
    assert_eq!(
        r["phase"], "escalated",
        "cumulative budget must trip the cap: {r}"
    );
    assert_eq!(r["escalated"], true, "{r}");
    let review = ids_of(&r["review"]);
    assert!(
        review[0].contains("re-plan cap (3)"),
        "the cap fired even though only one drift happened this session: {r}"
    );
    let out = &r["checkpoint"];
    assert_eq!(
        out["replans"], 3,
        "the tally continued from the resumed count: {r}"
    );
    // `a` landed before the cap tripped on `b`.
    assert_eq!(ids_of(&out["completed"]), vec!["a", "b"], "{r}");
}
