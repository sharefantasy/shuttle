//! Closing-gain loop (efficiency bet #1↔#3 bridge, P3).
//!
//! The context solver (bet #1) does not just say "truncated" — its
//! [`ContextDetail`] carries *which* nodes it dropped for budget and *which*
//! type refs stayed unresolved. This module turns that structured residue into
//! the two closing moves that make the loop converge instead of thrash:
//!
//! * **P3.1 dropped → precise re-fetch.** When a later turn is still short on
//!   context, don't re-pack from scratch and don't widen blindly. Take the
//!   highest-value `dropped` items (they already scored high enough to be
//!   *considered*, then lost only to budget) and re-fetch exactly those. See
//!   [`refetch_plan`].
//!
//! * **P3.2 unresolved → escalation signal.** An `unresolved` entry whose reason
//!   is a *definition* gap (`no-def`) is precisely the P2.1 "missing-definition"
//!   widen signal; an `ambiguous` unresolved is the §7 never-guess escalate
//!   signal. [`escalation_signal`] classifies the residue into the routing
//!   [`ModelTier`] the next step should use, so unresolved refs *drive* routing
//!   instead of being swallowed.

use crate::routing::ModelTier;
use crate::tool_outcome::{ContextDetail, MissingItem};
use serde::{Deserialize, Serialize};

/// A precise re-fetch plan: the exact symbols to pull back in on the next turn,
/// in priority order, capped to a caller budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefetchPlan {
    /// Symbols to re-fetch, highest-value first.
    pub symbols: Vec<String>,
    /// How many `dropped` items existed in total (so the caller can tell a
    /// partial plan from a complete one).
    pub total_dropped: usize,
    /// Estimated tokens the planned symbols will cost when re-fetched. `0` when
    /// no estimator was supplied (the count-capped [`refetch_plan`]).
    #[serde(default)]
    pub est_tokens: usize,
}

impl RefetchPlan {
    /// Whether there is anything worth re-fetching.
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

/// Build a precise re-fetch plan (P3.1) from a context slice's dropped residue.
///
/// Only `dropped` items with `reason == "budget"` are eligible — a symbol
/// dropped for *budget* is known-good and merely displaced, so re-fetching it is
/// safe. Items dropped for other reasons are NOT auto re-fetched (they belong to
/// the unresolved/escalate path). `max` caps the plan; `0` means "no cap".
///
/// The input `dropped` vec is already in the solver's descending value order, so
/// we preserve it (highest-value first) and simply take the head.
pub fn refetch_plan(detail: &ContextDetail, max: usize) -> RefetchPlan {
    let eligible: Vec<&MissingItem> = detail
        .dropped
        .iter()
        .filter(|m| m.reason == "budget")
        .collect();
    let total_dropped = eligible.len();
    let take = if max == 0 {
        total_dropped
    } else {
        max.min(total_dropped)
    };
    let symbols = eligible
        .into_iter()
        .take(take)
        .map(|m| m.symbol.clone())
        .collect();
    RefetchPlan {
        symbols,
        total_dropped,
        est_tokens: 0,
    }
}

/// Build a *budget-bounded* precise re-fetch plan (P3.1, budget-aware).
///
/// Like [`refetch_plan`], but instead of a count cap it greedily fills a token
/// `budget`: eligible budget-dropped symbols are taken highest-value first
/// (input order) until adding the next one would exceed `budget`. `est` returns
/// the estimated token cost of re-fetching a given symbol.
///
/// This makes the closing loop actually *converge*: a naive re-fetch of every
/// dropped symbol can re-overflow the very budget that dropped them. Bounding
/// the re-fetch to the freed budget guarantees the next pack fits.
///
/// `budget == 0` means "no budget" and takes nothing (a zero budget cannot
/// afford any re-fetch — fail closed, never silently ignore the cap).
///
/// # Skip vs. stop
/// A symbol that individually exceeds the remaining budget is *skipped*, and we
/// keep scanning cheaper candidates — a single fat symbol must not starve the
/// rest of the plan.
pub fn refetch_plan_within_budget<F>(detail: &ContextDetail, budget: usize, est: F) -> RefetchPlan
where
    F: Fn(&str) -> usize,
{
    let eligible: Vec<&MissingItem> = detail
        .dropped
        .iter()
        .filter(|m| m.reason == "budget")
        .collect();
    let total_dropped = eligible.len();

    let mut symbols = Vec::new();
    let mut spent = 0usize;
    for m in eligible {
        let cost = est(&m.symbol);
        if spent.saturating_add(cost) <= budget {
            spent += cost;
            symbols.push(m.symbol.clone());
        }
        // else: skip this one, keep scanning cheaper candidates.
    }
    RefetchPlan {
        symbols,
        total_dropped,
        est_tokens: spent,
    }
}

/// The next-step routing signal implied by a context slice's *unresolved*
/// residue (P3.2). Ordered by escalation strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSignal {
    /// Nothing unresolved — the slice is closed; proceed as-is.
    None,
    /// Only budget/definition gaps remain — mechanically widen (cheap): raise
    /// budget or fetch the missing definitions. Maps to P2.1 `RetryWiden`.
    Widen,
    /// A genuine ambiguity remains (§7 never-guess) — escalate to the expensive
    /// model to decide. Maps to P2.1 `Escalate`.
    Escalate,
}

