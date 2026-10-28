//! Dual-receipt audit + mechanical-task whitelist (efficiency bet #3, P2.3/P2.4).
//!
//! # P2.3 — the dual receipt
//!
//! When the router escalates (cheap model returns `Blocked`, expensive model
//! then returns `Green`), the external orchestrator's ledger must show **both** attempts, not just the
//! winning one — otherwise the cheap-first economics are invisible and a
//! reviewer cannot tell an escalation from a first-try success.
//!
//! Each leg is lowered into the **frozen** [`ProviderReceipt`]
//! (`external_provider_receipt/v1`) *unchanged* — its 9-field snapshot has
//! `additionalProperties: false`, so shuttle must NOT stuff tier/route metadata into
//! it. Instead the shuttle-local [`ReceiptLeg`] wrapper carries `tier` + `action`
//! alongside the pristine external receipt, and [`DualReceipt`] pairs the cheap leg
//! with the (optional) escalated leg. Both legs carry a distinct
//! `provider_run_id`, so both are independently traceable in the external orchestrator ledger.
//!
//! # P2.4 — the mechanical whitelist
//!
//! Some operations are *mechanical* by construction — a rename, a parameter
//! promotion, a mechanical recipe — and never need the expensive model on the
//! first try. [`is_mechanical_task`] is the whitelist; [`default_tier_for_task`]
//! maps a mechanical op to [`ModelTier::Cheap`] and everything else to
//! [`ModelTier::Expensive`], so dispatch can pick the cheap entry point by
//! default without inspecting an outcome that does not exist yet.

use crate::anchor::{ProviderReceipt, ProviderStatus, ReconciliationHint};
use crate::routing::{route, ModelTier, RouteAction, RoutingDecision};
use crate::tool_outcome::{OutcomeState, ToolOutcome};
use serde::{Deserialize, Serialize};

/// One attempt in a (possibly escalated) run: which tier ran, what the router
/// decided from its outcome, and the pristine external receipt for that attempt.
///
/// `receipt` is the frozen `external_provider_receipt/v1` verbatim — the
/// `tier`/`action` fields live *here*, in the shuttle-local wrapper, never inside the
/// external snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptLeg {
    /// The model tier that produced this attempt.
    pub tier: ModelTier,
    /// The routing action derived from this attempt's outcome.
    pub action: RouteAction,
    /// The frozen external receipt for this attempt (schema unchanged).
    pub receipt: ProviderReceipt,
}

impl ReceiptLeg {
    /// Lower a `(tier, outcome, run_id)` triple into a leg, deriving the external
    /// receipt fields from the outcome's state.
    ///
    /// * `Green`  → `Completed`,   hint `QueryProvider`, terminal iff `is_final`.
    /// * `Blocked`→ `Failed`,      hint `Restart` (escalate signal), terminal iff `is_final`.
    /// * `Error`  → `Failed`,      hint `Block`,          terminal (always).
    pub fn from_outcome(
        tier: ModelTier,
        outcome: &ToolOutcome,
        provider_run_id: impl Into<String>,
        is_final: bool,
    ) -> Self {
        let decision = route(outcome);
        let (status, hint, terminal) = match outcome.state {
            OutcomeState::Green => (
                ProviderStatus::Completed,
                ReconciliationHint::QueryProvider,
                is_final,
            ),
            OutcomeState::Blocked => (
                ProviderStatus::Failed,
                ReconciliationHint::Restart,
                is_final,
            ),
            OutcomeState::Error => (ProviderStatus::Failed, ReconciliationHint::Block, true),
        };
        let receipt = ProviderReceipt {
            provider_status: status,
            reconciliation_hint: hint,
            result_ref: None,
            provider_run_id: Some(provider_run_id.into()),
            terminal,
            extra: Default::default(),
        };
        ReceiptLeg {
            tier,
            action: decision.action,
            receipt,
        }
    }

    /// The `provider_run_id` of this leg's external receipt, if present.
    pub fn run_id(&self) -> Option<&str> {
        self.receipt.provider_run_id.as_deref()
    }
}

/// A cheap-first / escalate audit pair. The cheap leg is always present; the
/// escalated leg exists only when the router actually escalated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DualReceipt {
    /// The first (cheap-tier) attempt — recorded even when it was `Blocked`.
    pub cheap: ReceiptLeg,
    /// The escalated (expensive-tier) attempt, if escalation happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated: Option<ReceiptLeg>,
}

impl DualReceipt {
    /// Record a cheap-only run (the cheap tier settled it — Green or a terminal
    /// Error — with no escalation).
    pub fn cheap_only(cheap: ReceiptLeg) -> Self {
        DualReceipt {
            cheap,
            escalated: None,
        }
    }

