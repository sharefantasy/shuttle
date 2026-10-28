//! coding_flow — reference-form Rust anchors for the external orchestrator's **frozen coding-flow
//! contract family**.
//!
//! The external orchestrator owns and freezes these nine schemas; shuttle is a DOWNSTREAM consumer. The
//! types here mirror the frozen JSON Schemas *fail-closed*: every struct is
//! `#[serde(deny_unknown_fields)]` (so an arbitrary field — e.g. a raw `shell`
//! on a quality command, or an extra field on an agent context — is rejected at
//! deserialize), every enum rejects unknown variants, and [`CodingFlowContract::
//! validate`] re-checks the cross-field / semantic invariants JSON Schema
//! expresses through `allOf`/`if`/`then` and the external orchestrator's own semantic validator.
//!
//! shuttle never re-freezes these. It consumes them by ref + digest and copies external
//! digests verbatim. The nine frozen digests are
//! captured in [`schema_digests`] so any drift against the external orchestrator's `freeze-manifest.json`
//! is a compile-time-visible constant, reconciled on upstream sync.

use serde::{Deserialize, Serialize};

use super::contracts::ArtifactRef;
use super::digest::Sha256Digest;

/// The nine frozen contract digests from the external orchestrator's `freeze-manifest.json`
/// (`coding_flow_contract_freeze_manifest/v1`, task `C0`). shuttle copies these
/// verbatim; a mismatch on upstream sync is a deliberate, visible edit.
pub mod schema_digests {
    /// `shuttle_code_delivery_handoff/v1`.
    pub const HANDOFF: &str =
        "sha256:ad3dbf731381ccf72ee731ea11cfa515f18f1fe2ca2f233566a538600ae9cfb4";
    /// `code_delivery_manifest/v1`.
    pub const MANIFEST: &str =
        "sha256:d50d005cd63d38f67489ee0a9d4b1802596fd4d006309e4a3e524be06220dc6b";
    /// `code_delivery_packet_plan/v1`.
    pub const PACKET_PLAN: &str =
        "sha256:a180c8e3a11f8cf433c52ced0a25d112065ddc096c884f1b8802e731140fd05c";
    /// `coding_agent_context_manifest/v1`.
    pub const CONTEXT_MANIFEST: &str =
        "sha256:e03cc91518a2be5c0942ae917cbec2f1d8c6a65b4510a522d461a0446674b5f1";
    /// `coding_attempt_result/v1` (owner: agent-provider — the schema shuttle emits).
    pub const ATTEMPT_RESULT: &str =
        "sha256:43e848cb830c670f680bd2a010b9d184df0c83bc770f5b43a1162637a225ff3c";
    /// `code_delivery_quality_plan/v1`.
    pub const QUALITY_PLAN: &str =
        "sha256:ba634af4415651132125b2e4f9da3d339d104cb852409920d45d2c6885db7b29";
    /// `code_delivery_quality_receipt/v1` (owner: quality-executor).
    pub const QUALITY_RECEIPT: &str =
        "sha256:b4826fd8625e637fc11f73167d47c26370d1f9d8bc431a4daa3ee6c4688f770b";
    /// `code_delivery_repair_decision/v1`.
    pub const REPAIR_DECISION: &str =
        "sha256:d43c2ebcb5d1f74ab46d7bb1211d89f1e90e9f868978a4661a937fb2fd9b182c";
    /// `code_delivery_receipt/v1`.
    pub const RECEIPT: &str =
        "sha256:466d6c67c24c693dbf2c3b4d68d21c906294fdd14b5b325e5656ab37add51c89";
}

/// Why a coding-flow contract failed fail-closed validation. Every variant maps
/// to a specific frozen-schema constraint or external semantic rule.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodingFlowError {
    #[error("{field}: schema_version must be {expected:?}, got {actual:?}")]
    SchemaVersion {
        field: &'static str,
        expected: &'static str,
        actual: String,
    },
    #[error("{0}: must be a non-empty string")]
    EmptyString(&'static str),
    #[error("{0}: id must be 1..=256 chars")]
    IdLength(&'static str),
    #[error("{field}: must have at least {min} item(s)")]
    TooFewItems { field: &'static str, min: usize },
    #[error("{field}: must have at most {max} item(s)")]
    TooManyItems { field: &'static str, max: usize },
    #[error("{0}: items must be unique")]
    NotUnique(&'static str),
    #[error("{field}: expected the frozen constant {expected}")]
    ConstMismatch {
        field: &'static str,
        expected: &'static str,
    },
    #[error("{field}: value {value} is outside [{min}, {max}]")]
    OutOfRange {
        field: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },
    #[error("path must be relative and non-empty (no leading `/`): {0:?}")]
    BadPath(String),
    #[error("commit_oid must match ^[0-9a-f]{{40,64}}$: {0:?}")]
    BadCommitOid(String),
    #[error("semantic: approval {0} drift (must equal the top-level digest)")]
    ApprovalDigestDrift(&'static str),
    #[error(
        "semantic: manifest attempt budget drift (max_attempts != repair_policy.max_attempts)"
    )]
    AttemptBudgetDrift,
    #[error("semantic: slice {slice_id} expands write scope {path} beyond source_write_scopes")]
    ScopeExpansion { slice_id: String, path: String },
    #[error("semantic: slice sequence is not contiguous 1..=n")]
    NonContiguousSequence,
    #[error("semantic: status=completed but scope_violations is non-empty")]
    CompletedWithScopeViolations,
    #[error(
        "semantic: status=completed but provider_terminal_receipt.provider_status != completed"
    )]
    CompletedProviderStatusMismatch,
    #[error("semantic: aggregate_verdict=passed but blocking_failures is non-empty")]
    PassedWithFailuresListed,
    #[error("semantic: aggregate_verdict=passed but a blocking command did not pass")]
    PassedWithBlockingFailure,
    #[error("semantic: aggregate_verdict=failed but blocking_failures is empty")]
    FailedWithoutFailures,
    #[error("semantic: retry decision requires a next_attempt_id")]
    RepairNextAttemptMissing,
    #[error("semantic: non-retry decision must not carry a next_attempt_id")]
    RepairNextAttemptForbidden,
    #[error(
        "semantic: retry overwrites the current attempt (next_attempt_id == current_attempt_id)"
    )]
    RepairOverwritesAttempt,
    #[error("semantic: retry exceeds the attempt budget (attempt_number >= max_attempts)")]
    RepairExceedsBudget,
    #[error("semantic: completion_verdict=completed but reconciliation effect_state != applied")]
    CompletedEffectNotApplied,
}

