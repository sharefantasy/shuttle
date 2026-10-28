//! Routing criteria table (efficiency bet #3, P2.1).
//!
//! The whole point of the uniform [`ToolOutcome`] envelope is that *one* pure
//! function can read `state` + `reason_code` and decide the cheapest correct
//! next move — **without** parsing prose and **without** each tool special-cased
//! by the router. This module is that function: [`route`].
//!
//! # The table
//!
//! | state   | reason_code           | action                | model tier |
//! |---------|-----------------------|-----------------------|------------|
//! | Green   | ok                    | Accept                | keep cheap |
//! | Green   | ok-low-confidence     | AcceptWithVerify      | keep cheap |
//! | Blocked | needs-force           | RetryWithForce        | keep cheap |
//! | Blocked | missing-definition    | RetryWiden            | keep cheap |
//! | Blocked | context-truncated     | RetryWiden            | keep cheap |
//! | Blocked | stale-index           | RetryWiden            | keep cheap |
//! | Blocked | ambiguous             | Escalate              | expensive  |
//! | Blocked | needs-review          | Escalate              | expensive  |
//! | Blocked | build-blocked         | Escalate              | expensive  |
//! | Blocked | test-blocked          | Escalate              | expensive  |
//! | Blocked | needs-run             | Escalate              | expensive  |
//! | Blocked | (other)               | Escalate              | expensive  |
//! | Error   | invalid-input         | Abort                 | none       |
//! | Error   | io / parse / internal | Abort                 | none       |
//!
//! # Why these
//! * **Green/ok** — the cheap model got it right; spend nothing more.
//! * **Green/ok-low-confidence** — a real result, but flagged sites exist. Keep
//!   the cheap tier and (optionally) run a *mechanical* verify pass over the
//!   sites in `whats_missing`; never burn the expensive model on a hit we have.
//! * **Blocked/needs-force** — a legality gate held. The single override is in
//!   `force_hint`; a router can re-run mechanically with that flag. No model.
//! * **Blocked/missing-definition & context-truncated & stale-index** —
//!   one-step-fixable by *widening / refreshing* (bigger scan scope, re-fetch
//!   dropped symbols, raise budget, or re-index at head). Mechanical, cheap, no
//!   model round-trip: `stale-index` just needs the graph rebuilt at the
//!   requested commit, then the same tool re-run.
//! * **Blocked/build-blocked, test-blocked, needs-run** — `shuttle verify --run`
//!   execution truth (G5). A red build/test is a genuine bug to fix, and a
//!   `needs-run` is a real authorization/environment decision (shuttle will not
//!   spawn an unauthorized command, §7/#11) — none is a mechanical re-run, so
//!   all three escalate to the expensive tier.
//! * **Blocked/ambiguous & needs-review** — §7 never-guess: a genuine decision between
//!   candidates. This is exactly where the expensive model earns its keep, so
//!   escalate. Any *unknown* Blocked reason also escalates (fail safe, not open).
//! * **Error/\*** — an unframeable hard failure. Escalation cannot help (bad
//!   input, unreadable file, a bug); surface it and stop.

use crate::tool_outcome::{reason, OutcomeState, ToolOutcome};
use serde::{Deserialize, Serialize};

/// Which model tier the router should use for the *next* step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// No model needed — the move is mechanical (a flag re-run) or already done.
    None,
    /// Stay on the cheap/fast model.
    Cheap,
    /// Escalate to the expensive/strong model.
    Expensive,
}

/// The concrete next move a router should take, derived purely from the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAction {
    /// Done. Take the result as-is.
    Accept,
    /// Take the result, but a cheap/mechanical verify of the flagged sites
    /// (`whats_missing`) is advisable before trusting it fully.
    AcceptWithVerify,
    /// Mechanically re-run the same tool with the `force_hint` flag applied.
    RetryWithForce,
    /// Mechanically re-run with a wider scope / re-fetch dropped items / raise
    /// budget — the fix is "get more", not "think harder".
    RetryWiden,
    /// Hand off to the expensive model: a genuine decision is required (§7).
    Escalate,
    /// Give up: an unframeable hard failure the router cannot repair.
    Abort,
}

impl RouteAction {
    /// Machine string for receipts / audit rows.
    pub fn as_str(self) -> &'static str {
        match self {
            RouteAction::Accept => "accept",
            RouteAction::AcceptWithVerify => "accept_with_verify",
            RouteAction::RetryWithForce => "retry_with_force",
            RouteAction::RetryWiden => "retry_widen",
            RouteAction::Escalate => "escalate",
            RouteAction::Abort => "abort",
        }
    }
    /// Whether this action can be executed by the router *without* any model
    /// round-trip (a pure re-run or a take-as-is).
    pub fn is_mechanical(self) -> bool {
        matches!(
            self,
            RouteAction::Accept
                | RouteAction::AcceptWithVerify
                | RouteAction::RetryWithForce
                | RouteAction::RetryWiden
        )
    }
}

/// A routing decision: what to do next, on which tier, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub action: RouteAction,
    pub tier: ModelTier,
    /// A short, stable, human-readable rationale (safe to log; not parsed).
    /// Derived from the decision, not wire data — skipped on deserialize.
    #[serde(skip_deserializing, default)]
    pub rationale: &'static str,
}