    /// Assemble a dual receipt from two already-built legs. The `first` leg is
    /// the primary (cheap-first) attempt; `escalated` is the expensive-tier
    /// escalation leg iff one happened. Unlike [`DualReceipt::build`], this does
    /// not re-derive tiers — the caller (e.g. the orchestrator) sets each leg's
    /// true tier, so a run that started directly on the expensive tier is
    /// recorded faithfully.
    pub fn from_legs(first: ReceiptLeg, escalated: Option<ReceiptLeg>) -> Self {
        DualReceipt {
            cheap: first,
            escalated,
        }
    }

    /// Build the dual receipt from a cheap outcome and its (optional) escalated
    /// outcome. The escalated leg is the final one iff it exists; otherwise the
    /// cheap leg is final.
    ///
    /// Distinct `provider_run_id`s keep both legs independently traceable.
    pub fn build(
        cheap_outcome: &ToolOutcome,
        cheap_run_id: impl Into<String>,
        escalated: Option<(&ToolOutcome, String)>,
    ) -> Self {
        match escalated {
            Some((esc_outcome, esc_run_id)) => {
                let cheap = ReceiptLeg::from_outcome(
                    ModelTier::Cheap,
                    cheap_outcome,
                    cheap_run_id,
                    /* is_final */ false,
                );
                let esc = ReceiptLeg::from_outcome(
                    ModelTier::Expensive,
                    esc_outcome,
                    esc_run_id,
                    /* is_final */ true,
                );
                DualReceipt {
                    cheap,
                    escalated: Some(esc),
                }
            }
            None => DualReceipt::cheap_only(ReceiptLeg::from_outcome(
                ModelTier::Cheap,
                cheap_outcome,
                cheap_run_id,
                /* is_final */ true,
            )),
        }
    }

    /// Whether this run escalated to the expensive tier.
    pub fn escalated_to_expensive(&self) -> bool {
        self.escalated.is_some()
    }

    /// The final (terminal) leg — the escalated one if present, else the cheap.
    pub fn final_leg(&self) -> &ReceiptLeg {
        self.escalated.as_ref().unwrap_or(&self.cheap)
    }

    /// The routing decision implied by the cheap leg's action (for auditing the
    /// escalate/keep-cheap fork).
    pub fn cheap_decision_action(&self) -> RouteAction {
        self.cheap.action
    }
}

/// The mechanical-task whitelist (P2.4). Names are normalized (see
/// [`normalize_task`]) before comparison, so `promote-param`, `promote_param`,
/// and `Promote-Param` all match.
///
/// A *mechanical recipe* is any op prefixed `recipe:` whose recipe name is in
/// [`MECHANICAL_RECIPES`].
pub const MECHANICAL_TASKS: &[&str] = &["rename", "promote_param"];

/// Recipe names considered mechanical (used with a `recipe:` prefix, e.g.
/// `recipe:inline_const`).
pub const MECHANICAL_RECIPES: &[&str] = &[
    "rename",
    "promote_param",
    "inline_const",
    "sort_imports",
    "format",
];

/// Normalize a task/op name for whitelist comparison: trim, lowercase, and map
/// `-` to `_`.
pub fn normalize_task(op: &str) -> String {
    op.trim().to_ascii_lowercase().replace('-', "_")
}

/// Whether `op` is a mechanical task that defaults to the cheap model entry.
///
/// Matches the [`MECHANICAL_TASKS`] whitelist directly, or a `recipe:<name>`
/// whose `<name>` is in [`MECHANICAL_RECIPES`].
pub fn is_mechanical_task(op: &str) -> bool {
    let n = normalize_task(op);
    if MECHANICAL_TASKS.contains(&n.as_str()) {
        return true;
    }
    if let Some(recipe) = n.strip_prefix("recipe:") {
        return MECHANICAL_RECIPES.contains(&recipe.trim_start_matches('_'));
    }
    false
}

/// The default model tier for a task *before* any outcome exists: mechanical
/// tasks start cheap, everything else starts expensive (fail safe — never
/// assume a non-whitelisted op is cheap).
pub fn default_tier_for_task(op: &str) -> ModelTier {
    if is_mechanical_task(op) {
        ModelTier::Cheap
    } else {
        ModelTier::Expensive
    }
}

