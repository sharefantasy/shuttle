//! Parallel scheduling over the *corrected* GoalGraph .
//!
//! # What this is
//! The GoalGraph outer loop ([`crate::goal`]) drives tasks to `Landed` in a
//! single **topological** order. This module answers a different question: of
//! the tasks that are *not* ordered relative to each other by a proven
//! dependency edge, which ones may run **concurrently** because they provably
//! touch disjoint work, and which must still be serialized because we cannot
//! prove they are independent?
//!
//! The output is a sequence of **concurrency batches** (an antichain
//! partition), not a linear order: `batch 0 = [t0, t3]`, `batch 1 = [t1]`, …
//! Every task in a batch is safe to launch in parallel; batch `k+1` starts
//! only after batch `k` lands.
//!
//! # The provable-independence bar (§7 「绝不瞎猜」)
//! Two tasks are put in the same batch **only** when *both* hold:
//!   1. neither is a (transitive) prerequisite of the other in the corrected
//!      AGREE/ADD edge set, and
//!   2. a [`ConcurrencyOracle`] returns `None` for the pair — i.e. it can find
//!      *no* reason they might collide.
//!
//! An oracle that is unsure MUST return `Some(reason)` (serialize), never
//! `None`. Unknown footprint ⇒ serialize + set [`ParallelSchedule::degraded`].
//! We never widen a batch on a hunch; the failure mode is "too serial", never
//! "raced".
//!
//! # Not a land权 (#11)
//! This module computes *which tasks may run together*. It never owns physical
//! paths, credentials, or the right to land anything — the runtime (`cell-run`,
//! the upstream orchestrator) does. A batch is a scheduling hint the caller is
//! free to honour or ignore.

use serde::{Deserialize, Serialize};
use shuttle_protocol::anchor::Sha256Digest;
use shuttle_protocol::{EdgeVerdict, SplitDetail};
use std::collections::{BTreeMap, BTreeSet};

use crate::split::{
    HybridAdoptionReasonCode, HybridConflict, HybridConflictKind, HybridDag, HybridEvidenceKind,
    HybridEvidenceState, HybridHardEdge, HybridTaskEdge, HybridTrustedCandidate, HybridWeakHint,
    HYBRID_DAG_POLICY_VERSION,
};

/// Stable wire version of the read-only parallel planning advice.
pub const PARALLEL_ADVICE_SCHEMA: &str = "shuttle_goal_parallel_advice/v1";
/// Stable wire version of monotonic Hybrid DAG advice. v1 remains closed and
/// unchanged; callers must opt into this separate contract explicitly.
pub const HYBRID_PARALLEL_ADVICE_SCHEMA: &str = "shuttle_goal_parallel_advice/v2";

/// Normalize a path for overlap comparison: trim, drop a single leading `./`.
fn norm(p: &str) -> String {
    let t = p.trim();
    let t = t.strip_prefix("./").unwrap_or(t);
    t.to_string()
}

/// Suffix-aware path overlap: equal after normalization, or one is a path-
/// component suffix of the other (`a/b/c.rs` vs `b/c.rs`). This is deliberately
/// *generous* about calling a pair a conflict — a false "overlap" only costs
/// concurrency, never correctness (§7: prefer too-serial).
fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    // component-boundary suffix match: long ends with "/{short}".
    long.len() > short.len()
        && long.ends_with(short.as_str())
        && long.as_bytes()[long.len() - short.len() - 1] == b'/'
}

/// A task reduced to just what the scheduler reasons over: its id and the
/// files the *corrected* graph attributed to it (never `pi`'s raw guess).
#[derive(Debug, Clone)]
pub struct BatchTask {
    pub id: String,
    pub touched: Vec<String>,
}

/// Prefix a serialize-reason carries when the cause is *unprovable* independence
/// (as opposed to a positively proven collision). Drives [`ParallelSchedule::degraded`].
pub const UNPROVABLE_PREFIX: &str = "unprovable";

/// The pluggable independence check. `conflict(a, b)` returns:
///   * `Some(reason)` — the pair MUST be serialized (a proven collision, or an
///     unprovable footprint). `reason` is machine-safe text for the receipt.
///   * `None` — the pair is *provably* safe to run concurrently.
///
/// # Contract (§7)
/// An implementor that cannot *prove* independence MUST return `Some(..)`. It is
/// a red-line violation to return `None` on a hunch. Prefix the reason with
/// [`UNPROVABLE_PREFIX`] when the cause is missing evidence rather than a proven
/// clash, so the schedule can mark itself degraded.
pub trait ConcurrencyOracle {
    fn conflict(&self, a: &BatchTask, b: &BatchTask) -> Option<String>;
}

/// The default, dependency-free oracle: two tasks conflict if they touch any
/// overlapping file. A task with *no* known touched files is conservatively
/// serialized against everything (we cannot prove its footprint is disjoint).
#[derive(Debug, Default, Clone, Copy)]
pub struct FileOverlapOracle;

impl ConcurrencyOracle for FileOverlapOracle {
    fn conflict(&self, a: &BatchTask, b: &BatchTask) -> Option<String> {
        if a.touched.is_empty() {
            return Some(format!(
                "{UNPROVABLE_PREFIX}: {} has no known touched files",
                a.id
            ));
        }
        if b.touched.is_empty() {
            return Some(format!(
                "{UNPROVABLE_PREFIX}: {} has no known touched files",
                b.id
            ));
        }
        for pa in &a.touched {
            for pb in &b.touched {
                if paths_overlap(pa, pb) {
                    return Some(format!("file-overlap: {}", norm(pa)));
                }
            }
        }
        None
    }
}

/// Fail-safe union of two oracles: the pair is safe only if *both* say safe.
/// The first oracle's reason wins (so a proven collision from a cheap check is
/// reported before a richer semantic one). Composing lets the CLI layer add a
/// `shuttle-conflict::classify`-based semantic oracle on top of file overlap without
/// this crate taking that dependency.
#[derive(Debug, Clone, Copy)]
pub struct EitherOracle<A, B> {
    pub first: A,
    pub second: B,
}

impl<A: ConcurrencyOracle, B: ConcurrencyOracle> ConcurrencyOracle for EitherOracle<A, B> {
    fn conflict(&self, a: &BatchTask, b: &BatchTask) -> Option<String> {
        if let Some(r) = self.first.conflict(a, b) {
            return Some(r);
        }
        self.second.conflict(a, b)
    }
}

/// One concurrency batch: an antichain of task ids all safe to launch together.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyBatch {
    pub index: usize,
    pub tasks: Vec<String>,
}

/// Why an otherwise-unordered pair was forced into different batches — the
/// receipt trail for a scheduling decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SerializeReason {
    pub earlier: String,
    pub later: String,
    pub reason: String,
}

/// The computed schedule: batches in execution order, the widest batch, the
/// serialize decisions, and whether any decision rested on *unprovable*
/// independence (in which case the caller knows the plan is conservative).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParallelSchedule {
    pub batches: Vec<ConcurrencyBatch>,
    pub max_width: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serialized: Vec<SerializeReason>,
    pub degraded: bool,
}

impl ParallelSchedule {
    /// Number of batches (the critical-path length in batch units).
    pub fn depth(&self) -> usize {
        self.batches.len()
    }
}

/// Provenance of the task footprint used by the v1 scheduler.
///
/// v1 deliberately has only `planner_guess`: a non-empty guessed footprint is
/// not promoted to mechanical write-scope proof merely because it produced a
/// useful schedule. The consumer must independently adopt and constrain it
/// before any effect is authorized.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FootprintSource {
    PlannerGuess,
}

