//! contracts — reference-form anchors for the 7 frozen schemas shuttle consumes.
//!
//! These types mirror the **upstream ruling** (,
//! authoritative original `upstream/documents/cf_bline_ruling_ea_alignment.md`).
//! They are *reference-form*: shuttle consumes the external orchestrator's frozen contracts by their refs +
//! digests and the small subset of fields the ruling maps into shuttle's runtime,
//! rather than re-declaring every external field. shuttle is a DOWNSTREAM consumer — it
//! never freezes these; external owns the schemas.
//!
//! > 待与 upstream 正本核对: the frozen JSON Schemas live in
//! > `upstream/collaboration-fabric/assets/schemas/*.schema.json`, which is NOT
//! > materialized on this machine. Every field here is transcribed from the
//! > ruling's frozen-field tables (§0, §4, §5). On first upstream sync, diff
//! > each struct against its `.schema.json` and delete this banner only when
//! > they match. Until then, unknown incoming fields are preserved (see the
//! > `extra` maps) so shuttle never drops external data it does not yet model.

use serde::{Deserialize, Serialize};

use super::digest::Sha256Digest;

/// A content-addressed artifact reference used by the v2 launch contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub digest: Sha256Digest,
}

/// `project_identity/v1` — the explicit project identity. Missing identity is
/// represented by shuttle's local `legacy_unbound` mode, never by a made-up default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdentity {
    pub schema_version: String,
    pub project_id: String,
    pub authorization_scope: String,
}

/// One repository entry from `project_binding/v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRepositoryBinding {
    pub repo_id: String,
    pub repo_ref: String,
    pub protected_ref: String,
    pub workspace_namespace: String,
    pub allowed_write_scopes: Vec<String>,
}

/// `project_binding/v1` — immutable project-to-repository authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBinding {
    pub schema_version: String,
    pub binding_id: String,
    pub project_identity_ref: String,
    pub project_identity_digest: Sha256Digest,
    pub mode: String,
    pub repositories: Vec<ProjectRepositoryBinding>,
}

/// `project_workspace_binding/v1` — provider-owned materialization evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectWorkspaceBinding {
    pub schema_version: String,
    pub binding_id: String,
    pub project_binding_ref: String,
    pub project_binding_digest: Sha256Digest,
    pub workspace_bundle_ref: String,
    pub workspace_bundle_digest: Sha256Digest,
    pub materialization_ref: String,
    pub materialization_digest: Sha256Digest,
    pub repo_id: String,
    pub base_oid: String,
    pub workspace_path: String,
    pub workspace_ref: String,
    pub allowed_write_scopes: Vec<String>,
    pub provider_receipt_digest: Sha256Digest,
}

/// `external_context_manifest/v1` — the task-level source of truth for what a run may
/// touch (ruling §0, §1). shuttle reads the command allowlist and read/write scope
/// from here rather than from an shuttle-private schema.
///
/// Only the fields the ruling routes into shuttle's runtime are modeled explicitly;
/// everything else external freezes is kept verbatim in [`ContextManifest::extra`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    /// external identifier for this manifest instance.
    pub context_manifest_id: String,
    /// The canonical digest of this manifest (`sha256:<64hex>`). shuttle adopts this
    /// verbatim as its `assignment_digest` (adopted mapping) — never
    /// recomputed.
    pub context_manifest_digest: Sha256Digest,
    /// §1: the authoritative command allowlist. shuttle narrows candidate `argv[0]`
    /// basenames into this set; the `{program, argv_prefix}` matching mechanics
    /// stay shuttle-local (see [`crate::cell_binding`]).
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Read scope. external expresses the readable/observable surface here; shuttle's
    /// physical read confinement is a local mechanism.
    #[serde(default)]
    pub scope: Option<Scope>,
    /// Refs to the `external_resource_claim/v1` records (incl. the file_write_scope
    /// claim) mounted on this manifest.
    #[serde(default)]
    pub write_claim_refs: Vec<String>,
    /// `source_versions[].digest` — the materialization OID(s) shuttle checks out.
    /// shuttle's read-only `base_oid` maps here (NOT `canary_base_oid`).
    #[serde(default)]
    pub source_versions: Vec<SourceVersion>,
    /// external fields shuttle does not (yet) model, preserved so nothing is dropped.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `context_manifest.scope{in,out}` — the read/observe surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default, rename = "in")]
    pub scope_in: Vec<String>,
    #[serde(default, rename = "out")]
    pub scope_out: Vec<String>,
}

/// One `source_versions[]` entry — a materialization anchor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceVersion {
    /// Opaque source identifier (repo / package coordinate).
    #[serde(default)]
    pub source_id: Option<String>,
    /// The OID/digest shuttle executes on read-only. shuttle's `base_oid` lands here.
    pub digest: String,
}

