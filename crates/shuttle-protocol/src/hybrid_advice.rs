//! Shared wire types for monotonic Hybrid DAG advice.
//!
//! These types are deliberately policy-neutral projections. `shuttle-nav` owns the
//! merge and scheduling algorithms; this module owns their closed, validated
//! transport representation for SHUTTLE and external scheduler consumers.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, canonical_json, ClosedContractError};
use crate::symbol_footprint::{
    invalid, validate_oid, validate_sorted_strings, validate_strictly_sorted, validate_text_id,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const HYBRID_PARALLEL_ADVICE_SCHEMA: &str = "shuttle_goal_parallel_advice/v2";
pub const HYBRID_DAG_POLICY_VERSION: &str = "shuttle_hybrid_dag_policy/v1";
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// A dependency edge: `from` is prerequisite/provider, `to` is dependent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridTaskEdge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEvidenceKind {
    ImportDirected,
    ContractDirected,
    NameMatch,
    CoChange,
}

impl HybridEvidenceKind {
    pub fn is_trusted_directed(self) -> bool {
        matches!(self, Self::ImportDirected | Self::ContractDirected)
    }

    pub fn is_weak(self) -> bool {
        matches!(self, Self::NameMatch | Self::CoChange)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEvidenceState {
    FreshExactUnique,
    Partial,
    Unsupported,
    Timeout,
    Truncated,
    Ambiguous,
    Stale,
    Absent,
}

impl HybridEvidenceState {
    pub fn permits_hard_addition(self) -> bool {
        self == Self::FreshExactUnique
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridMechanicalEvidence {
    pub owner_task: String,
    pub callee_task: String,
    pub kind: HybridEvidenceKind,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridWriteClaim {
    pub task_id: String,
    pub symbol_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridEvidenceIssue {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridMergeInput {
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub model_edges: Vec<HybridTaskEdge>,
    #[serde(default)]
    pub mechanical_edges: Vec<HybridMechanicalEvidence>,
    #[serde(default)]
    pub write_claims: Vec<HybridWriteClaim>,
    #[serde(default)]
    pub evidence_issues: Vec<HybridEvidenceIssue>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEdgeProvenance {
    Model,
    TrustedMechanical,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridTrustedCandidate {
    pub edge: HybridTaskEdge,
    pub kinds: Vec<HybridEvidenceKind>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridHardEdge {
    pub edge: HybridTaskEdge,
    pub provenance: HybridEdgeProvenance,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<HybridEvidenceKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridWeakHint {
    pub task_a: String,
    pub task_b: String,
    pub kind: HybridEvidenceKind,
    pub evidence_ref: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridConflictKind {
    ReverseModelEdge,
    CycleProducingAddition,
    ModelCycle,
    WriteWriteCollision,
    AmbiguousEvidence,
    InvalidTaskReference,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridConflict {
    pub kind: HybridConflictKind,
    pub task_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<HybridTaskEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridDegradation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

/// Wire mirror of `shuttle-nav`'s automatic graph-assisted adoption verdict. Advice
/// only: it never authorizes a workspace, dispatch, or landing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridAdoptionVerdict {
    Adopt,
    Block,
}

/// Wire mirror of the enumerated reason a consumer may adopt or must withhold.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridAdoptionReasonCode {
    CyclicHardDag,
    UnresolvedConflicts,
    DegradedEvidence,
}

/// Wire mirror of one blocking cause plus the count it summarizes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridAdoptionReason {
    pub code: HybridAdoptionReasonCode,
    #[serde(default)]
    pub count: usize,
}

/// Wire mirror of the structured adoption decision. `reasons` is empty iff
/// `verdict == Adopt`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridAdoptionDecision {
    pub verdict: HybridAdoptionVerdict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<HybridAdoptionReason>,
}

impl Default for HybridAdoptionDecision {
    /// A defaulted decision is the safe permissive baseline (`Adopt`, no
    /// reasons) so a v1 projection lacking the field still deserializes.
    fn default() -> Self {
        Self {
            verdict: HybridAdoptionVerdict::Adopt,
            reasons: Vec::new(),
        }
    }
}

/// Normalized algorithm output before it is bound into the v2 envelope. This
/// mirrors `shuttle-nav` without importing policy code into the protocol crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridDagProjection {
    pub policy_version: String,
    pub task_ids: Vec<String>,
    pub model_edges: Vec<HybridTaskEdge>,
    pub trusted_addition_candidates: Vec<HybridTrustedCandidate>,
    pub weak_hints: Vec<HybridWeakHint>,
    pub conflicts: Vec<HybridConflict>,
    pub proposed_hard_edges: Vec<HybridHardEdge>,
    pub degradations: Vec<HybridDegradation>,
    pub acyclic: bool,
    pub automatic_adoption_allowed: bool,
    /// Structured adoption decision mirroring `shuttle-nav`. Defaults to the
    /// permissive baseline when absent so a v1 projection still parses.
    #[serde(default)]
    pub adoption: HybridAdoptionDecision,
}

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

/// Minimal scheduling projection for an external planner. `write_paths` remains the final
/// authority boundary; symbol exactness never grants a path scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridTaskFootprint {
    pub task_id: String,
    pub source: HybridFootprintSource,
    pub state: HybridEvidenceState,
    #[serde(default)]
    pub write_paths: Vec<String>,
}

impl HybridTaskFootprint {
    /// Build the external scheduling projection with stable path
    /// normalization. This does not confer authority on the resulting paths.
    pub fn new(
        task_id: impl Into<String>,
        source: HybridFootprintSource,
        state: HybridEvidenceState,
        write_paths: impl IntoIterator<Item = String>,
    ) -> Result<Self, ClosedContractError> {
        let mut write_paths: Vec<_> = write_paths
            .into_iter()
            .map(|path| normalize_project_path(&path))
            .collect();
        write_paths.sort();
        write_paths.dedup();
        let value = Self {
            task_id: task_id.into(),
            source,
            state,
            write_paths,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("task_id", &self.task_id)?;
        validate_sorted_strings("write_paths", &self.write_paths)?;
        for path in &self.write_paths {
            validate_project_path(path)?;
        }
        if self.source.is_exact() && self.state != HybridEvidenceState::FreshExactUnique {
            return Err(invalid(
                "exact footprint source requires fresh_exact_unique evidence",
            ));
        }
        Ok(())
    }
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

impl HybridArtifactBinding {
    pub fn content_addressed(digest: Sha256Digest) -> Self {
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("Sha256Digest always has the prefix");
        Self {
            artifact_ref: format!("artifact://sha256/{hex}"),
            digest,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridRepositoryBase {
    pub repo_id: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridScheduleBatch {
    pub index: usize,
    pub tasks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridSerializeReason {
    pub earlier: String,
    pub later: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridSchedule {
    pub batches: Vec<HybridScheduleBatch>,
    pub max_width: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serialized: Vec<HybridSerializeReason>,
    pub degraded: bool,
}

/// Input for [`HybridParallelAdvice::new`]. `schema_version` is intentionally
/// absent so callers cannot accidentally construct a differently-versioned
/// envelope.
#[derive(Debug, Clone)]
pub struct HybridParallelAdviceInput {
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
    pub schedule: HybridSchedule,
    pub degraded: HybridDegradedState,
}

/// Closed, digest-bound v2 advice. It contains no workspace, claim, approval,
/// credential, dispatch, land, or push authority.
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
    pub schedule: HybridSchedule,
    pub degraded: HybridDegradedState,
}

impl HybridParallelAdvice {
    /// Canonicalize unordered collections, construct v2, and validate it.
    pub fn new(mut input: HybridParallelAdviceInput) -> Result<Self, ClosedContractError> {
        normalize_input(&mut input);
        let value = Self {
            schema_version: HYBRID_PARALLEL_ADVICE_SCHEMA.to_string(),
            policy_version: input.policy_version,
            planning_basis: input.planning_basis,
            source_intent: input.source_intent,
            repository_bases: input.repository_bases,
            symbol_footprint: input.symbol_footprint,
            symbol_index_bindings: input.symbol_index_bindings,
            model_edges: input.model_edges,
            trusted_addition_candidates: input.trusted_addition_candidates,
            weak_hints: input.weak_hints,
            conflicts: input.conflicts,
            proposed_hard_edges: input.proposed_hard_edges,
            schedule: input.schedule,
            degraded: input.degraded,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 4MiB input limit"));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != HYBRID_PARALLEL_ADVICE_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        if self.policy_version != HYBRID_DAG_POLICY_VERSION {
            return Err(invalid(format!(
                "unsupported policy_version: {}",
                self.policy_version
            )));
        }
        validate_text_id("planning_ref", &self.planning_basis.planning_ref)?;
        validate_text_id("planning_revision", &self.planning_basis.planning_revision)?;
        validate_artifact_binding("source_intent", &self.source_intent)?;
        validate_artifact_binding("symbol_footprint", &self.symbol_footprint)?;

        validate_strictly_sorted("repository_bases", &self.repository_bases)?;
        if self.repository_bases.is_empty() {
            return Err(invalid("repository_bases must not be empty"));
        }
        let mut repo_ids = BTreeSet::new();
        for repository in &self.repository_bases {
            validate_text_id("repository_bases.repo_id", &repository.repo_id)?;
            validate_oid("repository_bases.base_oid", &repository.base_oid)?;
            if !repo_ids.insert(repository.repo_id.as_str()) {
                return Err(invalid(format!(
                    "duplicate repository base: {}",
                    repository.repo_id
                )));
            }
        }
        validate_strictly_sorted("symbol_index_bindings", &self.symbol_index_bindings)?;
        let mut index_repos = BTreeSet::new();
        for index in &self.symbol_index_bindings {
            validate_text_id("symbol_index_bindings.repo_id", &index.repo_id)?;
            if !repo_ids.contains(index.repo_id.as_str()) {
                return Err(invalid(format!(
                    "symbol index has no repository base: {}",
                    index.repo_id
                )));
            }
            if !index_repos.insert(index.repo_id.as_str()) {
                return Err(invalid(format!(
                    "duplicate symbol index binding: {}",
                    index.repo_id
                )));
            }
        }

        validate_strictly_sorted("model_edges", &self.model_edges)?;
        validate_strictly_sorted(
            "trusted_addition_candidates",
            &self.trusted_addition_candidates,
        )?;
        validate_strictly_sorted("weak_hints", &self.weak_hints)?;
        validate_strictly_sorted("conflicts", &self.conflicts)?;
        validate_strictly_sorted("proposed_hard_edges", &self.proposed_hard_edges)?;
        validate_edges_and_provenance(self)?;
        validate_schedule(self)?;
        validate_sorted_strings("degraded.reasons", &self.degraded.reasons)?;
        if self.degraded.value != !self.degraded.reasons.is_empty() {
            return Err(invalid(
                "degraded.value must exactly reflect degraded.reasons",
            ));
        }
        if (!self.conflicts.is_empty() || self.schedule.degraded) && !self.degraded.value {
            return Err(invalid(
                "conflicts or conservative scheduling require degraded=true",
            ));
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ClosedContractError> {
        self.validate()?;
        canonical_json(self)
    }

    pub fn canonical_digest(&self) -> Result<Sha256Digest, ClosedContractError> {
        self.validate()?;
        canonical_digest(self)
    }
}

fn normalize_input(input: &mut HybridParallelAdviceInput) {
    input.repository_bases.sort();
    input.repository_bases.dedup();
    input.symbol_index_bindings.sort();
    input.symbol_index_bindings.dedup();
    input.model_edges.sort();
    input.model_edges.dedup();
    for candidate in &mut input.trusted_addition_candidates {
        candidate.kinds.sort();
        candidate.kinds.dedup();
        candidate.evidence_refs.sort();
        candidate.evidence_refs.dedup();
    }
    input.trusted_addition_candidates.sort();
    input.trusted_addition_candidates.dedup();
    input.weak_hints.sort();
    input.weak_hints.dedup();
    for conflict in &mut input.conflicts {
        conflict.task_ids.sort();
        conflict.task_ids.dedup();
        conflict.evidence_refs.sort();
        conflict.evidence_refs.dedup();
    }
    input.conflicts.sort();
    input.conflicts.dedup();
    for hard in &mut input.proposed_hard_edges {
        hard.kinds.sort();
        hard.kinds.dedup();
        hard.evidence_refs.sort();
        hard.evidence_refs.dedup();
    }
    input.proposed_hard_edges.sort();
    input.proposed_hard_edges.dedup();
    for batch in &mut input.schedule.batches {
        batch.tasks.sort();
        batch.tasks.dedup();
    }
    input.schedule.serialized.sort();
    input.schedule.serialized.dedup();
    input.degraded.reasons.sort();
    input.degraded.reasons.dedup();
}

fn validate_edges_and_provenance(advice: &HybridParallelAdvice) -> Result<(), ClosedContractError> {
    let validate_edge = |edge: &HybridTaskEdge| {
        validate_text_id("edge.from", &edge.from)?;
        validate_text_id("edge.to", &edge.to)?;
        if edge.from == edge.to {
            return Err(invalid("self dependency edges are forbidden"));
        }
        Ok(())
    };
    for edge in &advice.model_edges {
        validate_edge(edge)?;
    }
    let model: BTreeSet<_> = advice.model_edges.iter().collect();
    let candidates: BTreeSet<_> = advice
        .trusted_addition_candidates
        .iter()
        .map(|candidate| &candidate.edge)
        .collect();
    for candidate in &advice.trusted_addition_candidates {
        validate_edge(&candidate.edge)?;
        validate_strictly_sorted("candidate.kinds", &candidate.kinds)?;
        validate_sorted_strings("candidate.evidence_refs", &candidate.evidence_refs)?;
        if candidate.kinds.is_empty()
            || candidate.evidence_refs.is_empty()
            || candidate
                .kinds
                .iter()
                .any(|kind| !kind.is_trusted_directed())
        {
            return Err(invalid(
                "trusted candidates require trusted kinds and evidence refs",
            ));
        }
    }
    for hint in &advice.weak_hints {
        validate_text_id("weak_hint.task_a", &hint.task_a)?;
        validate_text_id("weak_hint.task_b", &hint.task_b)?;
        validate_text_id("weak_hint.evidence_ref", &hint.evidence_ref)?;
        if hint.task_a >= hint.task_b || !hint.kind.is_weak() {
            return Err(invalid(
                "weak hints require a sorted distinct task pair and weak kind",
            ));
        }
    }
    let hard: BTreeSet<_> = advice
        .proposed_hard_edges
        .iter()
        .map(|hard| &hard.edge)
        .collect();
    for edge in &advice.model_edges {
        if !hard.contains(edge) {
            return Err(invalid(format!(
                "model edge missing from proposed_hard_edges: {} -> {}",
                edge.from, edge.to
            )));
        }
    }
    for hard in &advice.proposed_hard_edges {
        validate_edge(&hard.edge)?;
        validate_strictly_sorted("hard_edge.kinds", &hard.kinds)?;
        validate_sorted_strings("hard_edge.evidence_refs", &hard.evidence_refs)?;
        let in_model = model.contains(&hard.edge);
        let in_candidate = candidates.contains(&hard.edge);
        let valid = match hard.provenance {
            HybridEdgeProvenance::Model => {
                in_model && !in_candidate && hard.kinds.is_empty() && hard.evidence_refs.is_empty()
            }
            HybridEdgeProvenance::TrustedMechanical => {
                !in_model
                    && in_candidate
                    && !hard.kinds.is_empty()
                    && !hard.evidence_refs.is_empty()
            }
            HybridEdgeProvenance::Both => {
                in_model && in_candidate && !hard.kinds.is_empty() && !hard.evidence_refs.is_empty()
            }
        };
        if !valid || hard.kinds.iter().any(|kind| !kind.is_trusted_directed()) {
            return Err(invalid(format!(
                "hard edge provenance is inconsistent: {} -> {}",
                hard.edge.from, hard.edge.to
            )));
        }
    }
    for conflict in &advice.conflicts {
        validate_sorted_strings("conflict.task_ids", &conflict.task_ids)?;
        if let Some(edge) = &conflict.edge {
            validate_edge(edge)?;
        }
        if let Some(symbol_id) = &conflict.symbol_id {
            validate_text_id("conflict.symbol_id", symbol_id)?;
        }
        validate_sorted_strings("conflict.evidence_refs", &conflict.evidence_refs)?;
    }
    Ok(())
}

fn validate_schedule(advice: &HybridParallelAdvice) -> Result<(), ClosedContractError> {
    let mut batch_of = BTreeMap::new();
    for (position, batch) in advice.schedule.batches.iter().enumerate() {
        if batch.index != position || batch.tasks.is_empty() {
            return Err(invalid(
                "schedule batches must be non-empty and contiguously indexed",
            ));
        }
        validate_sorted_strings("schedule.tasks", &batch.tasks)?;
        for task in &batch.tasks {
            if batch_of.insert(task.as_str(), position).is_some() {
                return Err(invalid(format!(
                    "schedule task must occur exactly once: {task}"
                )));
            }
        }
    }
    let actual_width = advice
        .schedule
        .batches
        .iter()
        .map(|batch| batch.tasks.len())
        .max()
        .unwrap_or(0);
    if advice.schedule.max_width != actual_width {
        return Err(invalid("schedule.max_width does not match batch width"));
    }
    for hard in &advice.proposed_hard_edges {
        let (Some(from_batch), Some(to_batch)) = (
            batch_of.get(hard.edge.from.as_str()),
            batch_of.get(hard.edge.to.as_str()),
        ) else {
            return Err(invalid("hard edge task is absent from schedule"));
        };
        if from_batch >= to_batch {
            return Err(invalid(format!(
                "schedule violates hard edge: {} -> {}",
                hard.edge.from, hard.edge.to
            )));
        }
    }
    validate_strictly_sorted("schedule.serialized", &advice.schedule.serialized)?;
    for reason in &advice.schedule.serialized {
        validate_text_id("serialized.reason", &reason.reason)?;
        let (Some(earlier), Some(later)) = (
            batch_of.get(reason.earlier.as_str()),
            batch_of.get(reason.later.as_str()),
        ) else {
            return Err(invalid("serialized task is absent from schedule"));
        };
        if earlier >= later {
            return Err(invalid("serialized reason must point to a later batch"));
        }
    }
    Ok(())
}

fn validate_artifact_binding(
    name: &str,
    binding: &HybridArtifactBinding,
) -> Result<(), ClosedContractError> {
    let hex = binding
        .digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("Sha256Digest always has the prefix");
    if binding.artifact_ref != format!("artifact://sha256/{hex}") {
        return Err(invalid(format!(
            "{name}.ref must be the content address of {name}.digest"
        )));
    }
    Ok(())
}

fn normalize_project_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    path.strip_prefix("./").unwrap_or(&path).to_string()
}

fn validate_project_path(path: &str) -> Result<(), ClosedContractError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.as_bytes().get(1) == Some(&b':')
    {
        return Err(invalid(
            "write_paths entries must be normalized repository-relative paths",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn edge(from: &str, to: &str) -> HybridTaskEdge {
        HybridTaskEdge {
            from: from.into(),
            to: to.into(),
        }
    }

    fn valid_input() -> HybridParallelAdviceInput {
        let model = edge("a", "b");
        HybridParallelAdviceInput {
            policy_version: HYBRID_DAG_POLICY_VERSION.into(),
            planning_basis: HybridPlanningBasis {
                planning_ref: "planning://g10".into(),
                planning_revision: "7".into(),
                planning_structure_digest: digest('a'),
            },
            source_intent: HybridArtifactBinding::content_addressed(digest('b')),
            repository_bases: vec![HybridRepositoryBase {
                repo_id: "repo:shuttle".into(),
                base_oid: "c".repeat(40),
            }],
            symbol_footprint: HybridArtifactBinding::content_addressed(digest('d')),
            symbol_index_bindings: vec![HybridIndexBinding {
                repo_id: "repo:shuttle".into(),
                index_digest: digest('e'),
            }],
            model_edges: vec![model.clone()],
            trusted_addition_candidates: Vec::new(),
            weak_hints: Vec::new(),
            conflicts: Vec::new(),
            proposed_hard_edges: vec![HybridHardEdge {
                edge: model,
                provenance: HybridEdgeProvenance::Model,
                kinds: Vec::new(),
                evidence_refs: Vec::new(),
            }],
            schedule: HybridSchedule {
                batches: vec![
                    HybridScheduleBatch {
                        index: 0,
                        tasks: vec!["a".into()],
                    },
                    HybridScheduleBatch {
                        index: 1,
                        tasks: vec!["b".into()],
                    },
                ],
                max_width: 1,
                serialized: Vec::new(),
                degraded: false,
            },
            degraded: HybridDegradedState::default(),
        }
    }

    #[test]
    fn hybrid_advice_is_closed_parse_validated_and_deterministic() {
        let advice = HybridParallelAdvice::new(valid_input()).unwrap();
        let first = advice.canonical_json().unwrap();
        assert_eq!(
            advice.canonical_digest().unwrap(),
            HybridParallelAdvice::parse(&first)
                .unwrap()
                .canonical_digest()
                .unwrap()
        );
        let mut value = serde_json::to_value(advice).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("dispatch_authority".into(), serde_json::Value::Bool(true));
        assert!(HybridParallelAdvice::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn hybrid_advice_preserves_every_model_edge() {
        let mut input = valid_input();
        input.proposed_hard_edges.clear();
        assert!(HybridParallelAdvice::new(input).is_err());
    }

    #[test]
    fn weak_evidence_cannot_enter_the_hard_dag() {
        let mut input = valid_input();
        let candidate = HybridTrustedCandidate {
            edge: edge("b", "c"),
            kinds: vec![HybridEvidenceKind::NameMatch],
            evidence_refs: vec!["index://name".into()],
        };
        input.trusted_addition_candidates.push(candidate.clone());
        input.proposed_hard_edges.push(HybridHardEdge {
            edge: candidate.edge,
            provenance: HybridEdgeProvenance::TrustedMechanical,
            kinds: candidate.kinds,
            evidence_refs: candidate.evidence_refs,
        });
        input.schedule.batches.push(HybridScheduleBatch {
            index: 2,
            tasks: vec!["c".into()],
        });
        assert!(HybridParallelAdvice::new(input).is_err());
    }

    #[test]
    fn adoption_decision_serde_is_closed_and_round_trips() {
        // Block verdict with the full ordered set of reasons round-trips
        // byte-for-byte and rejects unknown fields (deny_unknown_fields).
        let decision = HybridAdoptionDecision {
            verdict: HybridAdoptionVerdict::Block,
            reasons: vec![
                HybridAdoptionReason {
                    code: HybridAdoptionReasonCode::CyclicHardDag,
                    count: 0,
                },
                HybridAdoptionReason {
                    code: HybridAdoptionReasonCode::UnresolvedConflicts,
                    count: 3,
                },
                HybridAdoptionReason {
                    code: HybridAdoptionReasonCode::DegradedEvidence,
                    count: 2,
                },
            ],
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: HybridAdoptionDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, back);
        // snake_case wire tags are stable.
        assert!(json.contains("\"verdict\":\"block\""));
        assert!(json.contains("\"cyclic_hard_dag\""));
        assert!(json.contains("\"unresolved_conflicts\""));
        assert!(json.contains("\"degraded_evidence\""));
        // Unknown field is rejected.
        let poisoned = json.replace("\"verdict\"", "\"authority\":true,\"verdict\"");
        assert!(serde_json::from_str::<HybridAdoptionDecision>(&poisoned).is_err());
    }

    #[test]
    fn adopt_verdict_omits_empty_reasons_and_default_is_permissive() {
        let adopt = HybridAdoptionDecision {
            verdict: HybridAdoptionVerdict::Adopt,
            reasons: Vec::new(),
        };
        let json = serde_json::to_string(&adopt).unwrap();
        // reasons is skipped when empty.
        assert!(
            !json.contains("reasons"),
            "empty reasons must be skipped: {json}"
        );
        assert_eq!(json, "{\"verdict\":\"adopt\"}");
        // Default is the permissive baseline.
        let d = HybridAdoptionDecision::default();
        assert_eq!(d, adopt);
    }

    #[test]
    fn projection_defaults_adoption_when_v1_wire_omits_the_field() {
        // A v1 projection (no `adoption` key) still deserializes and gets the
        // permissive default; an unknown top-level key is still rejected.
        let v1 = serde_json::json!({
            "policy_version": HYBRID_DAG_POLICY_VERSION,
            "task_ids": ["a", "b"],
            "model_edges": [],
            "trusted_addition_candidates": [],
            "weak_hints": [],
            "conflicts": [],
            "proposed_hard_edges": [],
            "degradations": [],
            "acyclic": true,
            "automatic_adoption_allowed": true
        });
        let proj: HybridDagProjection = serde_json::from_value(v1.clone()).unwrap();
        assert_eq!(proj.adoption, HybridAdoptionDecision::default());
        assert!(proj.adoption.reasons.is_empty());
        assert_eq!(proj.adoption.verdict, HybridAdoptionVerdict::Adopt);

        // A v2 projection carrying an explicit Block decision round-trips.
        let mut v2 = v1;
        v2["automatic_adoption_allowed"] = serde_json::Value::Bool(false);
        v2["adoption"] = serde_json::json!({
            "verdict": "block",
            "reasons": [{"code": "unresolved_conflicts", "count": 1}]
        });
        let proj2: HybridDagProjection = serde_json::from_value(v2.clone()).unwrap();
        assert_eq!(proj2.adoption.verdict, HybridAdoptionVerdict::Block);
        assert_eq!(proj2.adoption.reasons.len(), 1);
        assert_eq!(
            proj2.adoption.reasons[0].code,
            HybridAdoptionReasonCode::UnresolvedConflicts
        );
        assert_eq!(proj2.adoption.reasons[0].count, 1);

        // Unknown top-level projection field is rejected (deny_unknown_fields).
        let mut poisoned = v2;
        poisoned["landing_authority"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<HybridDagProjection>(poisoned).is_err());
    }

    #[test]
    fn scheduler_footprint_normalizes_paths_and_requires_fresh_exact_state() {
        let footprint = HybridTaskFootprint::new(
            "task-a",
            HybridFootprintSource::PlannerGuess,
            HybridEvidenceState::Partial,
            vec!["./src/lib.rs".into(), "src\\main.rs".into()],
        )
        .unwrap();
        assert_eq!(footprint.write_paths, vec!["src/lib.rs", "src/main.rs"]);

        assert!(HybridTaskFootprint::new(
            "task-a",
            HybridFootprintSource::PlannerDeclaredValidated,
            HybridEvidenceState::Stale,
            vec!["src/lib.rs".into()],
        )
        .is_err());
    }
}
