//! P1.4 — ToolOutcome contract tests.
//!
//! shuttle-cli is the one crate that depends on *every* tool crate (shuttle-nav,
//! shuttle-conflict, shuttle-refactor) plus the base shuttle-protocol, so it is the natural
//! home for the cross-tool golden samples that pin the universal envelope
//! contract (efficiency bet #2):
//!
//!   * green   → `is_error() == false`, no `next_action`, serializes with
//!     `state:"green"` and omits the Blocked-only fields.
//!   * blocked → `is_error() == false` (one-step-fixable), ALWAYS carries a
//!     `next_action`; `needs-force` additionally carries `force_hint`.
//!   * error   → the ONLY state that maps to MCP `isError: true`.
//!
//! Each tool lowering contributes one golden per state so a regression in any
//! single crate's mapping trips here.

use shuttle_protocol::{reason, OutcomeState, ToolOutcome, TypedDetail};

/// The universal invariant the MCP shim (P1.3) relies on: `isError` iff Error.
fn assert_mcp_iserror_matches_state(o: &ToolOutcome) {
    let is_error = o.state.is_error();
    assert_eq!(
        is_error,
        matches!(o.state, OutcomeState::Error),
        "is_error() must be true exactly for Error state"
    );
    // Round-trips through JSON without losing state/reason (the wire contract).
    let s = serde_json::to_string(o).unwrap();
    let back: ToolOutcome = serde_json::from_str(&s).unwrap();
    assert_eq!(back.state, o.state);
    assert_eq!(back.reason_code, o.reason_code);
}

/// Blocked is actionable, never a failure, and must always spell the next step.
fn assert_blocked_is_actionable(o: &ToolOutcome) {
    assert_eq!(o.state, OutcomeState::Blocked);
    assert!(!o.state.is_error(), "Blocked is not a hard failure");
    assert!(
        o.next_action.is_some(),
        "a Blocked outcome must carry exactly one concrete next_action"
    );
    assert_mcp_iserror_matches_state(o);
}

// ---- nav -----------------------------------------------------------------

mod nav_contract {
    use super::*;
    use shuttle_nav::outcome::nav_outcome;
    use shuttle_nav::{Backend, Confidence, NavHit, NavKind};

    fn hit(sym: &str, conf: Confidence) -> NavHit {
        NavHit {
            path: "m.rs".into(),
            line: 7,
            col: None,
            kind: NavKind::Definition,
            symbol: sym.into(),
            snippet: None,
            confidence: conf,
            source: Backend::Ctags,
        }
    }

    #[test]
    fn green_all_high() {
        let o = nav_outcome("defs", "Widget", Ok(vec![hit("Widget", Confidence::High)]));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        assert!(o.next_action.is_none());
        assert_mcp_iserror_matches_state(&o);
    }

    #[test]
    fn green_low_some_shaky() {
        let o = nav_outcome(
            "references",
            "Widget",
            Ok(vec![
                hit("Widget", Confidence::High),
                hit("Widget", Confidence::Low),
            ]),
        );
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
        assert!(!o.whats_missing.is_empty());
        assert_mcp_iserror_matches_state(&o);
    }

    #[test]
    fn blocked_zero_hits() {
        let o = nav_outcome("defs", "Widget", Ok(vec![]));
        assert_eq!(o.reason_code, reason::MISSING_DEFINITION);
        assert_blocked_is_actionable(&o);
    }
}

// ---- conflict ------------------------------------------------------------

mod conflict_contract {
    use super::*;
    use shuttle_conflict::outcome::conflict_outcome;
    use shuttle_conflict::{ConflictLevel, ConflictReceipt, HunkReceipt};

    fn hunk(id: usize, verify_passed: bool) -> HunkReceipt {
        HunkReceipt {
            hunk_id: id,
            file: "a.rs".into(),
            level: ConflictLevel::Structural,
            strategy: "auto".into(),
            model_involved: false,
            verify_passed,
        }
    }

    fn receipt(all_verified: bool, hunks: Vec<HunkReceipt>) -> ConflictReceipt {
        ConflictReceipt {
            run_id: "r1".into(),
            hunks,
            all_verified,
        }
    }

    #[test]
    fn green_all_verified() {
        let o = conflict_outcome("a.rs", &receipt(true, vec![hunk(0, true)]));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        assert_mcp_iserror_matches_state(&o);
    }

    #[test]
    fn blocked_unresolved_hunk() {
        let o = conflict_outcome("a.rs", &receipt(false, vec![hunk(0, true), hunk(1, false)]));
        assert_eq!(o.reason_code, reason::AMBIGUOUS);
        assert!(!o.whats_missing.is_empty());
        assert_blocked_is_actionable(&o);
    }
}

// ---- refactor / recipe ---------------------------------------------------

mod refactor_contract {
    use super::*;
    use shuttle_nav::Confidence;
    use shuttle_protocol::{EditPlan, CURRENT_VERSION};
    use shuttle_refactor::outcome::refactor_outcome;
    use shuttle_refactor::{RefactorKind, RefactorPlan};

    fn plan(kind: RefactorKind, conf: Confidence, warnings: Vec<String>) -> RefactorPlan {
        RefactorPlan {
            op: kind,
            plan: EditPlan {
                version: CURRENT_VERSION,
                intent: None,
                ops: vec![],
            },
            signature: None,
            params: vec![],
            returns: vec![],
            confidence: conf,
            warnings,
            sites: vec![],
        }
    }

    #[test]
    fn green_high_confidence() {
        let o = refactor_outcome(plan(
            RefactorKind::ExtractVariable,
            Confidence::High,
            vec![],
        ));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        // Green carries the appliable plan inline.
        assert!(matches!(o.detail, TypedDetail::Refactor(_)));
        assert_mcp_iserror_matches_state(&o);
    }

    #[test]
    fn green_low_medium_confidence() {
        let o = refactor_outcome(plan(
            RefactorKind::ExtractMethod,
            Confidence::Medium,
            vec![],
        ));
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
        assert_mcp_iserror_matches_state(&o);
    }

    #[test]
    fn blocked_low_confidence_needs_force() {
        let o = refactor_outcome(plan(
            RefactorKind::RenameLocal,
            Confidence::Low,
            vec!["ambiguous target".into()],
        ));
        assert_eq!(o.reason_code, reason::NEEDS_FORCE);
        assert_eq!(o.force_hint.as_deref(), Some("--force"));
        assert_blocked_is_actionable(&o);
    }
}

// ---- envelope-level golden -----------------------------------------------

#[test]
fn error_is_the_only_mcp_iserror() {
    let e = ToolOutcome::error("recipe", reason::PARSE, "cannot index root file");
    assert!(e.state.is_error());
    assert_mcp_iserror_matches_state(&e);

    // Every non-error golden must NOT be an MCP error.
    for o in [
        ToolOutcome::green("nav", TypedDetail::None),
        ToolOutcome::green_low("refactor", TypedDetail::None),
        ToolOutcome::blocked(
            "conflict",
            reason::AMBIGUOUS,
            "pick a side",
            TypedDetail::None,
        ),
    ] {
        assert!(!o.state.is_error());
        assert_mcp_iserror_matches_state(&o);
    }
}
