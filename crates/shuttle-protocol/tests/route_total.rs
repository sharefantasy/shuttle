//! B5 — totality / property coverage for the routing table.
//!
//! `route()` is the load-bearing pure function of efficiency bet #3: one
//! function reads `(state, reason_code)` and returns the cheapest correct move.
//! These tests pin the properties the rest of the system leans on:
//!
//!   1. **Total & panic-free** over every `state × reason` — including reasons
//!      that don't exist yet (forward-compat / fuzz-ish inputs).
//!   2. **Deterministic** — same input, same decision.
//!   3. **Tier invariants** — a mechanical action never demands the expensive
//!      tier; `Escalate` is always expensive; `Abort`/`Accept` never are.
//!   4. **Fail-safe** — every unknown Blocked reason escalates (never silently
//!      accepts); every Error aborts.
//!   5. **JSON roundtrip** — the wire fields of every decision survive a
//!      serialize→deserialize cycle.

use shuttle_protocol::routing::{route, ModelTier, RouteAction};
use shuttle_protocol::tool_outcome::{reason, ToolOutcome, TypedDetail};

/// Every reason constant the protocol currently names, plus synthetic strings
/// that stand in for "future / garbage" reason codes.
fn all_reason_codes() -> Vec<String> {
    let named = [
        reason::OK,
        reason::OK_LOW_CONFIDENCE,
        reason::NEEDS_FORCE,
        reason::AMBIGUOUS,
        reason::CONTEXT_TRUNCATED,
        reason::MISSING_DEFINITION,
        reason::NEEDS_REVIEW,
        reason::STALE_INDEX,
        reason::BUILD_BLOCKED,
        reason::TEST_BLOCKED,
        reason::NEEDS_RUN,
        reason::INVALID_INPUT,
        reason::IO,
        reason::PARSE,
        reason::INTERNAL,
    ];
    let mut v: Vec<String> = named.iter().map(|s| s.to_string()).collect();
    // synthetic / adversarial reason codes
    v.extend([
        "".to_string(),
        "some-future-reason".to_string(),
        "OK".to_string(),           // wrong case — must NOT match reason::OK
        "needs-force ".to_string(), // trailing space — must NOT match
        "🙂".to_string(),
        "a".repeat(512),
    ]);
    v
}

/// Build one outcome per (state, reason). `error` state ignores reason for
/// routing but we still exercise the constructor with each code.
fn outcome_for(state_tag: u8, rc: &str) -> ToolOutcome {
    match state_tag {
        0 => {
            // Green: reason lives in reason_code; use green() then override the
            // code to exercise the OK vs OK_LOW_CONFIDENCE branch and unknowns.
            let mut o = ToolOutcome::green("nav", TypedDetail::None);
            o.reason_code = rc.to_string();
            o
        }
        1 => ToolOutcome::blocked("t", rc, "next", TypedDetail::None),
        _ => {
            let mut o = ToolOutcome::error("t", reason::PARSE, "boom");
            o.reason_code = rc.to_string();
            o
        }
    }
}

#[test]
fn route_is_total_deterministic_and_invariant() {
    for state_tag in 0u8..=2 {
        for rc in all_reason_codes() {
            let o = outcome_for(state_tag, &rc);
            let d1 = route(&o);
            let d2 = route(&o);
            // determinism
            assert_eq!(
                d1, d2,
                "route not deterministic for state={state_tag} rc={rc:?}"
            );

            // tier invariants
            match d1.action {
                RouteAction::Escalate => assert_eq!(
                    d1.tier,
                    ModelTier::Expensive,
                    "escalate must be expensive (state={state_tag} rc={rc:?})"
                ),
                RouteAction::Abort | RouteAction::Accept => assert_eq!(
                    d1.tier,
                    ModelTier::None,
                    "abort/accept need no tier (state={state_tag} rc={rc:?})"
                ),
                RouteAction::AcceptWithVerify
                | RouteAction::RetryWithForce
                | RouteAction::RetryWiden => assert_eq!(
                    d1.tier,
                    ModelTier::Cheap,
                    "mechanical cheap actions stay cheap (state={state_tag} rc={rc:?})"
                ),
            }

            // a mechanical action must never demand the expensive tier
            if d1.action.is_mechanical() {
                assert_ne!(
                    d1.tier,
                    ModelTier::Expensive,
                    "mechanical action wrongly expensive (state={state_tag} rc={rc:?})"
                );
            }

            // rationale is always populated (safe to log)
            assert!(!d1.rationale.is_empty());
        }
    }
}

#[test]
fn every_blocked_reason_is_accounted_for_and_unknowns_fail_safe() {
    let known_blocked = [
        (reason::NEEDS_FORCE, RouteAction::RetryWithForce),
        (reason::MISSING_DEFINITION, RouteAction::RetryWiden),
        (reason::CONTEXT_TRUNCATED, RouteAction::RetryWiden),
        (reason::STALE_INDEX, RouteAction::RetryWiden),
        (reason::AMBIGUOUS, RouteAction::Escalate),
        (reason::NEEDS_REVIEW, RouteAction::Escalate),
        (reason::BUILD_BLOCKED, RouteAction::Escalate),
        (reason::TEST_BLOCKED, RouteAction::Escalate),
        (reason::NEEDS_RUN, RouteAction::Escalate),
    ];
    for (rc, want) in known_blocked {
        let d = route(&ToolOutcome::blocked("t", rc, "n", TypedDetail::None));
        assert_eq!(d.action, want, "blocked {rc} routed wrong");
    }
    // any reason NOT in the known set must escalate (fail safe), never accept.
    for rc in ["", "mystery", "needs-force-v2", "AMBIGUOUS"] {
        let d = route(&ToolOutcome::blocked("t", rc, "n", TypedDetail::None));
        assert_eq!(
            d.action,
            RouteAction::Escalate,
            "unknown blocked {rc:?} must escalate"
        );
        assert_eq!(d.tier, ModelTier::Expensive);
    }
}

#[test]
fn green_ok_accepts_only_exact_low_confidence_verifies() {
    // exact OK-low-confidence → verify; any other green code (incl. OK and
    // unknowns) → plain Accept.
    let verify = route(&ToolOutcome::green_low("r", TypedDetail::None));
    assert_eq!(verify.action, RouteAction::AcceptWithVerify);

    for rc in [reason::OK, "", "ok-low-confidence-x", "OK-LOW-CONFIDENCE"] {
        let mut o = ToolOutcome::green("nav", TypedDetail::None);
        o.reason_code = rc.to_string();
        let d = route(&o);
        assert_eq!(
            d.action,
            RouteAction::Accept,
            "green {rc:?} should plain-accept"
        );
        assert_eq!(d.tier, ModelTier::None);
    }
}

#[test]
fn error_always_aborts_regardless_of_reason() {
    for rc in all_reason_codes() {
        let mut o = ToolOutcome::error("t", reason::IO, "x");
        o.reason_code = rc.clone();
        let d = route(&o);
        assert_eq!(d.action, RouteAction::Abort, "error {rc:?} must abort");
        assert_eq!(d.tier, ModelTier::None);
    }
}

#[test]
fn decision_wire_fields_roundtrip_through_json_for_all_inputs() {
    for state_tag in 0u8..=2 {
        for rc in all_reason_codes() {
            let d = route(&outcome_for(state_tag, &rc));
            let s = serde_json::to_string(&d).expect("serialize");
            let back: shuttle_protocol::routing::RoutingDecision =
                serde_json::from_str(&s).expect("deserialize");
            assert_eq!(back.action, d.action);
            assert_eq!(back.tier, d.tier);
        }
    }
}