/// Every mirrored contract implements fail-closed validation of the constraints
/// JSON Schema cannot express inline (cross-field, sequence, digest-binding).
pub trait CodingFlowContract {
    /// The frozen `schema_version` const string this type mirrors.
    const SCHEMA_VERSION: &'static str;
    /// Re-check every semantic / cross-field invariant. Structural constraints
    /// (unknown fields, digest/path/enum shape) are already enforced at
    /// deserialize; this catches the `allOf`/`if`/`then` + external semantic rules.
    fn validate(&self) -> Result<(), CodingFlowError>;
}

// ---------------------------------------------------------------------------
// scalar newtypes mirroring external `$defs`
// ---------------------------------------------------------------------------

/// A `$defs/path`: relative (no leading `/`), non-empty. Validated at
/// deserialize so an absolute or empty path can never enter a contract.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CodingFlowPath(String);

impl CodingFlowPath {
    /// Validate and wrap a relative path.
    pub fn parse(s: impl Into<String>) -> Result<Self, CodingFlowError> {
        let s = s.into();
        if s.is_empty() || s.starts_with('/') {
            return Err(CodingFlowError::BadPath(s));
        }
        Ok(CodingFlowPath(s))
    }
    /// The verbatim relative path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodingFlowPath {
    type Error = CodingFlowError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        CodingFlowPath::parse(s)
    }
}
impl From<CodingFlowPath> for String {
    fn from(p: CodingFlowPath) -> String {
        p.0
    }
}

/// A `commit_oid`: 40..=64 lowercase hex chars. Validated at deserialize.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitOid(String);

impl CommitOid {
    /// Validate and wrap a Git commit oid.
    pub fn parse(s: impl Into<String>) -> Result<Self, CodingFlowError> {
        let s = s.into();
        let ok = (40..=64).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !ok {
            return Err(CodingFlowError::BadCommitOid(s));
        }
        Ok(CommitOid(s))
    }
    /// The verbatim oid string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitOid {
    type Error = CodingFlowError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        CommitOid::parse(s)
    }
}
impl From<CommitOid> for String {
    fn from(o: CommitOid) -> String {
        o.0
    }
}

/// A `$defs/write_scope`: `{repo_id, paths}` with a non-empty unique `paths`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteScope {
    pub repo_id: String,
    pub paths: Vec<String>,
}