impl EscalationSignal {
    /// The model tier this signal calls for on the next step.
    pub fn tier(self) -> ModelTier {
        match self {
            EscalationSignal::None => ModelTier::None,
            EscalationSignal::Widen => ModelTier::Cheap,
            EscalationSignal::Escalate => ModelTier::Expensive,
        }
    }
}

/// Classify a context slice's `unresolved` residue into an [`EscalationSignal`]
/// (P3.2).
///
/// Rules, strongest wins:
/// * any `unresolved` item with reason `ambiguous` → [`EscalationSignal::Escalate`]
///   (§7: a genuine decision — never guess it away).
/// * else any `unresolved` at all (`no-def`, `unreadable`, `budget`, …) →
///   [`EscalationSignal::Widen`] (mechanically get more; no model needed yet).
/// * else → [`EscalationSignal::None`].
///
/// Unknown reasons fall into `Widen`, not `None` — we never treat an unresolved
/// ref as resolved just because we don't recognize its reason (fail safe).
pub fn escalation_signal(detail: &ContextDetail) -> EscalationSignal {
    if detail.unresolved.is_empty() {
        return EscalationSignal::None;
    }
    if detail.unresolved.iter().any(|m| m.reason == "ambiguous") {
        EscalationSignal::Escalate
    } else {
        EscalationSignal::Widen
    }
}