/// `external_resource_claim/v1` — the authoritative expression of an isolated write
/// domain. shuttle's `allowed_paths` write allowlist is the
/// `file_write_scope`-typed claim's `resource_keys[]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceClaim {
    /// `resource_type ∈ {agent_slots, worktree, branch, file_write_scope}`.
    pub resource_type: ResourceType,
    /// Anti-traversal keys. For `file_write_scope` these are the write-domain
    /// globs shuttle lowers into `allowed_paths`.
    #[serde(default)]
    pub resource_keys: Vec<String>,
    /// `claim_mode ∈ {capacity, exclusive_write}`. A write domain is
    /// `exclusive_write`.
    pub claim_mode: ClaimMode,
    /// Monotonic lease epoch guarding against stale claims.
    #[serde(default)]
    pub lease_epoch: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `external_resource_claim.resource_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    AgentSlots,
    Worktree,
    Branch,
    FileWriteScope,
}

/// `external_resource_claim.claim_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMode {
    Capacity,
    ExclusiveWrite,
}

impl ResourceClaim {
    /// The write-domain globs iff this is an `exclusive_write` file_write_scope
    /// claim; otherwise `None` (fail-closed — a non-write claim contributes no
    /// write authority).
    pub fn file_write_scope_keys(&self) -> Option<&[String]> {
        if self.resource_type == ResourceType::FileWriteScope
            && self.claim_mode == ClaimMode::ExclusiveWrite
        {
            Some(&self.resource_keys)
        } else {
            None
        }
    }
}

/// `external_provider_capability_manifest/v1.admission` — the read/write ceiling and
/// recovery budget external grants a provider (ruling §0, §3). This is the external orchestrator ceiling
/// term of the effective-capability intersection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admission {
    /// Who authorized this admission (external authority handle).
    #[serde(default)]
    pub authority: Option<String>,
    /// The read/write ceiling. shuttle's read/write authorization derives from here,
    /// NOT from an env allowlist.
    pub target_kinds_authorized: TargetKindsAuthorized,
    #[serde(default)]
    pub max_parallelism: Option<u32>,
    /// external-side reconciliation/recovery time budget (ms). NOT shuttle's cancel grace
    /// (`grace ≠ recovery_rto_ms`).
    #[serde(default)]
    pub recovery_rto_ms: Option<u64>,
    #[serde(default)]
    pub canary_status: Option<String>,
    /// The canary's base OID at the provider capability level. NOT shuttle's
    /// per-task `base_oid`.
    #[serde(default)]
    pub canary_base_oid: Option<String>,
    #[serde(default)]
    pub cross_session_visibility: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `admission.target_kinds_authorized{readonly, product_source_write}` — the
/// authoritative read/write ceiling switch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TargetKindsAuthorized {
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub product_source_write: bool,
}

/// `external_launch_intent/v1` — the upstream anchor of shuttle's input envelope
/// (ruling §0, §5). All fields are **reference-form**; external/dispatch mints them
/// and shuttle backfills. shuttle's only self-produced value is `shuttle_binary_digest`, which
/// is shuttle-local and NOT part of this contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchIntent {
    /// Present and fixed to `external_launch_intent/v2` for project-bound
    /// launches. Omitted by the legacy v1 reference form.
    #[serde(default)]
    pub schema_version: Option<String>,
    pub launch_intent_id: String,
    pub attempt_id: String,
    /// Runtime instance id — independently required; shuttle carries it (ruling §4:
    /// NOT merged into `environment_id`).
    pub runtime_id: String,
    /// Work-package id — independently required.
    pub package_id: String,
    /// De-dup key minted by dispatch/external. **Must NOT change on retry** (ruling
    /// §5) — otherwise de-dup fails.
    pub idempotency_key: String,
    pub agent_type: String,
    pub execution_profile_ref: String,
    /// v2 assignment and project/workspace bindings. They remain optional in
    /// the reference type solely so legacy v1 launches continue to deserialize;
    /// [`crate::anchor::ExternalLaunchBundle`] requires all of them for project-bound mode.
    #[serde(default)]
    pub assignment_ref: Option<String>,
    #[serde(default)]
    pub assignment_digest: Option<Sha256Digest>,
    #[serde(default)]
    pub project_binding_digest: Option<Sha256Digest>,
    pub context_manifest_ref: String,
    #[serde(default)]
    pub context_manifest_digest: Option<Sha256Digest>,
    #[serde(default)]
    pub workspace_binding_ref: Option<String>,
    #[serde(default)]
    pub workspace_binding_digest: Option<Sha256Digest>,
    #[serde(default)]
    pub resource_claims: Vec<ArtifactRef>,
    #[serde(default)]
    pub resource_claim_refs: Vec<String>,
    pub deadline: String,
    pub requested_by: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `external_attempt/v1` state machine positions relevant to shuttle's internal
