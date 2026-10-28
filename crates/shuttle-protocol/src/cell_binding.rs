//! cell_binding — `cell_binding/v1`, the **shuttle-LOCAL** run envelope for
//! `shuttle cell-run`.
//!
//! ## Layering (see the contract notes)
//!
//! `cell_binding/v1` is **NOT a frozen contract** — the external orchestrator `schemas/`
//! directory contains no `cell_binding.schema.json`. It is a shuttle-local envelope
//! that carries exactly what the executor needs at spawn time. Its authoritative
//! *upstream* sources are the external orchestrator's real frozen contracts:
//!   * command allowlist  ← `external_context_manifest/v1.tool_allowlist[]`
//!   * write-domain globs  ← `external_resource_claim/v1` (`file_write_scope`)`.resource_keys[]`
//!   * assignment digest   ← `external_context_manifest/v1.context_manifest_digest` (adopted verbatim)
//!   * base OID            ← `external_context_manifest/v1.source_versions[].digest`
//!   * runtime/package id  ← `external_launch_intent/v1.{runtime_id,package_id}` (independently required)
//!
//! [`CellBinding::from_external`] *lowers* those external contracts (the 上游协议层,
//! [`crate::anchor`]) into this envelope. shuttle's `{program, argv_prefix}` matching,
//! glob semantics, empty-list=fail-closed, and `ScopedPathResolver` enforcement
//! all stay shuttle-local execution mechanics.
//!
//! ## Directly launchable as an agent
//!
//! The envelope is **also constructible without any external plumbing**: a minimal
//! hand-written `cell_binding.json` (workspace_root + allowed_paths +
//! allowed_commands + a couple of ids) parses and runs. This is the path for
//! launching shuttle directly as an agent — `shuttle cell-run --binding local.json
//! --prompt "…"` needs no `launch_intent`/`context_manifest`/`resource_claim`
//! orchestration. `from_external` is purely additive and never on the critical path
//! of the direct-launch flow.
//!
//! ## Fail-closed
//!
//! An absent/empty `allowed_paths` forbids every write; commands not in
//! `allowed_commands` never spawn; a version other than [`CELL_BINDING_VERSION`]
//! is rejected outright.

use serde::{Deserialize, Serialize};

use crate::anchor::{
    CodingAgentContextManifest, ContextManifest, LaunchIntent, ProjectFlowCodeDeliveryHandoff,
    ResourceClaim, Sha256Digest,
};
use crate::EaRuntimeDescriptor;

/// The single envelope version shuttle consumes. Anything else is refused.
pub const CELL_BINDING_VERSION: u32 = 1;

/// `cell_binding/v1` — the shuttle-local run envelope. A superset of the repo-set
/// task envelope: the existing repo-set fields are embedded as
/// [`CellBinding::repo_set`] and the cell-specific fields are added alongside.
/// A single parser handles both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellBinding {
    /// Schema version; must equal [`CELL_BINDING_VERSION`].
    pub version: u32,

    /// Canonical **absolute** path — the materialized workspace root. When
    /// lowered from external this is the materialization path the provider backfills
    /// after checking out `base_oid`; for a direct agent launch it is simply
    /// the directory shuttle should confine writes under. shuttle treats it as a
    /// read-only, run-time-immutable root and feeds it to
    /// `ScopedPathResolver::new` (after canonicalize). Never a relative path.
    pub workspace_root: String,

    /// gitignore-style **glob** patterns relative to `workspace_root` (POSIX
    /// `/`; supports `*` `**` `?` and `dir/`). Empty = fail-closed = nothing may
    /// be written. `["**"]` opens the whole tree (discouraged). Upstream source:
    /// `external_resource_claim/v1` (`file_write_scope`)`.resource_keys[]`.
    #[serde(default)]
    pub allowed_paths: Vec<String>,

    /// Command whitelist gate. A candidate passes only if its `argv[0]`
    /// basename exactly equals some entry's `program` (and, if present, its
    /// argv prefix matches `argv_prefix`). Empty = fail-closed = no command may
    /// spawn. Upstream source: `external_context_manifest/v1.tool_allowlist[]` (the
    /// authoritative tool-name set; shuttle supplies the matching mechanics).
    #[serde(default)]
    pub allowed_commands: Vec<AllowedCommand>,

    /// Read-only VCS anchor. shuttle executes on this OID and references it in the
    /// receipt; shuttle never writes it back. Upstream source:
    /// `external_context_manifest/v1.source_versions[].digest` (distinct
    /// from `admission.canary_base_oid`).
    pub base_oid: String,

    /// The assignment's canonical digest, in `sha256:<64hex>` form. Adopted
    /// **verbatim** from `external_context_manifest/v1.context_manifest_digest`
    /// (adopted mapping); shuttle copies it into the attempt/receipt chain
    /// and never recomputes it.
    pub assignment_digest: String,

    /// Content-addressed descriptor consumed for this run. Both fields are
    /// required together; external lowering rejects any descriptor loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_digest: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor: Option<EaRuntimeDescriptor>,

    /// Project ownership of this cell. Omission in an older v1 payload is
    /// normalized to the explicit `legacy_unbound` variant; it is never
    /// interpreted as a project named `default`.
    #[serde(default)]
    pub project: CellProjectBinding,

    /// `sha256:` digest of the shuttle-cli binary. Filled by **shuttle itself** at runtime
    /// (see [`CellBinding::is_shuttle_binary_digest_placeholder`]). shuttle-local, NOT a external
    /// field.
    #[serde(default)]
    pub shuttle_binary_digest: Option<String>,

    /// Opaque environment identifier — shuttle-local pass-through to the
    /// materialization layer, never interpreted (NOT a substitute for
    /// `runtime_id`/`package_id`).
    pub environment_id: String,

    /// Stable provider identity for durable lifecycle lookup. external lowers its
    /// launch idempotency key here; direct bindings may omit it and use the
    /// environment id as a compatibility fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_run_id: Option<String>,

    /// external attempt identity, preserved for receipt correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,

    /// external `runtime_id` (runtime instance). Independently required upstream
    ///; carried here when lowered from a `launch_intent`. Optional so
    /// a direct agent launch need not supply it.
    #[serde(default)]
    pub runtime_id: Option<String>,

    /// external `package_id` (work package). Independently required upstream
    ///; carried here when lowered from a `launch_intent`. Optional so
    /// a direct agent launch need not supply it.
    #[serde(default)]
    pub package_id: Option<String>,

    /// Optional reference to the execution profile (capability ceiling). Upstream
    /// this is `external_launch_intent/v1.execution_profile_ref`; the ref points
    /// at a shuttle-local profile doc (external does not freeze it, ruling §3).
    #[serde(default)]
    pub profile_ref: Option<String>,

    /// Optional pi session file location for this cell.
    #[serde(default)]
    pub session_file: Option<String>,

    /// Embedded repo-set task envelope (superset, not replacement). Kept opaque
    /// here so the repo-set fields can evolve without shuttle churn.
    #[serde(default)]
    pub repo_set: Option<serde_json::Value>,
}