/// Whether the closing loop still has work to do on this slice: either there are
/// dropped items to re-fetch or unresolved refs to act on.
pub fn needs_closing(detail: &ContextDetail) -> bool {
    !refetch_plan(detail, 0).is_empty() || escalation_signal(detail) != EscalationSignal::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_outcome::{ContextNode, OutcomeConfidence};

    fn detail(dropped: Vec<MissingItem>, unresolved: Vec<MissingItem>) -> ContextDetail {
        ContextDetail {
            root: "root".into(),
            root_path: "src/lib.rs".into(),
            included: vec![ContextNode {
                symbol: "root".into(),
                path: "src/lib.rs".into(),
                line: 1,
                confidence: OutcomeConfidence::High,
                est_tokens: 10,
            }],
            dropped,
            unresolved,
            ambiguous: Vec::new(),
            used_tokens: 10,
            budget_tokens: 100,
        }
    }
    fn item(sym: &str, reason: &str) -> MissingItem {
        MissingItem {
            symbol: sym.into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn refetch_takes_only_budget_dropped_in_priority_order() {
        let d = detail(
            vec![
                item("A", "budget"),
                item("B", "budget"),
                item("C", "no-def"), // not budget → not auto re-fetched
                item("D", "budget"),
            ],
            vec![],
        );
        let plan = refetch_plan(&d, 0);
        assert_eq!(plan.symbols, vec!["A", "B", "D"]);
        assert_eq!(plan.total_dropped, 3);
        assert!(!plan.is_empty());
    }

    #[test]
    fn refetch_respects_max_cap_and_preserves_head() {
        let d = detail(
            vec![
                item("A", "budget"),
                item("B", "budget"),
                item("C", "budget"),
            ],
            vec![],
        );
        let plan = refetch_plan(&d, 2);
        assert_eq!(plan.symbols, vec!["A", "B"]); // highest-value head
        assert_eq!(plan.total_dropped, 3); // total still reported
    }

    #[test]
    fn refetch_empty_when_nothing_budget_dropped() {
        let d = detail(vec![item("C", "no-def")], vec![]);
        assert!(refetch_plan(&d, 0).is_empty());
    }

    #[test]
    fn unresolved_ambiguous_escalates_to_expensive() {
        let d = detail(
            vec![],
            vec![item("Conn", "no-def"), item("Handler", "ambiguous")],
        );
        let sig = escalation_signal(&d);
        assert_eq!(sig, EscalationSignal::Escalate);
        assert_eq!(sig.tier(), ModelTier::Expensive);
    }

    #[test]
    fn unresolved_definition_gap_widens_cheap() {
        let d = detail(vec![], vec![item("Conn", "no-def")]);
        let sig = escalation_signal(&d);
        assert_eq!(sig, EscalationSignal::Widen);
        assert_eq!(sig.tier(), ModelTier::Cheap);
    }

    #[test]
    fn no_unresolved_is_closed() {
        let d = detail(vec![], vec![]);
        assert_eq!(escalation_signal(&d), EscalationSignal::None);
        assert_eq!(escalation_signal(&d).tier(), ModelTier::None);
    }

    #[test]
    fn unknown_unresolved_reason_fails_safe_to_widen() {
        let d = detail(vec![], vec![item("X", "mystery")]);
        // never treat an unresolved ref as resolved.
        assert_eq!(escalation_signal(&d), EscalationSignal::Widen);
    }

    #[test]
    fn needs_closing_true_when_either_residue_present() {
        assert!(needs_closing(&detail(vec![item("A", "budget")], vec![])));
        assert!(needs_closing(&detail(vec![], vec![item("X", "no-def")])));
        assert!(!needs_closing(&detail(vec![], vec![])));
        // dropped only for non-budget reason + no unresolved → nothing to do.
        assert!(!needs_closing(&detail(vec![item("C", "no-def")], vec![])));
    }

    #[test]
    fn budget_refetch_greedily_fills_and_stops() {
        let d = detail(
            vec![
                item("A", "budget"),
                item("B", "budget"),
                item("C", "budget"),
            ],
            vec![],
        );
        // each symbol costs 10; budget 25 fits A+B (20) but not C (30).
        let plan = refetch_plan_within_budget(&d, 25, |_| 10);
        assert_eq!(plan.symbols, vec!["A", "B"]);
        assert_eq!(plan.est_tokens, 20);
        assert_eq!(plan.total_dropped, 3);
    }

    #[test]
    fn budget_refetch_skips_a_fat_symbol_but_keeps_scanning() {
        let d = detail(
            vec![item("HUGE", "budget"), item("small", "budget")],
            vec![],
        );
        // HUGE (100) does not fit budget 20, but `small` (5) still should.
        let plan = refetch_plan_within_budget(&d, 20, |sym| if sym == "HUGE" { 100 } else { 5 });
        assert_eq!(plan.symbols, vec!["small"]);
        assert_eq!(plan.est_tokens, 5);
    }

    #[test]
    fn budget_refetch_zero_budget_takes_nothing() {
        let d = detail(vec![item("A", "budget")], vec![]);
        let plan = refetch_plan_within_budget(&d, 0, |_| 1);
        assert!(plan.is_empty());
        assert_eq!(plan.est_tokens, 0);
        // total_dropped still reported so caller sees there WAS residue.
        assert_eq!(plan.total_dropped, 1);
    }

    #[test]
    fn budget_refetch_only_budget_reason_eligible() {
        let d = detail(vec![item("A", "budget"), item("B", "no-def")], vec![]);
        let plan = refetch_plan_within_budget(&d, 100, |_| 1);
        assert_eq!(plan.symbols, vec!["A"]); // no-def not eligible
    }
}
