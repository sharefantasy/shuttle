//! B2 — economic observability over orchestrated runs.
//!
//! The three efficiency bets only pay off if we can *see* them pay off. This
//! module turns a batch of [`OrchestrationResult`]s into a small, JSON-able
//! ledger that answers the money questions:
//!
//!   * how often did the cheap tier settle it (cheap-hit rate)?
//!   * how often did we have to escalate (escalation rate)?
//!   * how many expensive calls did cheap-first *avoid* vs. an all-expensive
//!     baseline (`expensive_calls_avoided`)?
//!   * what share of traffic is mechanical (the whitelist that should never
//!     touch the expensive tier)?
//!   * how often did we exhaust the retry budget without settling?
//!
//! Metrics are computed from a decoupled [`RunSummary`] so callers can feed in
//! summaries harvested from anywhere (live runs, replayed ledgers) without
//! dragging the whole result graph along.

use serde::{Deserialize, Serialize};

use crate::audit::is_mechanical_task;
use crate::orchestrate::OrchestrationResult;
use crate::routing::ModelTier;

/// The minimal per-run facts the economic ledger needs. Decoupled from
/// [`OrchestrationResult`] so it can be persisted and re-aggregated later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The task op (drives the mechanical-vs-not classification).
    pub op: String,
    /// Whether `op` is on the mechanical whitelist.
    pub mechanical: bool,
    /// Ended Green (accepted).
    pub green: bool,
    /// Escalated cheap → expensive during the run.
    pub escalated: bool,
    /// Hit the attempt cap without a terminal Accept/Abort.
    pub exhausted: bool,
    /// Total attempts made.
    pub attempts: usize,
    /// Attempts that ran on the cheap tier.
    pub cheap_attempts: usize,
    /// Attempts that ran on the expensive tier.
    pub expensive_attempts: usize,
    /// The tier the final attempt ran on.
    pub final_tier: ModelTier,
}

impl OrchestrationResult {
    /// Distill this run into the minimal facts the economic ledger needs.
    pub fn summary(&self, op: &str) -> RunSummary {
        let cheap_attempts = self
            .attempts
            .iter()
            .filter(|a| a.tier != ModelTier::Expensive)
            .count();
        let expensive_attempts = self
            .attempts
            .iter()
            .filter(|a| a.tier == ModelTier::Expensive)
            .count();
        RunSummary {
            op: op.to_string(),
            mechanical: is_mechanical_task(op),
            green: self.is_green(),
            escalated: self.escalated,
            exhausted: self.exhausted,
            attempts: self.attempt_count(),
            cheap_attempts,
            expensive_attempts,
            final_tier: self.final_tier,
        }
    }
}

/// Aggregated economic view over a batch of runs. All `*_rate`/`*_share`
/// fields are in `[0.0, 1.0]`; an empty batch yields all-zero metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EconomicMetrics {
    /// Number of runs aggregated.
    pub runs: usize,
    /// Runs that ended Green.
    pub green_runs: usize,
    /// Fraction of runs that ended Green.
    pub green_rate: f64,
    /// Runs that escalated cheap → expensive.
    pub escalated_runs: usize,
    /// Fraction of runs that escalated.
    pub escalation_rate: f64,
    /// Runs that ended Green on the cheap tier with no escalation — the
    /// bet paying off.
    pub cheap_hit_runs: usize,
    /// Fraction of runs settled cheap-and-green.
    pub cheap_hit_rate: f64,
    /// Runs on the mechanical whitelist.
    pub mechanical_runs: usize,
    /// Fraction of runs that are mechanical.
    pub mechanical_share: f64,
    /// Runs that hit the retry cap without settling.
    pub exhausted_runs: usize,
    /// Fraction of runs that exhausted the budget.
    pub exhausted_rate: f64,
    /// Total attempts across all runs.
    pub total_attempts: usize,
    /// Total cheap-tier attempts.
    pub total_cheap_attempts: usize,
    /// Total expensive-tier attempts.
    pub total_expensive_attempts: usize,
    /// Runs that touched the expensive tier zero times — every one of these is
    /// an expensive call avoided vs. an all-expensive baseline.
    pub expensive_calls_avoided: usize,
    /// Expensive calls avoided as a fraction of runs.
    pub expensive_avoidance_rate: f64,
}

