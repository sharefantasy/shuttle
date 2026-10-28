//! Orchestration state machine (efficiency bet #3, the assembly line).
//!
//! `route` / `dispatch` / `audit` / `closing` are four *parts*. This module is
//! the **assembly line** that runs the loop they imply:
//!
//! ```text
//! pick tier (mechanical → cheap)               [audit::default_tier_for_task]
//!   └─ run one attempt (injected runner)
//!        └─ classify the outcome               [routing::route]
//!             ├─ Accept / AcceptWithVerify  → done (Green)
//!             ├─ RetryWithForce             → same tier, force flag, SAME idem key
//!             ├─ RetryWiden                 → same tier, precise re-fetch [closing]
//!             ├─ Escalate                   → expensive tier, NEW idem key
//!             └─ Abort                      → done (Error)
//!   └─ assemble the dual receipt              [audit::DualReceipt]
//! ```
//!
//! The only thing that touches the outside world is the injected
//! [`AttemptRunner`] — a closure/impl that actually *runs one attempt* (spawns a
//! tool or a model) and hands back a [`ToolOutcome`] + a provider run id. Every
//! decision — which tier, whether to force, what to re-fetch, when to escalate,
//! when to stop, which idempotency key to reuse — lives here, in pure tested
//! code. A launcher wires this in by implementing one trait method and calling
//! [`orchestrate`] once, instead of hand-rolling the loop in runtime code.
//!
//! # Idempotency
//!
//! The `idempotency_key` must stay **stable across retries** so
//! de-dup works. A *mechanical* retry (force / widen) is the same logical
//! attempt, so it reuses the key. An *escalation* is a distinct attempt on a
//! different tier, so it mints a derived key (`<base>-esc<n>`). See
//! [`IdempotencyGuard`].

use crate::audit::{default_tier_for_task, DualReceipt, ReceiptLeg};
use crate::closing::{refetch_plan, refetch_plan_within_budget};
use crate::routing::{route, ModelTier, RouteAction};
use crate::tool_outcome::{OutcomeState, ToolOutcome, TypedDetail};
use serde::{Deserialize, Serialize};

/// What the orchestrator asks the runner to execute for one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRequest {
    /// The task op (e.g. `"rename"`, `"context-pack"`) — lets the runner pick
    /// what to actually run.
    pub op: String,
    /// Which model tier this attempt must use.
    pub tier: ModelTier,
    /// Whether to apply the tool's `force_hint` (set on a `RetryWithForce`).
    pub force: bool,
    /// Symbols to precisely re-fetch (set on a `RetryWiden`); empty otherwise.
    pub refetch: Vec<String>,
    /// The external orchestrator idempotency key for this attempt (stable across mechanical
    /// retries; derived on escalation).
    pub idempotency_key: String,
    /// 0-based attempt counter.
    pub attempt_index: usize,
    /// Deterministic per-attempt trace id (`<base_run_id>-a<index>`). A real
    /// launcher may echo this back as its `provider_run_id` so every attempt
    /// is independently traceable even before the provider assigns one.
    pub trace_run_id: String,
}

/// What the runner returns for one attempt.
#[derive(Debug, Clone)]
pub struct AttemptOutput {
    /// The uniform outcome envelope this attempt produced.
    pub outcome: ToolOutcome,
    /// The provider/run id external should trace this attempt by (must be distinct
    /// per attempt so both legs are independently traceable).
    pub provider_run_id: String,
}

/// The one thing the orchestrator delegates to the outside world: run a single
/// attempt. Implemented by real launchers (spawn tool/model) and by tests
/// (scripted responses). A blanket impl makes any `FnMut(&AttemptRequest) ->
/// AttemptOutput` a runner.
pub trait AttemptRunner {
    fn run(&mut self, req: &AttemptRequest) -> AttemptOutput;
}

impl<F> AttemptRunner for F
where
    F: FnMut(&AttemptRequest) -> AttemptOutput,
{
    fn run(&mut self, req: &AttemptRequest) -> AttemptOutput {
        self(req)
    }
}