/// Immutable graph inputs against which the schedule was computed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParallelAdviceGraphBasis {
    pub base_oid: String,
    pub symbol_index_digest: Sha256Digest,
    pub correction_digest: Sha256Digest,
    pub footprint_source: FootprintSource,
}

/// Stable advice batch. This intentionally uses `task_ids`, rather than
/// exposing the scheduler's internal `tasks` field as a second wire contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParallelAdviceBatch {
    pub index: usize,
    pub task_ids: Vec<String>,
}

/// `shuttle_goal_parallel_advice/v1` — a digest-bound, read-only projection of a
/// [`ParallelSchedule`].
///
/// This object is planning advice, not dispatch authority. It contains no
/// workspace, claim, provider, approval, or execution fields. A downstream
/// Planning owner must explicitly adopt it and independently validate write
/// scopes and freshness before any effect can start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParallelAdvice {
    pub schema_version: String,
    pub source_intent_ref: String,
    pub source_intent_digest: Sha256Digest,
    pub graph_basis: ParallelAdviceGraphBasis,
    pub batches: Vec<ParallelAdviceBatch>,
    pub serialized: Vec<SerializeReason>,
    pub degraded: bool,
    pub max_width: usize,
}

/// Provenance of a v2 task footprint. `PlannerGuess` is retained as an
/// explicit compatibility state and never upgraded implicitly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridFootprintSource {
    PlannerGuess,
    PlannerDeclaredValidated,
    ObservedDiff,
}

impl HybridFootprintSource {
    pub fn is_exact(self) -> bool {
        !matches!(self, Self::PlannerGuess)
    }
}

