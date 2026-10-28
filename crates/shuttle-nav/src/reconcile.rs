//! G8-P3b · Reverse-reconcile: attribute a landed diff back to the intent DAG
//! (design docs/shuttle_orchestration_runtime_design.md §4.2 / §4.3).
//!
//! This is the inverse of `GoalTelemetry`. Forward, `shuttle goal` drives an intent
//! DAG to a landed diff; reverse, `shuttle goal --reconcile --diff <patch>` takes an
//! *already-landed* diff plus the intent DAG that claimed to produce it and
//! re-derives, per task, whether the diff **satisfied**, **violated**, or left
//! **no-trace** of that task — and which diff paths fit **no** task at all
//! (`unattributed`). It is for code review: "this PR claims it did A/B/C; the
//! mechanical evidence shows A satisfied, B violated (touched a file it swore
//! not to), C has no trace, and one file belongs to nothing you planned."
//!
//! The §7 never-guess red line governs the whole module (design §4.3):
//!
//!   * a diff path inside a task's declared `touched_files_guess` → satisfied;
//!   * a diff path that breaks a task's machine-checkable invariant → violated,
//!     with evidence (the exact forbidden path);
//!   * a declared task with no diff trace → no-trace;
//!   * a diff path in NO task's footprint and flagged by NO invariant →
//!     unattributed: surfaced verbatim and **never** hard-attributed to the
//!     nearest task.
//!
//! It is deliberately graph-free: the diff is commit-authoritative and the
//! declared footprints come from the intent DAG, so reconcile needs no ctags
//! scan and no symbol graph — it is pure evidence bookkeeping.

use std::collections::BTreeSet;

use shuttle_protocol::{
    reason, ReconcileDetail, ReconcileVerdict, ReconciledTask, ToolOutcome, TOOL_RECONCILE,
};

use crate::split::IntentDag;

/// The machine-checkable invariant prefix: `no-touch:<path>` declares that the
/// task must NOT cause `<path>` to change. When the diff touches a matching
/// path, the task is `violated` with that path as evidence. Any other invariant
/// string is treated as free-form prose (informational, never mechanically
/// adjudicated — §7 forbids guessing a verdict from natural language).
pub const INV_NO_TOUCH: &str = "no-touch:";

/// Normalize a path for comparison: `\\` → `/`, strip a leading `./`.
fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// A changed diff path `hit` belongs to a declared `footprint` entry when the
/// two are equal or one is a `/`-boundary suffix of the other — the same rule
/// shuttle-nav's impact/split use to bridge repo-relative vs differently-rooted
/// paths. Never a bare substring match (that would over-attribute).
fn path_matches(hit: &str, footprint_entry: &str) -> bool {
    let a = norm(hit);
    let b = norm(footprint_entry);
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// True when `hit` falls inside any entry of `footprint`.
fn in_footprint(hit: &str, footprint: &[String]) -> bool {
    footprint.iter().any(|f| path_matches(hit, f))
}

/// Extract the changed-file set from a unified diff (`git diff` / `diff -u`
/// output). Reads the authoritative `+++ b/<path>` (post-image) header, falling
/// back to the `--- a/<path>` (pre-image) header for a pure deletion where the
/// post-image is `/dev/null`. `git`'s `a/` and `b/` prefixes are stripped. This
/// is intentionally minimal and dependency-free: a diff is commit-authoritative,
/// so parsing its headers needs no VCS invocation.
pub fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    // Track the most recent pre-image path so a `+++ /dev/null` deletion can
    // still attribute the removed file.
    let mut pending_pre: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            pending_pre = strip_diff_prefix(rest.trim());
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let post = rest.trim();
            if post == "/dev/null" {
                // Deletion: the changed file is the pre-image.
                if let Some(p) = pending_pre.take() {
                    set.insert(p);
                }
            } else if let Some(p) = strip_diff_prefix(post) {
                set.insert(p);
                pending_pre = None;
            }
        }
    }
    set.into_iter().collect()
}