/// Knobs for a run. `Default` gives sane bounds.
#[derive(Debug, Clone)]
pub struct OrchestrationOptions {
    /// Hard cap on attempts — the loop's termination guarantee.
    pub max_attempts: usize,
    /// Count cap for a widen re-fetch when no token budget is set.
    pub refetch_max: usize,
    /// Token budget for a widen re-fetch. `> 0` switches on the budget-aware
    /// [`refetch_plan_within_budget`]; `0` falls back to `refetch_max`.
    pub refetch_token_budget: usize,
    /// Flat per-symbol token estimate used with `refetch_token_budget`.
    pub per_symbol_estimate: usize,
}

impl Default for OrchestrationOptions {
    fn default() -> Self {
        OrchestrationOptions {
            max_attempts: 6,
            refetch_max: 8,
            refetch_token_budget: 0,
            per_symbol_estimate: 64,
        }
    }
}

/// The task to orchestrate.
#[derive(Debug, Clone)]
pub struct OrchestrationTask {
    /// The op name — drives the mechanical whitelist and is echoed to the runner.
    pub op: String,
    /// Base provider run id; per-attempt ids are derived as `<base>-a<index>`.
    pub base_run_id: String,
    /// Base external idempotency key.
    pub idempotency_key: String,
}

/// One recorded attempt (for the audit transcript).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub index: usize,
    pub tier: ModelTier,
    pub action: RouteAction,
    pub reason_code: String,
    pub provider_run_id: String,
    pub idempotency_key: String,
}

/// The full result of an orchestrated run.
#[derive(Debug, Clone)]
pub struct OrchestrationResult {
    /// The terminal (or last) outcome.
    pub final_outcome: ToolOutcome,
    /// The tier the final attempt ran on.
    pub final_tier: ModelTier,
    /// Every attempt, in order.
    pub attempts: Vec<AttemptRecord>,
    /// The cheap-first / escalate dual receipt for the external orchestrator ledger.
    pub dual_receipt: DualReceipt,
    /// Whether the run escalated cheap → expensive.
    pub escalated: bool,
    /// True if the loop hit `max_attempts` without a terminal Accept/Abort.
    pub exhausted: bool,
}

impl OrchestrationResult {
    /// Whether the run ended Green (accepted).
    pub fn is_green(&self) -> bool {
        self.final_outcome.state == OutcomeState::Green
    }
    /// Number of attempts made.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }
}

/// Idempotency key discipline (B4). Mechanical retries reuse the current key;
/// escalations derive a fresh one so a re-tiered attempt is a distinct launch.
#[derive(Debug, Clone)]
pub struct IdempotencyGuard {
    base: String,
    current: String,
    escalations: usize,
}

impl IdempotencyGuard {
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        IdempotencyGuard {
            current: base.clone(),
            base,
            escalations: 0,
        }
    }
    /// The key for the current attempt.
    pub fn current(&self) -> &str {
        &self.current
    }
    /// A mechanical retry (force/widen) keeps the same key — external de-dup relies
    /// on stability across retries of the same attempt.
    pub fn on_mechanical_retry(&self) -> &str {
        &self.current
    }
    /// An escalation is a distinct attempt on a new tier → derive a fresh key.
    pub fn on_escalation(&mut self) -> &str {
        self.escalations += 1;
        self.current = format!("{}-esc{}", self.base, self.escalations);
        &self.current
    }
    pub fn escalation_count(&self) -> usize {
        self.escalations
    }
}

/// Extract the precise re-fetch symbols for a `RetryWiden`, honoring the token
/// budget when set. Empty when the outcome carries no context residue (the
/// runner then widens by its own means — a plain wider re-run).
fn widen_refetch(outcome: &ToolOutcome, opts: &OrchestrationOptions) -> Vec<String> {
    if let TypedDetail::Context(detail) = &outcome.detail {
        let plan = if opts.refetch_token_budget > 0 {
            let est = opts.per_symbol_estimate;
            refetch_plan_within_budget(detail, opts.refetch_token_budget, move |_| est)
        } else {
            refetch_plan(detail, opts.refetch_max)
        };
        plan.symbols
    } else {
        Vec::new()
    }
}