/// Scheduling footprint and its evidence state. `write_paths` remains the
/// final collision boundary; symbol precision may explain independence but
/// never grants path authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridTaskFootprint {
    pub task_id: String,
    pub source: HybridFootprintSource,
    pub state: HybridEvidenceState,
    #[serde(default)]
    pub write_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridPlanningBasis {
    pub planning_ref: String,
    pub planning_revision: String,
    pub planning_structure_digest: Sha256Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridArtifactBinding {
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub digest: Sha256Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridRepositoryBase {
    pub repo_id: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridIndexBinding {
    pub repo_id: String,
    pub index_digest: Sha256Digest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridDegradedState {
    pub value: bool,
    pub reasons: Vec<String>,
}

/// Closed v2 advice envelope. Its digest binds policy, planning, source
/// intent, repository snapshots, indexes, footprint, merge result and
/// schedule. It deliberately carries no execution or landing authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridParallelAdvice {
    pub schema_version: String,
    pub policy_version: String,
    pub planning_basis: HybridPlanningBasis,
    pub source_intent: HybridArtifactBinding,
    pub repository_bases: Vec<HybridRepositoryBase>,
    pub symbol_footprint: HybridArtifactBinding,
    pub symbol_index_bindings: Vec<HybridIndexBinding>,
    pub model_edges: Vec<HybridTaskEdge>,
    pub trusted_addition_candidates: Vec<HybridTrustedCandidate>,
    pub weak_hints: Vec<HybridWeakHint>,
    pub conflicts: Vec<HybridConflict>,
    pub proposed_hard_edges: Vec<HybridHardEdge>,
    pub schedule: ParallelSchedule,
    pub degraded: HybridDegradedState,
}

#[derive(Debug, thiserror::Error)]
pub enum ParallelAdviceError {
    #[error("invalid parallel advice: {0}")]
    Invalid(String),
    #[error("invalid parallel advice JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl ParallelAdvice {
    /// Project the existing scheduler result into the stable v1 wire envelope.
    pub fn from_schedule(
        schedule: &ParallelSchedule,
        source_intent_ref: String,
        source_intent_digest: Sha256Digest,
        graph_basis: ParallelAdviceGraphBasis,
    ) -> Result<Self, ParallelAdviceError> {
        let value = Self {
            schema_version: PARALLEL_ADVICE_SCHEMA.to_string(),
            source_intent_ref,
            source_intent_digest,
            graph_basis,
            batches: schedule
                .batches
                .iter()
                .map(|batch| ParallelAdviceBatch {
                    index: batch.index,
                    task_ids: batch.tasks.clone(),
                })
                .collect(),
            serialized: schedule.serialized.clone(),
            degraded: schedule.degraded,
            max_width: schedule.max_width,
        };
        value.validate()?;
        Ok(value)
    }

    /// Parse and validate an untrusted v1 envelope. Unknown fields are rejected
    /// by serde at every advice-owned object boundary.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParallelAdviceError> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(ParallelAdviceError::Invalid(
                "payload exceeds the 4MiB input limit".into(),
            ));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    /// Fail-closed semantic validation beyond serde's shape/type checks.
    pub fn validate(&self) -> Result<(), ParallelAdviceError> {
        if self.schema_version != PARALLEL_ADVICE_SCHEMA {
            return Err(ParallelAdviceError::Invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        validate_artifact_binding(&self.source_intent_ref, &self.source_intent_digest)?;
        validate_base_oid(&self.graph_basis.base_oid)?;

        let mut task_batch = BTreeMap::new();
        for (position, batch) in self.batches.iter().enumerate() {
            if batch.index != position {
                return Err(ParallelAdviceError::Invalid(
                    "batch indexes must be contiguous and ordered from zero".into(),
                ));
            }
            if batch.task_ids.is_empty() {
                return Err(ParallelAdviceError::Invalid(format!(
                    "batches[{position}].task_ids must not be empty"
                )));
            }
            for task_id in &batch.task_ids {
                validate_text_id("task_id", task_id)?;
                if task_batch.insert(task_id.as_str(), position).is_some() {
                    return Err(ParallelAdviceError::Invalid(format!(
                        "task_id must occur exactly once: {task_id}"
                    )));
                }
            }
        }

        let actual_width = self
            .batches
            .iter()
            .map(|batch| batch.task_ids.len())
            .max()
            .unwrap_or(0);
        if self.max_width != actual_width {
            return Err(ParallelAdviceError::Invalid(format!(
                "max_width {} does not match batch width {actual_width}",
                self.max_width
            )));
        }

        let mut reasons = BTreeSet::new();
        for reason in &self.serialized {
            validate_text_id("serialized.earlier", &reason.earlier)?;
            validate_text_id("serialized.later", &reason.later)?;
            if reason.reason.trim().is_empty() || reason.reason.len() > 4096 {
                return Err(ParallelAdviceError::Invalid(
                    "serialized.reason must be non-empty and at most 4096 bytes".into(),
                ));
            }
            let (Some(earlier_batch), Some(later_batch)) = (
                task_batch.get(reason.earlier.as_str()),
                task_batch.get(reason.later.as_str()),
            ) else {
                return Err(ParallelAdviceError::Invalid(
                    "serialized task ids must exist in batches".into(),
                ));
            };
            if earlier_batch >= later_batch {
                return Err(ParallelAdviceError::Invalid(format!(
                    "serialized pair must point from an earlier batch: {} -> {}",
                    reason.earlier, reason.later
                )));
            }
            if !reasons.insert((
                reason.earlier.as_str(),
                reason.later.as_str(),
                reason.reason.as_str(),
            )) {
                return Err(ParallelAdviceError::Invalid(
                    "serialized reasons must be unique".into(),
                ));
            }
        }
        let expected_degraded = self
            .serialized
            .iter()
            .any(|reason| reason.reason.starts_with(UNPROVABLE_PREFIX));
        if self.degraded != expected_degraded {
            return Err(ParallelAdviceError::Invalid(
                "degraded must exactly reflect unprovable serialization".into(),
            ));
        }
        Ok(())
    }

    /// Digest the RFC-8785-compatible JSON shape used by this contract: object
    /// keys recursively sorted, arrays preserved in their semantic order, and
    /// no insignificant whitespace.
    pub fn canonical_digest(&self) -> Result<Sha256Digest, ParallelAdviceError> {
        canonical_digest(self)
    }
}

impl HybridParallelAdvice {
    #[allow(clippy::too_many_arguments)]
    pub fn from_hybrid_dag(
        dag: &HybridDag,
        footprints: &[HybridTaskFootprint],
        planning_basis: HybridPlanningBasis,
        source_intent: HybridArtifactBinding,
        mut repository_bases: Vec<HybridRepositoryBase>,
        symbol_footprint: HybridArtifactBinding,
        mut symbol_index_bindings: Vec<HybridIndexBinding>,
    ) -> Result<Self, ParallelAdviceError> {
        repository_bases.sort();
        symbol_index_bindings.sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
        let schedule = schedule_hybrid(dag, footprints)?;
        let mut reasons: BTreeSet<String> = dag
            .degradations
            .iter()
            .map(|degradation| {
                let tasks = if degradation.task_ids.is_empty() {
                    "all".to_string()
                } else {
                    degradation.task_ids.join(",")
                };
                format!(
                    "evidence-{:?}:{}:{}",
                    degradation.state, tasks, degradation.evidence_ref
                )
                .to_ascii_lowercase()
            })
            .collect();
        if !dag.conflicts.is_empty() {
            reasons.insert("hybrid-conflicts-require-review".to_string());
        }
        // Expand the structured adoption verdict into per-cause reason strings
        // so a consumer sees *why* adoption was withheld (cycle / conflicts /
        // degraded evidence), not just an opaque blocked bit. The umbrella
        // "automatic-adoption-blocked" is retained for back-compat consumers.
        if !dag.adoption.is_adopt() {
            reasons.insert("automatic-adoption-blocked".to_string());
            for reason in &dag.adoption.reasons {
                let code = match reason.code {
                    HybridAdoptionReasonCode::CyclicHardDag => "cyclic-hard-dag",
                    HybridAdoptionReasonCode::UnresolvedConflicts => "unresolved-conflicts",
                    HybridAdoptionReasonCode::DegradedEvidence => "degraded-evidence",
                };
                reasons.insert(format!("adoption-blocked:{code}:{}", reason.count));
            }
        }
        if schedule.degraded {
            reasons.insert("schedule-used-conservative-fallback".to_string());
        }
        let value = Self {
            schema_version: HYBRID_PARALLEL_ADVICE_SCHEMA.to_string(),
            policy_version: dag.policy_version.clone(),
            planning_basis,
            source_intent,
            repository_bases,
            symbol_footprint,
            symbol_index_bindings,
            model_edges: dag.model_edges.clone(),
            trusted_addition_candidates: dag.trusted_addition_candidates.clone(),
            weak_hints: dag.weak_hints.clone(),
            conflicts: dag.conflicts.clone(),
            proposed_hard_edges: dag.proposed_hard_edges.clone(),
            schedule,
            degraded: HybridDegradedState {
                value: !reasons.is_empty(),
                reasons: reasons.into_iter().collect(),
            },
        };
        value.validate()?;
        Ok(value)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ParallelAdviceError> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(ParallelAdviceError::Invalid(
                "payload exceeds the 4MiB input limit".into(),
            ));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ParallelAdviceError> {
        if self.schema_version != HYBRID_PARALLEL_ADVICE_SCHEMA {
            return Err(ParallelAdviceError::Invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        if self.policy_version != HYBRID_DAG_POLICY_VERSION {
            return Err(ParallelAdviceError::Invalid(format!(
                "unsupported policy_version: {}",
                self.policy_version
            )));
        }
        validate_text_id("planning_ref", &self.planning_basis.planning_ref)?;
        validate_text_id("planning_revision", &self.planning_basis.planning_revision)?;
        validate_artifact_binding_value(&self.source_intent)?;
        validate_artifact_binding_value(&self.symbol_footprint)?;

        let mut repo_ids = BTreeSet::new();
        for base in &self.repository_bases {
            validate_text_id("repository_bases.repo_id", &base.repo_id)?;
            validate_base_oid(&base.base_oid)?;
            if !repo_ids.insert(base.repo_id.as_str()) {
                return Err(ParallelAdviceError::Invalid(format!(
                    "duplicate repository base: {}",
                    base.repo_id
                )));
            }
        }
        if !self
            .repository_bases
            .windows(2)
            .all(|items| items[0] < items[1])
        {
            return Err(ParallelAdviceError::Invalid(
                "repository_bases must be strictly sorted".into(),
            ));
        }
        let mut index_repos = BTreeSet::new();
        for binding in &self.symbol_index_bindings {
            validate_text_id("symbol_index_bindings.repo_id", &binding.repo_id)?;
            if !repo_ids.contains(binding.repo_id.as_str()) {
                return Err(ParallelAdviceError::Invalid(format!(
                    "symbol index has no repository base: {}",
                    binding.repo_id
                )));
            }
            if !index_repos.insert(binding.repo_id.as_str()) {
                return Err(ParallelAdviceError::Invalid(format!(
                    "duplicate symbol index binding: {}",
                    binding.repo_id
                )));
            }
        }
        if !self
            .symbol_index_bindings
            .windows(2)
            .all(|items| items[0].repo_id < items[1].repo_id)
        {
            return Err(ParallelAdviceError::Invalid(
                "symbol_index_bindings must be strictly sorted".into(),
            ));
        }

        validate_sorted_unique("model_edges", &self.model_edges)?;
        validate_sorted_unique(
            "trusted_addition_candidates",
            &self.trusted_addition_candidates,
        )?;
        validate_sorted_unique("weak_hints", &self.weak_hints)?;
        validate_sorted_unique("conflicts", &self.conflicts)?;
        validate_sorted_unique("proposed_hard_edges", &self.proposed_hard_edges)?;
        let hard: BTreeSet<_> = self
            .proposed_hard_edges
            .iter()
            .map(|edge| &edge.edge)
            .collect();
        for edge in &self.model_edges {
            if !hard.contains(edge) {
                return Err(ParallelAdviceError::Invalid(format!(
                    "model edge missing from proposed_hard_edges: {} -> {}",
                    edge.from, edge.to
                )));
            }
        }
        let model: BTreeSet<_> = self.model_edges.iter().collect();
        let candidates: BTreeSet<_> = self
            .trusted_addition_candidates
            .iter()
            .map(|candidate| &candidate.edge)
            .collect();
        for hard_edge in &self.proposed_hard_edges {
            let in_model = model.contains(&hard_edge.edge);
            let in_candidates = candidates.contains(&hard_edge.edge);
            let valid_provenance = match hard_edge.provenance {
                crate::split::HybridEdgeProvenance::Model => {
                    in_model
                        && !in_candidates
                        && hard_edge.kinds.is_empty()
                        && hard_edge.evidence_refs.is_empty()
                }
                crate::split::HybridEdgeProvenance::TrustedMechanical => {
                    !in_model
                        && in_candidates
                        && !hard_edge.kinds.is_empty()
                        && !hard_edge.evidence_refs.is_empty()
                }
                crate::split::HybridEdgeProvenance::Both => {
                    in_model
                        && in_candidates
                        && !hard_edge.kinds.is_empty()
                        && !hard_edge.evidence_refs.is_empty()
                }
            };
            if !valid_provenance {
                return Err(ParallelAdviceError::Invalid(format!(
                    "hard edge provenance is inconsistent: {} -> {}",
                    hard_edge.edge.from, hard_edge.edge.to
                )));
            }
        }
        for candidate in &self.trusted_addition_candidates {
            if candidate.kinds.is_empty()
                || candidate.evidence_refs.is_empty()
                || candidate
                    .kinds
                    .iter()
                    .any(|kind| !kind.is_trusted_directed())
            {
                return Err(ParallelAdviceError::Invalid(
                    "trusted addition candidates require trusted kinds and provenance".into(),
                ));
            }
        }
        if self.proposed_hard_edges.iter().any(|hard| {
            hard.kinds.iter().any(|kind| {
                matches!(
                    kind,
                    HybridEvidenceKind::NameMatch | HybridEvidenceKind::CoChange
                )
            })
        }) {
            return Err(ParallelAdviceError::Invalid(
                "weak evidence kind must not enter proposed_hard_edges".into(),
            ));
        }
        if self.weak_hints.iter().any(|hint| {
            !matches!(
                hint.kind,
                HybridEvidenceKind::NameMatch | HybridEvidenceKind::CoChange
            )
        }) {
            return Err(ParallelAdviceError::Invalid(
                "weak_hints must contain only weak evidence kinds".into(),
            ));
        }
        if self.schedule.max_width
            != self
                .schedule
                .batches
                .iter()
                .map(|batch| batch.tasks.len())
                .max()
                .unwrap_or(0)
        {
            return Err(ParallelAdviceError::Invalid(
                "schedule.max_width does not match batch width".into(),
            ));
        }
        let mut batch_of = BTreeMap::new();
        for (position, batch) in self.schedule.batches.iter().enumerate() {
            if batch.index != position || batch.tasks.is_empty() {
                return Err(ParallelAdviceError::Invalid(
                    "schedule batches must be non-empty and contiguously indexed".into(),
                ));
            }
            for task in &batch.tasks {
                validate_text_id("schedule.task", task)?;
                if batch_of.insert(task.as_str(), position).is_some() {
                    return Err(ParallelAdviceError::Invalid(format!(
                        "schedule task must occur exactly once: {task}"
                    )));
                }
            }
        }
        for hard in &self.proposed_hard_edges {
            let (Some(from_batch), Some(to_batch)) = (
                batch_of.get(hard.edge.from.as_str()),
                batch_of.get(hard.edge.to.as_str()),
            ) else {
                return Err(ParallelAdviceError::Invalid(
                    "hard edge task is absent from schedule".into(),
                ));
            };
            if from_batch >= to_batch {
                return Err(ParallelAdviceError::Invalid(format!(
                    "schedule violates hard edge: {} -> {}",
                    hard.edge.from, hard.edge.to
                )));
            }
        }
        if self.degraded.value != !self.degraded.reasons.is_empty() {
            return Err(ParallelAdviceError::Invalid(
                "degraded.value must exactly reflect degraded.reasons".into(),
            ));
        }
        if !self
            .degraded
            .reasons
            .windows(2)
            .all(|items| items[0] < items[1])
        {
            return Err(ParallelAdviceError::Invalid(
                "degraded.reasons must be strictly sorted".into(),
            ));
        }
        if (!self.conflicts.is_empty() || self.schedule.degraded) && !self.degraded.value {
            return Err(ParallelAdviceError::Invalid(
                "conflicts or conservative scheduling require degraded=true".into(),
            ));
        }
        Ok(())
    }

    pub fn canonical_digest(&self) -> Result<Sha256Digest, ParallelAdviceError> {
        canonical_digest(self)
    }
}

fn validate_artifact_binding_value(
    binding: &HybridArtifactBinding,
) -> Result<(), ParallelAdviceError> {
    validate_artifact_binding(&binding.artifact_ref, &binding.digest)
}

fn validate_sorted_unique<T: Ord>(name: &str, items: &[T]) -> Result<(), ParallelAdviceError> {
    if !items.windows(2).all(|window| window[0] < window[1]) {
        return Err(ParallelAdviceError::Invalid(format!(
            "{name} must be strictly sorted and unique"
        )));
    }
    Ok(())
}

/// Canonically digest any serializable SHUTTLE-owned artifact.
pub fn canonical_digest<T: Serialize>(value: &T) -> Result<Sha256Digest, ParallelAdviceError> {
    let value = serde_json::to_value(value)?;
    let bytes = serde_json::to_vec(&canonicalize_json(value))?;
    Sha256Digest::parse(format!("sha256:{}", shuttle_protocol::sha256_hex(&bytes)))
        .map_err(|error| ParallelAdviceError::Invalid(error.to_string()))
}

/// Mint the content-addressed source reference paired with a canonical digest.
pub fn artifact_binding<T: Serialize>(
    value: &T,
) -> Result<(String, Sha256Digest), ParallelAdviceError> {
    let digest = canonical_digest(value)?;
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("validated Sha256Digest always has the prefix");
    Ok((format!("artifact://sha256/{hex}"), digest))
}

/// Digest the exact corrected GoalGraph detail projected into the advice.
pub fn correction_digest(detail: &SplitDetail) -> Result<Sha256Digest, ParallelAdviceError> {
    canonical_digest(detail)
}

fn validate_artifact_binding(
    artifact_ref: &str,
    digest: &Sha256Digest,
) -> Result<(), ParallelAdviceError> {
    let expected = format!(
        "artifact://sha256/{}",
        digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("validated Sha256Digest always has the prefix")
    );
    if artifact_ref != expected {
        return Err(ParallelAdviceError::Invalid(
            "source_intent_ref must be the content address of source_intent_digest".into(),
        ));
    }
    Ok(())
}

fn validate_base_oid(value: &str) -> Result<(), ParallelAdviceError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ParallelAdviceError::Invalid(
            "graph_basis.base_oid must be a full 40- or 64-character lowercase hex OID".into(),
        ));
    }
    Ok(())
}

fn validate_text_id(name: &str, value: &str) -> Result<(), ParallelAdviceError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
    {
        return Err(ParallelAdviceError::Invalid(format!(
            "{name} must be non-empty, trimmed, control-free, and at most 256 bytes"
        )));
    }
    Ok(())
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(items) => {
            let sorted: BTreeMap<_, _> = items
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

/// Prerequisites of `id`: `from` of every AGREE/ADD edge into `id` (mirrors
/// [`crate::goal`] so the two views of the corrected graph never diverge).
fn prereqs_of(detail: &SplitDetail, id: &str) -> Vec<String> {
    let mut out: Vec<String> = detail
        .edges
        .iter()
        .filter(|e| e.to == id && matches!(e.verdict, EdgeVerdict::Agree | EdgeVerdict::Add))
        .map(|e| e.from.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Kahn topological order over the AGREE/ADD edge set, deterministic by
/// smallest id; `Err(cycle_nodes)` on a proven cycle. Identical policy to
/// [`crate::goal::topo_order`] — a proven cycle is an error, never a guessed order.
fn topo_order(detail: &SplitDetail) -> Result<Vec<String>, Vec<String>> {
    let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
    for n in &detail.nodes {
        indeg.entry(n.id.clone()).or_insert(0);
    }
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in &detail.edges {
        if !matches!(e.verdict, EdgeVerdict::Agree | EdgeVerdict::Add) {
            continue;
        }
        indeg.entry(e.from.clone()).or_insert(0);
        if adj.entry(e.from.clone()).or_default().insert(e.to.clone()) {
            *indeg.entry(e.to.clone()).or_insert(0) += 1;
        }
    }
    let mut zero: BTreeSet<String> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut order: Vec<String> = Vec::new();
    while let Some(n) = zero.iter().next().cloned() {
        zero.remove(&n);
        order.push(n.clone());
        if let Some(outs) = adj.get(&n) {
            for m in outs {
                let d = indeg.get_mut(m).expect("edge endpoint is a known node");
                *d -= 1;
                if *d == 0 {
                    zero.insert(m.clone());
                }
            }
        }
    }
    if order.len() == indeg.len() {
        Ok(order)
    } else {
        let done: BTreeSet<&String> = order.iter().collect();
        let cycle: Vec<String> = indeg
            .keys()
            .filter(|k| !done.contains(k))
            .cloned()
            .collect();
        Err(cycle)
    }
}

/// Transitive prerequisite closure of `id` over the corrected edge set, so two
/// tasks linked by a *chain* of dependencies are never considered for the same
/// batch (and the oracle is never even consulted about them).
fn ancestors(
    detail: &SplitDetail,
    id: &str,
    direct: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    let _ = detail;
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = direct.get(id).cloned().unwrap_or_default();
    while let Some(p) = stack.pop() {
        if seen.insert(p.clone()) {
            if let Some(ps) = direct.get(&p) {
                stack.extend(ps.iter().cloned());
            }
        }
    }
    seen
}

/// Schedule with the default [`FileOverlapOracle`].
pub fn schedule_parallel(detail: &SplitDetail) -> Result<ParallelSchedule, Vec<String>> {
    schedule_parallel_with(detail, &FileOverlapOracle)
}

/// Deterministic list scheduling into concurrency batches.
///
/// For each task in topological order, its `dep_floor` is one past the last
/// batch holding any (transitive) prerequisite. Starting from `dep_floor`, we
/// scan for the earliest batch whose every member the oracle proves independent
/// of this task; if none qualifies we open a new batch. Every time a task is
/// pushed past a batch by the oracle we record a [`SerializeReason`]; an
/// unprovable reason also flips [`ParallelSchedule::degraded`].
pub fn schedule_parallel_with<O: ConcurrencyOracle + ?Sized>(
    detail: &SplitDetail,
    oracle: &O,
) -> Result<ParallelSchedule, Vec<String>> {
    let order = topo_order(detail)?;

    // Task footprints from the *corrected* nodes (never pi's raw guess).
    let mut touched: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in &detail.nodes {
        touched.insert(n.id.clone(), n.touched_files.clone());
    }
    let task = |id: &str| BatchTask {
        id: id.to_string(),
        touched: touched.get(id).cloned().unwrap_or_default(),
    };

    // Direct prereqs, then transitive closure, for every task.
    let mut direct: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in &order {
        direct.insert(id.clone(), prereqs_of(detail, id));
    }

    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut batch_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut serialized: Vec<SerializeReason> = Vec::new();
    let mut degraded = false;

    for id in &order {
        let anc = ancestors(detail, id, &direct);
        let dep_floor = anc
            .iter()
            .filter_map(|p| batch_of.get(p))
            .map(|b| b + 1)
            .max()
            .unwrap_or(0);

        let me = task(id);
        let mut placed: Option<usize> = None;
        for (bi, batch) in batches.iter().enumerate().skip(dep_floor) {
            let mut clash: Option<String> = None;
            for other in batch {
                if let Some(reason) = oracle.conflict(&me, &task(other)) {
                    clash = Some(reason);
                    break;
                }
            }
            match clash {
                None => {
                    placed = Some(bi);
                    break;
                }
                Some(reason) => {
                    if reason.starts_with(UNPROVABLE_PREFIX) {
                        degraded = true;
                    }
                    // The earlier task (already placed) is the one this waits behind.
                    let earlier = batch
                        .iter()
                        .find(|o| oracle.conflict(&me, &task(o)).is_some())
                        .cloned()
                        .unwrap_or_default();
                    serialized.push(SerializeReason {
                        earlier,
                        later: id.clone(),
                        reason,
                    });
                }
            }
        }
        let bi = match placed {
            Some(bi) => bi,
            None => {
                batches.push(Vec::new());
                batches.len() - 1
            }
        };
        batches[bi].push(id.clone());
        batch_of.insert(id.clone(), bi);
    }

    let mut out: Vec<ConcurrencyBatch> = batches
        .into_iter()
        .enumerate()
        .map(|(index, mut tasks)| {
            tasks.sort();
            ConcurrencyBatch { index, tasks }
        })
        .collect();
    out.sort_by_key(|b| b.index);
    let max_width = out.iter().map(|b| b.tasks.len()).max().unwrap_or(0);
    serialized.sort_by(|a, b| a.later.cmp(&b.later).then(a.earlier.cmp(&b.earlier)));
    serialized.dedup();

    Ok(ParallelSchedule {
        batches: out,
        max_width,
        serialized,
        degraded,
    })
}

/// Schedule a Hybrid hard DAG using exact path footprints where available and
/// the existing conservative path oracle for every pair. Conflicts remain
/// advice/review data; only the acyclic proposed hard edge set is scheduled.
/// Missing, stale, ambiguous, truncated, timed-out, unsupported, partial or
/// planner-guessed footprints become unknown footprints and serialize safely.
pub fn schedule_hybrid(
    dag: &HybridDag,
    footprints: &[HybridTaskFootprint],
) -> Result<ParallelSchedule, ParallelAdviceError> {
    if !dag.acyclic {
        return Err(ParallelAdviceError::Invalid(
            "cannot schedule a cyclic Hybrid hard DAG".into(),
        ));
    }
    if dag
        .conflicts
        .iter()
        .any(|conflict| conflict.kind == HybridConflictKind::InvalidTaskReference)
    {
        return Err(ParallelAdviceError::Invalid(
            "cannot schedule Hybrid DAG with invalid task references".into(),
        ));
    }
    let task_ids: BTreeSet<String> = dag.task_ids.iter().cloned().collect();
    let mut by_task: BTreeMap<&str, &HybridTaskFootprint> = BTreeMap::new();
    for footprint in footprints {
        if !task_ids.contains(&footprint.task_id) {
            return Err(ParallelAdviceError::Invalid(format!(
                "footprint references unknown task: {}",
                footprint.task_id
            )));
        }
        if by_task
            .insert(footprint.task_id.as_str(), footprint)
            .is_some()
        {
            return Err(ParallelAdviceError::Invalid(format!(
                "duplicate task footprint: {}",
                footprint.task_id
            )));
        }
    }

    let collision_tasks: BTreeSet<&str> = dag
        .conflicts
        .iter()
        .filter(|conflict| conflict.kind == HybridConflictKind::WriteWriteCollision)
        .flat_map(|conflict| conflict.task_ids.iter().map(String::as_str))
        .collect();
    let degraded_tasks: BTreeSet<&str> = dag
        .degradations
        .iter()
        .flat_map(|degradation| degradation.task_ids.iter().map(String::as_str))
        .collect();
    let global_degradation = dag
        .degradations
        .iter()
        .any(|degradation| degradation.task_ids.is_empty());

    let nodes: Vec<_> = dag
        .task_ids
        .iter()
        .map(|id| {
            let touched_files = by_task
                .get(id.as_str())
                .filter(|footprint| {
                    footprint.source.is_exact()
                        && footprint.state.permits_hard_addition()
                        && !collision_tasks.contains(id.as_str())
                        && !degraded_tasks.contains(id.as_str())
                        && !global_degradation
                })
                .map(|footprint| {
                    footprint
                        .write_paths
                        .iter()
                        .map(|path| norm(path))
                        .filter(|path| !path.is_empty())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            shuttle_protocol::SplitNode {
                id: id.clone(),
                intent: String::new(),
                touched_files,
            }
        })
        .collect();
    let edges: Vec<_> = dag
        .proposed_hard_edges
        .iter()
        .map(|hard| shuttle_protocol::SplitEdge {
            from: hard.edge.from.clone(),
            to: hard.edge.to.clone(),
            verdict: EdgeVerdict::Agree,
            confidence: shuttle_protocol::OutcomeConfidence::High,
            source: shuttle_protocol::EdgeSource::Both,
            evidence: hard
                .evidence_refs
                .first()
                .cloned()
                .unwrap_or_else(|| "model".to_string()),
        })
        .collect();
    let detail = SplitDetail {
        goal: String::new(),
        nodes,
        edges,
        added: 0,
        dropped: 0,
        review_surface: dag.conflicts.len(),
        review_required: Vec::new(),
    };
    let mut schedule = schedule_parallel(&detail).map_err(|nodes| {
        ParallelAdviceError::Invalid(format!(
            "Hybrid hard DAG contains a cycle involving: {}",
            nodes.join(",")
        ))
    })?;
    let incomplete_footprint = dag.task_ids.iter().any(|id| {
        by_task.get(id.as_str()).is_none_or(|footprint| {
            !footprint.source.is_exact()
                || !footprint.state.permits_hard_addition()
                || footprint.write_paths.is_empty()
        })
    });
    schedule.degraded |= incomplete_footprint
        || global_degradation
        || !degraded_tasks.is_empty()
        || !collision_tasks.is_empty();
    Ok(schedule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{EdgeSource, OutcomeConfidence, SplitEdge, SplitNode};

    // --- builders ---------------------------------------------------------

    fn node(id: &str, files: &[&str]) -> SplitNode {
        SplitNode {
            id: id.into(),
            intent: format!("intent-{id}"),
            touched_files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn edge(from: &str, to: &str, verdict: EdgeVerdict) -> SplitEdge {
        SplitEdge {
            from: from.into(),
            to: to.into(),
            verdict,
            confidence: OutcomeConfidence::High,
            source: EdgeSource::Both,
            evidence: String::new(),
        }
    }

    fn detail(nodes: Vec<SplitNode>, edges: Vec<SplitEdge>) -> SplitDetail {
        SplitDetail {
            goal: "G".into(),
            nodes,
            edges,
            added: 0,
            dropped: 0,
            review_surface: 0,
            review_required: Vec::new(),
        }
    }

    fn digest(ch: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", ch.to_string().repeat(64))).unwrap()
    }

    fn advice(schedule: &ParallelSchedule) -> ParallelAdvice {
        ParallelAdvice::from_schedule(
            schedule,
            format!("artifact://sha256/{}", "a".repeat(64)),
            digest('a'),
            ParallelAdviceGraphBasis {
                base_oid: "b".repeat(40),
                symbol_index_digest: digest('c'),
                correction_digest: digest('d'),
                footprint_source: FootprintSource::PlannerGuess,
            },
        )
        .unwrap()
    }

    /// A test double: any pair whose ids are in the coupled set conflicts,
    /// regardless of files. Lets us prove the oracle seam serializes even
    /// file-disjoint pairs.
    struct PairOracle {
        coupled: Vec<(String, String)>,
    }
    impl ConcurrencyOracle for PairOracle {
        fn conflict(&self, a: &BatchTask, b: &BatchTask) -> Option<String> {
            for (x, y) in &self.coupled {
                if (a.id == *x && b.id == *y) || (a.id == *y && b.id == *x) {
                    return Some(format!("coupled: {x}~{y}"));
                }
            }
            None
        }
    }

    #[test]
    fn disjoint_files_share_one_batch() {
        let d = detail(
            vec![
                node("a", &["a.rs"]),
                node("b", &["b.rs"]),
                node("c", &["c.rs"]),
            ],
            vec![],
        );
        let s = schedule_parallel(&d).unwrap();
        assert_eq!(s.batches.len(), 1);
        assert_eq!(s.batches[0].tasks, vec!["a", "b", "c"]);
        assert_eq!(s.max_width, 3);
        assert!(!s.degraded);
        assert!(s.serialized.is_empty());
    }

    #[test]
    fn file_overlap_forces_serial_across_batches() {
        let d = detail(
            vec![node("a", &["shared.rs"]), node("b", &["shared.rs"])],
            vec![],
        );
        let s = schedule_parallel(&d).unwrap();
        assert_eq!(s.batches.len(), 2);
        assert_eq!(s.max_width, 1);
        assert_eq!(s.serialized.len(), 1);
        assert!(s.serialized[0].reason.starts_with("file-overlap"));
        assert!(!s.degraded); // a proven clash is not a degrade
    }

    #[test]
    fn dependency_edges_serialize_and_never_call_oracle_on_related_pairs() {
        // a -> b -> c chain; even though all files disjoint, deps serialize.
        let d = detail(
            vec![
                node("a", &["a.rs"]),
                node("b", &["b.rs"]),
                node("c", &["c.rs"]),
            ],
            vec![
                edge("a", "b", EdgeVerdict::Agree),
                edge("b", "c", EdgeVerdict::Add),
            ],
        );
        let s = schedule_parallel(&d).unwrap();
        assert_eq!(s.batches.len(), 3);
        assert_eq!(s.batches[0].tasks, vec!["a"]);
        assert_eq!(s.batches[1].tasks, vec!["b"]);
        assert_eq!(s.batches[2].tasks, vec!["c"]);
        // dependency serialization is structural, not an oracle "serialize" event
        assert!(s.serialized.is_empty());
    }

    #[test]
    fn diamond_widens_the_middle_batch() {
        // a -> {b, c} -> d ; b,c disjoint files => one batch of width 2.
        let d = detail(
            vec![
                node("a", &["a.rs"]),
                node("b", &["b.rs"]),
                node("c", &["c.rs"]),
                node("d", &["d.rs"]),
            ],
            vec![
                edge("a", "b", EdgeVerdict::Agree),
                edge("a", "c", EdgeVerdict::Agree),
                edge("b", "d", EdgeVerdict::Agree),
                edge("c", "d", EdgeVerdict::Agree),
            ],
        );
        let s = schedule_parallel(&d).unwrap();
        assert_eq!(s.batches.len(), 3);
        assert_eq!(s.batches[0].tasks, vec!["a"]);
        assert_eq!(s.batches[1].tasks, vec!["b", "c"]);
        assert_eq!(s.batches[2].tasks, vec!["d"]);
        assert_eq!(s.max_width, 2);
    }

    #[test]
    fn oracle_coupling_serializes_even_with_disjoint_files() {
        let d = detail(
            vec![
                node("a", &["a.rs"]),
                node("b", &["b.rs"]),
                node("c", &["c.rs"]),
            ],
            vec![],
        );
        let oracle = EitherOracle {
            first: FileOverlapOracle,
            second: PairOracle {
                coupled: vec![("a".into(), "b".into())],
            },
        };
        let s = schedule_parallel_with(&d, &oracle).unwrap();
        // a & b coupled => cannot share; c disjoint from both.
        assert_eq!(s.max_width, 2);
        assert_eq!(s.batches.len(), 2);
        // a,c together; b alone in next batch (or c with b) — either way a~b split.
        let batch_of = |id: &str| {
            s.batches
                .iter()
                .position(|batch| batch.tasks.iter().any(|t| t == id))
                .unwrap()
        };
        assert_ne!(batch_of("a"), batch_of("b"));
        assert!(s.serialized.iter().any(|r| r.reason.starts_with("coupled")));
    }

    #[test]
    fn unknown_footprint_is_conservatively_serial_and_degrades() {
        let d = detail(
            vec![node("a", &["a.rs"]), node("b", &[])], // b has no known files
            vec![],
        );
        let s = schedule_parallel(&d).unwrap();
        assert_eq!(s.batches.len(), 2);
        assert!(s.degraded);
        assert!(s
            .serialized
            .iter()
            .any(|r| r.reason.starts_with(UNPROVABLE_PREFIX)));
    }

    #[test]
    fn proven_cycle_is_an_error_never_an_arbitrary_schedule() {
        let d = detail(
            vec![node("a", &["a.rs"]), node("b", &["b.rs"])],
            vec![
                edge("a", "b", EdgeVerdict::Agree),
                edge("b", "a", EdgeVerdict::Agree),
            ],
        );
        let err = schedule_parallel(&d).unwrap_err();
        assert!(err.contains(&"a".to_string()) && err.contains(&"b".to_string()));
    }

    #[test]
    fn empty_goal_is_an_empty_schedule() {
        let d = detail(vec![], vec![]);
        let s = schedule_parallel(&d).unwrap();
        assert!(s.batches.is_empty());
        assert_eq!(s.max_width, 0);
        assert_eq!(s.depth(), 0);
        assert!(!s.degraded);
    }

    #[test]
    fn schedule_serializes_to_json_with_indexed_batches() {
        let d = detail(
            vec![node("a", &["a.rs"]), node("b", &["b.rs"])],
            vec![edge("a", "b", EdgeVerdict::Agree)],
        );
        let s = schedule_parallel(&d).unwrap();
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v["batches"][0]["index"], 0);
        assert_eq!(v["batches"][0]["tasks"][0], "a");
        assert_eq!(v["batches"][1]["tasks"][0], "b");
        assert_eq!(v["max_width"], 1);
        // no serialize events (pure dependency) and no degrade
        assert_eq!(v["degraded"], false);
        assert!(v.get("serialized").is_none());
    }

    #[test]
    fn stable_advice_projects_schedule_without_execution_authority() {
        let schedule = schedule_parallel(&detail(
            vec![node("a", &["a.rs"]), node("b", &["b.rs"])],
            vec![],
        ))
        .unwrap();
        let advice = advice(&schedule);
        let json = serde_json::to_value(&advice).unwrap();

        assert_eq!(json["schema_version"], PARALLEL_ADVICE_SCHEMA);
        assert_eq!(json["graph_basis"]["footprint_source"], "planner_guess");
        assert_eq!(
            json["batches"][0]["task_ids"],
            serde_json::json!(["a", "b"])
        );
        assert!(json.get("actions").is_none());
        assert!(json.get("approval").is_none());
        assert!(json.get("workspace").is_none());
        advice.validate().unwrap();
    }

    #[test]
    fn advice_parse_rejects_unknown_fields_and_inconsistent_projection() {
        let schedule = schedule_parallel(&detail(vec![node("a", &["a.rs"])], vec![])).unwrap();
        let valid = serde_json::to_value(advice(&schedule)).unwrap();

        let mut unknown = valid.clone();
        unknown["dispatch_authorized"] = serde_json::json!(true);
        assert!(ParallelAdvice::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());

        let mut nested_unknown = valid.clone();
        nested_unknown["graph_basis"]["proof"] = serde_json::json!("mechanical");
        assert!(ParallelAdvice::parse(&serde_json::to_vec(&nested_unknown).unwrap()).is_err());

        let mut wrong_width = valid.clone();
        wrong_width["max_width"] = serde_json::json!(9);
        assert!(ParallelAdvice::parse(&serde_json::to_vec(&wrong_width).unwrap()).is_err());

        let mut wrong_ref = valid;
        wrong_ref["source_intent_ref"] =
            serde_json::json!(format!("artifact://sha256/{}", "f".repeat(64)));
        assert!(ParallelAdvice::parse(&serde_json::to_vec(&wrong_ref).unwrap()).is_err());
    }

    #[test]
    fn canonical_advice_digest_is_stable_and_basis_drift_sensitive() {
        let schedule = schedule_parallel(&detail(
            vec![node("a", &["a.rs"]), node("b", &["b.rs"])],
            vec![],
        ))
        .unwrap();
        let original = advice(&schedule);
        assert_eq!(
            original.canonical_digest().unwrap(),
            original.canonical_digest().unwrap()
        );

        let round_trip = ParallelAdvice::parse(&serde_json::to_vec_pretty(&original).unwrap())
            .expect("pretty JSON parses to the same contract");
        assert_eq!(
            original.canonical_digest().unwrap(),
            round_trip.canonical_digest().unwrap(),
            "insignificant JSON formatting cannot change the canonical digest"
        );

        let mut base_drift = original.clone();
        base_drift.graph_basis.base_oid = "e".repeat(40);
        assert_ne!(
            original.canonical_digest().unwrap(),
            base_drift.canonical_digest().unwrap()
        );

        let mut index_drift = original.clone();
        index_drift.graph_basis.symbol_index_digest = digest('e');
        assert_ne!(
            original.canonical_digest().unwrap(),
            index_drift.canonical_digest().unwrap()
        );

        let mut correction_drift = original.clone();
        correction_drift.graph_basis.correction_digest = digest('e');
        assert_ne!(
            original.canonical_digest().unwrap(),
            correction_drift.canonical_digest().unwrap()
        );
    }

    #[test]
    fn source_intent_and_correction_bindings_are_canonical() {
        let intent = serde_json::json!({
            "tasks": [{"id": "a"}],
            "version": "intent/v1",
            "goal": "G"
        });
        let reordered = serde_json::json!({
            "goal": "G",
            "version": "intent/v1",
            "tasks": [{"id": "a"}]
        });
        let (artifact_ref, intent_digest) = artifact_binding(&intent).unwrap();
        assert_eq!(intent_digest, canonical_digest(&reordered).unwrap());
        assert_eq!(
            artifact_ref,
            format!(
                "artifact://sha256/{}",
                intent_digest.as_str().trim_start_matches("sha256:")
            )
        );

        let corrected = detail(vec![node("a", &["a.rs"])], vec![]);
        assert_eq!(
            correction_digest(&corrected).unwrap(),
            correction_digest(&corrected).unwrap()
        );
        let mut changed = corrected;
        changed.nodes[0].touched_files.push("drift.rs".into());
        assert_ne!(
            correction_digest(&changed).unwrap(),
            correction_digest(&detail(vec![node("a", &["a.rs"])], vec![])).unwrap()
        );
    }
    fn hybrid_dag() -> HybridDag {
        crate::split::merge_hybrid_dag(&crate::split::HybridMergeInput {
            task_ids: vec!["consumer".into(), "provider".into(), "peer".into()],
            model_edges: vec![HybridTaskEdge {
                from: "provider".into(),
                to: "consumer".into(),
            }],
            mechanical_edges: Vec::new(),
            write_claims: Vec::new(),
            evidence_issues: Vec::new(),
        })
    }

    fn hybrid_footprints() -> Vec<HybridTaskFootprint> {
        vec![
            HybridTaskFootprint {
                task_id: "provider".into(),
                source: HybridFootprintSource::PlannerDeclaredValidated,
                state: HybridEvidenceState::FreshExactUnique,
                write_paths: vec!["provider.rs".into()],
            },
            HybridTaskFootprint {
                task_id: "consumer".into(),
                source: HybridFootprintSource::PlannerDeclaredValidated,
                state: HybridEvidenceState::FreshExactUnique,
                write_paths: vec!["consumer.rs".into()],
            },
            HybridTaskFootprint {
                task_id: "peer".into(),
                source: HybridFootprintSource::PlannerDeclaredValidated,
                state: HybridEvidenceState::FreshExactUnique,
                write_paths: vec!["peer.rs".into()],
            },
        ]
    }

    fn hybrid_advice(dag: &HybridDag, footprints: &[HybridTaskFootprint]) -> HybridParallelAdvice {
        HybridParallelAdvice::from_hybrid_dag(
            dag,
            footprints,
            HybridPlanningBasis {
                planning_ref: "plan://g10.1".into(),
                planning_revision: "r1".into(),
                planning_structure_digest: digest("1".chars().next().unwrap()),
            },
            HybridArtifactBinding {
                artifact_ref: format!("artifact://sha256/{}", "2".repeat(64)),
                digest: digest("2".chars().next().unwrap()),
            },
            vec![HybridRepositoryBase {
                repo_id: "repo".into(),
                base_oid: "3".repeat(40),
            }],
            HybridArtifactBinding {
                artifact_ref: format!("artifact://sha256/{}", "4".repeat(64)),
                digest: digest("4".chars().next().unwrap()),
            },
            vec![HybridIndexBinding {
                repo_id: "repo".into(),
                index_digest: digest("5".chars().next().unwrap()),
            }],
        )
        .unwrap()
    }

    #[test]
    fn hybrid_schedule_uses_hard_dag_and_exact_paths() {
        let schedule = schedule_hybrid(&hybrid_dag(), &hybrid_footprints()).unwrap();
        assert_eq!(schedule.batches[0].tasks, vec!["peer", "provider"]);
        assert_eq!(schedule.batches[1].tasks, vec!["consumer"]);
        assert!(!schedule.degraded);
    }

    #[test]
    fn hybrid_schedule_fails_safe_for_guess_stale_and_collision() {
        let mut dag = hybrid_dag();
        dag.conflicts.push(HybridConflict {
            kind: HybridConflictKind::WriteWriteCollision,
            task_ids: vec!["peer".into(), "provider".into()],
            edge: None,
            symbol_id: Some("same".into()),
            evidence_refs: Vec::new(),
        });
        dag.conflicts.sort();
        // Recompute the adoption gate from the mutated conflict set so the
        // structured decision and the legacy bool stay in lock-step.
        dag.adoption = crate::split::HybridAdoptionDecision::from_gates(
            dag.acyclic,
            dag.conflicts.len(),
            dag.degradations.len(),
        );
        dag.automatic_adoption_allowed = dag.adoption.is_adopt();
        let mut footprints = hybrid_footprints();
        footprints[0].source = HybridFootprintSource::PlannerGuess;
        footprints[1].state = HybridEvidenceState::Stale;
        let schedule = schedule_hybrid(&dag, &footprints).unwrap();
        assert_eq!(schedule.max_width, 1);
        assert!(schedule.degraded);
        assert!(schedule
            .serialized
            .iter()
            .all(|reason| reason.reason.starts_with(UNPROVABLE_PREFIX)));
    }

    #[test]
    fn hybrid_v2_digest_binds_every_basis_and_parse_is_closed() {
        let dag = hybrid_dag();
        let advice = hybrid_advice(&dag, &hybrid_footprints());
        assert_eq!(advice.schema_version, HYBRID_PARALLEL_ADVICE_SCHEMA);
        assert_eq!(advice.policy_version, HYBRID_DAG_POLICY_VERSION);
        assert!(!advice.degraded.value);
        let original = advice.canonical_digest().unwrap();
        let round_trip =
            HybridParallelAdvice::parse(&serde_json::to_vec_pretty(&advice).unwrap()).unwrap();
        assert_eq!(original, round_trip.canonical_digest().unwrap());

        let mut mutations = Vec::new();
        let mut changed = advice.clone();
        changed.planning_basis.planning_revision = "r2".into();
        mutations.push(changed);
        let mut changed = advice.clone();
        changed.repository_bases[0].base_oid = "6".repeat(40);
        mutations.push(changed);
        let mut changed = advice.clone();
        changed.symbol_index_bindings[0].index_digest = digest("7".chars().next().unwrap());
        mutations.push(changed);
        let mut changed = advice.clone();
        changed.symbol_footprint.digest = digest("8".chars().next().unwrap());
        changed.symbol_footprint.artifact_ref = format!("artifact://sha256/{}", "8".repeat(64));
        mutations.push(changed);
        let mut changed = advice.clone();
        changed.policy_version = "other-policy".into();
        mutations.push(changed);
        assert!(mutations
            .iter()
            .all(|changed| changed.canonical_digest().unwrap() != original));

        let mut json = serde_json::to_value(&advice).unwrap();
        json["dispatch_authorized"] = serde_json::json!(true);
        assert!(HybridParallelAdvice::parse(&serde_json::to_vec(&json).unwrap()).is_err());
        let mut weak_in_hard = advice;
        weak_in_hard.proposed_hard_edges[0]
            .kinds
            .push(HybridEvidenceKind::NameMatch);
        assert!(weak_in_hard.validate().is_err());
    }

    #[test]
    fn hybrid_v2_is_deterministic_over_basis_input_order() {
        let dag = hybrid_dag();
        let footprints = hybrid_footprints();
        let first = HybridParallelAdvice::from_hybrid_dag(
            &dag,
            &footprints,
            HybridPlanningBasis {
                planning_ref: "plan://g10.1".into(),
                planning_revision: "r1".into(),
                planning_structure_digest: digest("1".chars().next().unwrap()),
            },
            HybridArtifactBinding {
                artifact_ref: format!("artifact://sha256/{}", "2".repeat(64)),
                digest: digest("2".chars().next().unwrap()),
            },
            vec![
                HybridRepositoryBase {
                    repo_id: "z".into(),
                    base_oid: "3".repeat(40),
                },
                HybridRepositoryBase {
                    repo_id: "a".into(),
                    base_oid: "4".repeat(40),
                },
            ],
            HybridArtifactBinding {
                artifact_ref: format!("artifact://sha256/{}", "5".repeat(64)),
                digest: digest("5".chars().next().unwrap()),
            },
            vec![
                HybridIndexBinding {
                    repo_id: "z".into(),
                    index_digest: digest("6".chars().next().unwrap()),
                },
                HybridIndexBinding {
                    repo_id: "a".into(),
                    index_digest: digest("7".chars().next().unwrap()),
                },
            ],
        )
        .unwrap();
        let mut bases = first.repository_bases.clone();
        bases.reverse();
        let mut indexes = first.symbol_index_bindings.clone();
        indexes.reverse();
        let second = HybridParallelAdvice::from_hybrid_dag(
            &dag,
            &footprints,
            first.planning_basis.clone(),
            first.source_intent.clone(),
            bases,
            first.symbol_footprint.clone(),
            indexes,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.canonical_digest().unwrap(),
            second.canonical_digest().unwrap()
        );
    }
}
