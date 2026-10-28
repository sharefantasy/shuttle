//! ingest — the external→shuttle handoff envelope (`external_launch_bundle/v1`, shuttle-LOCAL).
//!
//! When external (or a dispatcher acting for external) drives shuttle, it hands off the small
//! subset of frozen contracts shuttle consumes at spawn time. This module is the
//! **single ingest seam** that packages them together and lowers them into an
//! shuttle-local [`CellBinding`] via [`CellBinding::from_external`], closing the gap that
//! previously left `from_external` as dead code with no production caller.
//!
//! `external_launch_bundle/v1` is **NOT a frozen contract** — it is a shuttle-local
//! transport that carries three frozen anchors (`external_launch_intent/v1`,
//! `external_context_manifest/v1`, and the `file_write_scope` `external_resource_claim/v1`)
//! plus the two shuttle-local pass-throughs the external orchestrator's frozen contracts do not carry
//! (`workspace_root`, `environment_id`). It exists only so a external-side orchestrator
//! and shuttle agree on one JSON shape to move those anchors across the process
//! boundary; the direct-agent-launch path (a hand-written `cell_binding.json`)
//! does not use it at all.
//!
//! > The frozen JSON Schemas for the three carried
//! > contracts live in the external orchestrator's schema set
//! > and are not materialized in this crate. The carried structs are the
//! > reference-form anchors in [`super::contracts`]; reconcile them on
//! > upstream sync. This wrapper adds no new external fields — it only
//! > transports the existing anchors — so it introduces no fresh guesswork.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::contracts::{
    ContextManifest, LaunchIntent, ProjectBinding, ProjectIdentity, ProjectWorkspaceBinding,
    ResourceClaim,
};
use crate::cell_binding::{
    ApprovedExternalInputs, CellBinding, CellBindingError, CellProjectBinding, ProjectBoundCell,
};
use crate::{EaRuntimeDescriptor, RuntimeMode};

/// The single bundle version shuttle ingests. Anything else is refused fail-closed.
pub const CF_LAUNCH_BUNDLE_VERSION: u32 = 1;

/// `external_launch_bundle/v1` — the shuttle-local external→shuttle handoff envelope.
///
/// A external-side dispatcher fills this with the frozen contracts it already holds
/// for the attempt, plus the materialized `workspace_root` and the run's
/// `environment_id`. shuttle lowers it to a [`CellBinding`] via
/// [`CellBinding::from_external`] and runs the exact same caged code path a
/// hand-written binding takes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLaunchBundle {
    /// Schema version; must equal [`CF_LAUNCH_BUNDLE_VERSION`].
    pub version: u32,

    /// shuttle-local pass-through: the materialized workspace root the provider
    /// checked `base_oid` out into. the external orchestrator's frozen contracts do not carry a
    /// filesystem path, so the dispatcher supplies it here. Must be absolute
    /// (enforced by [`CellBinding::validate`] after lowering).
    pub workspace_root: String,

    /// shuttle-local pass-through: the opaque environment id for this run. Not a external
    /// frozen field; carried through untouched.
    pub environment_id: String,

    /// SHUTTLE-local runtime identity sidecar. The triple is all-or-nothing and the
    /// digest must match the canonical descriptor before lowering.
    #[serde(default)]
    pub runtime_descriptor_ref: Option<String>,
    #[serde(default)]
    pub runtime_descriptor_digest: Option<super::Sha256Digest>,
    #[serde(default)]
    pub runtime_descriptor: Option<EaRuntimeDescriptor>,

    /// `external_launch_intent/v1` — supplies `runtime_id` / `package_id` /
    /// `execution_profile_ref` into the binding, and `idempotency_key` /
    /// `attempt_id` used to stamp the receipt (see [`Self::provider_run_id`]).
    pub launch_intent: LaunchIntent,

    /// `external_context_manifest/v1` — supplies the `tool_allowlist`, the adopted
    /// `assignment_digest`, and `source_versions[0].digest` (`base_oid`).
    pub context_manifest: ContextManifest,

    /// The `file_write_scope` `external_resource_claim/v1`, if this attempt may write.
    /// Absent ⇒ fail-closed: the lowered binding has an empty write allowlist,
    /// so every write is refused.
    #[serde(default)]
    pub resource_claim: Option<ResourceClaim>,

    /// Project-bound launches carry the frozen identity, binding and provider
    /// workspace evidence as one all-or-nothing sidecar. Absence means this is
    /// an explicit legacy-unbound launch only when no v2 project fields appear.
    #[serde(default)]
    pub project: Option<ProjectLaunchContext>,

    /// The task prompt handed to the caged planner. Optional so a bundle can be
    /// ingested preflight-only (validate + describe boundary) before external commits
    /// to a run.
    #[serde(default)]
    pub prompt: Option<String>,

    /// Opt-in coding-flow artifact coordinates. Absent preserves the legacy
    /// telemetry-only result_ref behavior.
    #[serde(default)]
    pub coding_result_export: Option<CodingResultExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingResultExport {
    pub delivery_id: String,
    pub workspace_ref: String,
    pub repo_id: String,
    pub change_id: String,
    #[serde(default)]
    pub generated_tests: Vec<String>,
    #[serde(default)]
    pub unresolved_items: Vec<String>,
}

