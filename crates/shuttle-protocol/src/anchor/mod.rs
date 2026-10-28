//! cf — protocol anchor types for shuttle's execution contracts.
//!
//! This module holds Rust anchors for the schemas that downstream runtimes may
//! consume, plus the fail-closed effective-capability intersection.
//!
//! Layering:
//!   * **Anchor layer (this module)** — reference-form types that mirror the
//!     shared execution schemas. shuttle never freezes these; they are
//!     consumed by ref + digest.
//!   * **shuttle local envelope ([`crate::cell_binding`])** — the shuttle-local
//!     run envelope. [`crate::cell_binding::CellBinding::from_external`] *lowers*
//!     the contracts here into that envelope. The envelope is also directly
//!     constructible from a minimal hand-written binding, so `shuttle cell-run`
//!     stays launchable as an agent WITHOUT any external orchestration plumbing.
//!
//! > 待与 upstream 正本核对: authoritative JSON Schemas live in an external
//! > upstream repository and are NOT materialized on this machine. Reconcile
//! > every type here against its `.schema.json` on first upstream sync.

pub mod capability;
pub mod coding_flow;
pub mod contracts;
pub mod digest;
pub mod ingest;

pub use capability::{
    assert_within_ceiling, effective_capabilities, CapabilityError, EffectiveCapabilities,
    ExecutionProfileGrant,
};
pub use coding_flow::{
    schema_digests as coding_flow_schema_digests, AggregateVerdict, ApprovalDecision,
    ApprovalReceipt, AttemptCommit, AttemptStatus, AuthorityOwner, BlockingFailureVerdict,
    CancellationDomain, CodeDeliveryManifest, CodeDeliveryPacketPlan, CodeDeliveryQualityPlan,
    CodeDeliveryQualityReceipt, CodeDeliveryReceipt, CodeDeliveryRepairDecision,
    CodingAgentContextManifest, CodingAttemptResult, CodingFlowContract, CodingFlowError,
    CodingFlowPath, CommandKind, CommandReceipt, CommandStatus, CommitOid, CompletionVerdict,
    ContextBudget, EffectState, FailureClass, FinalRef, FitStatus, ForceReport, FormalAssurance,
    FormalLevel, IntegrationMode, IntegrationPolicy, IntegrationUnknownEffect, PacketSlice,
    PriorAttempt, ProjectFlowCodeDeliveryHandoff, ProviderTerminalReceipt, ProviderTerminalStatus,
    QualityAggregatePolicy, QualityCommand, RecommendedAction, ReconciliationReceipt,
    RepairDecisionKind, RepairPolicy, RepairUnknownEffect, RequiredEvidence,
    RequiredQualityVerdict, RuntimeMutationPolicy, WorkspaceRef, WriteScope,
};
pub use contracts::{
    Admission, ArtifactRef, AttemptState, ClaimMode, ContextManifest, LaunchIntent, ProjectBinding,
    ProjectIdentity, ProjectRepositoryBinding, ProjectWorkspaceBinding, ProviderReceipt,
    ProviderStatus, ReconciliationHint, ResourceClaim, ResourceType, Scope, SourceVersion,
    TargetKindsAuthorized, TextTransaction,
};
pub use digest::{DigestError, Sha256Digest};
pub use ingest::{
    CodingResultExport, ExternalLaunchBundle, ProjectLaunchContext, CF_LAUNCH_BUNDLE_VERSION,
};
