//! T5 — splitter feedback telemetry: turning the GoalGraph correction loop into
//! a small, JSON-able, *regressible* ledger.
//!
//! # Why this exists
//!  — the whole GoalGraph
//! outer loop is only worth building if we can *measure* how much the mechanical
//! symbol graph corrects the model's guessed dependencies. T5 is that meter. It
//! answers, per run and aggregated across runs:
//!
//!   * how many dependencies did the model *miss* that the graph proved
//!     (`added_by_symbolgraph` → the model's **under**-specification / 漏报)?
//!   * how many did the model *over*-declare that the graph proved redundant
//!     (`dropped_redundant` → the model's **over**-specification / 误报)?
//!   * how often did the two independent sources already agree
//!     (`agreement_rate` → how good the raw guesses were)?
//!   * how much did the machine decide by itself vs. escalate to a human
//!     (`auto_rate` / `review_rate` → the automation win)?
//!   * at land time, how big was the minimal re-verify set (`reverify_size`)?
//!
//! # Never invented (§7)
//! Every number here is a *deterministic projection* of data the mechanical
//! layers already emitted — [`SplitDetail`] (T1) and [`ReverifyDetail`] (T2).
//! This module counts; it never re-derives a verdict, never guesses a rate from
//! a partial signal, and never fabricates a field the source did not carry. Feed
//! it the same details and it yields the same ledger, byte for byte.
//!
//! # Layering
//! Lives in `shuttle-protocol` next to [`metrics`](crate::metrics) (B2's economic
//! ledger) because it is a pure wire/aggregation contract with no I/O and no
//! dependency on the CLI or the symbol graph — the same decoupling that lets
//! [`crate::metrics::RunSummary`] be persisted and replayed later.

use serde::{Deserialize, Serialize};

use crate::tool_outcome::{
    EdgeSource, EdgeVerdict, SplitDetail, ToolOutcome, TypedDetail, VerifyDetail,
};

/// Schema version stamped into persisted telemetry so a later aggregator can
/// reject or migrate an incompatible ledger.
pub const TELEMETRY_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Per-run split feedback — the deterministic projection of one SplitDetail.
// ---------------------------------------------------------------------------

/// The feedback distilled from a single `shuttle split --correct` adjudication.
///
/// Built solely from a [`SplitDetail`] via [`SplitFeedback::from_detail`]; every
/// field is a count of edges the corrector already classified. `intent_edges`
/// and `mechanical_edges` are provenance totals (an `Agree` edge is *both*, so
/// they overlap on purpose); the five verdict tallies partition `total_edges`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitFeedback {
    /// Total adjudicated edges (`agree + add + drop + conflict + unknown`).
    pub total_edges: usize,
    /// Edges the planner proposed (provenance `intent` or `both`). Denominator
    /// for the "how good were the raw guesses" rates.
    pub intent_edges: usize,
    /// Edges the symbol graph corroborated or proved (provenance `symbolgraph`
    /// or `both`).
    pub mechanical_edges: usize,
    /// Both sources independently agreed on the edge (`Agree`).
    pub agree: usize,
    /// Symbol graph proved a dependency the model omitted (`Add`) — the model's
    /// under-specification (漏报).
    pub added_by_symbolgraph: usize,
    /// Author edge the graph proved transitively redundant (`Drop`) — the
    /// model's over-specification (误报).
    pub dropped_redundant: usize,
    /// Mechanical direction contradicts the intent direction (`Conflict`).
    pub conflict: usize,
    /// Neither provable nor disprovable (`Unknown`) — escalated, never guessed.
    pub unknown: usize,
    /// Edges surfaced to the human/model review gate (`conflict + unknown`).
    /// Mirrors [`SplitDetail::review_surface`].
    pub review_surface: usize,
    /// Edges the machine decided on its own (`agree + add + drop`).
    pub auto_decided: usize,
}