/// Run the full cheap-first → route → escalate loop for a task, delegating each
/// attempt to `runner`.
///
/// Pure except for `runner`. Terminates in at most `opts.max_attempts` attempts.
pub fn orchestrate<R: AttemptRunner>(
    task: &OrchestrationTask,
    runner: &mut R,
    opts: &OrchestrationOptions,
) -> OrchestrationResult {
    let mut tier = default_tier_for_task(&task.op);
    let mut force = false;
    let mut refetch: Vec<String> = Vec::new();
    let mut idem = IdempotencyGuard::new(&task.idempotency_key);

    let mut attempts: Vec<AttemptRecord> = Vec::new();
    let mut last_cheap_leg: Option<ReceiptLeg> = None;
    let mut last_expensive_leg: Option<ReceiptLeg> = None;
    let mut escalated = false;

    let mut final_outcome: Option<ToolOutcome> = None;
    let mut exhausted = true;

    let max = opts.max_attempts.max(1);
    for attempt_index in 0..max {
        let req = AttemptRequest {
            op: task.op.clone(),
            tier,
            force,
            refetch: refetch.clone(),
            idempotency_key: idem.current().to_string(),
            attempt_index,
            trace_run_id: format!("{}-a{}", task.base_run_id, attempt_index),
        };
        let output = runner.run(&req);
        let outcome = output.outcome;
        let decision = route(&outcome);

        // Build this attempt's leg (terminal flag fixed up after the loop).
        let leg = ReceiptLeg::from_outcome(tier, &outcome, output.provider_run_id.clone(), false);
        match tier {
            ModelTier::Expensive => last_expensive_leg = Some(leg),
            _ => last_cheap_leg = Some(leg),
        }

        attempts.push(AttemptRecord {
            index: attempt_index,
            tier,
            action: decision.action,
            reason_code: outcome.reason_code.clone(),
            provider_run_id: output.provider_run_id,
            idempotency_key: req.idempotency_key,
        });

        match decision.action {
            RouteAction::Accept | RouteAction::AcceptWithVerify => {
                final_outcome = Some(outcome);
                exhausted = false;
                break;
            }
            RouteAction::Abort => {
                final_outcome = Some(outcome);
                exhausted = false;
                break;
            }
            RouteAction::RetryWithForce => {
                force = true;
                refetch = Vec::new();
                let _ = idem.on_mechanical_retry(); // key unchanged
                final_outcome = Some(outcome);
            }
            RouteAction::RetryWiden => {
                force = false;
                refetch = widen_refetch(&outcome, opts);
                let _ = idem.on_mechanical_retry(); // key unchanged
                final_outcome = Some(outcome);
            }
            RouteAction::Escalate => {
                if tier == ModelTier::Expensive {
                    // Already at the top tier and still not resolved — nothing
                    // higher to escalate to. Stop with the unresolved outcome.
                    final_outcome = Some(outcome);
                    exhausted = false;
                    break;
                }
                tier = ModelTier::Expensive;
                escalated = true;
                force = false;
                refetch = Vec::new();
                let _ = idem.on_escalation(); // NEW key for the escalated attempt
                final_outcome = Some(outcome);
            }
        }
    }

    let final_outcome = final_outcome.expect("loop runs at least once");
    let final_tier = attempts.last().map(|a| a.tier).unwrap_or(tier);

    // Assemble the dual receipt: cheap-first leg + (optional) escalated leg.
    let dual_receipt = if escalated {
        let mut cheap = last_cheap_leg
            .clone()
            .expect("escalation implies a prior cheap leg");
        let mut esc = last_expensive_leg
            .clone()
            .expect("escalation implies an expensive leg");
        cheap.receipt.terminal = false;
        esc.receipt.terminal = true;
        DualReceipt::from_legs(cheap, Some(esc))
    } else {
        // No tier transition: the single (final) leg is terminal.
        let mut only = last_expensive_leg
            .clone()
            .or_else(|| last_cheap_leg.clone())
            .expect("at least one attempt ran");
        only.receipt.terminal = true;
        DualReceipt::from_legs(only, None)
    };

    OrchestrationResult {
        final_outcome,
        final_tier,
        attempts,
        dual_receipt,
        escalated,
        exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::ProviderStatus;
    use crate::tool_outcome::{reason, ContextDetail, MissingItem, ToolOutcome, TypedDetail};

    fn task(op: &str) -> OrchestrationTask {
        OrchestrationTask {
            op: op.into(),
            base_run_id: "run".into(),
            idempotency_key: "idem-1".into(),
        }
    }

    fn green(tool: &str) -> ToolOutcome {
        ToolOutcome::green(tool, TypedDetail::None)
    }
    fn blocked(tool: &str, code: &str) -> ToolOutcome {
        ToolOutcome::blocked(tool, code, "next", TypedDetail::None)
    }
    fn ctx_blocked() -> ToolOutcome {
        let d = ContextDetail {
            root: "sync".into(),
            root_path: "m.py".into(),
            included: vec![],
            dropped: vec![
                MissingItem {
                    symbol: "A".into(),
                    reason: "budget".into(),
                },
                MissingItem {
                    symbol: "B".into(),
                    reason: "budget".into(),
                },
            ],
            unresolved: vec![],
            ambiguous: vec![],
            used_tokens: 10,
            budget_tokens: 100,
        };
        ToolOutcome::blocked(
            "context-pack",
            reason::CONTEXT_TRUNCATED,
            "widen",
            TypedDetail::Context(d),
        )
    }

    #[test]
    fn mechanical_task_green_first_try_stays_cheap() {
        let mut runner = |_req: &AttemptRequest| AttemptOutput {
            outcome: green("refactor"),
            provider_run_id: "p0".into(),
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert!(r.is_green());
        assert!(!r.escalated);
        assert_eq!(r.attempt_count(), 1);
        assert_eq!(r.final_tier, ModelTier::Cheap);
        assert!(!r.dual_receipt.escalated_to_expensive());
        assert!(r.dual_receipt.final_leg().receipt.terminal);
    }

    #[test]
    fn force_retry_reuses_idempotency_key_then_succeeds() {
        let mut n = 0;
        let mut runner = |_req: &AttemptRequest| {
            n += 1;
            if n == 1 {
                AttemptOutput {
                    outcome: blocked("refactor", reason::NEEDS_FORCE).with_force_hint("--force"),
                    provider_run_id: "p0".into(),
                }
            } else {
                AttemptOutput {
                    outcome: green("refactor"),
                    provider_run_id: "p1".into(),
                }
            }
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert!(r.is_green());
        assert!(!r.escalated);
        assert_eq!(r.attempt_count(), 2);
        // B4: mechanical retry keeps the SAME idempotency key.
        assert_eq!(r.attempts[0].idempotency_key, "idem-1");
        assert_eq!(r.attempts[1].idempotency_key, "idem-1");
        assert_eq!(r.attempts[0].action, RouteAction::RetryWithForce);
    }

    #[test]
    fn widen_pulls_precise_refetch_symbols() {
        let mut seen_refetch: Vec<Vec<String>> = Vec::new();
        let mut n = 0;
        let mut runner = |req: &AttemptRequest| {
            seen_refetch.push(req.refetch.clone());
            n += 1;
            if n == 1 {
                AttemptOutput {
                    outcome: ctx_blocked(),
                    provider_run_id: "p0".into(),
                }
            } else {
                AttemptOutput {
                    outcome: green("context-pack"),
                    provider_run_id: "p1".into(),
                }
            }
        };
        let r = orchestrate(
            &task("context-pack"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert!(r.is_green());
        assert_eq!(r.attempts[0].action, RouteAction::RetryWiden);
        // first attempt had no refetch; second carried the precise dropped set.
        assert!(seen_refetch[0].is_empty());
        assert_eq!(seen_refetch[1], vec!["A", "B"]);
    }

    #[test]
    fn widen_respects_token_budget() {
        let mut seen: Vec<Vec<String>> = Vec::new();
        let mut n = 0;
        let mut runner = |req: &AttemptRequest| {
            seen.push(req.refetch.clone());
            n += 1;
            if n == 1 {
                AttemptOutput {
                    outcome: ctx_blocked(),
                    provider_run_id: "p0".into(),
                }
            } else {
                AttemptOutput {
                    outcome: green("context-pack"),
                    provider_run_id: "p1".into(),
                }
            }
        };
        // budget 64, each symbol estimated 64 → only ONE symbol fits.
        let opts = OrchestrationOptions {
            refetch_token_budget: 64,
            per_symbol_estimate: 64,
            ..OrchestrationOptions::default()
        };
        let r = orchestrate(&task("context-pack"), &mut runner, &opts);
        assert!(r.is_green());
        assert_eq!(seen[1], vec!["A"]); // B budgeted out
    }

    #[test]
    fn ambiguous_escalates_to_expensive_with_new_key_and_dual_receipt() {
        let mut n = 0;
        let mut runner = |req: &AttemptRequest| {
            n += 1;
            if n == 1 {
                assert_eq!(req.tier, ModelTier::Cheap);
                AttemptOutput {
                    outcome: blocked("nav", reason::AMBIGUOUS),
                    provider_run_id: "cheap-run".into(),
                }
            } else {
                assert_eq!(req.tier, ModelTier::Expensive);
                AttemptOutput {
                    outcome: green("nav"),
                    provider_run_id: "exp-run".into(),
                }
            }
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert!(r.is_green());
        assert!(r.escalated);
        assert_eq!(r.final_tier, ModelTier::Expensive);
        // B4: escalation mints a NEW idempotency key.
        assert_eq!(r.attempts[0].idempotency_key, "idem-1");
        assert_eq!(r.attempts[1].idempotency_key, "idem-1-esc1");
        // dual receipt: cheap Blocked leg (non-terminal) + expensive Green (terminal).
        assert!(r.dual_receipt.escalated_to_expensive());
        assert_eq!(
            r.dual_receipt.cheap.receipt.provider_status,
            ProviderStatus::Failed
        );
        assert!(!r.dual_receipt.cheap.receipt.terminal);
        let esc = r.dual_receipt.escalated.as_ref().unwrap();
        assert_eq!(esc.receipt.provider_status, ProviderStatus::Completed);
        assert!(esc.receipt.terminal);
        assert_eq!(esc.run_id(), Some("exp-run"));
    }

    #[test]
    fn non_mechanical_task_starts_expensive_no_escalation() {
        let mut runner = |req: &AttemptRequest| {
            assert_eq!(req.tier, ModelTier::Expensive);
            AttemptOutput {
                outcome: green("nav"),
                provider_run_id: "p0".into(),
            }
        };
        let r = orchestrate(
            &task("redesign_api"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert!(r.is_green());
        assert!(!r.escalated, "starting expensive is not an escalation");
        assert!(!r.dual_receipt.escalated_to_expensive());
    }

    #[test]
    fn expensive_still_ambiguous_stops_without_infinite_loop() {
        let mut runner = |_req: &AttemptRequest| AttemptOutput {
            outcome: blocked("nav", reason::AMBIGUOUS),
            provider_run_id: "p".into(),
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        // cheap ambiguous → escalate → expensive ambiguous → stop.
        assert!(!r.is_green());
        assert!(r.escalated);
        assert_eq!(r.attempt_count(), 2);
        assert!(!r.exhausted); // stopped deliberately, not by hitting the cap
    }

    #[test]
    fn max_attempts_bounds_a_thrashing_widen() {
        // Always returns a widenable context-blocked → would loop forever
        // without the cap.
        let mut runner = |_req: &AttemptRequest| AttemptOutput {
            outcome: ctx_blocked(),
            provider_run_id: "p".into(),
        };
        let opts = OrchestrationOptions {
            max_attempts: 3,
            ..OrchestrationOptions::default()
        };
        let r = orchestrate(&task("context-pack"), &mut runner, &opts);
        assert!(!r.is_green());
        assert_eq!(r.attempt_count(), 3);
        assert!(r.exhausted);
    }

    #[test]
    fn hard_error_aborts_immediately() {
        let mut runner = |_req: &AttemptRequest| AttemptOutput {
            outcome: ToolOutcome::error("context-pack", reason::IO, "unreadable"),
            provider_run_id: "p".into(),
        };
        let r = orchestrate(
            &task("context-pack"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        assert_eq!(r.final_outcome.state, OutcomeState::Error);
        assert_eq!(r.attempt_count(), 1);
        assert_eq!(r.attempts[0].action, RouteAction::Abort);
        assert!(!r.exhausted);
    }

    #[test]
    fn idempotency_guard_discipline() {
        let mut g = IdempotencyGuard::new("k");
        assert_eq!(g.current(), "k");
        assert_eq!(g.on_mechanical_retry(), "k");
        assert_eq!(g.on_escalation(), "k-esc1");
        assert_eq!(g.current(), "k-esc1");
        assert_eq!(g.on_mechanical_retry(), "k-esc1"); // stable after escalation
        assert_eq!(g.on_escalation(), "k-esc2");
        assert_eq!(g.escalation_count(), 2);
    }
}