/// Strip a `git`-style `a/` or `b/` prefix (and skip `/dev/null`). Returns the
/// bare repo-relative path, or `None` for `/dev/null` / an empty header.
fn strip_diff_prefix(p: &str) -> Option<String> {
    if p.is_empty() || p == "/dev/null" {
        return None;
    }
    // Drop a trailing tab-separated timestamp some `diff` variants append.
    let p = p.split('\t').next().unwrap_or(p);
    let stripped = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    if stripped.is_empty() || stripped == "/dev/null" {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Reverse-reconcile a landed `changed_files` set against the `intent` DAG,
/// building the [`ReconcileDetail`] evidence (design §4.3). Pure and
/// deterministic: no I/O, no graph.
pub fn reconcile(intent: &IntentDag, changed_files: &[String]) -> ReconcileDetail {
    let changed: Vec<String> = {
        let mut v: Vec<String> = changed_files.iter().map(|f| norm(f)).collect();
        v.sort();
        v.dedup();
        v
    };

    // A changed file is "attributed" once it lands inside SOME task's footprint
    // or trips SOME task's no-touch invariant. Anything left over is unattributed.
    let mut attributed: BTreeSet<String> = BTreeSet::new();

    let mut tasks: Vec<ReconciledTask> = Vec::with_capacity(intent.tasks.len());
    let (mut satisfied, mut violated, mut no_trace) = (0usize, 0usize, 0usize);

    for t in &intent.tasks {
        // 1) Machine-checkable invariant breach dominates (violated + evidence).
        let mut breach: Option<String> = None;
        if let Some(invs) = &t.invariants {
            for inv in invs {
                if let Some(forbidden) = inv.strip_prefix(INV_NO_TOUCH) {
                    let forbidden = forbidden.trim();
                    if forbidden.is_empty() {
                        continue;
                    }
                    if let Some(hit) = changed.iter().find(|c| path_matches(c, forbidden)) {
                        attributed.insert(hit.clone());
                        breach = Some(format!(
                            "touched '{hit}' forbidden by invariant '{INV_NO_TOUCH}{forbidden}'"
                        ));
                        break;
                    }
                }
            }
        }

        // 2) Files that landed inside this task's declared footprint.
        let matched: Vec<String> = changed
            .iter()
            .filter(|c| in_footprint(c, &t.touched_files_guess))
            .cloned()
            .collect();
        for m in &matched {
            attributed.insert(m.clone());
        }

        let (verdict, evidence, matched_files) = if let Some(ev) = breach {
            violated += 1;
            (ReconcileVerdict::Violated, ev, matched)
        } else if !matched.is_empty() {
            satisfied += 1;
            (ReconcileVerdict::Satisfied, String::new(), matched)
        } else {
            no_trace += 1;
            (ReconcileVerdict::NoTrace, String::new(), Vec::new())
        };

        tasks.push(ReconciledTask {
            id: t.id.clone(),
            verdict,
            matched_files,
            evidence,
        });
    }

    // §7: every changed file the evidence could not place — reported verbatim,
    // NEVER hard-attributed to the nearest-looking task.
    let unattributed: Vec<String> = changed
        .iter()
        .filter(|c| !attributed.contains(*c))
        .cloned()
        .collect();

    ReconcileDetail {
        goal: intent.goal.clone(),
        changed_files: changed,
        tasks,
        unattributed,
        satisfied,
        violated,
        no_trace,
    }
}

/// Reconcile an already-parsed `intent` against a unified `diff` string and
/// lower the result into a [`ToolOutcome`] (the shape the CLI prints).
pub fn reconcile_outcome_from_diff(intent: &IntentDag, diff: &str) -> ToolOutcome {
    let changed = changed_files_from_diff(diff);
    let detail = reconcile(intent, &changed);
    shuttle_protocol::reconcile_outcome(detail)
}

/// I/O wrapper: read the intent DAG from `plan_path` and the unified diff from
/// `diff_path`, reconcile, and return the lowered [`ToolOutcome`]. A missing or
/// malformed file becomes an `Error` outcome (never an `Err`) so the CLI always
/// has a machine envelope to print.
pub fn reconcile_files(plan_path: &str, diff_path: &str) -> Result<ToolOutcome, crate::NavError> {
    let raw_plan = match std::fs::read_to_string(plan_path) {
        Ok(s) => s,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_RECONCILE,
                reason::IO,
                format!("cannot read plan '{plan_path}': {e}"),
            ));
        }
    };
    let intent: IntentDag = match serde_json::from_str(&raw_plan) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_RECONCILE,
                reason::INVALID_INPUT,
                format!("plan '{plan_path}' is not a valid intent DAG: {e}"),
            ));
        }
    };
    let diff = match std::fs::read_to_string(diff_path) {
        Ok(s) => s,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_RECONCILE,
                reason::IO,
                format!("cannot read diff '{diff_path}': {e}"),
            ));
        }
    };
    Ok(reconcile_outcome_from_diff(&intent, &diff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::{IntentTask, INTENT_VERSION};
    use shuttle_protocol::OutcomeState;

    fn task(id: &str, touched: &[&str], deps: &[&str]) -> IntentTask {
        IntentTask {
            id: id.into(),
            intent: String::new(),
            touched_files_guess: touched.iter().map(|s| s.to_string()).collect(),
            depends_on_guess: deps.iter().map(|s| s.to_string()).collect(),
            acceptance: None,
            invariants: None,
        }
    }

    fn dag(tasks: Vec<IntentTask>) -> IntentDag {
        IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks,
        }
    }

    // -- diff parsing ------------------------------------------------------

    #[test]
    fn parses_git_style_added_and_modified_paths() {
        let diff = "\
diff --git a/src/alpha.py b/src/alpha.py
index e69de29..4b825dc 100644
--- a/src/alpha.py
+++ b/src/alpha.py
@@ -0,0 +1 @@
+x = 1
diff --git a/src/beta.py b/src/beta.py
--- a/src/beta.py
+++ b/src/beta.py
@@ -1 +1 @@
-y = 1
+y = 2
";
        let files = changed_files_from_diff(diff);
        assert_eq!(files, vec!["src/alpha.py", "src/beta.py"]);
    }

    #[test]
    fn parses_a_pure_deletion_via_pre_image() {
        let diff = "\
diff --git a/gone.py b/gone.py
deleted file mode 100644
--- a/gone.py
+++ /dev/null
@@ -1 +0,0 @@
-obsolete = True
";
        assert_eq!(changed_files_from_diff(diff), vec!["gone.py"]);
    }

    #[test]
    fn empty_diff_has_no_changed_files() {
        assert!(changed_files_from_diff("").is_empty());
    }

    // -- three-state attribution (§4.3) ------------------------------------

    #[test]
    fn a_file_inside_a_task_footprint_is_satisfied() {
        let d = dag(vec![task("alpha", &["alpha.py"], &[])]);
        let r = reconcile(&d, &["alpha.py".into()]);
        assert_eq!(r.satisfied, 1);
        assert_eq!(r.tasks[0].verdict, ReconcileVerdict::Satisfied);
        assert_eq!(r.tasks[0].matched_files, vec!["alpha.py"]);
        assert!(r.unattributed.is_empty());
    }

    #[test]
    fn a_claimed_task_with_no_diff_trace_is_no_trace() {
        let d = dag(vec![task("alpha", &["alpha.py"], &[])]);
        // The diff touched something else entirely — alpha has no trace.
        let r = reconcile(&d, &["unrelated.py".into()]);
        assert_eq!(r.no_trace, 1);
        assert_eq!(r.tasks[0].verdict, ReconcileVerdict::NoTrace);
        assert!(r.tasks[0].matched_files.is_empty());
        // …and the unrelated file fits no task → unattributed, not guessed onto alpha.
        assert_eq!(r.unattributed, vec!["unrelated.py"]);
    }

    #[test]
    fn a_no_touch_invariant_breach_is_violated_with_evidence() {
        let mut t = task("alpha", &["alpha.py"], &[]);
        t.invariants = Some(vec![format!("{INV_NO_TOUCH}schema.sql")]);
        let d = dag(vec![t]);
        // The diff touched alpha.py (would be satisfied) AND schema.sql (forbidden).
        let r = reconcile(&d, &["alpha.py".into(), "schema.sql".into()]);
        assert_eq!(r.violated, 1);
        assert_eq!(r.tasks[0].verdict, ReconcileVerdict::Violated);
        assert!(r.tasks[0].evidence.contains("schema.sql"));
        assert!(r.tasks[0].evidence.contains("no-touch"));
        // The breach file is attributed (as a violation), so not unattributed.
        assert!(r.unattributed.is_empty());
    }

    #[test]
    fn a_diff_path_in_no_footprint_is_unattributed_never_guessed() {
        // Two tasks with tight footprints; the diff touches a THIRD file that
        // belongs to neither. §7: it is reported, never hard-attributed.
        let d = dag(vec![
            task("alpha", &["alpha.py"], &[]),
            task("beta", &["beta.py"], &["alpha"]),
        ]);
        let r = reconcile(&d, &["alpha.py".into(), "mystery.py".into()]);
        assert_eq!(r.satisfied, 1, "alpha satisfied");
        assert_eq!(r.no_trace, 1, "beta has no trace");
        assert_eq!(r.unattributed, vec!["mystery.py"]);
        // The nearest task did NOT absorb it.
        assert!(r
            .tasks
            .iter()
            .all(|t| !t.matched_files.contains(&"mystery.py".to_string())));
    }

    #[test]
    fn all_three_states_plus_unattributed_in_one_diff() {
        // alpha satisfied, beta violated (breaks no-touch), gamma no-trace,
        // and one path attributable to nobody.
        let mut beta = task("beta", &["beta.py"], &[]);
        beta.invariants = Some(vec![format!("{INV_NO_TOUCH}locked.py")]);
        let d = dag(vec![
            task("alpha", &["alpha.py"], &[]),
            beta,
            task("gamma", &["gamma.py"], &[]),
        ]);
        let r = reconcile(
            &d,
            &[
                "alpha.py".into(),
                "beta.py".into(),
                "locked.py".into(),
                "orphan.py".into(),
            ],
        );
        assert_eq!(r.satisfied, 1);
        assert_eq!(r.violated, 1);
        assert_eq!(r.no_trace, 1);
        assert_eq!(r.unattributed, vec!["orphan.py"]);
    }

    // -- outcome lowering --------------------------------------------------

    #[test]
    fn clean_attribution_lowers_to_green() {
        let d = dag(vec![task("alpha", &["alpha.py"], &[])]);
        let o = shuttle_protocol::reconcile_outcome(reconcile(&d, &["alpha.py".into()]));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
    }

    #[test]
    fn a_no_trace_only_result_is_green_low_not_full_green() {
        let d = dag(vec![task("alpha", &["alpha.py"], &[])]);
        // alpha satisfied, beta declared but untouched → informational, green-low.
        let d = dag({
            let mut v = d.tasks;
            v.push(task("beta", &["beta.py"], &[]));
            v
        });
        let o = shuttle_protocol::reconcile_outcome(reconcile(&d, &["alpha.py".into()]));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
    }

    #[test]
    fn a_violation_blocks_needs_review() {
        let mut t = task("alpha", &["alpha.py"], &[]);
        t.invariants = Some(vec![format!("{INV_NO_TOUCH}alpha.py")]);
        let d = dag(vec![t]);
        let o = shuttle_protocol::reconcile_outcome(reconcile(&d, &["alpha.py".into()]));
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::NEEDS_REVIEW);
        assert!(o.whats_missing.iter().any(|m| m.contains("alpha.py")));
    }

    #[test]
    fn an_unattributed_path_blocks_needs_review() {
        let d = dag(vec![task("alpha", &["alpha.py"], &[])]);
        let o = shuttle_protocol::reconcile_outcome(reconcile(
            &d,
            &["alpha.py".into(), "orphan.py".into()],
        ));
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::NEEDS_REVIEW);
        assert!(o.whats_missing.contains(&"orphan.py".to_string()));
    }

    #[test]
    fn violation_dominates_unattributed_in_the_block_reason() {
        // Both a violation and an unattributed path present → the violated
        // branch wins (worst-first), naming the broken intent.
        let mut t = task("alpha", &["alpha.py"], &[]);
        t.invariants = Some(vec![format!("{INV_NO_TOUCH}alpha.py")]);
        let d = dag(vec![t]);
        let o = shuttle_protocol::reconcile_outcome(reconcile(
            &d,
            &["alpha.py".into(), "orphan.py".into()],
        ));
        assert_eq!(o.state, OutcomeState::Blocked);
        // The next_action cites the intent conflict, not the orphan.
        assert!(o
            .next_action
            .unwrap()
            .contains("conflicts with a claimed intent"));
    }

    #[test]
    fn io_wrapper_reads_plan_and_diff_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "shuttle_reconcile_io_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.json");
        std::fs::write(
            &plan,
            serde_json::to_string_pretty(&serde_json::json!({
                "goal": "demo",
                "version": "intent/v2",
                "tasks": [
                    { "id": "alpha", "touched_files_guess": ["alpha.py"] }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let diff = dir.join("change.patch");
        std::fs::write(
            &diff,
            "--- a/alpha.py\n+++ b/alpha.py\n@@ -0,0 +1 @@\n+x = 1\n",
        )
        .unwrap();
        let o = reconcile_files(&plan.to_string_lossy(), &diff.to_string_lossy()).unwrap();
        assert_eq!(o.state, OutcomeState::Green);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_wrapper_reports_a_missing_plan_as_error() {
        let o = reconcile_files("/nonexistent/plan.json", "/nonexistent/x.patch").unwrap();
        assert!(o.state.is_error());
        assert_eq!(o.reason_code, reason::IO);
    }
}
