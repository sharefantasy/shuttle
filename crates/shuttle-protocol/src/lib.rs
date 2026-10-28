//! shuttle-protocol — the wire contract between the upstream "smart slow" planner
//! and the downstream "fast small" executor.
//!
//! Design goal: the executor should never need to re-generate a whole file.
//! The planner emits [`EditPlan`]s made of content-anchored [`EditOp`]s
//! (search/replace blocks), never line-numbered unified diffs.

use serde::{Deserialize, Serialize};

pub mod path_scope;
pub use path_scope::{PathViolation, ScopedPathResolver};

pub mod anchor;

pub mod canonical;
pub use canonical::{canonical_digest, canonical_json, ClosedContractError};

pub mod symbol_footprint;
pub use symbol_footprint::{
    ObservedEvidenceSource, ObservedSymbolEvidence, PlannedSymbolEvidence, PlannerEvidenceSource,
    SymbolCoverage, SymbolFootprint, SymbolFootprintBasis, SymbolIdentity, SymbolIndexFreshness,
    SymbolRepositoryBase, TaskSymbolFootprint, UnresolvedSymbol, UnresolvedSymbolReason,
    SYMBOL_FOOTPRINT_SCHEMA,
};

pub mod hybrid_advice;
pub use hybrid_advice::{
    HybridAdoptionDecision, HybridAdoptionReason, HybridAdoptionReasonCode, HybridAdoptionVerdict,
    HybridArtifactBinding, HybridConflict, HybridConflictKind, HybridDagProjection,
    HybridDegradation, HybridDegradedState, HybridEdgeProvenance, HybridEvidenceIssue,
    HybridEvidenceKind, HybridEvidenceState, HybridFootprintSource, HybridHardEdge,
    HybridIndexBinding, HybridMechanicalEvidence, HybridMergeInput, HybridParallelAdvice,
    HybridParallelAdviceInput, HybridPlanningBasis, HybridRepositoryBase, HybridSchedule,
    HybridScheduleBatch, HybridSerializeReason, HybridTaskEdge, HybridTaskFootprint,
    HybridTrustedCandidate, HybridWeakHint, HybridWriteClaim, HYBRID_DAG_POLICY_VERSION,
    HYBRID_PARALLEL_ADVICE_SCHEMA,
};

pub mod cell_binding;
pub use cell_binding::{
    AllowedCommand, ApprovedExternalInputs, CellBinding, CellBindingError, CellProjectBinding,
    ProjectBoundCell, CELL_BINDING_VERSION,
};

pub mod runtime_descriptor;
pub use runtime_descriptor::{
    EaRuntimeDescriptor, ProviderRuntime, RuntimeCore, RuntimeDescriptorError, RuntimeMode,
    RuntimeTools, SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA,
};

pub mod tool_outcome;
pub use tool_outcome::{
    reason, ConflictDetail, ContextDetail, ContextNode, DerivationSymbolRef, DerivedTask,
    DerivedTaskEdge, EdgeSource, EdgeVerdict, MissingItem, NavDetail, NavHitLite,
    OutcomeConfidence, OutcomeState, RecipeDetail, ReconcileDetail, ReconcileVerdict,
    ReconciledTask, RefactorDetail, RenameDetail, ReverifyDetail, ReverifyUnit,
    SplitDerivationDetail, SplitDetail, SplitEdge, SplitNode, ToolOutcome, TypedDetail,
    OUTCOME_VERSION,
};
pub use tool_outcome::{reconcile_outcome, TOOL_RECONCILE};
pub use tool_outcome::{
    skip_reason, verify_kind, verify_outcome, FailedUnit, SkippedUnit, VerifyDetail, TOOL_VERIFY,
};

pub mod routing;
pub use routing::{route, ModelTier, RouteAction, RoutingDecision};

pub mod dispatch;
pub use dispatch::{env_for_tier, Dispatcher, ModelEndpoint};

pub mod audit;
pub use audit::{
    default_tier_for_task, initial_decision_for_task, is_mechanical_task, DualReceipt, ReceiptLeg,
};

pub mod closing;
pub use closing::{
    escalation_signal, needs_closing, refetch_plan, refetch_plan_within_budget, EscalationSignal,
    RefetchPlan,
};

pub mod ui_atom;
pub use ui_atom::{
    AllenRelation, Axis, AxisKind, CoordAtom, CoordAtomProposition, Holds, Provenance,
    ProvenanceClass, RelationFamily, TimeQuantifier, VerificationTier, COORD_ATOM_SCHEMA,
};

pub mod ui_principle;
pub use ui_principle::{DesignPrinciple, Formula, PrincipleVerdict, DESIGN_PRINCIPLE_SCHEMA};

