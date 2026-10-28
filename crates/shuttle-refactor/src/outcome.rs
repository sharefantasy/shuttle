//! Lowering of `shuttle-refactor`'s native results into the universal
//! [`shuttle_protocol::ToolOutcome`] envelope (efficiency bet #2, P1.2).
//!
//! `shuttle-refactor` is *above* `shuttle-protocol` in the dependency graph, so it is the
//! right place to own the mapping: the base crate stays ignorant of refactor
//! types, and each native result *lowers itself* here into a `ToolOutcome` + a
//! [`TypedDetail`] variant.
//!
//! # The two rules this encodes
//! * **Confidence drives state.** A `High`/`Medium` plan is [`Green`] — the
//!   `EditPlan` is carried inline in the detail, directly appliable. A `Low`
//!   plan is *not* silently applied (§7 never-guess): it becomes [`Blocked`]
//!   with `reason = needs-force`, spelling out the single override
//!   (`--force`) so a router can decide cheap-retry-with-force vs. escalate.
//! * **Recipe gate is one-step-fixable.** A recipe that expands cleanly is
//!   `Green`; an expansion error (missing/unknown param, unresolved template)
//!   is a genuine [`Error`] the recipe cannot frame as a next action — those
//!   are honest hard failures by construction (§7).
//!
//! [`Green`]: shuttle_protocol::OutcomeState::Green
//! [`Blocked`]: shuttle_protocol::OutcomeState::Blocked
//! [`Error`]: shuttle_protocol::OutcomeState::Error

use crate::recipe::{Recipe, RecipeError};
use crate::RefactorPlan;
use shuttle_nav::Confidence;
use shuttle_protocol::{
    reason, OutcomeConfidence, RecipeDetail, RefactorDetail, ToolOutcome, TypedDetail,
};
use std::collections::BTreeMap;

/// Tool name used in every refactor-produced envelope.
pub const TOOL_REFACTOR: &str = "refactor";
/// Tool name used in every recipe-produced envelope.
pub const TOOL_RECIPE: &str = "recipe";

/// Map `shuttle-nav`'s canonical [`Confidence`] onto the protocol mirror. 1:1.
pub fn lower_confidence(c: Confidence) -> OutcomeConfidence {
    match c {
        Confidence::High => OutcomeConfidence::High,
        Confidence::Medium => OutcomeConfidence::Medium,
        Confidence::Low => OutcomeConfidence::Low,
    }
}

/// The kebab op string serde already knows how to produce for a `RefactorKind`.
fn op_kebab(plan: &RefactorPlan) -> String {
    // `RefactorKind` serializes as a kebab-case string; go through serde so the
    // mapping stays the single source of truth (no hand-kept second table).
    serde_json::to_value(plan.op)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", plan.op))
}

/// Lower a native [`RefactorPlan`] into a [`ToolOutcome`].
///
/// * `High`/`Medium` confidence → `Green`, plan carried inline.
/// * `Low` confidence → `Blocked` (`needs-force`), with `--force` as the single
///   override and the plan's `warnings` surfaced as `whats_missing` so a router
///   sees exactly *why* the gate held.
pub fn refactor_outcome(plan: RefactorPlan) -> ToolOutcome {
    let op = op_kebab(&plan);
    let low = plan.has_low_confidence();
    let confidence = lower_confidence(plan.confidence);
    let warnings = plan.warnings.clone();

    let detail = TypedDetail::Refactor(RefactorDetail {
        op: op.clone(),
        plan: plan.plan,
        confidence,
        warnings: warnings.clone(),
    });

    if low {
        let mut missing = warnings;
        if missing.is_empty() {
            missing.push("confidence is Low — review before applying".to_string());
        }
        ToolOutcome::blocked(
            TOOL_REFACTOR,
            reason::NEEDS_FORCE,
            format!("review the plan, then re-run `{op}` with --force to apply the low-confidence result"),
            detail,
        )
        .with_force_hint("--force")
        .with_missing(missing)
        .with_summary(format!("{op}: low-confidence plan held for review"))
    } else if matches!(confidence, OutcomeConfidence::Medium) {
        // Medium is still Green (appliable), but flagged best-effort so a router
        // may choose to double-check without treating it as a block.
        ToolOutcome::green_low(TOOL_REFACTOR, detail)
            .with_summary(format!("{op}: medium-confidence plan"))
    } else {
        ToolOutcome::green(TOOL_REFACTOR, detail).with_summary(format!("{op}: plan ready to apply"))
    }
}