/// Convenience: the full initial routing intent for a task name — tier plus a
/// stable rationale — mirroring [`RoutingDecision`] shape for uniform logging.
pub fn initial_decision_for_task(op: &str) -> RoutingDecision {
    if is_mechanical_task(op) {
        RoutingDecision {
            action: RouteAction::Accept,
            tier: ModelTier::Cheap,
            rationale: "mechanical task: default to cheap model entry",
        }
    } else {
        RoutingDecision {
            action: RouteAction::Escalate,
            tier: ModelTier::Expensive,
            rationale: "non-mechanical task: start on expensive model",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_outcome::{reason, ToolOutcome, TypedDetail};

    fn green() -> ToolOutcome {
        ToolOutcome::green("nav", TypedDetail::None)
    }
    fn blocked_ambiguous() -> ToolOutcome {
        ToolOutcome::blocked(
            "nav",
            reason::AMBIGUOUS,
            "pick a candidate",
            TypedDetail::None,
        )
    }
    fn hard_error() -> ToolOutcome {
        ToolOutcome::error("nav", reason::IO, "unreadable")
    }

    #[test]
    fn cheap_only_when_green_first_try() {
        let dr = DualReceipt::build(&green(), "run-cheap-1", None);
        assert!(!dr.escalated_to_expensive());
        assert_eq!(dr.cheap.tier, ModelTier::Cheap);
        assert_eq!(dr.cheap.receipt.provider_status, ProviderStatus::Completed);
        assert!(
            dr.cheap.receipt.terminal,
            "cheap-only green leg is terminal"
        );
        assert_eq!(dr.final_leg().run_id(), Some("run-cheap-1"));
    }

    #[test]
    fn dual_receipt_records_both_legs_with_distinct_run_ids() {
        let dr = DualReceipt::build(
            &blocked_ambiguous(),
            "run-cheap-2",
            Some((&green(), "run-exp-2".into())),
        );
        assert!(dr.escalated_to_expensive());

        // cheap leg: a recorded Blocked → Failed + Restart, NON-terminal.
        assert_eq!(dr.cheap.tier, ModelTier::Cheap);
        assert_eq!(dr.cheap.receipt.provider_status, ProviderStatus::Failed);
        assert_eq!(
            dr.cheap.receipt.reconciliation_hint,
            ReconciliationHint::Restart
        );
        assert!(!dr.cheap.receipt.terminal, "cheap leg is not the final leg");
        assert_eq!(dr.cheap.action, RouteAction::Escalate);

        // escalated leg: the Green winner, terminal.
        let esc = dr.escalated.as_ref().unwrap();
        assert_eq!(esc.tier, ModelTier::Expensive);
        assert_eq!(esc.receipt.provider_status, ProviderStatus::Completed);
        assert!(esc.receipt.terminal);

        // both traceable, distinct.
        assert_eq!(dr.cheap.run_id(), Some("run-cheap-2"));
        assert_eq!(esc.run_id(), Some("run-exp-2"));
        assert_ne!(dr.cheap.run_id(), esc.run_id());
    }

    #[test]
    fn error_leg_is_always_terminal_and_blocks() {
        let dr = DualReceipt::build(&hard_error(), "run-err", None);
        assert!(dr.cheap.receipt.terminal);
        assert_eq!(
            dr.cheap.receipt.reconciliation_hint,
            ReconciliationHint::Block
        );
        assert_eq!(dr.cheap.action, RouteAction::Abort);
    }

    #[test]
    fn receipt_leg_stays_within_frozen_9field_schema() {
        // The external orchestrator receipt must round-trip without any shuttle-local fields leaking in.
        let leg = ReceiptLeg::from_outcome(ModelTier::Cheap, &green(), "run-x", true);
        let json = serde_json::to_string(&leg.receipt).unwrap();
        // tier/action must NOT appear inside the external orchestrator receipt JSON.
        assert!(!json.contains("tier"));
        assert!(!json.contains("action"));
        assert!(json.contains("provider_status"));
        let back: ProviderReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider_status, ProviderStatus::Completed);
    }

    #[test]
    fn mechanical_whitelist_matches_normalized_names() {
        assert!(is_mechanical_task("rename"));
        assert!(is_mechanical_task("promote-param"));
        assert!(is_mechanical_task("promote_param"));
        assert!(is_mechanical_task("Promote-Param"));
        assert!(is_mechanical_task("  rename  "));
        assert!(!is_mechanical_task("extract-function"));
        assert!(!is_mechanical_task("summarize"));
        assert!(!is_mechanical_task(""));
    }

    #[test]
    fn mechanical_recipes_match_by_prefix() {
        assert!(is_mechanical_task("recipe:inline_const"));
        assert!(is_mechanical_task("recipe:sort-imports"));
        assert!(is_mechanical_task("RECIPE:format"));
        assert!(!is_mechanical_task("recipe:rewrite_module"));
        assert!(!is_mechanical_task("recipe:"));
    }

    #[test]
    fn default_tier_fails_safe_for_unknown_tasks() {
        assert_eq!(default_tier_for_task("rename"), ModelTier::Cheap);
        assert_eq!(default_tier_for_task("recipe:format"), ModelTier::Cheap);
        // unknown → expensive, never assume cheap.
        assert_eq!(default_tier_for_task("mystery_op"), ModelTier::Expensive);
    }

    #[test]
    fn initial_decision_carries_tier_and_rationale() {
        let mech = initial_decision_for_task("rename");
        assert_eq!(mech.tier, ModelTier::Cheap);
        assert_eq!(mech.action, RouteAction::Accept);
        assert!(!mech.rationale.is_empty());

        let heavy = initial_decision_for_task("redesign_api");
        assert_eq!(heavy.tier, ModelTier::Expensive);
        assert_eq!(heavy.action, RouteAction::Escalate);
    }
}