pub mod ui_projection;
pub use ui_projection::{ProjectionBasis, ProjectionOutcome, UiProjection, UI_PROJECTION_SCHEMA};

pub mod ui_receipt;
pub use ui_receipt::{
    PdeArtifactRef, PdeCampaignContract, PdeCampaignPhaseReceipt, PdeDecision, PdeEffect,
    PdeEffectState, PdePhase, PdePhaseAction, UiPhaseReceiptRequest,
    PDE_CAMPAIGN_PHASE_RECEIPT_SCHEMA, PDE_CAMPAIGN_PROTOCOL_REVISION, UI_PROJECTION_ARTIFACT_TYPE,
};

pub mod ui_adapter;
pub use ui_adapter::{
    AdapterBasis, AdapterDisposition, AdapterReceipt, AestheticScore, ComponentMapping,
    HardGateKind, HardGateRow, HardGateVerdict, ADAPTER_RECEIPT_SCHEMA,
};

pub mod ui_canvas;
pub use ui_canvas::{
    CanvasBasis, CanvasBox, CanvasFidelity, CanvasNode, CanvasReadiness, CanvasUnderlay,
    CANVAS_UNDERLAY_SCHEMA,
};

pub mod orchestrate;
pub use orchestrate::{
    orchestrate, AttemptOutput, AttemptRecord, AttemptRequest, AttemptRunner, IdempotencyGuard,
    OrchestrationOptions, OrchestrationResult, OrchestrationTask,
};

pub mod metrics;
pub use metrics::{EconomicMetrics, RunSummary};

pub mod telemetry;
pub use telemetry::{
    reverify_size_of, GoalTelemetry, SplitFeedback, SplitTelemetryLedger, VerifyTelemetry,
    TELEMETRY_VERSION,
};

pub mod hashing;
pub use hashing::sha256_hex;

pub mod ledger;
pub use ledger::{evidence_digest, ProvenanceEntry, ProvenanceLedger, LEDGER_VERSION};

pub mod calibrate;
pub use calibrate::{
    calibrate, CalibratedThresholds, CalibrationInput, TierSample, TierVerdict,
    CALIBRATION_VERSION, MIN_SAMPLE,
};

/// A batch of edits, typically produced in one planning turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPlan {
    /// Schema version so the CLI can reject incompatible payloads fast.
    pub version: u32,
    /// Optional human/agent-readable intent, for logging & audit.
    #[serde(default)]
    pub intent: Option<String>,
    pub ops: Vec<EditOp>,
}

/// A single edit operation. Kept intentionally minimal to reduce tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditOp {
    /// Content-anchored replacement. `search` must match exactly once after
    /// the configured normalization; `replace` is written back with the
    /// original indentation preserved.
    SearchReplace {
        path: String,
        search: String,
        replace: String,
        /// Optional line hint used only as a tie-breaker when `search`
        /// matches multiple locations. Never used as a hard locator.
        #[serde(default)]
        line_hint: Option<usize>,
    },
    /// Insert `content` relative to an anchor (before/after).
    Insert {
        path: String,
        anchor: String,
        position: InsertPosition,
        content: String,
    },
    /// Create a brand-new file. Goes through the terminal-write fast path,
    /// bypassing the diff engine entirely.
    CreateFile { path: String, content: String },
    /// Apply a named codemod rule (ast-grep) — zero model tokens at apply time.
    Codemod {
        rule_id: String,
        /// Restrict scope; empty means the whole project.
        #[serde(default)]
        paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertPosition {
    Before,
    After,
}

/// The result of applying one op, returned to the caller for verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub path: String,
    pub status: ApplyStatus,
    /// Which fuzzy tier succeeded (0 = exact). Useful telemetry for tuning.
    #[serde(default)]
    pub match_tier: u8,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Applied,
    /// Anchor not found even after fuzzy fallback — must bounce back to model.
    AnchorMiss,
    /// Anchor ambiguous and no tie-breaker resolved it.
    Ambiguous,
    /// Write path failed containment (P0-2). Security refusal — this op is
    /// NOT retried by the model (unlike AnchorMiss); under a cell profile a
    /// Rejected op fails the whole plan closed.
    Rejected {
        reason: String,
    },
    Error,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported plan version: {0}")]
    UnsupportedVersion(u32),
    #[error("invalid payload: {0}")]
    Invalid(#[from] serde_json::Error),
}

pub const CURRENT_VERSION: u32 = 1;

impl EditPlan {
    pub fn parse(json: &str) -> Result<Self, ProtocolError> {
        let plan: EditPlan = serde_json::from_str(json)?;
        if plan.version != CURRENT_VERSION {
            return Err(ProtocolError::UnsupportedVersion(plan.version));
        }
        Ok(plan)
    }
}