/// The project-bound portion of the external→shuttle launch. The approval record keeps
/// the external source commit and capability snapshot coupled to the exact
/// project binding digest consumed by this cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectLaunchContext {
    pub project_identity: ProjectIdentity,
    pub project_binding: ProjectBinding,
    pub workspace_binding: ProjectWorkspaceBinding,
    pub approved_external_inputs: ApprovedExternalInputs,
}

impl ExternalLaunchBundle {
    /// Parse a `external_launch_bundle/v1` JSON envelope, rejecting a wrong version
    /// fail-closed before any lowering.
    pub fn parse(json: &str) -> Result<Self, CellBindingError> {
        let bundle: ExternalLaunchBundle =
            serde_json::from_str(json).map_err(|e| CellBindingError::Invalid(e.to_string()))?;
        if bundle.version != CF_LAUNCH_BUNDLE_VERSION {
            return Err(CellBindingError::UnsupportedVersion(bundle.version));
        }
        bundle.validate_runtime_descriptor()?;
        Ok(bundle)
    }

    /// Lower the carried external contracts into a shuttle-local, validated
    /// [`CellBinding`]. This is the production caller of
    /// [`CellBinding::from_external`]: it routes each field to its authoritative external
    /// source and then re-checks the result with [`CellBinding::validate`] so a
    /// bad `workspace_root` / manifest cannot slip past the fail-closed gate.
    pub fn into_binding(&self) -> Result<CellBinding, CellBindingError> {
        let mut binding = CellBinding::from_external(
            self.workspace_root.clone(),
            self.environment_id.clone(),
            &self.context_manifest,
            self.resource_claim.as_ref(),
            Some(&self.launch_intent),
        );
        binding.runtime_descriptor_ref = self.runtime_descriptor_ref.clone();
        binding.runtime_descriptor_digest = self.runtime_descriptor_digest.clone();
        binding.runtime_descriptor = self.runtime_descriptor.clone();
        binding.project = self.lower_project_binding(&binding)?;
        binding.validate()?;
        Ok(binding)
    }

    fn validate_runtime_descriptor(&self) -> Result<(), CellBindingError> {
        match (
            self.runtime_descriptor_ref.as_deref(),
            self.runtime_descriptor_digest.as_ref(),
            self.runtime_descriptor.as_ref(),
        ) {
            (None, None, None) => Ok(()),
            (Some(reference), Some(digest), Some(descriptor)) => {
                if reference.trim().is_empty() || reference.contains(['\n', '\r', '\0']) {
                    return Err(runtime_error("runtime_descriptor_ref must be non-empty and single-line"));
                }
                descriptor.validate().map_err(|error| runtime_error(error.to_string()))?;
                if descriptor.digest().map_err(|error| runtime_error(error.to_string()))? != *digest {
                    return Err(runtime_error("runtime_descriptor_digest does not match the canonical descriptor"));
                }
                if descriptor.mode == RuntimeMode::MountedBundle {
                    return Err(runtime_error("mounted_bundle is not launchable until materialization proves the mount capability"));
                }
                Ok(())
            }
            _ => Err(runtime_error("runtime descriptor, ref and digest are all-or-nothing; descriptor loss is forbidden")),
        }
    }

