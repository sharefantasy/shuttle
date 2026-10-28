use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::fmt;
use std::str::FromStr;

pub const GOAL_SCHEMA: &str = "goal_control_goal/v1";
pub const ATTACHMENT_SCHEMA: &str = "goal_control_attachment/v1";
pub const PROJECTION_SCHEMA: &str = "goal_control_projection/v1";
pub const CAPABILITY_SCHEMA: &str = "goal_control_provider_capability/v1";
pub const CONDITION_SCHEMA: &str = "goal_control_condition_evaluation/v1";
pub const ACCEPTANCE_SCHEMA: &str = "goal_acceptance/v1";
pub const ACTION_BUNDLE_SCHEMA: &str = "shuttle_goal_action_bundle/v1";
pub const ACTION_RESULT_SCHEMA: &str = "shuttle_goal_action_result/v1";
pub const RUN_PLAN_SCHEMA: &str = "shuttle_goal_run_plan/v1";
pub const RUN_RESULT_SCHEMA: &str = "shuttle_goal_run_result/v1";
pub const MIGRATION_BUNDLE_SCHEMA: &str = "goal_control_migration_bundle/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceContract {
    pub schema_version: String,
    pub required_gates: Vec<String>,
}

impl AcceptanceContract {
    pub fn parse(bytes: &[u8]) -> Result<Self, GoalError> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.schema_version != ACCEPTANCE_SCHEMA {
            return Err(GoalError::InvalidInput(format!(
                "unsupported acceptance schema: {}",
                value.schema_version
            )));
        }
        if value.required_gates.is_empty() {
            return Err(GoalError::InvalidInput(
                "acceptance requires at least one gate".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for gate in &value.required_gates {
            if gate.trim().is_empty() || !seen.insert(gate.clone()) {
                return Err(GoalError::InvalidInput(
                    "acceptance gate ids must be non-empty and unique".into(),
                ));
            }
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBackend {
    Local,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRef {
    pub backend: GoalBackend,
    pub instance_id: String,
    pub goal_id: String,
}

impl fmt::Display for GoalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match self.backend {
            GoalBackend::Local => "local",
            GoalBackend::External => "external",
        };
        write!(f, "goal:{backend}:{}:{}", self.instance_id, self.goal_id)
    }
}

impl FromStr for GoalRef {
    type Err = GoalError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = value.split(':').collect();
        if parts.len() != 4 || parts[0] != "goal" || !safe_id(parts[2]) || !safe_id(parts[3]) {
            return Err(GoalError::InvalidInput(format!(
                "invalid Goal ref: {value}"
            )));
        }
        let backend = match parts[1] {
            "local" => GoalBackend::Local,
            "external" => GoalBackend::External,
            other => {
                return Err(GoalError::InvalidInput(format!(
                    "unsupported Goal backend: {other}"
                )))
            }
        };
        Ok(Self {
            backend,
            instance_id: parts[2].into(),
            goal_id: parts[3].into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Draft,
    Active,
    Paused,
    Waiting,
    Blocked,
    Complete,
    Cancelled,
    Transferred,
    Sealed,
}

impl GoalStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Cancelled | Self::Transferred | Self::Sealed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalHealth {
    Healthy,
    Degraded,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentRole {
    Controller,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    Active,
    Released,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionStatus {
    Satisfied,
    Unsatisfied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Committed,
    Running,
    Settled,
    ReconcileRequired,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActionRole {
    #[default]
    Executor,
    Verifier,
}

/// An authority-free action template consumed by the bounded Goal driver.
///
/// Goal and attachment revisions are deliberately absent: the trusted driver
/// re-reads them from the selected backend immediately before every action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalActionTemplate {
    pub action_id: String,
    pub action_kind: String,
    pub role: GoalActionRole,
    pub idempotency_key: String,
    pub cell_binding: shuttle_protocol::CellBinding,
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    /// Required acceptance gates this action may satisfy after a successful,
    /// attributable result. Unknown gates are rejected by the backend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub satisfies: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRunPlan {
    pub schema_version: String,
    pub goal_ref: String,
    pub attachment_id: String,
    pub actions: Vec<GoalActionTemplate>,
}

impl GoalRunPlan {
    pub fn parse(bytes: &[u8]) -> Result<Self, GoalError> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(GoalError::InvalidInput(
                "run plan exceeds the 4MiB input limit".into(),
            ));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        if value.schema_version != RUN_PLAN_SCHEMA {
            return Err(GoalError::InvalidInput(format!(
                "unsupported run plan schema: {}",
                value.schema_version
            )));
        }
        value.goal_ref.parse::<GoalRef>()?;
        if !safe_id(&value.attachment_id) {
            return Err(GoalError::InvalidInput(
                "attachment_id must be a safe non-empty id".into(),
            ));
        }
        if value.actions.is_empty() {
            return Err(GoalError::InvalidInput(
                "run plan requires at least one action".into(),
            ));
        }
        if value.actions.len() > 1024 {
            return Err(GoalError::InvalidInput(
                "run plan exceeds the 1024 action limit".into(),
            ));
        }
        let mut action_ids = std::collections::BTreeSet::new();
        let mut idempotency_keys = std::collections::BTreeSet::new();
        let mut verifier_count = 0usize;
        for (index, action) in value.actions.iter().enumerate() {
            for (name, field) in [
                ("action_id", &action.action_id),
                ("action_kind", &action.action_kind),
                ("idempotency_key", &action.idempotency_key),
                ("prompt", &action.prompt),
            ] {
                if field.trim().is_empty() {
                    return Err(GoalError::InvalidInput(format!(
                        "actions[{index}].{name} must not be empty"
                    )));
                }
            }
            if !safe_id(&action.action_id) {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}].action_id must be a safe id"
                )));
            }
            if !action_ids.insert(action.action_id.clone()) {
                return Err(GoalError::InvalidInput(format!(
                    "duplicate action_id: {}",
                    action.action_id
                )));
            }
            if !idempotency_keys.insert(action.idempotency_key.clone()) {
                return Err(GoalError::InvalidInput(format!(
                    "duplicate idempotency_key: {}",
                    action.idempotency_key
                )));
            }
            let mut gates = std::collections::BTreeSet::new();
            if action
                .satisfies
                .iter()
                .any(|gate| !safe_id(gate) || !gates.insert(gate.clone()))
            {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}].satisfies must contain unique safe gate ids"
                )));
            }
            if action.timeout_ms == 0 {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}].timeout_ms must be positive"
                )));
            }
            if action.timeout_ms > 24 * 60 * 60 * 1000 {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}].timeout_ms exceeds 24 hours"
                )));
            }
            if action.prompt.len() > 1024 * 1024 || action.expected_artifacts.len() > 64 {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}] exceeds prompt or artifact-count limits"
                )));
            }
            action
                .cell_binding
                .validate()
                .map_err(|e| GoalError::InvalidInput(e.to_string()))?;
            if action.cell_binding.session_file.is_some() {
                return Err(GoalError::InvalidInput(format!(
                    "actions[{index}] must use a disposable session"
                )));
            }
            if action.role == GoalActionRole::Verifier {
                verifier_count += 1;
                if action.action_kind != "verify" {
                    return Err(GoalError::InvalidInput(format!(
                        "verifier action {} must use action_kind=verify",
                        action.action_id
                    )));
                }
                if !action.cell_binding.allowed_paths.is_empty() {
                    return Err(GoalError::InvalidInput(format!(
                        "verifier action {} must be read-only (allowed_paths must be empty)",
                        action.action_id
                    )));
                }
                if index + 1 != value.actions.len() {
                    return Err(GoalError::InvalidInput(
                        "the independent verifier must be the final action".into(),
                    ));
                }
                if !action
                    .expected_artifacts
                    .iter()
                    .any(|kind| kind == "verification_result")
                {
                    return Err(GoalError::InvalidInput(format!(
                        "verifier action {} must require verification_result",
                        action.action_id
                    )));
                }
            }
        }
        if verifier_count != 1 {
            return Err(GoalError::InvalidInput(format!(
                "run plan requires exactly one independent verifier; found {verifier_count}"
            )));
        }
        let verifier = value.actions.last().expect("non-empty run plan");
        if verifier
            .cell_binding
            .allowed_commands
            .iter()
            .any(|command| shell_program(&command.program))
        {
            return Err(GoalError::InvalidInput(
                "verifier commands cannot invoke a shell program".into(),
            ));
        }
        if verifier.cell_binding.allowed_commands.is_empty()
            || verifier.cell_binding.allowed_commands.len() > 16
            || verifier
                .cell_binding
                .allowed_commands
                .iter()
                .any(|command| command.argv_prefix.is_none())
        {
            return Err(GoalError::InvalidInput(
                "verifier requires 1..=16 exact allowed commands".into(),
            ));
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalActionBundle {
    pub schema_version: String,
    pub goal_ref: String,
    pub goal_revision: u64,
    pub attachment_id: String,
    pub attachment_revision: u64,
    pub fencing_epoch: u64,
    pub action_id: String,
    pub action_kind: String,
    /// Execution responsibility, persisted in the immutable bundle so a replay
    /// cannot relabel a normal action as an independent verifier.
    #[serde(default)]
    pub role: GoalActionRole,
    pub idempotency_key: String,
    pub cell_binding: shuttle_protocol::CellBinding,
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub satisfies: Vec<String>,
    pub deadline_ms: i64,
}

impl GoalActionBundle {
    pub fn parse(bytes: &[u8]) -> Result<Self, GoalError> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.schema_version != ACTION_BUNDLE_SCHEMA {
            return Err(GoalError::InvalidInput(format!(
                "unsupported action schema: {}",
                value.schema_version
            )));
        }
        value.goal_ref.parse::<GoalRef>()?;
        value
            .cell_binding
            .validate()
            .map_err(|e| GoalError::InvalidInput(e.to_string()))?;
        for (name, field) in [
            ("action_id", &value.action_id),
            ("action_kind", &value.action_kind),
            ("idempotency_key", &value.idempotency_key),
            ("prompt", &value.prompt),
        ] {
            if field.trim().is_empty() {
                return Err(GoalError::InvalidInput(format!("{name} must not be empty")));
            }
        }
        if !safe_id(&value.action_id) {
            return Err(GoalError::InvalidInput(
                "action_id must contain only ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        let mut gates = std::collections::BTreeSet::new();
        if value
            .satisfies
            .iter()
            .any(|gate| !safe_id(gate) || !gates.insert(gate.clone()))
        {
            return Err(GoalError::InvalidInput(
                "satisfies must contain unique safe gate ids".into(),
            ));
        }
        if value.deadline_ms <= 0 {
            return Err(GoalError::InvalidInput(
                "deadline_ms must be positive".into(),
            ));
        }
        if value.cell_binding.session_file.is_some() {
            return Err(GoalError::InvalidInput(
                "Goal actions must use disposable Pi sessions".into(),
            ));
        }
        if value.role == GoalActionRole::Verifier {
            if value.action_kind != "verify" {
                return Err(GoalError::InvalidInput(
                    "verifier action must use action_kind=verify".into(),
                ));
            }
            if !value.cell_binding.allowed_paths.is_empty() {
                return Err(GoalError::InvalidInput(
                    "verifier action must be read-only".into(),
                ));
            }
            if !value
                .expected_artifacts
                .iter()
                .any(|kind| kind == "verification_result")
            {
                return Err(GoalError::InvalidInput(
                    "verifier action must require verification_result".into(),
                ));
            }
            if value
                .cell_binding
                .allowed_commands
                .iter()
                .any(|command| shell_program(&command.program))
            {
                return Err(GoalError::InvalidInput(
                    "verifier action cannot invoke a shell program".into(),
                ));
            }
            if value.cell_binding.allowed_commands.is_empty()
                || value.cell_binding.allowed_commands.len() > 16
                || value
                    .cell_binding
                    .allowed_commands
                    .iter()
                    .any(|command| command.argv_prefix.is_none())
            {
                return Err(GoalError::InvalidInput(
                    "verifier action requires 1..=16 exact allowed commands".into(),
                ));
            }
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalActionResult {
    pub schema_version: String,
    pub goal_ref: String,
    pub action_id: String,
    #[serde(default)]
    pub role: GoalActionRole,
    pub provider_run_id: String,
    pub started_at_ms: i64,
    pub settled_at_ms: i64,
    pub exit_code: i32,
    pub stdout_ref: String,
    pub stdout_digest: String,
    pub stderr_ref: String,
    pub stderr_digest: String,
    #[serde(default)]
    pub verification_command_evidence_ref: Option<String>,
    #[serde(default)]
    pub verification_command_evidence_digest: Option<String>,
    pub proposal: String,
    pub residual_risks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionProjection {
    pub action_id: String,
    pub goal_ref: String,
    pub basis_goal_revision: u64,
    pub committed_goal_revision: u64,
    pub attachment_id: String,
    pub attachment_revision: u64,
    pub fencing_epoch: u64,
    pub action_kind: String,
    pub idempotency_key: String,
    pub bundle_ref: String,
    pub bundle_digest: String,
    pub status: ActionStatus,
    pub result_ref: Option<String>,
    pub result_digest: Option<String>,
    pub exit_code: Option<i32>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceProgress {
    pub required_conditions: u32,
    pub satisfied_conditions: u32,
    pub unsatisfied_conditions: u32,
    pub unknown_conditions: u32,
    pub pending_effects: u32,
}

#[derive(Debug, Clone)]
pub struct EvaluateConditionRequest<'a> {
    pub goal_ref: &'a str,
    pub gate_id: &'a str,
    pub status: ConditionStatus,
    pub evidence: &'a [u8],
    pub expected_goal_revision: u64,
    pub acceptance_basis_revision: u64,
    pub attachment_id: &'a str,
    pub expected_attachment_revision: u64,
    pub fencing_epoch: u64,
    pub complete_when_satisfied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalProjection {
    pub schema_version: String,
    pub goal_ref: String,
    pub goal_id: String,
    pub backend: GoalBackend,
    pub backend_instance_id: String,
    pub revision: u64,
    pub repository_identity: String,
    pub objective_ref: String,
    pub objective_digest: String,
    pub acceptance_ref: String,
    pub acceptance_digest: String,
    pub acceptance_basis_revision: u64,
    pub status: GoalStatus,
    pub health: GoalHealth,
    pub evidence: EvidenceProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<serde_json::Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachmentProjection {
    pub schema_version: String,
    pub attachment_id: String,
    pub goal_ref: String,
    pub revision: u64,
    pub role: AttachmentRole,
    pub driver_id: String,
    pub generation: u64,
    pub fencing_epoch: u64,
    pub lease_expires_at_ms: i64,
    pub state: AttachmentState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConditionEvaluation {
    pub schema_version: String,
    pub goal_ref: String,
    pub acceptance_basis_revision: u64,
    pub gate_id: String,
    pub status: ConditionStatus,
    pub evidence_ref: String,
    pub evidence_digest: String,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub schema_version: String,
    pub backend: GoalBackend,
    pub backend_instance_id: String,
    pub read_status: bool,
    pub attach_observer: bool,
    pub attach_controller: bool,
    pub cross_session_reconnect: bool,
    pub provider_cas: bool,
    pub conditional_update: bool,
    pub action_effect_lookup: bool,
    pub bounded_continuation: bool,
    pub graph_advice: bool,
    pub independent_verifier: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationArtifact {
    pub r#ref: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationCondition {
    pub gate_id: String,
    pub status: ConditionStatus,
    pub evidence_ref: String,
    pub evidence_digest: String,
    pub observed_at_ms: i64,
    pub basis_goal_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalMigrationBundle {
    pub schema_version: String,
    pub source_goal: GoalProjection,
    pub objective: MigrationArtifact,
    pub acceptance: MigrationArtifact,
    pub conditions: Vec<MigrationCondition>,
    pub actions: Vec<ActionProjection>,
    pub artifacts: Vec<MigrationArtifact>,
    pub exported_at_ms: i64,
    pub bundle_digest: String,
}

impl GoalMigrationBundle {
    pub fn verify(&self) -> Result<(), GoalError> {
        if self.schema_version != MIGRATION_BUNDLE_SCHEMA
            || self.source_goal.status != GoalStatus::Sealed
            || self.source_goal.evidence.pending_effects != 0
        {
            return Err(GoalError::UnsupportedContract(
                "migration bundle schema/source state is unsupported".into(),
            ));
        }
        for artifact in std::iter::once(&self.objective)
            .chain(std::iter::once(&self.acceptance))
            .chain(self.artifacts.iter())
        {
            let digest = format!("sha256:{:x}", sha2::Sha256::digest(&artifact.bytes));
            if artifact.digest != digest
                || artifact.r#ref != format!("artifact://sha256/{}", &digest[7..])
            {
                return Err(GoalError::Integrity(
                    "migration artifact digest does not verify".into(),
                ));
            }
        }
        let core = serde_json::json!({
            "schema_version":self.schema_version,
            "source_goal":self.source_goal,
            "objective":self.objective,
            "acceptance":self.acceptance,
            "conditions":self.conditions,
            "actions":self.actions,
            "artifacts":self.artifacts,
            "exported_at_ms":self.exported_at_ms,
        });
        let digest = format!(
            "sha256:{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&core)?)
        );
        if self.bundle_digest != digest {
            return Err(GoalError::Integrity(
                "migration bundle digest does not verify".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("goal not found: {0}")]
    NotFound(String),
    #[error("revision conflict: expected {expected}, current {current}")]
    RevisionConflict { expected: u64, current: u64 },
    #[error("controller already held by {0}")]
    ControllerHeld(String),
    #[error("stale fencing epoch: expected {expected}, current {current}")]
    StaleFence { expected: u64, current: u64 },
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("backend timeout: {0}")]
    BackendTimeout(String),
    #[error("unsupported contract: {0}")]
    UnsupportedContract(String),
    #[error("backend protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("backend request returned a non-result outcome: {0}")]
    BackendOutcome(serde_json::Value),
    #[error("action effect requires reconciliation: {0}")]
    ReconcileRequired(String),
    #[error("store integrity failure: {0}")]
    Integrity(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn shell_program(program: &str) -> bool {
    matches!(
        program.to_ascii_lowercase().as_str(),
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "fish"
            | "csh"
            | "ksh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "cmd.exe"
    )
}

#[cfg(test)]
mod contract_tests {
    use super::{safe_id, GoalRef};

    #[test]
    fn identifiers_are_non_empty_and_goal_refs_require_all_segments() {
        assert!(!safe_id(""));
        assert!("goal:external::g1".parse::<GoalRef>().is_err());
        assert!("goal:external:".parse::<GoalRef>().is_err());
        assert!("goal:external:remote:g1".parse::<GoalRef>().is_ok());
    }
}