/// quality gate. shuttle's six-predicate + build/test gate drives
/// `running → result_validating → completed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Accepted,
    Running,
    /// The landing point of shuttle's internal gate (ruling §0, §2).
    ResultValidating,
    Completed,
    Failed,
    Cancelled,
}

/// Provider receipt carrier shared with external. SHUTTLE serializes exactly the five
/// explicit fields below; an external consumer may use `extra` while lifting that
/// inner observation into its canonical nine-field receipt. Gate detail and
/// inline artifacts remain behind `result_ref`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderReceipt {
    pub provider_status: ProviderStatus,
    pub reconciliation_hint: ReconciliationHint,
    /// Reference to the landed result artifact (gate evidence goes HERE, via
    /// ref — never inlined into the receipt).
    #[serde(default)]
    pub result_ref: Option<String>,
    #[serde(default)]
    pub provider_run_id: Option<String>,
    /// True only in a terminal state.
    pub terminal: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `provider_status` enum (ruling §0, §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Accepted,
    Running,
    Completed,
    Failed,
    Cancelled,
    NotFound,
    Unknown,
}

/// `reconciliation_hint` — a first-class field telling external the next reconcile
/// step when a run is non-terminal or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationHint {
    QueryProvider,
    Reconnect,
    Restart,
    Block,
}

impl ProviderStatus {
    /// Whether this status is terminal (drives the receipt's `terminal` flag).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ProviderStatus::Completed
                | ProviderStatus::Failed
                | ProviderStatus::Cancelled
                | ProviderStatus::NotFound
        )
    }
}

/// `external_text_transaction/v1` — the **separate** contract for write transactions
///. Never folded into the receipt. Modeled reference-form: shuttle
/// carries the revision/digest chain and leaves `events[]`/`outbox[]` opaque
/// until the write path is wired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextTransaction {
    pub ledger_transaction_id: String,
    pub parent_revision: u64,
    pub revision: u64,
    pub fencing_token: u64,
    /// Same de-dup discipline as launch_intent.
    pub idempotency_key: String,
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
    #[serde(default)]
    pub outbox: Vec<serde_json::Value>,
    /// Digest of the pre-transaction revision.
    pub previous_digest: Sha256Digest,
    /// Digest of the post-transaction revision.
    pub digest: Sha256Digest,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_write_scope_keys_only_for_exclusive_write() {
        let claim = ResourceClaim {
            resource_type: ResourceType::FileWriteScope,
            resource_keys: vec!["src/**".into()],
            claim_mode: ClaimMode::ExclusiveWrite,
            lease_epoch: Some(1),
            extra: Default::default(),
        };
        assert_eq!(
            claim.file_write_scope_keys(),
            Some(&["src/**".to_string()][..])
        );

        let capacity = ResourceClaim {
            claim_mode: ClaimMode::Capacity,
            ..claim.clone()
        };
        assert!(capacity.file_write_scope_keys().is_none());

        let worktree = ResourceClaim {
            resource_type: ResourceType::Worktree,
            ..claim
        };
        assert!(worktree.file_write_scope_keys().is_none());
    }

    #[test]
    fn provider_status_terminality() {
        assert!(ProviderStatus::Completed.is_terminal());
        assert!(ProviderStatus::Failed.is_terminal());
        assert!(ProviderStatus::Cancelled.is_terminal());
        assert!(ProviderStatus::NotFound.is_terminal());
        assert!(!ProviderStatus::Running.is_terminal());
        assert!(!ProviderStatus::Accepted.is_terminal());
        assert!(!ProviderStatus::Unknown.is_terminal());
    }

    #[test]
    fn context_manifest_preserves_unknown_fields() {
        let json = format!(
            r#"{{
                "context_manifest_id": "cm-1",
                "context_manifest_digest": "sha256:{}",
                "tool_allowlist": ["git", "cargo"],
                "future_cf_field": {{"nested": true}}
            }}"#,
            "a".repeat(64)
        );
        let cm: ContextManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(cm.tool_allowlist, vec!["git", "cargo"]);
        assert!(cm.extra.contains_key("future_cf_field"));
    }

    #[test]
    fn receipt_round_trips_five_field_inner_snapshot() {
        let r = ProviderReceipt {
            provider_status: ProviderStatus::Completed,
            reconciliation_hint: ReconciliationHint::QueryProvider,
            result_ref: Some("artifact://run/1".into()),
            provider_run_id: Some("run-1".into()),
            terminal: true,
            extra: Default::default(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ProviderReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider_status, ProviderStatus::Completed);
        assert!(back.terminal);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            5
        );
    }
}