impl WriteScope {
    fn validate(&self, field: &'static str) -> Result<(), CodingFlowError> {
        require_id(field, &self.repo_id)?;
        require_min(field, self.paths.len(), 1)?;
        require_unique(field, &self.paths)?;
        for p in &self.paths {
            require_text(field, p)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn require_text(field: &'static str, s: &str) -> Result<(), CodingFlowError> {
    if s.is_empty() {
        return Err(CodingFlowError::EmptyString(field));
    }
    Ok(())
}

fn require_id(field: &'static str, s: &str) -> Result<(), CodingFlowError> {
    if s.is_empty() || s.chars().count() > 256 {
        return Err(CodingFlowError::IdLength(field));
    }
    Ok(())
}

fn require_min(field: &'static str, len: usize, min: usize) -> Result<(), CodingFlowError> {
    if len < min {
        return Err(CodingFlowError::TooFewItems { field, min });
    }
    Ok(())
}

fn require_max(field: &'static str, len: usize, max: usize) -> Result<(), CodingFlowError> {
    if len > max {
        return Err(CodingFlowError::TooManyItems { field, max });
    }
    Ok(())
}

fn require_unique<T: Eq + std::hash::Hash>(
    field: &'static str,
    items: &[T],
) -> Result<(), CodingFlowError> {
    let mut seen = std::collections::HashSet::with_capacity(items.len());
    for item in items {
        if !seen.insert(item) {
            return Err(CodingFlowError::NotUnique(field));
        }
    }
    Ok(())
}

fn range_i64(field: &'static str, value: i64, min: i64, max: i64) -> Result<(), CodingFlowError> {
    if value < min || value > max {
        return Err(CodingFlowError::OutOfRange {
            field,
            value,
            min,
            max,
        });
    }
    Ok(())
}

fn require_text_list(
    field: &'static str,
    items: &[String],
    min: usize,
) -> Result<(), CodingFlowError> {
    require_min(field, items.len(), min)?;
    require_unique(field, items)?;
    for item in items {
        require_text(field, item)?;
    }
    Ok(())
}

fn validate_artifact(field: &'static str, a: &ArtifactRef) -> Result<(), CodingFlowError> {
    require_text(field, &a.artifact_ref)
}

fn validate_artifacts(
    field: &'static str,
    items: &[ArtifactRef],
    min: usize,
) -> Result<(), CodingFlowError> {
    require_min(field, items.len(), min)?;
    for a in items {
        validate_artifact(field, a)?;
    }
    Ok(())
}

/// Whether a `child` write-scope glob is contained by a frozen `parent` glob.
/// Mirrors the external orchestrator's `_path_is_within` exactly (equal, or parent ends `/**` and the
/// child sits under `parent[:-3]/`).
pub fn glob_within(child: &str, parent: &str) -> bool {
    if child == parent {
        return true;
    }
    if let Some(prefix) = parent.strip_suffix("/**") {
        let base = prefix.trim_end_matches('/');
        return child.starts_with(&format!("{base}/"));
    }
    false
}

fn check_schema_version(
    field: &'static str,
    expected: &'static str,
    actual: &str,
) -> Result<(), CodingFlowError> {
    if actual != expected {
        return Err(CodingFlowError::SchemaVersion {
            field,
            expected,
            actual: actual.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// enums (unknown variants are rejected at deserialize = fail-closed)
// ---------------------------------------------------------------------------

/// `formal_assurance.level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormalLevel {
    F0,
    F1,
    F2,
    F3,
}

/// `approval_receipt.decision` const `approved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
}

/// `repair_policy.unknown_effect_action` const `reconcile_before_retry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairUnknownEffect {
    ReconcileBeforeRetry,
}

/// `integration_policy.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMode {
    ChildFanIn,
    PerTask,
}

/// `integration_policy.required_quality_verdict` const `passed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredQualityVerdict {
    Passed,
}

/// `integration_policy.unknown_effect_action` const `reconcile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationUnknownEffect {
    Reconcile,
}

/// `slice.context_budget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBudget {
    Tight,
    Medium,
    Wide,
}

/// `force_report.fit_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitStatus {
    InsideSweetSpot,
    NearEdge,
    OutsideSweetSpot,
}

/// `force_report.recommended_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Keep,
    Split,
    IncreaseContext,
    Block,
}

/// `runtime_mutation_policy.authority_owner` const `coding-flow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorityOwner {
    #[serde(rename = "coding-flow")]
    CodingFlow,
}

/// `coding_attempt_result.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Completed,
    Partial,
    Failed,
    Cancelled,
}

/// `provider_terminal_receipt.provider_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

/// `quality command.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Build,
    Lint,
    Test,
    Coverage,
    Formal,
    Benchmark,
    Package,
}

/// `quality command.required_evidence[]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredEvidence {
    ExitCode,
    StdoutDigest,
    StderrDigest,
    ReportArtifact,
    ModelTrace,
}

/// `aggregate_policy.blocking_failure_verdict` const `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingFailureVerdict {
    Failed,
}

/// `command_receipt.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
}

/// `quality_receipt.aggregate_verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateVerdict {
    Passed,
    Failed,
    Incomplete,
    Cancelled,
}

/// `repair_decision.failure_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ResultContract,
    ScopeViolation,
    Build,
    Lint,
    Test,
    Coverage,
    Formal,
    IntegrationConflict,
    ProviderUnknown,
    DesignDrift,
    Cancelled,
    Other,
}

/// `repair_decision.decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairDecisionKind {
    RetrySameProfile,
    SpawnNewProfile,
    ReturnToProjectFlow,
    BlockForHuman,
    TerminalFailure,
}

impl RepairDecisionKind {
    /// Whether this decision spawns a follow-up attempt (needs a next id).
    pub fn is_retry(self) -> bool {
        matches!(self, Self::RetrySameProfile | Self::SpawnNewProfile)
    }
}

/// `reconciliation_receipt.effect_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    Applied,
    NotApplied,
    Unknown,
}

/// `code_delivery_receipt.completion_verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionVerdict {
    Completed,
    Blocked,
    Cancelled,
    Failed,
}

// ---------------------------------------------------------------------------
// 1. shuttle_code_delivery_handoff/v1
// ---------------------------------------------------------------------------

/// `formal_assurance` sub-object of the handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormalAssurance {
    pub level: FormalLevel,
    pub affected_invariants: Vec<String>,
    pub positive_obligations: Vec<String>,
}