/// Approval-bound external inputs for a project-bound cell. The approval
/// artifact commits to the exact source commit and provider capability
/// snapshot; shuttle adopts the tuple and verifies it against the workspace/base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedExternalInputs {
    pub schema_version: String,
    pub decision: String,
    pub approval_ref: String,
    pub approval_digest: Sha256Digest,
    pub external_commit_oid: String,
    pub provider_capability_snapshot_ref: String,
    pub provider_capability_snapshot_digest: Sha256Digest,
    pub project_binding_digest: Sha256Digest,
}

/// Project binding carried by the local cell envelope. This is deliberately a
/// tagged all-or-nothing state so partially supplied identity cannot degrade
/// into an implicit default project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CellProjectBinding {
    #[default]
    LegacyUnbound,
    ProjectBound(Box<ProjectBoundCell>),
}

/// The immutable project-bound payload, boxed to keep legacy bindings cheap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBoundCell {
    pub project_id: String,
    pub authorization_scope: String,
    pub assignment_ref: String,
    pub execution_assignment_digest: Sha256Digest,
    pub project_binding_ref: String,
    pub project_binding_digest: Sha256Digest,
    pub workspace_binding_ref: String,
    pub workspace_binding_digest: Sha256Digest,
    pub repo_id: String,
    pub provider_workspace_root: String,
    pub provider_allowed_write_scopes: Vec<String>,
    pub provider_receipt_digest: Sha256Digest,
    pub approved_external_inputs: ApprovedExternalInputs,
}

impl CellProjectBinding {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::LegacyUnbound => "legacy_unbound",
            Self::ProjectBound(_) => "project_bound",
        }
    }

    pub fn binding_digest(&self) -> Option<&Sha256Digest> {
        match self {
            Self::LegacyUnbound => None,
            Self::ProjectBound(project) => Some(&project.project_binding_digest),
        }
    }

    pub fn provider_workspace_root(&self) -> Option<&str> {
        match self {
            Self::LegacyUnbound => None,
            Self::ProjectBound(project) => Some(&project.provider_workspace_root),
        }
    }

    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::LegacyUnbound => None,
            Self::ProjectBound(project) => Some(&project.project_id),
        }
    }
}

/// One command whitelist entry. `program` is a bare basename (no path
/// separators); the optional `argv_prefix` pins leading argv items exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllowedCommand {
    /// Executable basename, matched case-sensitively against `argv[0]`'s
    /// basename. Must not contain a path separator.
    pub program: String,
    /// If present, the candidate argv's leading items must equal these
    /// exactly (e.g. `["status"]` narrows `git` to `git status`).
    #[serde(default)]
    pub argv_prefix: Option<Vec<String>>,
}