/// Route a [`ToolOutcome`] to its cheapest correct next move.
///
/// Pure and total: every `(state, reason_code)` maps to exactly one decision,
/// with unknown Blocked reasons failing *safe* (escalate) and unknown Error
/// reasons aborting.
pub fn route(o: &ToolOutcome) -> RoutingDecision {
    match o.state {
        OutcomeState::Green => {
            if o.reason_code == reason::OK_LOW_CONFIDENCE {
                RoutingDecision {
                    action: RouteAction::AcceptWithVerify,
                    tier: ModelTier::Cheap,
                    rationale: "green but low-confidence: keep cheap, verify flagged sites",
                }
            } else {
                RoutingDecision {
                    action: RouteAction::Accept,
                    tier: ModelTier::None,
                    rationale: "green: done, no further model needed",
                }
            }
        }
        OutcomeState::Blocked => match o.reason_code.as_str() {
            reason::NEEDS_FORCE => RoutingDecision {
                action: RouteAction::RetryWithForce,
                tier: ModelTier::Cheap,
                rationale: "gate held: mechanical re-run with force_hint",
            },
            reason::MISSING_DEFINITION | reason::CONTEXT_TRUNCATED | reason::STALE_INDEX => {
                RoutingDecision {
                    action: RouteAction::RetryWiden,
                    tier: ModelTier::Cheap,
                    rationale: "one-step-fixable by widening scope / re-fetching / re-indexing",
                }
            }
            reason::AMBIGUOUS | reason::NEEDS_REVIEW => RoutingDecision {
                action: RouteAction::Escalate,
                tier: ModelTier::Expensive,
                rationale: "genuine ambiguity / review edge (§7): escalate for a decision",
            },
            reason::BUILD_BLOCKED | reason::TEST_BLOCKED | reason::NEEDS_RUN => RoutingDecision {
                action: RouteAction::Escalate,
                tier: ModelTier::Expensive,
                rationale:
                    "execution truth (G5): a red build/test or a run authorization is real work",
            },
            _ => RoutingDecision {
                action: RouteAction::Escalate,
                tier: ModelTier::Expensive,
                rationale: "unknown blocked reason: fail safe to escalation",
            },
        },
        OutcomeState::Error => RoutingDecision {
            action: RouteAction::Abort,
            tier: ModelTier::None,
            rationale: "unframeable hard failure: surface and stop",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_outcome::{ToolOutcome, TypedDetail};

    fn green() -> ToolOutcome {
        ToolOutcome::green("nav", TypedDetail::None)
    }
    fn green_low() -> ToolOutcome {
        ToolOutcome::green_low("refactor", TypedDetail::None)
    }
    fn blocked(rc: &str) -> ToolOutcome {
        ToolOutcome::blocked("t", rc, "next", TypedDetail::None)
    }
    fn error() -> ToolOutcome {
        ToolOutcome::error("t", reason::PARSE, "boom")
    }

    #[test]
    fn green_ok_accepts_without_model() {
        let d = route(&green());
        assert_eq!(d.action, RouteAction::Accept);
        assert_eq!(d.tier, ModelTier::None);
        assert!(d.action.is_mechanical());
    }

    #[test]
    fn green_low_keeps_cheap_and_verifies() {
        let d = route(&green_low());
        assert_eq!(d.action, RouteAction::AcceptWithVerify);
        assert_eq!(d.tier, ModelTier::Cheap);
    }

    #[test]
    fn needs_force_is_mechanical_cheap_retry() {
        let d = route(&blocked(reason::NEEDS_FORCE));
        assert_eq!(d.action, RouteAction::RetryWithForce);
        assert_eq!(d.tier, ModelTier::Cheap);
        assert!(d.action.is_mechanical());
    }

    #[test]
    fn missing_def_and_truncated_widen_cheaply() {
        for rc in [reason::MISSING_DEFINITION, reason::CONTEXT_TRUNCATED] {
            let d = route(&blocked(rc));
            assert_eq!(d.action, RouteAction::RetryWiden);
            assert_eq!(d.tier, ModelTier::Cheap);
        }
    }

    #[test]
    fn ambiguous_escalates_to_expensive() {
        let d = route(&blocked(reason::AMBIGUOUS));
        assert_eq!(d.action, RouteAction::Escalate);
        assert_eq!(d.tier, ModelTier::Expensive);
        assert!(!d.action.is_mechanical());
    }

    #[test]
    fn stale_index_widens_cheaply() {
        let d = route(&blocked(reason::STALE_INDEX));
        assert_eq!(d.action, RouteAction::RetryWiden);
        assert_eq!(d.tier, ModelTier::Cheap);
        assert!(d.action.is_mechanical());
    }

    #[test]
    fn needs_review_escalates() {
        let d = route(&blocked(reason::NEEDS_REVIEW));
        assert_eq!(d.action, RouteAction::Escalate);
        assert_eq!(d.tier, ModelTier::Expensive);
    }

    #[test]
    fn verify_execution_blocks_escalate_to_expensive() {
        for rc in [
            reason::BUILD_BLOCKED,
            reason::TEST_BLOCKED,
            reason::NEEDS_RUN,
        ] {
            let d = route(&blocked(rc));
            assert_eq!(d.action, RouteAction::Escalate, "{rc} should escalate");
            assert_eq!(d.tier, ModelTier::Expensive);
            assert!(!d.action.is_mechanical());
        }
    }

    #[test]
    fn unknown_blocked_reason_fails_safe_to_escalate() {
        let d = route(&blocked("some-future-reason"));
        assert_eq!(d.action, RouteAction::Escalate);
        assert_eq!(d.tier, ModelTier::Expensive);
    }

    #[test]
    fn error_aborts_without_model() {
        let d = route(&error());
        assert_eq!(d.action, RouteAction::Abort);
        assert_eq!(d.tier, ModelTier::None);
    }

    #[test]
    fn decision_roundtrips_through_json() {
        let d = route(&blocked(reason::AMBIGUOUS));
        let s = serde_json::to_string(&d).unwrap();
        let back: RoutingDecision = serde_json::from_str(&s).unwrap();
        // rationale is derived (skip_deserializing), so compare wire fields.
        assert_eq!(back.action, d.action);
        assert_eq!(back.tier, d.tier);
    }
}