    fn lower_project_binding(
        &self,
        binding: &CellBinding,
    ) -> Result<CellProjectBinding, CellBindingError> {
        let has_v2_fields = self.launch_intent.schema_version.as_deref()
            == Some("external_launch_intent/v2")
            || self.launch_intent.project_binding_digest.is_some()
            || self.launch_intent.workspace_binding_digest.is_some()
            || self.launch_intent.assignment_digest.is_some();
        let Some(project) = &self.project else {
            if has_v2_fields {
                return Err(project_error(
                    "project-bound launch metadata requires a project context",
                ));
            }
            return Ok(CellProjectBinding::LegacyUnbound);
        };

        if self.launch_intent.schema_version.as_deref() != Some("external_launch_intent/v2") {
            return Err(project_error(
                "project context requires external_launch_intent/v2",
            ));
        }
        if project.project_identity.schema_version != "project_identity/v1"
            || project.project_binding.schema_version != "project_binding/v1"
            || project.project_binding.mode != "project_bound"
            || project.workspace_binding.schema_version != "project_workspace_binding/v1"
        {
            return Err(project_error(
                "unsupported project contract version or mode",
            ));
        }
        if canonical_digest(&project.project_identity)?
            != project.project_binding.project_identity_digest
        {
            return Err(project_error(
                "project identity digest does not match the carried identity",
            ));
        }

        let launch_project_digest = self
            .launch_intent
            .project_binding_digest
            .as_ref()
            .ok_or_else(|| project_error("launch intent is missing project_binding_digest"))?;
        let launch_workspace_digest = self
            .launch_intent
            .workspace_binding_digest
            .as_ref()
            .ok_or_else(|| project_error("launch intent is missing workspace_binding_digest"))?;
        let launch_workspace_ref = self
            .launch_intent
            .workspace_binding_ref
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| project_error("launch intent is missing workspace_binding_ref"))?;
        if self
            .launch_intent
            .assignment_ref
            .as_deref()
            .is_none_or(str::is_empty)
            || self.launch_intent.assignment_digest.is_none()
        {
            return Err(project_error(
                "project-bound launch is missing assignment ref or digest",
            ));
        }
        if self.launch_intent.resource_claims.is_empty() {
            return Err(project_error(
                "project-bound launch requires digest-bound resource_claims",
            ));
        }
        if self.launch_intent.context_manifest_digest.as_ref()
            != Some(&self.context_manifest.context_manifest_digest)
        {
            return Err(project_error(
                "launch context_manifest_digest does not match the carried manifest",
            ));
        }
        if launch_project_digest != &project.workspace_binding.project_binding_digest
            || launch_project_digest != &project.approved_external_inputs.project_binding_digest
        {
            return Err(project_error(
                "project binding digest drift across launch, workspace and approval",
            ));
        }
        if canonical_digest(&project.project_binding)? != *launch_project_digest {
            return Err(project_error(
                "project_binding_digest does not match the carried project binding",
            ));
        }
        if canonical_digest(&project.workspace_binding)? != *launch_workspace_digest {
            return Err(project_error(
                "workspace_binding_digest does not match the carried workspace binding",
            ));
        }
        if binding.assignment_digest
            != self
                .launch_intent
                .context_manifest_digest
                .as_ref()
                .expect("checked above")
                .as_str()
        {
            return Err(project_error(
                "cell assignment digest does not preserve the context digest",
            ));
        }
        if project.workspace_binding.workspace_path != self.workspace_root
            || project.workspace_binding.base_oid != binding.base_oid
            || project.approved_external_inputs.external_commit_oid != binding.base_oid
        {
            return Err(project_error(
                "provider workspace path or approved commit does not match the cell",
            ));
        }
        if project.workspace_binding.allowed_write_scopes != binding.allowed_paths {
            return Err(project_error(
                "provider workspace write scopes do not match the active claim",
            ));
        }