/// `approval_receipt` sub-object of the handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalReceipt {
    pub receipt_id: String,
    pub approved_by: String,
    pub approved_at: String,
    pub decision: ApprovalDecision,
    pub planning_structure_digest: Sha256Digest,
    pub workspace_strategy_digest: Sha256Digest,
}

/// `shuttle_code_delivery_handoff/v1` — the external orchestrator's authorized handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFlowCodeDeliveryHandoff {
    pub schema_version: String,
    pub handoff_id: String,
    pub program_id: String,
    pub child_dag_id: String,
    pub task_id: String,
    pub planning_structure_digest: Sha256Digest,
    pub spec_ref: String,
    pub task_dag_ref: String,
    pub objective: String,
    pub scope: Vec<String>,
    pub non_goals: Vec<String>,
    pub acceptance: Vec<String>,
    pub upstream_receipts: Vec<ArtifactRef>,
    pub workspace_strategy_ref: String,
    pub workspace_strategy_digest: Sha256Digest,
    pub write_scopes: Vec<WriteScope>,
    pub quality_policy_ref: String,
    pub formal_assurance: FormalAssurance,
    pub approval_receipt: ApprovalReceipt,
}

impl CodingFlowContract for ProjectFlowCodeDeliveryHandoff {
    const SCHEMA_VERSION: &'static str = "shuttle_code_delivery_handoff/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("handoff_id", &self.handoff_id)?;
        require_id("program_id", &self.program_id)?;
        require_id("child_dag_id", &self.child_dag_id)?;
        require_id("task_id", &self.task_id)?;
        require_text("spec_ref", &self.spec_ref)?;
        require_text("task_dag_ref", &self.task_dag_ref)?;
        require_text("objective", &self.objective)?;
        require_text_list("scope", &self.scope, 0)?;
        require_text_list("non_goals", &self.non_goals, 0)?;
        require_text_list("acceptance", &self.acceptance, 1)?;
        validate_artifacts("upstream_receipts", &self.upstream_receipts, 0)?;
        require_text("workspace_strategy_ref", &self.workspace_strategy_ref)?;
        require_min("write_scopes", self.write_scopes.len(), 1)?;
        for ws in &self.write_scopes {
            ws.validate("write_scopes")?;
        }
        require_text("quality_policy_ref", &self.quality_policy_ref)?;
        require_text_list(
            "formal_assurance.affected_invariants",
            &self.formal_assurance.affected_invariants,
            0,
        )?;
        require_text_list(
            "formal_assurance.positive_obligations",
            &self.formal_assurance.positive_obligations,
            0,
        )?;
        require_id(
            "approval_receipt.receipt_id",
            &self.approval_receipt.receipt_id,
        )?;
        require_id(
            "approval_receipt.approved_by",
            &self.approval_receipt.approved_by,
        )?;
        require_text(
            "approval_receipt.approved_at",
            &self.approval_receipt.approved_at,
        )?;
        // semantic: approval digests must equal the top-level digests.
        if self.approval_receipt.planning_structure_digest != self.planning_structure_digest {
            return Err(CodingFlowError::ApprovalDigestDrift(
                "planning_structure_digest",
            ));
        }
        if self.approval_receipt.workspace_strategy_digest != self.workspace_strategy_digest {
            return Err(CodingFlowError::ApprovalDigestDrift(
                "workspace_strategy_digest",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 2. code_delivery_manifest/v1
// ---------------------------------------------------------------------------

/// `repair_policy` of the delivery manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairPolicy {
    pub max_attempts: i64,
    pub same_failure_fingerprint_limit: i64,
    pub unknown_effect_action: RepairUnknownEffect,
}

/// `integration_policy` of the delivery manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationPolicy {
    pub mode: IntegrationMode,
    pub required_quality_verdict: RequiredQualityVerdict,
    pub unknown_effect_action: IntegrationUnknownEffect,
}

/// `cancellation_domain` of the delivery manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationDomain {
    pub domain_id: String,
    pub epoch: i64,
}

/// `code_delivery_manifest/v1` — coding-flow's per-delivery execution manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryManifest {
    pub schema_version: String,
    pub delivery_id: String,
    pub handoff: ArtifactRef,
    pub planning_structure_digest: Sha256Digest,
    pub workspace_bundle: ArtifactRef,
    pub coding_package: ArtifactRef,
    pub prompt_package: ArtifactRef,
    pub packet_plan: ArtifactRef,
    pub quality_plan: ArtifactRef,
    pub repair_policy: RepairPolicy,
    pub integration_policy: IntegrationPolicy,
    pub provider_capability_snapshot: ArtifactRef,
    pub max_attempts: i64,
    pub deadline: String,
    pub cancellation_domain: CancellationDomain,
}

