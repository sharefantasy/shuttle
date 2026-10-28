//! Post-plan re-parse safety net (docs §7 「绝不瞎猜」 taken one step further).
//!
//! Every refactor guarantees it never *guesses*, but until now none guaranteed
//! that the edited file still *parses*. A splice into a line with nested quotes,
//! a mishandled trailing comma, or an off-by-one column can silently produce a
//! syntactically broken file that only shows up when the caller compiles it.
//!
//! This module closes that gap generically for all refactors: it applies the
//! plan's same-file `SearchReplace` ops in memory (reusing the very same
//! `shuttle-editcore` matcher the writer uses), re-parses the result with
//! tree-sitter, and counts `ERROR` / `MISSING` nodes. If the edit introduced
//! *new* syntax errors that were not in the original, the plan is downgraded to
//! `Low` confidence with an explanatory warning — the CLI's §9.5 gate then
//! refuses to write it without `--force`. A refactor that was already correct
//! pays only one extra parse (sub-millisecond), so this is always on.
//!
//! Scope: only same-file `SearchReplace` ops against `path` are replayed. Cross-
//! file `Insert` / `CreateFile` ops (move-function's destination writes) are not
//! reparsed here because their target text is not in hand; the origin-file delete
//! that move-function emits *is* a same-file `SearchReplace` and is checked.

use shuttle_protocol::{ApplyStatus, EditOp};
use tree_sitter::Parser;

use shuttle_nav::Confidence;

use crate::locals::Lang;
use crate::RefactorPlan;

/// Replay a plan's same-file edits, re-parse, and downgrade to `Low` if the
/// rewrite introduced new syntax errors. Returns the (possibly degraded) plan.
pub(crate) fn verify_reparse(plan: RefactorPlan, path: &str, original: &str) -> RefactorPlan {
    let Some(lang) = Lang::from_path(path) else {
        return plan; // unknown grammar: nothing to check
    };

    // Apply the same-file SearchReplace ops in plan order.
    let mut src = original.to_string();
    let mut applied_any = false;
    for op in &plan.plan.ops {
        if let EditOp::SearchReplace {
            path: p,
            search,
            replace,
            line_hint,
        } = op
        {
            if p != path {
                continue;
            }
            let (next, outcome) =
                shuttle_editcore::apply_search_replace(&src, search, replace, *line_hint);
            if matches!(outcome.status, ApplyStatus::Applied) {
                src = next;
                applied_any = true;
            }
            // A non-applying op is the CLI apply-gate's problem, not ours; we
            // only judge the syntactic well-formedness of what *does* apply.
        }
    }
    if !applied_any {
        return plan;
    }

    let before = error_count(lang, original);
    let after = error_count(lang, &src);
    if after > before {
        let mut plan = plan;
        plan.confidence = Confidence::Low;
        plan.warnings.push(format!(
            "post-edit re-parse found {} new syntax-error node(s) in {path} — the rewrite may \
             not be well-formed; downgraded to Low (inspect with --json, apply with --force)",
            after - before
        ));
        return plan;
    }
    plan
}

/// Count `ERROR` + `MISSING` nodes in `src` under `lang`. A parser failure
/// returns 0 (we cannot judge, so we do not penalize).
pub(crate) fn error_count(lang: Lang, src: &str) -> usize {
    let mut parser = Parser::new();
    if parser.set_language(&lang.language()).is_err() {
        return 0;
    }
    let Some(tree) = parser.parse(src, None) else {
        return 0;
    };
    let mut count = 0usize;
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.is_error() || n.is_missing() {
            count += 1;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plan_from_ops, search_replace, RefactorKind};
    use shuttle_protocol::EditOp;

    fn mk_plan(path: &str, ops: Vec<EditOp>) -> RefactorPlan {
        RefactorPlan {
            op: RefactorKind::RenameLocal,
            plan: plan_from_ops(RefactorKind::RenameLocal, path, ops, "test"),
            signature: None,
            params: Vec::new(),
            returns: Vec::new(),
            confidence: Confidence::High,
            warnings: Vec::new(),
            sites: Vec::new(),
        }
    }

    #[test]
    fn clean_edit_stays_high() {
        let src = "def f(a):\n    return a + 1\n";
        let ops = vec![search_replace(
            "m.py",
            "    return a + 1".into(),
            "    return a + 2".into(),
            2,
        )];
        let out = verify_reparse(mk_plan("m.py", ops), "m.py", src);
        assert_eq!(out.confidence, Confidence::High);
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn broken_edit_downgrades_to_low() {
        // Replacing the body with an unbalanced paren makes the file unparseable.
        let src = "def f(a):\n    return a + 1\n";
        let ops = vec![search_replace(
            "m.py",
            "    return a + 1".into(),
            "    return a + (".into(),
            2,
        )];
        let out = verify_reparse(mk_plan("m.py", ops), "m.py", src);
        assert_eq!(out.confidence, Confidence::Low);
        assert!(out.warnings.iter().any(|w| w.contains("re-parse")));
    }

    #[test]
    fn no_matching_op_is_noop() {
        // Op targets a different file: nothing is replayed, plan unchanged.
        let src = "def f(a):\n    return a\n";
        let ops = vec![search_replace("other.py", "x".into(), "y".into(), 1)];
        let out = verify_reparse(mk_plan("m.py", ops), "m.py", src);
        assert_eq!(out.confidence, Confidence::High);
    }

    #[test]
    fn rust_broken_edit_downgrades() {
        let src = "fn f(a: i32) -> i32 {\n    a + 1\n}\n";
        let ops = vec![search_replace(
            "m.rs",
            "    a + 1".into(),
            "    a + 1 }".into(),
            2,
        )];
        let out = verify_reparse(mk_plan("m.rs", ops), "m.rs", src);
        assert_eq!(out.confidence, Confidence::Low);
    }
}