impl EconomicMetrics {
    /// Aggregate a batch of run summaries into the economic ledger.
    pub fn aggregate<'a, I>(summaries: I) -> Self
    where
        I: IntoIterator<Item = &'a RunSummary>,
    {
        let mut m = EconomicMetrics::default();
        for s in summaries {
            m.runs += 1;
            if s.green {
                m.green_runs += 1;
            }
            if s.escalated {
                m.escalated_runs += 1;
            }
            if s.green && !s.escalated && s.final_tier != ModelTier::Expensive {
                m.cheap_hit_runs += 1;
            }
            if s.mechanical {
                m.mechanical_runs += 1;
            }
            if s.exhausted {
                m.exhausted_runs += 1;
            }
            m.total_attempts += s.attempts;
            m.total_cheap_attempts += s.cheap_attempts;
            m.total_expensive_attempts += s.expensive_attempts;
            if s.expensive_attempts == 0 {
                m.expensive_calls_avoided += 1;
            }
        }
        if m.runs > 0 {
            let n = m.runs as f64;
            m.green_rate = m.green_runs as f64 / n;
            m.escalation_rate = m.escalated_runs as f64 / n;
            m.cheap_hit_rate = m.cheap_hit_runs as f64 / n;
            m.mechanical_share = m.mechanical_runs as f64 / n;
            m.exhausted_rate = m.exhausted_runs as f64 / n;
            m.expensive_avoidance_rate = m.expensive_calls_avoided as f64 / n;
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrate::{
        orchestrate, AttemptOutput, AttemptRequest, OrchestrationOptions, OrchestrationTask,
    };
    use crate::tool_outcome::{reason, ToolOutcome, TypedDetail};

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

    #[test]
    fn empty_batch_is_all_zero() {
        let m = EconomicMetrics::aggregate(std::iter::empty());
        assert_eq!(m, EconomicMetrics::default());
        assert_eq!(m.runs, 0);
        assert_eq!(m.cheap_hit_rate, 0.0);
    }

    #[test]
    fn cheap_hit_counts_a_mechanical_green() {
        let mut runner = |_r: &AttemptRequest| AttemptOutput {
            outcome: green("refactor"),
            provider_run_id: "p0".into(),
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        let s = r.summary("rename");
        assert!(s.mechanical && s.green && !s.escalated);
        assert_eq!(s.expensive_attempts, 0);

        let m = EconomicMetrics::aggregate(std::iter::once(&s));
        assert_eq!(m.runs, 1);
        assert_eq!(m.cheap_hit_runs, 1);
        assert_eq!(m.cheap_hit_rate, 1.0);
        assert_eq!(m.mechanical_share, 1.0);
        assert_eq!(m.expensive_calls_avoided, 1);
        assert_eq!(m.total_expensive_attempts, 0);
    }

    #[test]
    fn escalation_is_not_a_cheap_hit_and_uses_expensive() {
        let mut n = 0;
        let mut runner = |_r: &AttemptRequest| {
            n += 1;
            if n == 1 {
                AttemptOutput {
                    outcome: blocked("nav", reason::AMBIGUOUS),
                    provider_run_id: "c".into(),
                }
            } else {
                AttemptOutput {
                    outcome: green("nav"),
                    provider_run_id: "e".into(),
                }
            }
        };
        let r = orchestrate(
            &task("rename"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        let s = r.summary("rename");
        assert!(s.escalated && s.green);
        assert_eq!(s.expensive_attempts, 1);
        assert_eq!(s.cheap_attempts, 1);

        let m = EconomicMetrics::aggregate(std::iter::once(&s));
        assert_eq!(m.escalated_runs, 1);
        assert_eq!(m.cheap_hit_runs, 0);
        assert_eq!(m.expensive_calls_avoided, 0);
        assert_eq!(m.total_expensive_attempts, 1);
    }

    #[test]
    fn non_mechanical_start_expensive_is_not_avoided_not_escalated() {
        let mut runner = |_r: &AttemptRequest| AttemptOutput {
            outcome: green("nav"),
            provider_run_id: "p".into(),
        };
        let r = orchestrate(
            &task("redesign_api"),
            &mut runner,
            &OrchestrationOptions::default(),
        );
        let s = r.summary("redesign_api");
        assert!(!s.mechanical && !s.escalated && s.green);
        assert_eq!(s.expensive_attempts, 1);
        let m = EconomicMetrics::aggregate(std::iter::once(&s));
        assert_eq!(m.escalation_rate, 0.0);
        assert_eq!(m.expensive_calls_avoided, 0);
        assert_eq!(m.cheap_hit_runs, 0); // final tier expensive
    }

    #[test]
    fn mixed_batch_rates() {
        // three runs: cheap-green, escalated-green, cheap-green
        let mk = |esc: bool, ft: ModelTier, mech: bool| RunSummary {
            op: "x".into(),
            mechanical: mech,
            green: true,
            escalated: esc,
            exhausted: false,
            attempts: if esc { 2 } else { 1 },
            cheap_attempts: 1,
            expensive_attempts: if esc { 1 } else { 0 },
            final_tier: ft,
        };
        let s = vec![
            mk(false, ModelTier::Cheap, true),
            mk(true, ModelTier::Expensive, true),
            mk(false, ModelTier::Cheap, false),
        ];
        let m = EconomicMetrics::aggregate(&s);
        assert_eq!(m.runs, 3);
        assert_eq!(m.cheap_hit_runs, 2);
        assert!((m.cheap_hit_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!((m.escalation_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!((m.mechanical_share - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(m.expensive_calls_avoided, 2);
        assert_eq!(m.total_expensive_attempts, 1);
    }
}