/// Why a cell binding was rejected at parse/validation time (fail-closed).
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CellBindingError {
    #[error("unsupported cell_binding version: {0}")]
    UnsupportedVersion(u32),
    #[error("invalid cell_binding payload: {0}")]
    Invalid(String),
    #[error("workspace_root must be an absolute path: {0}")]
    NonAbsoluteRoot(String),
    #[error("allowed_commands[{index}].program must not contain a path separator: {program}")]
    CommandHasSeparator { index: usize, program: String },
    #[error("invalid project binding: {0}")]
    InvalidProjectBinding(String),
    #[error("invalid runtime descriptor binding: {0}")]
    InvalidRuntimeDescriptor(String),
}

impl CellBinding {
    /// Parse and validate a `cell_binding/v1` JSON envelope. Fail-closed:
    /// wrong version, non-absolute root, or a `program` containing a path
    /// separator are all hard rejects.
    pub fn parse(json: &str) -> Result<Self, CellBindingError> {
        let binding: CellBinding =
            serde_json::from_str(json).map_err(|e| CellBindingError::Invalid(e.to_string()))?;
        binding.validate()?;
        Ok(binding)
    }

    /// Lower the 上游协议层 contracts into a shuttle-local `cell_binding/v1` envelope.
    ///
    /// This is the external-orchestrated construction path (the opposite of a
    /// hand-written direct-launch binding). It routes each field to its
    /// authoritative external source per the ruling:
    ///   * `allowed_commands` ← `manifest.tool_allowlist` (name-only entries;
    ///     shuttle's argv-prefix narrowing is applied later, locally),
    ///   * `allowed_paths`    ← the `file_write_scope` claim's `resource_keys`
    ///     (fail-closed: no such claim ⇒ empty ⇒ no writes),
    ///   * `assignment_digest`← `manifest.context_manifest_digest` (verbatim),
    ///   * `base_oid`         ← first `manifest.source_versions[].digest`,
    ///   * `runtime_id`/`package_id`/`profile_ref` ← the `launch_intent`.
    ///
    /// `workspace_root` and `environment_id` are shuttle-local pass-throughs the external orchestrator's
    /// frozen contracts do not carry, so they are supplied by the caller. The
    /// result still goes through [`CellBinding::validate`] semantics when parsed
    /// back; callers building it programmatically should call
    /// [`CellBinding::validate`].
    pub fn from_external(
        workspace_root: impl Into<String>,
        environment_id: impl Into<String>,
        manifest: &ContextManifest,
        write_claim: Option<&ResourceClaim>,
        launch_intent: Option<&LaunchIntent>,
    ) -> Self {
        let allowed_commands = manifest
            .tool_allowlist
            .iter()
            .map(|program| AllowedCommand {
                program: program.clone(),
                argv_prefix: None,
            })
            .collect();

        // Fail-closed: only an exclusive_write file_write_scope claim yields a
        // write domain; anything else (or None) ⇒ empty ⇒ no writes.
        let allowed_paths = write_claim
            .and_then(|c| c.file_write_scope_keys())
            .map(|keys| keys.to_vec())
            .unwrap_or_default();

        let base_oid = manifest
            .source_versions
            .first()
            .map(|sv| sv.digest.clone())
            .unwrap_or_default();

        CellBinding {
            version: CELL_BINDING_VERSION,
            workspace_root: workspace_root.into(),
            allowed_paths,
            allowed_commands,
            base_oid,
            // Adopted verbatim — never recomputed.
            assignment_digest: manifest.context_manifest_digest.as_str().to_string(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            project: CellProjectBinding::LegacyUnbound,
            shuttle_binary_digest: None,
            environment_id: environment_id.into(),
            provider_run_id: launch_intent.map(|li| li.idempotency_key.clone()),
            attempt_id: launch_intent.map(|li| li.attempt_id.clone()),
            runtime_id: launch_intent.map(|li| li.runtime_id.clone()),
            package_id: launch_intent.map(|li| li.package_id.clone()),
            profile_ref: launch_intent.map(|li| li.execution_profile_ref.clone()),
            session_file: None,
            repo_set: None,
        }
    }

    /// Lower the frozen **coding-flow** contract family into a shuttle-local
    /// `cell_binding/v1` envelope.
    ///
    /// This is the second external-orchestrated construction path (alongside
    /// [`Self::from_external`], which lowers the agent-lifecycle contracts). It
    /// consumes the two frozen coding-flow schemas that carry an shuttle-consumable
    /// run envelope — `project_flow_code_delivery_handoff/v1` and
    /// `coding_agent_context_manifest/v1` — and routes each field to its
    /// authoritative external source:
    ///   * `allowed_paths`     ← `context_manifest.allowed_write_scopes[].paths`
    ///     (flattened, order-preserving; **fail-closed**: no scope ⇒ empty ⇒
    ///     nothing may be written),
    ///   * `base_oid`          ← `context_manifest.workspace.digest` (the frozen
    ///     VCS anchor shuttle executes on and references in the receipt; shuttle never
    ///     writes it back),
    ///   * `assignment_digest` ← `handoff.planning_structure_digest` (adopted
    ///     **verbatim**; shuttle copies it into the attempt/receipt chain and never
    ///     recomputes it),
    ///   * `attempt_id`        ← `context_manifest.attempt_id`.
    ///
    /// The frozen coding-flow family deliberately carries **no** program-name
    /// allowlist and **no** shell strings (all execution is by `command_id`
    /// reference into a separate quality plan), so `allowed_commands` is left
    /// **empty** — fail-closed, no command may spawn from this path until shuttle
    /// narrows it locally. `context_manifest.quality_command_ids` and
    /// `acceptance` remain read-only anchors on the external orchestrator contract; shuttle does not
    /// copy them into the run envelope because the envelope has no field that
    /// authoritatively owns them (red-line #11: shuttle only reasons/proves, never
    /// re-freezes external state).
    ///
    /// `workspace_root` and `environment_id` are shuttle-local pass-throughs the
    /// frozen contracts do not carry (the contracts hold a `workspace.ref`, not
    /// a physical path — red-line #11: shuttle never owns physical paths), so the
    /// caller supplies them. This method performs **pure lowering** and does not
    /// itself validate the external orchestrator contracts; callers should validate the source
    /// contracts (`CodingFlowContract::validate`) and re-check the result with
    /// [`CellBinding::validate`].
    pub fn from_coding_flow(
        workspace_root: impl Into<String>,
        environment_id: impl Into<String>,
        handoff: &ProjectFlowCodeDeliveryHandoff,
        context_manifest: &CodingAgentContextManifest,
    ) -> Self {
        // Fail-closed write domain: flatten every allowed_write_scope's paths.
        // Empty ⇒ no writes.
        let allowed_paths: Vec<String> = context_manifest
            .allowed_write_scopes
            .iter()
            .flat_map(|scope| scope.paths.iter().cloned())
            .collect();

        CellBinding {
            version: CELL_BINDING_VERSION,
            workspace_root: workspace_root.into(),
            allowed_paths,
            // No program-name allowlist in the frozen coding-flow family ⇒
            // fail-closed: no command may spawn from this path.
            allowed_commands: Vec::new(),
            // Read-only VCS anchor ← the frozen workspace digest.
            base_oid: context_manifest.workspace.digest.as_str().to_string(),
            // Adopted verbatim (never recomputed).
            assignment_digest: handoff.planning_structure_digest.as_str().to_string(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            project: CellProjectBinding::LegacyUnbound,
            shuttle_binary_digest: None,
            environment_id: environment_id.into(),
            provider_run_id: None,
            attempt_id: Some(context_manifest.attempt_id.clone()),
            runtime_id: None,
            package_id: None,
            profile_ref: None,
            session_file: None,
            repo_set: None,
        }
    }

    /// Structural validation shared by [`CellBinding::parse`]. Kept public so
    /// callers that build a binding programmatically (incl. [`Self::from_external`])
    /// can re-check it.
    pub fn validate(&self) -> Result<(), CellBindingError> {
        if self.version != CELL_BINDING_VERSION {
            return Err(CellBindingError::UnsupportedVersion(self.version));
        }
        // root must be absolute — POSIX `/...` or a Windows-style prefix.
        if !std::path::Path::new(&self.workspace_root).is_absolute() {
            return Err(CellBindingError::NonAbsoluteRoot(
                self.workspace_root.clone(),
            ));
        }
        // program entries must be bare basenames.
        for (index, cmd) in self.allowed_commands.iter().enumerate() {
            if cmd.program.contains('/') || cmd.program.contains('\\') {
                return Err(CellBindingError::CommandHasSeparator {
                    index,
                    program: cmd.program.clone(),
                });
            }
        }
        match (
            self.runtime_descriptor_ref.as_deref(),
            self.runtime_descriptor_digest.as_ref(),
            self.runtime_descriptor.as_ref(),
        ) {
            (None, None, None) => {}
            (Some(reference), Some(digest), Some(descriptor)) => {
                if reference.trim().is_empty() || reference.contains(['\n', '\r', '\0']) {
                    return Err(CellBindingError::InvalidRuntimeDescriptor(
                        "runtime_descriptor_ref must be non-empty and single-line".into(),
                    ));
                }
                descriptor.validate().map_err(|error| {
                    CellBindingError::InvalidRuntimeDescriptor(error.to_string())
                })?;
                let actual = descriptor.digest().map_err(|error| {
                    CellBindingError::InvalidRuntimeDescriptor(error.to_string())
                })?;
                if &actual != digest {
                    return Err(CellBindingError::InvalidRuntimeDescriptor(
                        "runtime_descriptor_digest does not match the canonical descriptor".into(),
                    ));
                }
            }
            _ => {
                return Err(CellBindingError::InvalidRuntimeDescriptor(
                    "runtime_descriptor_ref, runtime_descriptor_digest and runtime_descriptor must be supplied together".into(),
                ));
            }
        }
        if let CellProjectBinding::ProjectBound(project) = &self.project {
            let ProjectBoundCell {
                project_id,
                authorization_scope,
                assignment_ref,
                project_binding_ref,
                project_binding_digest,
                workspace_binding_ref,
                repo_id,
                provider_workspace_root,
                provider_allowed_write_scopes,
                approved_external_inputs,
                ..
            } = project.as_ref();
            if !valid_project_id(project_id) {
                return Err(CellBindingError::InvalidProjectBinding(format!(
                    "invalid project_id {project_id:?}"
                )));
            }
            for (name, value) in [
                ("authorization_scope", authorization_scope),
                ("assignment_ref", assignment_ref),
                ("project_binding_ref", project_binding_ref),
                ("workspace_binding_ref", workspace_binding_ref),
                ("repo_id", repo_id),
                ("approval_ref", &approved_external_inputs.approval_ref),
                (
                    "provider_capability_snapshot_ref",
                    &approved_external_inputs.provider_capability_snapshot_ref,
                ),
            ] {
                if value.trim().is_empty() {
                    return Err(CellBindingError::InvalidProjectBinding(format!(
                        "{name} must be non-empty"
                    )));
                }
            }
            if !std::path::Path::new(provider_workspace_root).is_absolute() {
                return Err(CellBindingError::InvalidProjectBinding(format!(
                    "provider_workspace_root must be absolute: {provider_workspace_root}"
                )));
            }
            if approved_external_inputs.project_binding_digest != *project_binding_digest {
                return Err(CellBindingError::InvalidProjectBinding(
                    "approval project_binding_digest does not match cell binding".into(),
                ));
            }
            approved_external_inputs.validate()?;
            if approved_external_inputs.external_commit_oid != self.base_oid {
                return Err(CellBindingError::InvalidProjectBinding(
                    "approved external commit does not match base_oid".into(),
                ));
            }
            if !is_lower_hex_oid(&self.base_oid) {
                return Err(CellBindingError::InvalidProjectBinding(
                    "project-bound base_oid must be 40 lowercase hex characters".into(),
                ));
            }
            if self.workspace_root != *provider_workspace_root {
                return Err(CellBindingError::InvalidProjectBinding(
                    "workspace_root does not match the provider workspace binding".into(),
                ));
            }
            if self.allowed_paths != *provider_allowed_write_scopes {
                return Err(CellBindingError::InvalidProjectBinding(
                    "allowed_paths do not match the provider workspace binding".into(),
                ));
            }
            if provider_allowed_write_scopes.is_empty()
                || provider_allowed_write_scopes
                    .iter()
                    .any(|scope| !valid_relative_scope(scope))
            {
                return Err(CellBindingError::InvalidProjectBinding(
                    "provider write scopes must be non-empty relative paths without traversal"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Whether the binding still needs shuttle to fill `shuttle_binary_digest`. True when
    /// the field is absent or empty — shuttle self-computes it then.
    pub fn is_shuttle_binary_digest_placeholder(&self) -> bool {
        self.shuttle_binary_digest
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true)
    }

    /// Command gate. Returns true iff `argv[0]`'s basename exactly matches some
    /// entry's `program` and (when set) the entry's `argv_prefix` is an exact
    /// prefix of `argv`. Empty allowlist or empty argv → false (fail-closed).
    pub fn command_allowed(&self, argv: &[String]) -> bool {
        let Some(program) = argv.first() else {
            return false;
        };
        let basename = std::path::Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        for cmd in &self.allowed_commands {
            if cmd.program != basename {
                continue;
            }
            match &cmd.argv_prefix {
                None => return true,
                Some(prefix) => {
                    let rest = &argv[1..];
                    if rest.len() >= prefix.len() && prefix.iter().zip(rest).all(|(a, b)| a == b) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Strict verifier gate: argv must equal one explicit basename + prefix.
    /// Name-only entries and model-supplied suffix arguments are rejected.
    pub fn command_exactly_allowed(&self, argv: &[String]) -> bool {
        let Some(program) = argv.first() else {
            return false;
        };
        let basename = std::path::Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        self.allowed_commands.iter().any(|command| {
            command.program == basename
                && command
                    .argv_prefix
                    .as_ref()
                    .is_some_and(|prefix| argv[1..] == prefix[..])
        })
    }
}

impl ApprovedExternalInputs {
    /// Verify that the approval digest commits to the exact external commit,
    /// provider capability snapshot and project binding consumed by the cell.
    fn validate(&self) -> Result<(), CellBindingError> {
        if self.schema_version != "ea_external_project_approval/v1" || self.decision != "approved" {
            return Err(CellBindingError::InvalidProjectBinding(
                "external inputs require an approved v1 approval".into(),
            ));
        }
        let payload = serde_json::json!({
            "decision": self.decision,
            "external_commit_oid": self.external_commit_oid,
            "project_binding_digest": self.project_binding_digest,
            "provider_capability_snapshot_digest": self.provider_capability_snapshot_digest,
            "provider_capability_snapshot_ref": self.provider_capability_snapshot_ref,
            "schema_version": self.schema_version,
        });
        let bytes = serde_json::to_vec(&payload)
            .map_err(|error| CellBindingError::InvalidProjectBinding(error.to_string()))?;
        let actual = Sha256Digest::parse(format!("sha256:{}", crate::hashing::sha256_hex(&bytes)))
            .expect("sha256_hex always emits a canonical digest");
        if actual != self.approval_digest {
            return Err(CellBindingError::InvalidProjectBinding(
                "approval digest does not bind the external commit and capability snapshot".into(),
            ));
        }
        Ok(())
    }
}

fn valid_project_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && !value.contains("..")
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn is_lower_hex_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_relative_scope(value: &str) -> bool {
    !value.is_empty()
        && !std::path::Path::new(value).is_absolute()
        && !value
            .split(['/', '\\'])
            .any(|component| component == ".." || component.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{
        ApprovalDecision, ApprovalReceipt, ClaimMode, CodingAgentContextManifest, ContextManifest,
        FormalAssurance, FormalLevel, LaunchIntent, ProjectFlowCodeDeliveryHandoff, ResourceClaim,
        ResourceType, Sha256Digest, SourceVersion, WorkspaceRef, WriteScope,
    };

    fn minimal_json() -> String {
        r#"{
            "version": 1,
            "workspace_root": "/tmp/cell/ws",
            "allowed_paths": ["src/**"],
            "allowed_commands": [{"program": "git", "argv_prefix": ["status"]}],
            "base_oid": "abc123",
            "assignment_digest": "sha256:deadbeef",
            "environment_id": "env-1"
        }"#
        .to_string()
    }

    #[test]
    fn parses_minimal_binding() {
        let b = CellBinding::parse(&minimal_json()).unwrap();
        assert_eq!(b.version, 1);
        assert_eq!(b.workspace_root, "/tmp/cell/ws");
        assert_eq!(b.allowed_paths, vec!["src/**".to_string()]);
        assert_eq!(b.base_oid, "abc123");
        assert_eq!(b.environment_id, "env-1");
        assert!(b.is_shuttle_binary_digest_placeholder());
        // Direct-launch bindings carry no external runtime/package ids.
        assert!(b.runtime_id.is_none());
        assert!(b.package_id.is_none());
    }

    #[test]
    fn omitted_trace_ids_stay_omitted_after_round_trip() {
        let b = CellBinding::parse(&minimal_json()).unwrap();
        let value = serde_json::to_value(b).unwrap();
        assert!(value.get("provider_run_id").is_none());
        assert!(value.get("attempt_id").is_none());
    }

    #[test]
    fn rejects_wrong_version() {
        let j = minimal_json().replace("\"version\": 1", "\"version\": 2");
        assert!(matches!(
            CellBinding::parse(&j),
            Err(CellBindingError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn rejects_relative_root() {
        let j = minimal_json().replace("/tmp/cell/ws", "cell/ws");
        assert!(matches!(
            CellBinding::parse(&j),
            Err(CellBindingError::NonAbsoluteRoot(_))
        ));
    }

    #[test]
    fn rejects_program_with_separator() {
        let j = minimal_json().replace("\"program\": \"git\"", "\"program\": \"bin/git\"");
        assert!(matches!(
            CellBinding::parse(&j),
            Err(CellBindingError::CommandHasSeparator { .. })
        ));
    }

    #[test]
    fn empty_allowed_commands_is_fail_closed() {
        let j = minimal_json().replace(
            "\"allowed_commands\": [{\"program\": \"git\", \"argv_prefix\": [\"status\"]}],",
            "\"allowed_commands\": [],",
        );
        let b = CellBinding::parse(&j).unwrap();
        assert!(!b.command_allowed(&["git".to_string()]));
    }

    #[test]
    fn exact_command_gate_rejects_name_only_and_suffix_widening() {
        let binding = CellBinding::parse(&minimal_json()).unwrap();
        assert!(binding.command_exactly_allowed(&["git".into(), "status".into()]));
        assert!(!binding.command_exactly_allowed(&[
            "git".into(),
            "status".into(),
            "--porcelain".into(),
        ]));
        let mut name_only = binding;
        name_only.allowed_commands[0].argv_prefix = None;
        assert!(!name_only.command_exactly_allowed(&["git".into()]));
    }

    #[test]
    fn command_gate_matches_basename_and_prefix() {
        let b = CellBinding::parse(&minimal_json()).unwrap();
        assert!(b.command_allowed(&["/opt/tools/git".to_string(), "status".to_string()]));
        assert!(!b.command_allowed(&["git".to_string(), "push".to_string()]));
        assert!(b.command_allowed(&[
            "git".to_string(),
            "status".to_string(),
            "--porcelain".to_string()
        ]));
        assert!(!b.command_allowed(&["rm".to_string(), "-rf".to_string()]));
    }

    #[test]
    fn missing_required_field_is_invalid() {
        let j = r#"{"version":1,"allowed_paths":[],"base_oid":"x","assignment_digest":"y","environment_id":"z"}"#;
        assert!(matches!(
            CellBinding::parse(j),
            Err(CellBindingError::Invalid(_))
        ));
    }

    // ---- from_external lowering (ruling §1, §4) ------------------------------------

    fn cf_digest() -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap()
    }

    fn manifest_with(tools: Vec<&str>, oid: &str) -> ContextManifest {
        ContextManifest {
            context_manifest_id: "cm-1".into(),
            context_manifest_digest: cf_digest(),
            tool_allowlist: tools.into_iter().map(String::from).collect(),
            scope: None,
            write_claim_refs: vec![],
            source_versions: vec![SourceVersion {
                source_id: Some("repo".into()),
                digest: oid.into(),
            }],
            extra: Default::default(),
        }
    }

    fn file_write_claim(keys: Vec<&str>) -> ResourceClaim {
        ResourceClaim {
            resource_type: ResourceType::FileWriteScope,
            resource_keys: keys.into_iter().map(String::from).collect(),
            claim_mode: ClaimMode::ExclusiveWrite,
            lease_epoch: Some(1),
            extra: Default::default(),
        }
    }

    fn launch_intent() -> LaunchIntent {
        LaunchIntent {
            schema_version: None,
            launch_intent_id: "li-1".into(),
            attempt_id: "at-1".into(),
            runtime_id: "rt-9".into(),
            package_id: "pkg-9".into(),
            idempotency_key: "idem-1".into(),
            agent_type: "shuttle-cell".into(),
            execution_profile_ref: "profile://shuttle/local/leaf".into(),
            assignment_ref: None,
            assignment_digest: None,
            project_binding_digest: None,
            context_manifest_ref: "cm://1".into(),
            context_manifest_digest: None,
            workspace_binding_ref: None,
            workspace_binding_digest: None,
            resource_claims: vec![],
            resource_claim_refs: vec!["claim://w".into()],
            deadline: "2026-09-02T00:00:00Z".into(),
            requested_by: "root-session".into(),
            extra: Default::default(),
        }
    }

    #[test]
    fn from_external_lowers_all_authoritative_sources() {
        let manifest = manifest_with(vec!["git", "cargo"], "oid-777");
        let claim = file_write_claim(vec!["src/**", "docs/**"]);
        let li = launch_intent();

        let b = CellBinding::from_external(
            "/abs/ws",
            "env-materialized-1",
            &manifest,
            Some(&claim),
            Some(&li),
        );

        // command allowlist ← tool_allowlist (name-only, no argv prefix yet).
        assert_eq!(
            b.allowed_commands,
            vec![
                AllowedCommand {
                    program: "git".into(),
                    argv_prefix: None
                },
                AllowedCommand {
                    program: "cargo".into(),
                    argv_prefix: None
                },
            ]
        );
        // write domain ← file_write_scope.resource_keys.
        assert_eq!(
            b.allowed_paths,
            vec!["src/**".to_string(), "docs/**".to_string()]
        );
        // assignment_digest adopted verbatim.
        assert_eq!(b.assignment_digest, cf_digest().as_str());
        // base_oid ← first source_versions[].digest.
        assert_eq!(b.base_oid, "oid-777");
        // runtime/package/profile ← launch_intent (§4 independently carried).
        assert_eq!(b.runtime_id.as_deref(), Some("rt-9"));
        assert_eq!(b.package_id.as_deref(), Some("pkg-9"));
        assert_eq!(
            b.profile_ref.as_deref(),
            Some("profile://shuttle/local/leaf")
        );
        // shuttle self-fills the binary digest later.
        assert!(b.is_shuttle_binary_digest_placeholder());
        // The lowered envelope validates like any other.
        assert!(b.validate().is_ok());
    }

    #[test]
    fn from_external_without_write_claim_is_fail_closed() {
        let manifest = manifest_with(vec!["git"], "oid-1");
        // No file_write_scope claim ⇒ empty allowed_paths ⇒ no writes.
        let b = CellBinding::from_external("/abs/ws", "env-1", &manifest, None, None);
        assert!(b.allowed_paths.is_empty());
        assert!(b.runtime_id.is_none());
        assert!(b.package_id.is_none());
    }

    #[test]
    fn from_external_ignores_non_write_claim() {
        let manifest = manifest_with(vec!["git"], "oid-1");
        let worktree = ResourceClaim {
            resource_type: ResourceType::Worktree,
            resource_keys: vec!["ws-a".into()],
            claim_mode: ClaimMode::ExclusiveWrite,
            lease_epoch: Some(1),
            extra: Default::default(),
        };
        let b = CellBinding::from_external("/abs/ws", "env-1", &manifest, Some(&worktree), None);
        assert!(
            b.allowed_paths.is_empty(),
            "a worktree claim is not a write domain — fail-closed"
        );
    }

    // ---- from_coding_flow lowering (N4b) -------------------------------------

    fn ps_digest() -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap()
    }

    fn ws_digest() -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).unwrap()
    }

    fn strat_digest() -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", "d".repeat(64))).unwrap()
    }

    fn handoff_with(scopes: Vec<WriteScope>) -> ProjectFlowCodeDeliveryHandoff {
        ProjectFlowCodeDeliveryHandoff {
            schema_version: "project_flow_code_delivery_handoff/v1".into(),
            handoff_id: "ho-1".into(),
            program_id: "prog-1".into(),
            child_dag_id: "cd-1".into(),
            task_id: "C0".into(),
            planning_structure_digest: ps_digest(),
            spec_ref: "spec://x".into(),
            task_dag_ref: "dag://x".into(),
            objective: "do the thing".into(),
            scope: vec!["a".into()],
            non_goals: vec![],
            acceptance: vec!["works".into()],
            upstream_receipts: vec![],
            workspace_strategy_ref: "strat://x".into(),
            workspace_strategy_digest: strat_digest(),
            write_scopes: scopes,
            quality_policy_ref: "qp://x".into(),
            formal_assurance: FormalAssurance {
                level: FormalLevel::F0,
                affected_invariants: vec![],
                positive_obligations: vec![],
            },
            approval_receipt: ApprovalReceipt {
                receipt_id: "rc-1".into(),
                approved_by: "approver-1".into(),
                approved_at: "2026-09-02T00:00:00Z".into(),
                decision: ApprovalDecision::Approved,
                planning_structure_digest: ps_digest(),
                workspace_strategy_digest: strat_digest(),
            },
        }
    }

    fn context_manifest_with(scopes: Vec<WriteScope>) -> CodingAgentContextManifest {
        CodingAgentContextManifest {
            schema_version: "coding_agent_context_manifest/v1".into(),
            context_id: "cx-1".into(),
            delivery_id: "dl-1".into(),
            attempt_id: "at-77".into(),
            role: "coding-agent".into(),
            objective: "do the thing".into(),
            non_goals: vec![],
            source_refs: vec![],
            workspace: WorkspaceRef {
                workspace_ref: "ws://materialize/1".into(),
                digest: ws_digest(),
            },
            allowed_write_scopes: scopes,
            upstream_receipts: vec![],
            acceptance: vec!["works".into()],
            quality_command_ids: vec!["cmd-build".into(), "cmd-test".into()],
            formal_obligations: vec![],
            prior_attempt: None,
            result_schema_ref: "schema://coding_attempt_result/v1".into(),
        }
    }

    #[test]
    fn from_coding_flow_lowers_all_authoritative_sources() {
        let scopes = vec![
            WriteScope {
                repo_id: "repo-a".into(),
                paths: vec!["src/**".into(), "docs/**".into()],
            },
            WriteScope {
                repo_id: "repo-b".into(),
                paths: vec!["lib/**".into()],
            },
        ];
        let handoff = handoff_with(scopes.clone());
        let cm = context_manifest_with(scopes);

        let b = CellBinding::from_coding_flow("/abs/ws", "env-materialized-9", &handoff, &cm);

        // allowed_paths ← flattened allowed_write_scopes[].paths, order-preserving.
        assert_eq!(
            b.allowed_paths,
            vec![
                "src/**".to_string(),
                "docs/**".to_string(),
                "lib/**".to_string(),
            ]
        );
        // No program allowlist in the frozen coding-flow family ⇒ fail-closed.
        assert!(b.allowed_commands.is_empty());
        // base_oid ← workspace.digest (frozen VCS anchor).
        assert_eq!(b.base_oid, ws_digest().as_str());
        // assignment_digest ← handoff.planning_structure_digest, verbatim.
        assert_eq!(b.assignment_digest, ps_digest().as_str());
        // attempt_id ← context_manifest.attempt_id.
        assert_eq!(b.attempt_id.as_deref(), Some("at-77"));
        // shuttle self-fills the binary digest later.
        assert!(b.is_shuttle_binary_digest_placeholder());
        // The lowered envelope validates like any other.
        assert!(b.validate().is_ok());
    }

    #[test]
    fn from_coding_flow_without_write_scopes_is_fail_closed() {
        let handoff = handoff_with(vec![WriteScope {
            repo_id: "repo-a".into(),
            paths: vec!["src/**".into()],
        }]);
        // Empty allowed_write_scopes on the context manifest ⇒ no writes.
        let cm = context_manifest_with(vec![]);
        let b = CellBinding::from_coding_flow("/abs/ws", "env-1", &handoff, &cm);
        assert!(
            b.allowed_paths.is_empty(),
            "no write scope ⇒ empty allowed_paths ⇒ nothing writable"
        );
        assert!(b.allowed_commands.is_empty());
    }
}