impl SplitFeedback {
    /// Project a [`SplitDetail`] into per-run feedback. Pure counting — the
    /// corrector already assigned every verdict and provenance (§7: we never
    /// re-adjudicate here).
    pub fn from_detail(detail: &SplitDetail) -> Self {
        let mut f = SplitFeedback {
            total_edges: detail.edges.len(),
            ..Default::default()
        };
        for e in &detail.edges {
            match e.source {
                EdgeSource::Intent => f.intent_edges += 1,
                EdgeSource::Symbolgraph => f.mechanical_edges += 1,
                EdgeSource::Both => {
                    f.intent_edges += 1;
                    f.mechanical_edges += 1;
                }
            }
            match e.verdict {
                EdgeVerdict::Agree => f.agree += 1,
                EdgeVerdict::Add => f.added_by_symbolgraph += 1,
                EdgeVerdict::Drop => f.dropped_redundant += 1,
                EdgeVerdict::Conflict => f.conflict += 1,
                EdgeVerdict::Unknown => f.unknown += 1,
            }
        }
        f.review_surface = f.conflict + f.unknown;
        f.auto_decided = f.agree + f.added_by_symbolgraph + f.dropped_redundant;
        f
    }

    /// Project the [`SplitDetail`] carried inside a [`ToolOutcome`]. Returns
    /// `None` when the envelope is not a split result (e.g. an `Error` outcome
    /// produced before adjudication) — the caller then has nothing to feed back.
    pub fn from_outcome(outcome: &ToolOutcome) -> Option<Self> {
        match &outcome.detail {
            TypedDetail::Split(d) => Some(Self::from_detail(d)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-run ledger — aggregate feedback into the rates that steer splitting.
// ---------------------------------------------------------------------------

/// The aggregated splitter-quality ledger over a batch of runs. These are the
/// rates the outer loop feeds back into the next round of goal decomposition
/// (`§4.3-3`): a high `add_rate` means the planner keeps missing real
/// dependencies (tighten its prompt toward completeness); a high `drop_rate`
/// means it keeps over-linking (loosen it); a high `agreement_rate` means the
/// guesses are already good; `auto_rate` is the headline automation win.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SplitTelemetryLedger {
    /// Number of split runs aggregated.
    pub runs: usize,
    /// Sum of all adjudicated edges across runs.
    pub total_edges: usize,
    /// Sum of intent-provenance edges (the guessing denominator).
    pub total_intent_edges: usize,
    /// Sum of `Agree` edges.
    pub total_agree: usize,
    /// Sum of `Add` edges (machine-added, model-missed).
    pub total_added: usize,
    /// Sum of `Drop` edges (redundant author edges).
    pub total_dropped: usize,
    /// Sum of `Conflict` edges.
    pub total_conflict: usize,
    /// Sum of `Unknown` edges.
    pub total_unknown: usize,
    /// Sum of review-surface edges (`conflict + unknown`).
    pub total_review_surface: usize,
    /// Sum of machine-decided edges (`agree + add + drop`).
    pub total_auto_decided: usize,
    /// `total_agree / total_intent_edges` — share of the planner's own edges the
    /// graph independently corroborated. Higher = better raw guesses.
    pub agreement_rate: f64,
    /// `total_added / total_intent_edges` — dependencies the model missed, per
    /// edge it did propose. Higher = worse under-specification (漏报).
    pub add_rate: f64,
    /// `total_dropped / total_intent_edges` — redundant edges the model
    /// over-declared, per edge proposed. Higher = worse over-specification (误报).
    pub drop_rate: f64,
    /// `total_review_surface / total_edges` — share of edges that had to escalate
    /// to a human/model. Lower = more of the DAG settled mechanically.
    pub review_rate: f64,
    /// `total_auto_decided / total_edges` — share the machine decided by itself.
    /// The headline automation win; `review_rate + auto_rate == 1` when
    /// `total_edges > 0`.
    pub auto_rate: f64,
}

impl SplitTelemetryLedger {
    /// Fold a batch of per-run [`SplitFeedback`] into the steering ledger. Rates
    /// stay `0.0` when their denominator is zero (no divide-by-zero, no NaN).
    pub fn aggregate<'a, I>(feedback: I) -> Self
    where
        I: IntoIterator<Item = &'a SplitFeedback>,
    {
        let mut l = SplitTelemetryLedger::default();
        for f in feedback {
            l.runs += 1;
            l.total_edges += f.total_edges;
            l.total_intent_edges += f.intent_edges;
            l.total_agree += f.agree;
            l.total_added += f.added_by_symbolgraph;
            l.total_dropped += f.dropped_redundant;
            l.total_conflict += f.conflict;
            l.total_unknown += f.unknown;
            l.total_review_surface += f.review_surface;
            l.total_auto_decided += f.auto_decided;
        }
        if l.total_intent_edges > 0 {
            let d = l.total_intent_edges as f64;
            l.agreement_rate = l.total_agree as f64 / d;
            l.add_rate = l.total_added as f64 / d;
            l.drop_rate = l.total_dropped as f64 / d;
        }
        if l.total_edges > 0 {
            let d = l.total_edges as f64;
            l.review_rate = l.total_review_surface as f64 / d;
            l.auto_rate = l.total_auto_decided as f64 / d;
        }
        l
    }
}

// ---------------------------------------------------------------------------
// End-to-end goal telemetry — the §6.5 record for one GoalGraph outer loop.
// ---------------------------------------------------------------------------

/// The full end-to-end telemetry for one `shuttle goal` outer loop, matching the
/// schema frozen in . It stitches
/// the mechanical correction feedback (from T1) together with the land-time
/// re-verify size (from T2) and the execution economics (wall time, tokens,
/// tool calls, patches applied, acceptance) so a single JSON line captures the
/// whole loop for regression and prompt-strategy feedback.
///
/// Build it with [`GoalTelemetry::new`] from a [`SplitFeedback`] plus the
/// runtime facts; `reverify_size` comes straight off a [`ReverifyDetail`] via
/// [`reverify_size_of`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GoalTelemetry {
    /// Schema version ([`TELEMETRY_VERSION`]).
    pub version: u32,
    /// Wall-clock milliseconds for the whole outer loop.
    pub wall_ms: u64,
    /// Model tokens consumed (planner side).
    pub tokens: u64,
    /// Tool calls made across the loop.
    pub tool_calls: u64,
    /// Edges the planner proposed (intent provenance).
    pub intent_edges: usize,
    /// Edges the symbol graph corroborated or proved.
    pub mechanical_edges: usize,
    /// Dependencies the model missed, machine-added (`Add`).
    pub added_by_symbolgraph: usize,
    /// Redundant author edges the machine dropped (`Drop`).
    pub dropped_redundant: usize,
    /// Edges escalated to the review gate (`conflict + unknown`).
    pub review_surface: usize,
    /// Minimal land-time re-verify set size (T2). `0` when no land occurred.
    pub reverify_size: usize,
    /// Patches actually applied during execution.
    pub patches: usize,
    /// Fraction of proposed patches accepted (`[0.0, 1.0]`); `0.0` when none
    /// were proposed.
    pub accept_rate: f64,
}

impl GoalTelemetry {
    /// Assemble the end-to-end record from the split feedback (T1) and the
    /// runtime facts. `reverify_size` is the T2 land-time set size (0 if the
    /// loop did not reach land); `accepted`/`proposed` yield `accept_rate`
    /// (guarded against divide-by-zero).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        feedback: &SplitFeedback,
        wall_ms: u64,
        tokens: u64,
        tool_calls: u64,
        reverify_size: usize,
        patches: usize,
        proposed: usize,
        accepted: usize,
    ) -> Self {
        let accept_rate = if proposed > 0 {
            accepted as f64 / proposed as f64
        } else {
            0.0
        };
        GoalTelemetry {
            version: TELEMETRY_VERSION,
            wall_ms,
            tokens,
            tool_calls,
            intent_edges: feedback.intent_edges,
            mechanical_edges: feedback.mechanical_edges,
            added_by_symbolgraph: feedback.added_by_symbolgraph,
            dropped_redundant: feedback.dropped_redundant,
            review_surface: feedback.review_surface,
            reverify_size,
            patches,
            accept_rate,
        }
    }
}

/// Pull the land-time re-verify set size out of a T2 [`ToolOutcome`]. Returns
/// `None` when the envelope is not a reverify result, so a caller can tell
/// "no land happened" apart from "land produced an empty set".
pub fn reverify_size_of(outcome: &ToolOutcome) -> Option<usize> {
    match &outcome.detail {
        TypedDetail::Reverify(d) => Some(d.reverify_size),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Execution-verify telemetry — the §6-5 projection of one VerifyDetail (G5).
// ---------------------------------------------------------------------------

/// Per-run execution-verification telemetry, the deterministic projection of a
/// single `shuttle verify --run` [`VerifyDetail`] (G5, axis-2). It answers, for one
/// land-time re-verify pass:
///
///   * how big was the paper re-verify set T2 asserted (`reverify_size`)?
///   * how many units did we actually build + run (`ran`)?
///   * how many of those passed (`passed`) and how many failed (`failed`)?
///   * how many were skipped, and how many of those skips were *blocking*
///     (`skipped` / `blocking_skips` — a blocking skip is why we could not
///     claim green)?
///   * was the whole pass only `partial` (an UNKNOWN symbol-graph edge) or
///     `degraded` (a stale index)?
///
/// # Never invented (§7)
/// Every number is a straight count off the [`VerifyDetail`] the cage already
/// produced — the runner ran the units and the corrector classified the skips;
/// this module only tallies. `pass_rate` is `passed / ran` (not `passed /
/// reverify_size`): it measures *what actually ran*, so a pass_rate of 1.0 next
/// to a non-empty `blocking_skips` still honestly reads "everything we ran
/// passed, but we could not run all of it" — the telemetry never launders an
/// unrun unit into a pass, mirroring [`verify_outcome`](crate::tool_outcome::verify_outcome).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VerifyTelemetry {
    /// Schema version ([`TELEMETRY_VERSION`]).
    pub version: u32,
    /// The paper re-verify cardinality T2 asserted (before running anything).
    pub reverify_size: usize,
    /// Units actually built + run.
    pub ran: usize,
    /// Ran units that passed (build + test).
    pub passed: usize,
    /// Ran units that failed (build or test), regardless of stage.
    pub failed: usize,
    /// Units skipped for any reason (`no-test` benign + blocking).
    pub skipped: usize,
    /// Skips that forced `needs-run` (everything but `no-test`) — the reason a
    /// pass could not be claimed green.
    pub blocking_skips: usize,
    /// True when an UNKNOWN symbol-graph edge capped the pass at Green_low.
    pub partial: bool,
    /// True when the index was stale vs head (re-verify set may be incomplete).
    pub degraded: bool,
    /// `passed / ran` — share of *what ran* that passed; `0.0` when nothing ran
    /// (no divide-by-zero, no NaN). Deliberately not divided by `reverify_size`
    /// so it never implies unrun units passed.
    pub pass_rate: f64,
}

impl VerifyTelemetry {
    /// Project a [`VerifyDetail`] into per-run telemetry. Pure counting — the
    /// cage already ran the units and classified the skips (§7: we never
    /// re-derive a verdict here).
    pub fn from_detail(detail: &VerifyDetail) -> Self {
        let failed = detail.failed.len();
        let skipped = detail.skipped.len();
        let blocking_skips = detail.skipped.iter().filter(|s| s.is_blocking()).count();
        let pass_rate = if detail.ran > 0 {
            detail.passed as f64 / detail.ran as f64
        } else {
            0.0
        };
        VerifyTelemetry {
            version: TELEMETRY_VERSION,
            reverify_size: detail.reverify_size,
            ran: detail.ran,
            passed: detail.passed,
            failed,
            skipped,
            blocking_skips,
            partial: detail.partial,
            degraded: detail.degraded,
            pass_rate,
        }
    }

    /// Project the [`VerifyDetail`] carried inside a [`ToolOutcome`]. Returns
    /// `None` when the envelope is not a verify result (e.g. an `Error` outcome
    /// produced before anything ran) — the caller then has nothing to emit.
    pub fn from_outcome(outcome: &ToolOutcome) -> Option<Self> {
        match &outcome.detail {
            TypedDetail::Verify(d) => Some(Self::from_detail(d)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_outcome::{
        reason, verify_kind, FailedUnit, OutcomeConfidence, ReverifyDetail, ReverifyUnit,
        SkippedUnit, SplitDetail, SplitEdge, SplitNode, ToolOutcome, TypedDetail, VerifyDetail,
    };

    fn edge(from: &str, to: &str, verdict: EdgeVerdict, source: EdgeSource) -> SplitEdge {
        SplitEdge {
            from: from.into(),
            to: to.into(),
            verdict,
            confidence: OutcomeConfidence::High,
            source,
            evidence: String::new(),
        }
    }

    /// A detail with one edge of each verdict, provenance matching how the
    /// corrector actually assigns it (agree=both, add=symbolgraph, drop=intent,
    /// conflict=intent, unknown=intent).
    fn mixed_detail() -> SplitDetail {
        let edges = vec![
            edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both),
            edge("c", "d", EdgeVerdict::Add, EdgeSource::Symbolgraph),
            edge("e", "f", EdgeVerdict::Drop, EdgeSource::Intent),
            edge("g", "h", EdgeVerdict::Conflict, EdgeSource::Intent),
            edge("i", "j", EdgeVerdict::Unknown, EdgeSource::Intent),
        ];
        SplitDetail {
            goal: "g".into(),
            nodes: vec![SplitNode {
                id: "a".into(),
                intent: String::new(),
                touched_files: vec![],
            }],
            edges,
            added: 1,
            dropped: 1,
            review_surface: 2,
            review_required: vec![3, 4],
        }
    }

    #[test]
    fn feedback_counts_every_verdict_and_provenance() {
        let f = SplitFeedback::from_detail(&mixed_detail());
        assert_eq!(f.total_edges, 5);
        // agree(both) + drop(intent) + conflict(intent) + unknown(intent) = 4
        assert_eq!(f.intent_edges, 4);
        // agree(both) + add(symbolgraph) = 2
        assert_eq!(f.mechanical_edges, 2);
        assert_eq!(f.agree, 1);
        assert_eq!(f.added_by_symbolgraph, 1);
        assert_eq!(f.dropped_redundant, 1);
        assert_eq!(f.conflict, 1);
        assert_eq!(f.unknown, 1);
        assert_eq!(f.review_surface, 2);
        assert_eq!(f.auto_decided, 3);
    }

    #[test]
    fn feedback_review_surface_matches_detail() {
        let d = mixed_detail();
        let f = SplitFeedback::from_detail(&d);
        assert_eq!(f.review_surface, d.review_surface);
    }

    #[test]
    fn feedback_from_outcome_only_for_split() {
        let split = ToolOutcome::green("split", TypedDetail::Split(mixed_detail()));
        assert!(SplitFeedback::from_outcome(&split).is_some());

        let err = ToolOutcome::error("split", reason::INVALID_INPUT, "bad plan");
        assert!(SplitFeedback::from_outcome(&err).is_none());
    }

    #[test]
    fn empty_detail_yields_zeroed_feedback() {
        let d = SplitDetail {
            goal: String::new(),
            nodes: vec![],
            edges: vec![],
            added: 0,
            dropped: 0,
            review_surface: 0,
            review_required: vec![],
        };
        let f = SplitFeedback::from_detail(&d);
        assert_eq!(f, SplitFeedback::default());
    }

    #[test]
    fn ledger_aggregates_rates() {
        // Two runs of the mixed detail: intent=8, edges=10, agree=2, add=2,
        // drop=2, review=4, auto=6.
        let f = SplitFeedback::from_detail(&mixed_detail());
        let batch = vec![f.clone(), f];
        let l = SplitTelemetryLedger::aggregate(&batch);
        assert_eq!(l.runs, 2);
        assert_eq!(l.total_edges, 10);
        assert_eq!(l.total_intent_edges, 8);
        assert_eq!(l.total_agree, 2);
        assert_eq!(l.total_added, 2);
        assert_eq!(l.total_dropped, 2);
        assert_eq!(l.total_review_surface, 4);
        assert_eq!(l.total_auto_decided, 6);
        assert!((l.agreement_rate - 2.0 / 8.0).abs() < 1e-9);
        assert!((l.add_rate - 2.0 / 8.0).abs() < 1e-9);
        assert!((l.drop_rate - 2.0 / 8.0).abs() < 1e-9);
        assert!((l.review_rate - 4.0 / 10.0).abs() < 1e-9);
        assert!((l.auto_rate - 6.0 / 10.0).abs() < 1e-9);
        // auto + review partition every edge.
        assert!((l.auto_rate + l.review_rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_ledger_has_no_nan_rates() {
        let l = SplitTelemetryLedger::aggregate(std::iter::empty());
        assert_eq!(l.runs, 0);
        assert_eq!(l.agreement_rate, 0.0);
        assert_eq!(l.add_rate, 0.0);
        assert_eq!(l.review_rate, 0.0);
        assert!(!l.auto_rate.is_nan());
    }

    #[test]
    fn goal_telemetry_stitches_split_and_reverify() {
        let f = SplitFeedback::from_detail(&mixed_detail());
        let t = GoalTelemetry::new(&f, 1234, 5000, 12, 7, 4, 5, 4);
        assert_eq!(t.version, TELEMETRY_VERSION);
        assert_eq!(t.wall_ms, 1234);
        assert_eq!(t.intent_edges, 4);
        assert_eq!(t.mechanical_edges, 2);
        assert_eq!(t.added_by_symbolgraph, 1);
        assert_eq!(t.dropped_redundant, 1);
        assert_eq!(t.review_surface, 2);
        assert_eq!(t.reverify_size, 7);
        assert_eq!(t.patches, 4);
        assert!((t.accept_rate - 4.0 / 5.0).abs() < 1e-9);
    }

    #[test]
    fn goal_telemetry_accept_rate_guards_zero() {
        let f = SplitFeedback::default();
        let t = GoalTelemetry::new(&f, 0, 0, 0, 0, 0, 0, 0);
        assert_eq!(t.accept_rate, 0.0);
        assert!(!t.accept_rate.is_nan());
    }

    #[test]
    fn reverify_size_of_reads_reverify_only() {
        let rv = ReverifyDetail {
            base_oid: "b".into(),
            head_oid: "h".into(),
            index_base_oid: "h".into(),
            changed_files: vec!["x.rs".into()],
            reverify: vec![ReverifyUnit {
                path: "x.rs".into(),
                reason: "changed".into(),
                confidence: OutcomeConfidence::High,
                distance: 0,
            }],
            granularity: "file".into(),
            degraded: false,
            reverify_size: 1,
        };
        let out = ToolOutcome::green("impact", TypedDetail::Reverify(rv));
        assert_eq!(reverify_size_of(&out), Some(1));

        let split = ToolOutcome::green("split", TypedDetail::Split(mixed_detail()));
        assert_eq!(reverify_size_of(&split), None);
    }

    #[test]
    fn telemetry_roundtrips_through_json() {
        let f = SplitFeedback::from_detail(&mixed_detail());
        let t = GoalTelemetry::new(&f, 10, 20, 3, 2, 1, 2, 1);
        let s = serde_json::to_string(&t).unwrap();
        let back: GoalTelemetry = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);

        let l = SplitTelemetryLedger::aggregate(std::iter::once(&f));
        let ls = serde_json::to_string(&l).unwrap();
        let lback: SplitTelemetryLedger = serde_json::from_str(&ls).unwrap();
        assert_eq!(l, lback);
    }

    // ---- VerifyTelemetry projection (G5 §6-5) ----

    fn verify_detail() -> VerifyDetail {
        // ran=3, passed=2, one test failure, one blocking skip + one benign.
        VerifyDetail {
            base_oid: "base".into(),
            head_oid: "head".into(),
            reverify_size: 5,
            ran: 3,
            passed: 2,
            failed: vec![FailedUnit {
                unit: "a.rs".into(),
                kind: verify_kind::TEST.into(),
                evidence: "assert failed".into(),
            }],
            skipped: vec![
                SkippedUnit {
                    unit: "b.rs".into(),
                    reason: crate::tool_outcome::skip_reason::NOT_ALLOWED.into(),
                },
                SkippedUnit {
                    unit: "c.rs".into(),
                    reason: crate::tool_outcome::skip_reason::NO_TEST.into(),
                },
            ],
            partial: false,
            degraded: false,
        }
    }

    #[test]
    fn verify_telemetry_counts_every_field() {
        let t = VerifyTelemetry::from_detail(&verify_detail());
        assert_eq!(t.version, TELEMETRY_VERSION);
        assert_eq!(t.reverify_size, 5);
        assert_eq!(t.ran, 3);
        assert_eq!(t.passed, 2);
        assert_eq!(t.failed, 1);
        assert_eq!(t.skipped, 2);
        // only the NOT_ALLOWED skip is blocking; NO_TEST is benign.
        assert_eq!(t.blocking_skips, 1);
        assert!(!t.partial);
        assert!(!t.degraded);
        assert!((t.pass_rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn verify_telemetry_pass_rate_measures_what_ran_not_the_paper_set() {
        // Everything that ran passed, but a blocking skip means the set was not
        // fully run — pass_rate is 1.0 (of what ran) yet blocking_skips > 0.
        let mut d = verify_detail();
        d.failed = vec![];
        d.ran = 2;
        d.passed = 2;
        let t = VerifyTelemetry::from_detail(&d);
        assert_eq!(t.failed, 0);
        assert_eq!(t.blocking_skips, 1);
        assert!((t.pass_rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn verify_telemetry_zero_ran_has_no_nan_rate() {
        let mut d = verify_detail();
        d.ran = 0;
        d.passed = 0;
        let t = VerifyTelemetry::from_detail(&d);
        assert_eq!(t.pass_rate, 0.0);
        assert!(!t.pass_rate.is_nan());
    }

    #[test]
    fn verify_telemetry_carries_partial_and_degraded_flags() {
        let mut d = verify_detail();
        d.partial = true;
        d.degraded = true;
        let t = VerifyTelemetry::from_detail(&d);
        assert!(t.partial);
        assert!(t.degraded);
    }

    #[test]
    fn verify_telemetry_from_outcome_only_for_verify() {
        let out = ToolOutcome::green("verify", TypedDetail::Verify(verify_detail()));
        assert!(VerifyTelemetry::from_outcome(&out).is_some());

        let err = ToolOutcome::error("verify", reason::INVALID_INPUT, "bad base");
        assert!(VerifyTelemetry::from_outcome(&err).is_none());

        let split = ToolOutcome::green("split", TypedDetail::Split(mixed_detail()));
        assert!(VerifyTelemetry::from_outcome(&split).is_none());
    }

    #[test]
    fn verify_telemetry_roundtrips_through_json() {
        let t = VerifyTelemetry::from_detail(&verify_detail());
        let s = serde_json::to_string(&t).unwrap();
        let back: VerifyTelemetry = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }
}