impl CodingFlowContract for CodeDeliveryManifest {
    const SCHEMA_VERSION: &'static str = "code_delivery_manifest/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("delivery_id", &self.delivery_id)?;
        validate_artifact("handoff", &self.handoff)?;
        validate_artifact("workspace_bundle", &self.workspace_bundle)?;
        validate_artifact("coding_package", &self.coding_package)?;
        validate_artifact("prompt_package", &self.prompt_package)?;
        validate_artifact("packet_plan", &self.packet_plan)?;
        validate_artifact("quality_plan", &self.quality_plan)?;
        validate_artifact(
            "provider_capability_snapshot",
            &self.provider_capability_snapshot,
        )?;
        range_i64(
            "repair_policy.max_attempts",
            self.repair_policy.max_attempts,
            1,
            20,
        )?;
        range_i64(
            "repair_policy.same_failure_fingerprint_limit",
            self.repair_policy.same_failure_fingerprint_limit,
            1,
            10,
        )?;
        range_i64("max_attempts", self.max_attempts, 1, 20)?;
        require_text("deadline", &self.deadline)?;
        require_id(
            "cancellation_domain.domain_id",
            &self.cancellation_domain.domain_id,
        )?;
        range_i64(
            "cancellation_domain.epoch",
            self.cancellation_domain.epoch,
            1,
            i64::MAX,
        )?;
        // semantic: one authoritative attempt budget.
        if self.max_attempts != self.repair_policy.max_attempts {
            return Err(CodingFlowError::AttemptBudgetDrift);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 3. code_delivery_packet_plan/v1
// ---------------------------------------------------------------------------

/// One slice of the packet plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketSlice {
    pub slice_id: String,
    pub sequence: i64,
    pub objective: String,
    pub write_scopes: Vec<WriteScope>,
    pub context_budget: ContextBudget,
    pub acceptance: Vec<String>,
    pub required: bool,
}

/// `force_report` of the packet plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForceReport {
    pub fit_status: FitStatus,
    pub overall_score: i64,
    pub recommended_action: RecommendedAction,
}

/// `runtime_mutation_policy` of the packet plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeMutationPolicy {
    pub split_merge_requires_authority_witness: bool,
    pub authority_owner: AuthorityOwner,
}

/// `code_delivery_packet_plan/v1` — the estimator's slice plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryPacketPlan {
    pub schema_version: String,
    pub plan_id: String,
    pub estimator_id: String,
    pub estimator_version: String,
    pub planning_structure_digest: Sha256Digest,
    pub package_id: String,
    pub source_write_scopes: Vec<WriteScope>,
    pub slices: Vec<PacketSlice>,
    pub agent_profile_ref: String,
    pub force_report: ForceReport,
    pub runtime_mutation_policy: RuntimeMutationPolicy,
}

impl CodingFlowContract for CodeDeliveryPacketPlan {
    const SCHEMA_VERSION: &'static str = "code_delivery_packet_plan/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("plan_id", &self.plan_id)?;
        require_id("estimator_id", &self.estimator_id)?;
        require_id("estimator_version", &self.estimator_version)?;
        require_id("package_id", &self.package_id)?;
        require_min("source_write_scopes", self.source_write_scopes.len(), 1)?;
        for ws in &self.source_write_scopes {
            ws.validate("source_write_scopes")?;
        }
        require_min("slices", self.slices.len(), 1)?;
        require_text("agent_profile_ref", &self.agent_profile_ref)?;
        range_i64(
            "force_report.overall_score",
            self.force_report.overall_score,
            0,
            100,
        )?;
        // runtime_mutation_policy consts.
        if !self
            .runtime_mutation_policy
            .split_merge_requires_authority_witness
        {
            return Err(CodingFlowError::ConstMismatch {
                field: "runtime_mutation_policy.split_merge_requires_authority_witness",
                expected: "true",
            });
        }
        for slice in &self.slices {
            require_id("slices.slice_id", &slice.slice_id)?;
            range_i64("slices.sequence", slice.sequence, 1, i64::MAX)?;
            require_text("slices.objective", &slice.objective)?;
            require_min("slices.write_scopes", slice.write_scopes.len(), 1)?;
            for ws in &slice.write_scopes {
                ws.validate("slices.write_scopes")?;
            }
            require_text_list("slices.acceptance", &slice.acceptance, 1)?;
        }
        // semantic: each slice write path must sit inside source_write_scopes.
        for slice in &self.slices {
            for repo in &slice.write_scopes {
                let source = self
                    .source_write_scopes
                    .iter()
                    .filter(|s| s.repo_id == repo.repo_id);
                let source_paths: Vec<&String> = source.flat_map(|s| s.paths.iter()).collect();
                for path in &repo.paths {
                    let within = source_paths.iter().any(|parent| glob_within(path, parent));
                    if !within {
                        return Err(CodingFlowError::ScopeExpansion {
                            slice_id: slice.slice_id.clone(),
                            path: path.clone(),
                        });
                    }
                }
            }
        }
        // semantic: sequences must be contiguous 1..=n in declaration order.
        let expected: Vec<i64> = (1..=self.slices.len() as i64).collect();
        let actual: Vec<i64> = self.slices.iter().map(|s| s.sequence).collect();
        if actual != expected {
            return Err(CodingFlowError::NonContiguousSequence);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 4. coding_agent_context_manifest/v1
// ---------------------------------------------------------------------------

/// `workspace` sub-object `{ref, digest}` of the agent context manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
    #[serde(rename = "ref")]
    pub workspace_ref: String,
    pub digest: Sha256Digest,
}

/// Optional `prior_attempt` sub-object of the agent context manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorAttempt {
    pub attempt_id: String,
    pub failure_evidence: Vec<ArtifactRef>,
}