/// Lower a recipe *expansion* into a [`ToolOutcome`].
///
/// A clean expansion is `Green` (the gate passed, N steps ready). An expansion
/// [`RecipeError`] is an honest hard failure (§7) → [`Error`], with the error's
/// own message as the summary and a reason mapped from the error kind.
///
/// [`Error`]: shuttle_protocol::OutcomeState::Error
pub fn recipe_outcome(recipe: &Recipe, file: &str, args: &BTreeMap<String, String>) -> ToolOutcome {
    match crate::recipe::expand(recipe, file, args) {
        Ok(batch) => {
            let steps = batch.steps.len();
            ToolOutcome::green(
                TOOL_RECIPE,
                TypedDetail::Recipe(RecipeDetail {
                    recipe_id: format!("{}@{}", recipe.name, recipe.version),
                    steps,
                    gate_passed: true,
                }),
            )
            .with_summary(format!(
                "recipe `{}` expanded to {steps} steps",
                recipe.name
            ))
        }
        Err(e) => {
            let code = recipe_error_reason(&e);
            ToolOutcome::error(TOOL_RECIPE, code, e.to_string())
        }
    }
}

/// Map a [`RecipeError`] kind onto a stable [`reason`] code. Param/template
/// problems are `invalid-input`; file problems are `io`/`parse`; a registry
/// lint break is `internal`.
fn recipe_error_reason(e: &RecipeError) -> &'static str {
    match e {
        RecipeError::NotFound(_)
        | RecipeError::VersionMismatch { .. }
        | RecipeError::MissingRequiredParam { .. }
        | RecipeError::UnknownParam { .. }
        | RecipeError::UnresolvedTemplate { .. } => reason::INVALID_INPUT,
        RecipeError::FileIo(_) => reason::IO,
        RecipeError::FileParse(_) => reason::PARSE,
        RecipeError::Lint(_) => reason::INTERNAL,
        // Any future variant: default to invalid-input (a user-facing refusal),
        // never a silent Green.
        _ => reason::INVALID_INPUT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::OutcomeState;

    #[test]
    fn low_confidence_plan_blocks_with_force_hint() {
        // Build a Low-confidence plan through the public rename-local path is
        // heavy; instead assert the mapping directly on a hand-made plan.
        let plan = RefactorPlan {
            op: crate::RefactorKind::RenameLocal,
            plan: shuttle_protocol::EditPlan {
                version: shuttle_protocol::CURRENT_VERSION,
                intent: Some("rename".into()),
                ops: vec![],
            },
            signature: None,
            params: vec![],
            returns: vec![],
            confidence: Confidence::Low,
            warnings: vec!["ambiguous target".into()],
            sites: vec![],
        };
        let o = refactor_outcome(plan);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::NEEDS_FORCE);
        assert_eq!(o.force_hint.as_deref(), Some("--force"));
        assert_eq!(o.whats_missing, vec!["ambiguous target".to_string()]);
    }

    #[test]
    fn high_confidence_plan_is_green_with_inline_plan() {
        let plan = RefactorPlan {
            op: crate::RefactorKind::ExtractVariable,
            plan: shuttle_protocol::EditPlan {
                version: shuttle_protocol::CURRENT_VERSION,
                intent: None,
                ops: vec![],
            },
            signature: None,
            params: vec![],
            returns: vec![],
            confidence: Confidence::High,
            warnings: vec![],
            sites: vec![],
        };
        let o = refactor_outcome(plan);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        match o.detail {
            TypedDetail::Refactor(d) => {
                assert_eq!(d.op, "extract-variable");
                assert!(matches!(d.confidence, OutcomeConfidence::High));
            }
            _ => panic!("wrong detail variant"),
        }
    }

    #[test]
    fn medium_confidence_is_green_low() {
        let plan = RefactorPlan {
            op: crate::RefactorKind::ExtractMethod,
            plan: shuttle_protocol::EditPlan {
                version: shuttle_protocol::CURRENT_VERSION,
                intent: None,
                ops: vec![],
            },
            signature: None,
            params: vec![],
            returns: vec![],
            confidence: Confidence::Medium,
            warnings: vec![],
            sites: vec![],
        };
        let o = refactor_outcome(plan);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
    }
}