        let repo = project
            .project_binding
            .repositories
            .iter()
            .find(|repo| repo.repo_id == project.workspace_binding.repo_id)
            .ok_or_else(|| project_error("workspace repository is absent from project binding"))?;
        if repo.allowed_write_scopes != project.workspace_binding.allowed_write_scopes {
            return Err(project_error(
                "workspace write scopes drift from the project binding",
            ));
        }
        let repo_ids: Vec<&str> = project
            .project_binding
            .repositories
            .iter()
            .map(|repo| repo.repo_id.as_str())
            .collect();
        if repo_ids.is_empty() || !repo_ids.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(project_error(
                "project repositories must be non-empty, unique and sorted by repo_id",
            ));
        }
        Ok(CellProjectBinding::ProjectBound(Box::new(
            ProjectBoundCell {
                project_id: project.project_identity.project_id.clone(),
                authorization_scope: project.project_identity.authorization_scope.clone(),
                assignment_ref: self
                    .launch_intent
                    .assignment_ref
                    .clone()
                    .expect("checked above"),
                execution_assignment_digest: self
                    .launch_intent
                    .assignment_digest
                    .clone()
                    .expect("checked above"),
                project_binding_ref: project.workspace_binding.project_binding_ref.clone(),
                project_binding_digest: launch_project_digest.clone(),
                workspace_binding_ref: launch_workspace_ref.to_string(),
                workspace_binding_digest: launch_workspace_digest.clone(),
                repo_id: project.workspace_binding.repo_id.clone(),
                provider_workspace_root: project.workspace_binding.workspace_path.clone(),
                provider_allowed_write_scopes: project
                    .workspace_binding
                    .allowed_write_scopes
                    .clone(),
                provider_receipt_digest: project.workspace_binding.provider_receipt_digest.clone(),
                approved_external_inputs: project.approved_external_inputs.clone(),
            },
        )))
    }

    /// The external orchestrator idempotency key minted by dispatch — carried verbatim, never
    /// changed on retry. shuttle stamps it onto the receipt's
    /// `provider_run_id` so external can de-dup a re-attempt to the same launch.
    pub fn provider_run_id(&self) -> &str {
        &self.launch_intent.idempotency_key
    }
}

fn project_error(message: impl Into<String>) -> CellBindingError {
    CellBindingError::InvalidProjectBinding(message.into())
}