/// `coding_agent_context_manifest/v1` — the per-attempt agent brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAgentContextManifest {
    pub schema_version: String,
    pub context_id: String,
    pub delivery_id: String,
    pub attempt_id: String,
    pub role: String,
    pub objective: String,
    pub non_goals: Vec<String>,
    pub source_refs: Vec<ArtifactRef>,
    pub workspace: WorkspaceRef,
    pub allowed_write_scopes: Vec<WriteScope>,
    pub upstream_receipts: Vec<ArtifactRef>,
    pub acceptance: Vec<String>,
    pub quality_command_ids: Vec<String>,
    pub formal_obligations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_attempt: Option<PriorAttempt>,
    pub result_schema_ref: String,
}

impl CodingFlowContract for CodingAgentContextManifest {
    const SCHEMA_VERSION: &'static str = "coding_agent_context_manifest/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("context_id", &self.context_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_id("attempt_id", &self.attempt_id)?;
        require_text("role", &self.role)?;
        require_text("objective", &self.objective)?;
        require_text_list("non_goals", &self.non_goals, 0)?;
        validate_artifacts("source_refs", &self.source_refs, 1)?;
        require_text("workspace.ref", &self.workspace.workspace_ref)?;
        require_min("allowed_write_scopes", self.allowed_write_scopes.len(), 1)?;
        for ws in &self.allowed_write_scopes {
            ws.validate("allowed_write_scopes")?;
        }
        validate_artifacts("upstream_receipts", &self.upstream_receipts, 0)?;
        require_text_list("acceptance", &self.acceptance, 1)?;
        require_min("quality_command_ids", self.quality_command_ids.len(), 1)?;
        require_unique("quality_command_ids", &self.quality_command_ids)?;
        for id in &self.quality_command_ids {
            require_id("quality_command_ids", id)?;
        }
        require_text_list("formal_obligations", &self.formal_obligations, 0)?;
        require_text("result_schema_ref", &self.result_schema_ref)?;
        if let Some(prior) = &self.prior_attempt {
            require_id("prior_attempt.attempt_id", &prior.attempt_id)?;
            validate_artifacts("prior_attempt.failure_evidence", &prior.failure_evidence, 1)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 5. coding_attempt_result/v1  (the schema shuttle emits)
// ---------------------------------------------------------------------------

/// One `commits[]` entry of a coding attempt result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptCommit {
    pub repo_id: String,
    pub change_id: String,
    pub commit_oid: CommitOid,
}

/// `provider_terminal_receipt` sub-object of a coding attempt result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderTerminalReceipt {
    #[serde(rename = "ref")]
    pub receipt_ref: String,
    pub digest: Sha256Digest,
    pub provider_status: ProviderTerminalStatus,
}

/// `coding_attempt_result/v1` — the provider's terminal result of one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAttemptResult {
    pub schema_version: String,
    pub result_id: String,
    pub delivery_id: String,
    pub attempt_id: String,
    pub provider_run_id: String,
    pub workspace_ref: String,
    pub status: AttemptStatus,
    pub changed_files: Vec<CodingFlowPath>,
    pub commits: Vec<AttemptCommit>,
    pub generated_tests: Vec<CodingFlowPath>,
    pub unresolved_items: Vec<String>,
    pub scope_violations: Vec<CodingFlowPath>,
    pub artifact_digest: Sha256Digest,
    pub provider_terminal_receipt: ProviderTerminalReceipt,
}

impl CodingFlowContract for CodingAttemptResult {
    const SCHEMA_VERSION: &'static str = "coding_attempt_result/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("result_id", &self.result_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_id("attempt_id", &self.attempt_id)?;
        require_id("provider_run_id", &self.provider_run_id)?;
        require_text("workspace_ref", &self.workspace_ref)?;
        require_unique("changed_files", &self.changed_files)?;
        require_unique("generated_tests", &self.generated_tests)?;
        for c in &self.commits {
            require_id("commits.repo_id", &c.repo_id)?;
            require_id("commits.change_id", &c.change_id)?;
        }
        for item in &self.unresolved_items {
            require_text("unresolved_items", item)?;
        }
        require_text(
            "provider_terminal_receipt.ref",
            &self.provider_terminal_receipt.receipt_ref,
        )?;
        // allOf: status=completed ⟹ no scope violations, provider completed.
        if self.status == AttemptStatus::Completed {
            if !self.scope_violations.is_empty() {
                return Err(CodingFlowError::CompletedWithScopeViolations);
            }
            if self.provider_terminal_receipt.provider_status != ProviderTerminalStatus::Completed {
                return Err(CodingFlowError::CompletedProviderStatusMismatch);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 6. code_delivery_quality_plan/v1
// ---------------------------------------------------------------------------

/// One command of the quality plan. No raw shell string is modeled — a `shell`
/// field is rejected by `deny_unknown_fields` (external fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityCommand {
    pub command_id: String,
    pub kind: CommandKind,
    pub blocking: bool,
    pub timeout_seconds: i64,
    pub required_evidence: Vec<RequiredEvidence>,
}

/// `aggregate_policy` of the quality plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityAggregatePolicy {
    pub blocking_failure_verdict: BlockingFailureVerdict,
    pub all_blocking_must_pass: bool,
}

/// `code_delivery_quality_plan/v1` — the declarative quality command set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryQualityPlan {
    pub schema_version: String,
    pub plan_id: String,
    pub delivery_id: String,
    pub workspace_ref: String,
    pub commands: Vec<QualityCommand>,
    pub aggregate_policy: QualityAggregatePolicy,
}

impl CodingFlowContract for CodeDeliveryQualityPlan {
    const SCHEMA_VERSION: &'static str = "code_delivery_quality_plan/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("plan_id", &self.plan_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_text("workspace_ref", &self.workspace_ref)?;
        require_min("commands", self.commands.len(), 1)?;
        for cmd in &self.commands {
            require_id("commands.command_id", &cmd.command_id)?;
            range_i64("commands.timeout_seconds", cmd.timeout_seconds, 1, 86400)?;
            require_min("commands.required_evidence", cmd.required_evidence.len(), 1)?;
            require_unique("commands.required_evidence", &cmd.required_evidence)?;
        }
        if !self.aggregate_policy.all_blocking_must_pass {
            return Err(CodingFlowError::ConstMismatch {
                field: "aggregate_policy.all_blocking_must_pass",
                expected: "true",
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 7. code_delivery_quality_receipt/v1
// ---------------------------------------------------------------------------

/// One `command_receipts[]` entry of the quality receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub command_id: String,
    pub status: CommandStatus,
    pub blocking: bool,
    pub exit_code: Option<i64>,
    pub stdout_digest: Sha256Digest,
    pub stderr_digest: Sha256Digest,
    pub evidence: Vec<ArtifactRef>,
}

/// `code_delivery_quality_receipt/v1` — the quality executor's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryQualityReceipt {
    pub schema_version: String,
    pub receipt_id: String,
    pub delivery_id: String,
    pub attempt_id: String,
    pub quality_plan: ArtifactRef,
    pub command_receipts: Vec<CommandReceipt>,
    pub aggregate_verdict: AggregateVerdict,
    pub blocking_failures: Vec<String>,
}

impl CodingFlowContract for CodeDeliveryQualityReceipt {
    const SCHEMA_VERSION: &'static str = "code_delivery_quality_receipt/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("receipt_id", &self.receipt_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_id("attempt_id", &self.attempt_id)?;
        validate_artifact("quality_plan", &self.quality_plan)?;
        require_min("command_receipts", self.command_receipts.len(), 1)?;
        for cr in &self.command_receipts {
            require_id("command_receipts.command_id", &cr.command_id)?;
            validate_artifacts("command_receipts.evidence", &cr.evidence, 0)?;
        }
        require_unique("blocking_failures", &self.blocking_failures)?;
        for id in &self.blocking_failures {
            require_id("blocking_failures", id)?;
        }
        let failed_blocking = self
            .command_receipts
            .iter()
            .any(|cr| cr.blocking && cr.status != CommandStatus::Passed);
        match self.aggregate_verdict {
            AggregateVerdict::Passed => {
                // allOf: passed ⟹ maxItems 0.
                if !self.blocking_failures.is_empty() {
                    return Err(CodingFlowError::PassedWithFailuresListed);
                }
                // semantic: passed aggregate must contain no failed blocking cmd.
                if failed_blocking {
                    return Err(CodingFlowError::PassedWithBlockingFailure);
                }
            }
            AggregateVerdict::Failed => {
                // allOf: failed ⟹ minItems 1.
                if self.blocking_failures.is_empty() {
                    return Err(CodingFlowError::FailedWithoutFailures);
                }
            }
            AggregateVerdict::Incomplete | AggregateVerdict::Cancelled => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 8. code_delivery_repair_decision/v1
// ---------------------------------------------------------------------------

/// `code_delivery_repair_decision/v1` — coding-flow's per-attempt repair ruling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryRepairDecision {
    pub schema_version: String,
    pub decision_id: String,
    pub delivery_id: String,
    pub current_attempt_id: String,
    pub attempt_number: i64,
    pub max_attempts: i64,
    pub failure_class: FailureClass,
    pub failure_fingerprint: Sha256Digest,
    pub same_fingerprint_count: i64,
    pub decision: RepairDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt_id: Option<String>,
    pub evidence: Vec<ArtifactRef>,
    pub rationale: String,
}

impl CodingFlowContract for CodeDeliveryRepairDecision {
    const SCHEMA_VERSION: &'static str = "code_delivery_repair_decision/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("decision_id", &self.decision_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_id("current_attempt_id", &self.current_attempt_id)?;
        range_i64("attempt_number", self.attempt_number, 1, i64::MAX)?;
        range_i64("max_attempts", self.max_attempts, 1, i64::MAX)?;
        range_i64(
            "same_fingerprint_count",
            self.same_fingerprint_count,
            1,
            i64::MAX,
        )?;
        validate_artifacts("evidence", &self.evidence, 1)?;
        require_text("rationale", &self.rationale)?;
        if let Some(id) = &self.next_attempt_id {
            require_id("next_attempt_id", id)?;
        }
        if self.decision.is_retry() {
            // if/then: retry ⟹ next_attempt_id required.
            let Some(next) = &self.next_attempt_id else {
                return Err(CodingFlowError::RepairNextAttemptMissing);
            };
            // semantic: retry must not overwrite the current attempt.
            if next == &self.current_attempt_id {
                return Err(CodingFlowError::RepairOverwritesAttempt);
            }
            // semantic: retry must be within budget.
            if self.attempt_number >= self.max_attempts {
                return Err(CodingFlowError::RepairExceedsBudget);
            }
        } else {
            // if/then: non-retry ⟹ next_attempt_id forbidden.
            if self.next_attempt_id.is_some() {
                return Err(CodingFlowError::RepairNextAttemptForbidden);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 9. code_delivery_receipt/v1
// ---------------------------------------------------------------------------

/// `reconciliation_receipt` sub-object of the delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationReceipt {
    #[serde(rename = "ref")]
    pub receipt_ref: String,
    pub digest: Sha256Digest,
    pub effect_state: EffectState,
}

/// One `final_refs[]` entry of the delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalRef {
    pub repo_id: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub commit_oid: CommitOid,
}

/// `code_delivery_receipt/v1` — coding-flow's terminal delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeDeliveryReceipt {
    pub schema_version: String,
    pub receipt_id: String,
    pub delivery_id: String,
    pub program_id: String,
    pub child_dag_id: String,
    pub task_id: String,
    pub planning_structure_digest: Sha256Digest,
    pub workspace: ArtifactRef,
    pub attempts: Vec<ArtifactRef>,
    pub terminal_coding_result: ArtifactRef,
    pub quality_receipts: Vec<ArtifactRef>,
    pub integration_receipt: ArtifactRef,
    pub land_receipt: ArtifactRef,
    pub reconciliation_receipt: ReconciliationReceipt,
    pub final_refs: Vec<FinalRef>,
    pub residual_risks: Vec<String>,
    pub completion_verdict: CompletionVerdict,
    pub completed_at: String,
}

impl CodingFlowContract for CodeDeliveryReceipt {
    const SCHEMA_VERSION: &'static str = "code_delivery_receipt/v1";
    fn validate(&self) -> Result<(), CodingFlowError> {
        check_schema_version("schema_version", Self::SCHEMA_VERSION, &self.schema_version)?;
        require_id("receipt_id", &self.receipt_id)?;
        require_id("delivery_id", &self.delivery_id)?;
        require_id("program_id", &self.program_id)?;
        require_id("child_dag_id", &self.child_dag_id)?;
        require_id("task_id", &self.task_id)?;
        validate_artifact("workspace", &self.workspace)?;
        validate_artifacts("attempts", &self.attempts, 1)?;
        validate_artifact("terminal_coding_result", &self.terminal_coding_result)?;
        validate_artifacts("quality_receipts", &self.quality_receipts, 1)?;
        validate_artifact("integration_receipt", &self.integration_receipt)?;
        validate_artifact("land_receipt", &self.land_receipt)?;
        require_text(
            "reconciliation_receipt.ref",
            &self.reconciliation_receipt.receipt_ref,
        )?;
        require_min("final_refs", self.final_refs.len(), 1)?;
        for fr in &self.final_refs {
            require_id("final_refs.repo_id", &fr.repo_id)?;
            require_text("final_refs.ref", &fr.git_ref)?;
        }
        for risk in &self.residual_risks {
            require_text("residual_risks", risk)?;
        }
        require_text("completed_at", &self.completed_at)?;
        // allOf: completed ⟹ reconciliation effect applied.
        if self.completion_verdict == CompletionVerdict::Completed
            && self.reconciliation_receipt.effect_state != EffectState::Applied
        {
            return Err(CodingFlowError::CompletedEffectNotApplied);
        }
        Ok(())
    }
}

// silence unused-helper warnings for require_max in builds that do not exercise
// a max-bound field yet (kept for completeness of the $defs mirror).
#[allow(dead_code)]
fn _assert_helpers_used() {
    let _ = require_max;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rejects_leading_slash_and_empty() {
        assert!(CodingFlowPath::parse("src/lib.rs").is_ok());
        assert!(matches!(
            CodingFlowPath::parse("/abs"),
            Err(CodingFlowError::BadPath(_))
        ));
        assert!(matches!(
            CodingFlowPath::parse(""),
            Err(CodingFlowError::BadPath(_))
        ));
    }

    #[test]
    fn commit_oid_pattern() {
        assert!(CommitOid::parse("2".repeat(40)).is_ok());
        assert!(CommitOid::parse("a".repeat(64)).is_ok());
        assert!(matches!(
            CommitOid::parse("a".repeat(39)),
            Err(CodingFlowError::BadCommitOid(_))
        ));
        assert!(matches!(
            CommitOid::parse("A".repeat(40)),
            Err(CodingFlowError::BadCommitOid(_))
        ));
    }

    #[test]
    fn glob_within_matches_validator() {
        assert!(glob_within("src/**", "src/**"));
        assert!(glob_within("src/a.rs", "src/**"));
        assert!(!glob_within("infra/**", "src/**"));
        assert!(!glob_within("srcx/a", "src/**"));
    }
}