fn runtime_error(message: impl Into<String>) -> CellBindingError {
    CellBindingError::InvalidRuntimeDescriptor(message.into())
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<super::Sha256Digest, CellBindingError> {
    let value = serde_json::to_value(value)
        .map_err(|e| project_error(format!("cannot serialize binding for digest: {e}")))?;
    let canonical = canonicalize_json(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|e| project_error(format!("cannot encode binding for digest: {e}")))?;
    super::Sha256Digest::parse(format!("sha256:{}", crate::hashing::sha256_hex(&bytes)))
        .map_err(|e| project_error(e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::contracts::{
        ArtifactRef, ClaimMode, ProjectRepositoryBinding, ResourceType,
    };
    use crate::anchor::Sha256Digest;

    fn digest(ch: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", ch.to_string().repeat(64))).unwrap()
    }

    fn sample_manifest() -> ContextManifest {
        serde_json::from_str(&format!(
            r#"{{
                "context_manifest_id": "cm-1",
                "context_manifest_digest": "sha256:{}",
                "tool_allowlist": ["git", "cargo"],
                "source_versions": [{{"digest": "oid-abc"}}]
            }}"#,
            "a".repeat(64)
        ))
        .unwrap()
    }

    fn sample_launch_intent() -> LaunchIntent {
        serde_json::from_str(
            r#"{
                "launch_intent_id": "li-1",
                "attempt_id": "att-1",
                "runtime_id": "rt-1",
                "package_id": "pkg-1",
                "idempotency_key": "idem-xyz",
                "agent_type": "shuttle",
                "execution_profile_ref": "profile://shuttle/default",
                "context_manifest_ref": "cm-1",
                "resource_claim_refs": ["rc-1"],
                "deadline": "2026-01-01T00:00:00Z",
                "requested_by": "dispatch"
            }"#,
        )
        .unwrap()
    }

    fn project_launch_context() -> ProjectLaunchContext {
        let project_identity = ProjectIdentity {
            schema_version: "project_identity/v1".into(),
            project_id: "project-a".into(),
            authorization_scope: "auth/project-a".into(),
        };
        let project_binding = ProjectBinding {
            schema_version: "project_binding/v1".into(),
            binding_id: "pb-1".into(),
            project_identity_ref: "project://a".into(),
            project_identity_digest: canonical_digest(&project_identity).unwrap(),
            mode: "project_bound".into(),
            repositories: vec![ProjectRepositoryBinding {
                repo_id: "shuttle".into(),
                repo_ref: "repo://shuttle".into(),
                protected_ref: "refs/heads/main".into(),
                workspace_namespace: "project-a".into(),
                allowed_write_scopes: vec!["src/**".into(), "docs/**".into()],
            }],
        };
        let project_binding_digest = canonical_digest(&project_binding).unwrap();
        let workspace_binding = ProjectWorkspaceBinding {
            schema_version: "project_workspace_binding/v1".into(),
            binding_id: "pwb-1".into(),
            project_binding_ref: "binding://pb-1".into(),
            project_binding_digest: project_binding_digest.clone(),
            workspace_bundle_ref: "bundle://one".into(),
            workspace_bundle_digest: digest('3'),
            materialization_ref: "materialization://one".into(),
            materialization_digest: digest('4'),
            repo_id: "shuttle".into(),
            base_oid: "a".repeat(40),
            workspace_path: "/abs/ws".into(),
            workspace_ref: "workspace://one".into(),
            allowed_write_scopes: vec!["src/**".into(), "docs/**".into()],
            provider_receipt_digest: digest('5'),
        };
        ProjectLaunchContext {
            project_identity,
            project_binding,
            workspace_binding,
            approved_external_inputs: approved_external_inputs(project_binding_digest),
        }
    }

    fn approved_external_inputs(project_binding_digest: Sha256Digest) -> ApprovedExternalInputs {
        let mut approval = ApprovedExternalInputs {
            schema_version: "shuttle_external_project_approval/v1".into(),
            decision: "approved".into(),
            approval_ref: "approval://shuttle-project-a".into(),
            approval_digest: digest('0'),
            external_commit_oid: "a".repeat(40),
            provider_capability_snapshot_ref: "capability://shuttle".into(),
            provider_capability_snapshot_digest: digest('7'),
            project_binding_digest,
        };
        let payload = serde_json::json!({
            "decision": approval.decision,
            "external_commit_oid": approval.external_commit_oid,
            "project_binding_digest": approval.project_binding_digest,
            "provider_capability_snapshot_digest": approval.provider_capability_snapshot_digest,
            "provider_capability_snapshot_ref": approval.provider_capability_snapshot_ref,
            "schema_version": approval.schema_version,
        });
        approval.approval_digest = canonical_digest(&payload).unwrap();
        approval
    }

    fn project_bound_bundle() -> ExternalLaunchBundle {
        let mut bundle = bundle_with(Some(write_claim()));
        bundle.context_manifest.source_versions[0].digest = "a".repeat(40);
        bundle.launch_intent.schema_version = Some("external_launch_intent/v2".into());
        bundle.launch_intent.assignment_ref = Some("assignment://one".into());
        bundle.launch_intent.assignment_digest = Some(digest('8'));
        bundle.launch_intent.resource_claims = vec![ArtifactRef {
            artifact_ref: "claim://w".into(),
            digest: digest('a'),
        }];
        let project = project_launch_context();
        bundle.launch_intent.project_binding_digest =
            Some(project.workspace_binding.project_binding_digest.clone());
        bundle.launch_intent.context_manifest_digest =
            Some(bundle.context_manifest.context_manifest_digest.clone());
        bundle.launch_intent.workspace_binding_ref = Some("workspace-binding://one".into());
        bundle.launch_intent.workspace_binding_digest =
            Some(canonical_digest(&project.workspace_binding).unwrap());
        bundle.project = Some(project);
        bundle
    }

    fn write_claim() -> ResourceClaim {
        ResourceClaim {
            resource_type: ResourceType::FileWriteScope,
            resource_keys: vec!["src/**".into(), "docs/**".into()],
            claim_mode: ClaimMode::ExclusiveWrite,
            lease_epoch: Some(1),
            extra: Default::default(),
        }
    }

    fn bundle_with(claim: Option<ResourceClaim>) -> ExternalLaunchBundle {
        ExternalLaunchBundle {
            version: CF_LAUNCH_BUNDLE_VERSION,
            workspace_root: "/abs/ws".into(),
            environment_id: "env-1".into(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            launch_intent: sample_launch_intent(),
            context_manifest: sample_manifest(),
            resource_claim: claim,
            project: None,
            prompt: Some("do the task".into()),
            coding_result_export: None,
        }
    }

    #[test]
    fn into_binding_lowers_all_cf_sources() {
        let bundle = bundle_with(Some(write_claim()));
        let b = bundle.into_binding().expect("must lower + validate");

        // tool_allowlist -> allowed_commands (name-only)
        assert_eq!(b.allowed_commands.len(), 2);
        assert_eq!(b.allowed_commands[0].program, "git");
        assert!(b.allowed_commands[0].argv_prefix.is_none());
        // file_write_scope keys -> allowed_paths
        assert_eq!(
            b.allowed_paths,
            vec!["src/**".to_string(), "docs/**".to_string()]
        );
        // manifest digest adopted verbatim
        assert_eq!(b.assignment_digest, format!("sha256:{}", "a".repeat(64)));
        // first source_versions digest -> base_oid
        assert_eq!(b.base_oid, "oid-abc");
        // launch_intent ids carried
        assert_eq!(b.runtime_id.as_deref(), Some("rt-1"));
        assert_eq!(b.package_id.as_deref(), Some("pkg-1"));
        assert_eq!(b.profile_ref.as_deref(), Some("profile://shuttle/default"));
        // pass-throughs
        assert_eq!(b.workspace_root, "/abs/ws");
        assert_eq!(b.environment_id, "env-1");
    }

    fn runtime_descriptor() -> EaRuntimeDescriptor {
        EaRuntimeDescriptor {
            schema_version: crate::SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA.into(),
            mode: RuntimeMode::HostBundle,
            core: crate::RuntimeCore {
                version: "0.2.0".into(),
                runtime_abi: "shuttle-runtime/v1".into(),
                executable_path: "/usr/local/bin/shuttle".into(),
                digest: digest('b'),
            },
            tools: crate::RuntimeTools {
                source: "file:///bundle".into(),
                distribution_digest: digest('c'),
                manifest_digest: digest('d'),
                root: "/opt/shuttle-tools".into(),
                profiles: vec!["source-analysis".into()],
            },
            provider: crate::ProviderRuntime {
                kind: "pi".into(),
                version: "1".into(),
                binary: "/usr/local/bin/pi".into(),
                binary_digest: digest('f'),
                extension_path: "/opt/shuttle-provider/shuttle-tools.ts".into(),
                extension_digest: digest('e'),
            },
            composed_image_digest: None,
            mount_receipt: None,
        }
    }

    #[test]
    fn runtime_descriptor_is_explicitly_lowered_without_loss() {
        let mut bundle = bundle_with(Some(write_claim()));
        let descriptor = runtime_descriptor();
        let descriptor_digest = descriptor.digest().unwrap();
        bundle.runtime_descriptor_ref = Some("artifact://runtime/one".into());
        bundle.runtime_descriptor_digest = Some(descriptor_digest.clone());
        bundle.runtime_descriptor = Some(descriptor.clone());
        let encoded = serde_json::to_string(&bundle).unwrap();
        let lowered = ExternalLaunchBundle::parse(&encoded)
            .unwrap()
            .into_binding()
            .unwrap();
        assert_eq!(
            lowered.runtime_descriptor_ref.as_deref(),
            Some("artifact://runtime/one")
        );
        assert_eq!(lowered.runtime_descriptor_digest, Some(descriptor_digest));
        assert_eq!(lowered.runtime_descriptor, Some(descriptor));
    }

    #[test]
    fn descriptor_loss_and_mismatch_are_rejected() {
        let descriptor = runtime_descriptor();
        let descriptor_digest = descriptor.digest().unwrap();
        for present in 1_u8..7 {
            let mut bundle = bundle_with(Some(write_claim()));
            if present & 0b001 != 0 {
                bundle.runtime_descriptor_ref = Some("artifact://runtime/lost".into());
            }
            if present & 0b010 != 0 {
                bundle.runtime_descriptor_digest = Some(descriptor_digest.clone());
            }
            if present & 0b100 != 0 {
                bundle.runtime_descriptor = Some(descriptor.clone());
            }
            let error = ExternalLaunchBundle::parse(&serde_json::to_string(&bundle).unwrap())
                .expect_err("every partial runtime descriptor triple must fail closed");
            assert!(error.to_string().contains("descriptor loss"));
        }

        let mut bundle = bundle_with(Some(write_claim()));
        bundle.runtime_descriptor_ref = Some("artifact://runtime/drift".into());
        bundle.runtime_descriptor_digest = Some(digest('f'));
        bundle.runtime_descriptor = Some(runtime_descriptor());
        assert!(ExternalLaunchBundle::parse(&serde_json::to_string(&bundle).unwrap()).is_err());
    }

    #[test]
    fn into_binding_without_claim_is_fail_closed() {
        let bundle = bundle_with(None);
        let b = bundle.into_binding().unwrap();
        assert!(b.allowed_paths.is_empty(), "no write claim => no writes");
    }

    #[test]
    fn parse_rejects_wrong_version() {
        let json = r#"{
            "version": 2,
            "workspace_root": "/abs/ws",
            "environment_id": "env-1",
            "launch_intent": {},
            "context_manifest": {}
        }"#;
        assert!(matches!(
            ExternalLaunchBundle::parse(json),
            Err(CellBindingError::UnsupportedVersion(2)) | Err(CellBindingError::Invalid(_))
        ));
    }

    #[test]
    fn provider_run_id_is_idempotency_key() {
        let bundle = bundle_with(Some(write_claim()));
        assert_eq!(bundle.provider_run_id(), "idem-xyz");
    }

    #[test]
    fn into_binding_rejects_relative_root() {
        let mut bundle = bundle_with(Some(write_claim()));
        bundle.workspace_root = "relative/ws".into();
        assert!(
            bundle.into_binding().is_err(),
            "non-absolute root must fail-closed"
        );
    }

    #[test]
    fn project_bound_ingest_preserves_binding_and_approval() {
        let binding = project_bound_bundle().into_binding().unwrap();
        match binding.project {
            CellProjectBinding::ProjectBound(project) => {
                assert_eq!(project.project_id, "project-a");
                assert_eq!(
                    project.project_binding_digest,
                    canonical_digest(&project_launch_context().project_binding).unwrap()
                );
                assert_eq!(
                    project.workspace_binding_digest,
                    canonical_digest(&project_launch_context().workspace_binding).unwrap()
                );
                assert_eq!(project.provider_workspace_root, "/abs/ws");
                assert_eq!(
                    project.approved_external_inputs.external_commit_oid,
                    "a".repeat(40)
                );
                assert_eq!(
                    project
                        .approved_external_inputs
                        .provider_capability_snapshot_digest,
                    digest('7')
                );
            }
            CellProjectBinding::LegacyUnbound => panic!("project context must not be dropped"),
        }
    }

    #[test]
    fn project_bound_ingest_rejects_binding_digest_drift() {
        let mut bundle = project_bound_bundle();
        bundle.launch_intent.project_binding_digest = Some(digest('f'));
        let error = bundle.into_binding().unwrap_err().to_string();
        assert!(error.contains("project binding digest drift"));
    }

    #[test]
    fn project_bound_ingest_rejects_unapproved_commit_or_capability() {
        let mut commit_drift = project_bound_bundle();
        commit_drift
            .project
            .as_mut()
            .unwrap()
            .approved_external_inputs
            .external_commit_oid = "b".repeat(40);
        assert!(commit_drift
            .into_binding()
            .unwrap_err()
            .to_string()
            .contains("approved commit"));

        let mut missing_capability = project_bound_bundle();
        missing_capability
            .project
            .as_mut()
            .unwrap()
            .approved_external_inputs
            .provider_capability_snapshot_ref
            .clear();
        assert!(missing_capability
            .into_binding()
            .unwrap_err()
            .to_string()
            .contains("provider_capability_snapshot_ref"));
    }

    #[test]
    fn missing_project_context_is_legacy_or_rejected_never_defaulted() {
        let legacy = bundle_with(Some(write_claim())).into_binding().unwrap();
        assert_eq!(legacy.project, CellProjectBinding::LegacyUnbound);

        let mut partial = bundle_with(Some(write_claim()));
        partial.launch_intent.schema_version = Some("external_launch_intent/v2".into());
        partial.launch_intent.project_binding_digest = Some(digest('2'));
        assert!(partial.into_binding().is_err());
    }
}
