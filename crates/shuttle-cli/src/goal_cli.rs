use clap::{Args, Subcommand, ValueEnum};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use shuttle_goal::{
    ActionStatus, AttachmentRole, AttachmentState, ConditionStatus, EvaluateConditionRequest,
    ExternalFacadeResponse, ExternalGoalClient, ExternalOperation, GoalActionBundle,
    GoalActionResult, GoalActionRole, GoalActionTemplate, GoalBackend, GoalError, GoalRef,
    GoalRunPlan, GoalStatus, LocalGoalStore, StorePaths, ACTION_BUNDLE_SCHEMA,
    ACTION_RESULT_SCHEMA, RUN_RESULT_SCHEMA,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_LEASE_SECONDS: u64 = 90;

#[derive(Debug, Subcommand)]
pub enum GoalCmd {
    /// Create an active local Goal and acquire its initial controller.
    Start(StartArgs),
    /// Discover Goals bound to an approved logical repository identity.
    Discover(DiscoverArgs),
    /// Read the current durable Goal projection.
    Status(StatusArgs),
    /// Acquire a controller or observer attachment.
    Attach(AttachArgs),
    /// Renew an attachment lease using revision CAS and its fencing epoch.
    Renew(RenewArgs),
    /// Explicitly release an attachment without changing Goal lifecycle.
    Detach(DetachArgs),
    /// Pause an active Goal with controller authority.
    Pause(TransitionArgs),
    /// Resume a paused/waiting/blocked Goal with controller authority.
    Resume(TransitionArgs),
    /// Cancel a non-terminal Goal with controller authority.
    Cancel(TransitionArgs),
    /// Permanently seal a local source Goal before migration export.
    Seal(TransitionArgs),
    /// Export a sealed local Goal as a digest-bound migration bundle.
    Export(ExportArgs),
    /// Import a migration bundle into an explicit external backend as a paused Goal.
    Import(ImportArgs),
    /// Activate an imported paused external Goal and acquire its first controller.
    Activate(ActivateArgs),
    /// Mark a sealed local source as transferred after target activation.
    Transfer(TransferArgs),
    /// Record typed acceptance evidence and atomically complete when all required gates pass.
    Evaluate(EvaluateArgs),
    /// Execute at most one already-bound action through the existing caged cell-run path.
    Step(StepArgs),
    /// Execute a strict action plan under bounded continuation policy.
    Run(RunArgs),
    /// Read a durable action projection without executing it.
    ActionStatus(ActionStatusArgs),
    /// Mark a running/unknown effect as reconcile-required; never redispatch implicitly.
    Reconcile(ReconcileArgs),
    /// Explain current evidence, health, capabilities, and the safe next action.
    Explain(StatusArgs),
    /// Validate local store integrity and report effective capabilities.
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Args)]
pub struct StoreArgs {
    /// Goal authority. A goal:external:* ref always selects external even when omitted.
    #[arg(long, value_enum)]
    backend: Option<BackendArg>,
    /// Stable local backend instance id embedded in every Goal ref.
    #[arg(long, default_value = "default")]
    store_id: String,
    /// Override the platform state root (tests/portable installations).
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Absolute path to the governed one-shot goal-control facade.
    #[arg(long)]
    cf_facade: Option<PathBuf>,
    /// Per-request facade deadline.
    #[arg(long, default_value_t = 10_000)]
    cf_timeout_ms: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum BackendArg {
    Local,
    External,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Optional Goal ref used for an in-process backend status benchmark.
    #[arg(long)]
    goal_ref: Option<String>,
    /// Number of backend reads when --goal-ref is supplied.
    #[arg(long, default_value_t = 100)]
    samples: usize,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    objective: String,
    /// A file containing explicit, reviewable acceptance conditions.
    #[arg(long)]
    acceptance: PathBuf,
    /// Logical repository identity; never a physical workspace path.
    #[arg(long)]
    repository_identity: String,
    /// Stable identity of the SHUTTLE driver acquiring controller authority.
    #[arg(long)]
    driver_id: String,
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    #[arg(long)]
    repository_identity: String,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    goal_ref: String,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
    Controller,
    Observer,
}

impl From<RoleArg> for AttachmentRole {
    fn from(value: RoleArg) -> Self {
        match value {
            RoleArg::Controller => Self::Controller,
            RoleArg::Observer => Self::Observer,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConditionArg {
    Satisfied,
    Unsatisfied,
    Unknown,
}

impl From<ConditionArg> for ConditionStatus {
    fn from(value: ConditionArg) -> Self {
        match value {
            ConditionArg::Satisfied => Self::Satisfied,
            ConditionArg::Unsatisfied => Self::Unsatisfied,
            ConditionArg::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    goal_ref: String,
    #[arg(long)]
    driver_id: String,
    #[arg(long, value_enum, default_value = "observer")]
    role: RoleArg,
    #[arg(long)]
    expected_goal_revision: u64,
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct RenewArgs {
    attachment_id: String,
    #[arg(long)]
    expected_attachment_revision: u64,
    #[arg(long)]
    fencing_epoch: u64,
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct DetachArgs {
    attachment_id: String,
    #[arg(long)]
    expected_attachment_revision: u64,
    #[arg(long)]
    fencing_epoch: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct TransitionArgs {
    goal_ref: String,
    #[arg(long)]
    attachment_id: String,
    #[arg(long)]
    expected_goal_revision: u64,
    #[arg(long)]
    expected_attachment_revision: u64,
    #[arg(long)]
    fencing_epoch: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct EvaluateArgs {
    goal_ref: String,
    #[arg(long)]
    gate_id: String,
    #[arg(long, value_enum)]
    status: ConditionArg,
    #[arg(long)]
    evidence: PathBuf,
    #[arg(long)]
    expected_goal_revision: u64,
    #[arg(long)]
    acceptance_basis_revision: u64,
    #[arg(long)]
    attachment_id: String,
    #[arg(long)]
    expected_attachment_revision: u64,
    #[arg(long)]
    fencing_epoch: u64,
    /// Complete the Goal in the same transaction when every required gate is satisfied.
    #[arg(long)]
    complete_when_satisfied: bool,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    goal_ref: String,
    #[arg(long)]
    output: PathBuf,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ActivateArgs {
    goal_ref: String,
    #[arg(long)]
    driver_id: String,
    #[arg(long)]
    expected_goal_revision: u64,
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct TransferArgs {
    goal_ref: String,
    #[arg(long)]
    expected_goal_revision: u64,
    #[arg(long)]
    target_goal_ref: String,
    #[arg(long)]
    bundle: PathBuf,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct StepArgs {
    /// Strict `shuttle_goal_action_bundle/v1` JSON file.
    #[arg(long)]
    bundle: PathBuf,
    /// Controller lease duration maintained while the action is running.
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Strict `shuttle_goal_run_plan/v1` JSON. It contains no authority revisions.
    #[arg(long)]
    plan: PathBuf,
    /// Maximum new provider invocations in this continuation.
    #[arg(long, default_value_t = 8)]
    max_actions: usize,
    /// Maximum wall-clock time for this continuation.
    #[arg(long, default_value_t = 1800)]
    max_active_seconds: u64,
    /// Stop after this many observed non-zero provider exits.
    #[arg(long, default_value_t = 1)]
    max_provider_failures: usize,
    /// Maximum aggregate model tokens. Unknown usage stops fail-closed.
    #[arg(long, default_value_t = 100_000)]
    max_tokens: u64,
    /// Stop after this many consecutive actions that fail to advance Goal revision.
    #[arg(long, default_value_t = 8)]
    max_no_progress: usize,
    /// Controller lease duration maintained while each action is running.
    #[arg(long, default_value_t = DEFAULT_LEASE_SECONDS)]
    lease_seconds: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ActionStatusArgs {
    action_id: String,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
pub struct ReconcileArgs {
    action_id: String,
    #[arg(long)]
    attachment_id: String,
    #[arg(long)]
    expected_attachment_revision: u64,
    #[arg(long)]
    fencing_epoch: u64,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Serialize)]
struct StartResult<T, U> {
    goal: T,
    attachment: U,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RunStopReason {
    PlanExhausted,
    MaxActions,
    MaxActiveTime,
    ProviderFailureBudget,
    TokenBudget,
    TokenUsageUnknown,
    NoProgressBudget,
    GoalNotActive,
    AttachmentNotController,
    ReconcileRequired,
}

#[derive(Debug, Serialize)]
struct RunActionSummary {
    action_id: String,
    role: GoalActionRole,
    idempotent_replay: bool,
    exit_code: Option<i64>,
    verification_evidence_ref: Option<String>,
    goal_revision: u64,
}

#[derive(Debug, Serialize)]
struct RunResult {
    schema_version: String,
    goal_ref: String,
    stop_reason: RunStopReason,
    actions_planned: usize,
    actions_executed: usize,
    actions_replayed: usize,
    provider_failures: usize,
    tokens_used: u64,
    token_usage_complete: bool,
    no_progress_events: usize,
    active_ms: u128,
    independent_verifier_settled: bool,
    actions: Vec<RunActionSummary>,
    final_goal: shuttle_goal::GoalProjection,
    final_attachment: shuttle_goal::AttachmentProjection,
}

pub fn run(cmd: GoalCmd) -> anyhow::Result<()> {
    match execute(cmd) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Err(error) => emit_error(error),
    }
}

fn execute(cmd: GoalCmd) -> Result<serde_json::Value, GoalError> {
    match cmd {
        GoalCmd::Start(a) => {
            let acceptance = std::fs::read(&a.acceptance)?;
            if selected_backend(&a.store, None)? == GoalBackend::External {
                let acceptance: serde_json::Value = serde_json::from_slice(&acceptance)?;
                return call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Start,
                    json!({
                        "objective":a.objective,
                        "acceptance":acceptance,
                        "repository_identity":a.repository_identity,
                        "driver_id":a.driver_id,
                        "lease_ms":ttl_ms(a.lease_seconds),
                    }),
                );
            }
            let mut store = open(&a.store)?;
            store
                .start_goal(
                    &a.repository_identity,
                    &a.objective,
                    &acceptance,
                    &a.driver_id,
                    ttl_ms(a.lease_seconds),
                )
                .and_then(|(goal, attachment)| to_value(StartResult { goal, attachment }))
        }
        GoalCmd::Discover(a) => {
            if selected_backend(&a.store, None)? == GoalBackend::External {
                call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Discover,
                    json!({"repository_identity":a.repository_identity}),
                )
            } else {
                open_readonly(&a.store)
                    .and_then(|s| s.discover(&a.repository_identity))
                    .and_then(to_value)
            }
        }
        GoalCmd::Status(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? == GoalBackend::External {
                call_cf(
                    &a.store,
                    Some(&a.goal_ref),
                    ExternalOperation::Get,
                    json!({"goal_ref":a.goal_ref}),
                )
            } else {
                open_readonly(&a.store)
                    .and_then(|s| s.get(&a.goal_ref))
                    .and_then(to_value)
            }
        }
        GoalCmd::Attach(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    Some(&a.goal_ref),
                    ExternalOperation::Attach,
                    json!({
                        "goal_ref":a.goal_ref,
                        "driver_id":a.driver_id,
                        "role":AttachmentRole::from(a.role),
                        "expected_goal_revision":a.expected_goal_revision,
                        "lease_ms":ttl_ms(a.lease_seconds),
                    }),
                );
            }
            let mut s = open(&a.store)?;
            s.attach(
                &a.goal_ref,
                &a.driver_id,
                a.role.into(),
                a.expected_goal_revision,
                ttl_ms(a.lease_seconds),
            )
            .and_then(to_value)
        }
        GoalCmd::Renew(a) => {
            if selected_backend(&a.store, None)? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Renew,
                    json!({
                        "attachment_id":a.attachment_id,
                        "expected_attachment_revision":a.expected_attachment_revision,
                        "fencing_epoch":a.fencing_epoch,
                        "lease_ms":ttl_ms(a.lease_seconds),
                    }),
                );
            }
            let mut s = open(&a.store)?;
            s.renew(
                &a.attachment_id,
                a.expected_attachment_revision,
                a.fencing_epoch,
                ttl_ms(a.lease_seconds),
            )
            .and_then(to_value)
        }
        GoalCmd::Detach(a) => {
            if selected_backend(&a.store, None)? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Release,
                    json!({
                        "attachment_id":a.attachment_id,
                        "expected_attachment_revision":a.expected_attachment_revision,
                        "fencing_epoch":a.fencing_epoch,
                    }),
                );
            }
            let mut s = open(&a.store)?;
            s.release(
                &a.attachment_id,
                a.expected_attachment_revision,
                a.fencing_epoch,
            )
            .and_then(to_value)
        }
        GoalCmd::Pause(a) => transition(a, GoalStatus::Paused),
        GoalCmd::Resume(a) => transition(a, GoalStatus::Active),
        GoalCmd::Cancel(a) => transition(a, GoalStatus::Cancelled),
        GoalCmd::Seal(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? != GoalBackend::Local {
                return Err(GoalError::UnsupportedContract(
                    "v1 migration seals a local source Goal only".into(),
                ));
            }
            open(&a.store)?
                .seal_for_migration(
                    &a.goal_ref,
                    a.expected_goal_revision,
                    &a.attachment_id,
                    a.expected_attachment_revision,
                    a.fencing_epoch,
                )
                .and_then(to_value)
        }
        GoalCmd::Export(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? != GoalBackend::Local {
                return Err(GoalError::UnsupportedContract(
                    "v1 migration export reads a local source Goal only".into(),
                ));
            }
            let bundle = open(&a.store)?.export_migration(&a.goal_ref)?;
            atomic_write(&a.output, &serde_json::to_vec_pretty(&bundle)?)?;
            to_value(bundle)
        }
        GoalCmd::Import(a) => {
            if selected_backend(&a.store, None)? != GoalBackend::External {
                return Err(GoalError::InvalidInput(
                    "migration import requires --backend cf".into(),
                ));
            }
            let bundle: shuttle_goal::GoalMigrationBundle =
                serde_json::from_slice(&std::fs::read(&a.bundle)?)?;
            bundle.verify()?;
            call_cf(
                &a.store,
                None,
                ExternalOperation::Import,
                json!({
                    "target_goal_id":format!("import-{}", &bundle.bundle_digest[7..31]),
                    "bundle":bundle
                }),
            )
        }
        GoalCmd::Activate(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? != GoalBackend::External {
                return Err(GoalError::UnsupportedContract(
                    "v1 migration activates the imported external target only".into(),
                ));
            }
            call_cf(
                &a.store,
                Some(&a.goal_ref),
                ExternalOperation::Activate,
                json!({
                    "goal_ref":a.goal_ref,
                    "driver_id":a.driver_id,
                    "expected_goal_revision":a.expected_goal_revision,
                    "lease_ms":ttl_ms(a.lease_seconds),
                }),
            )
        }
        GoalCmd::Transfer(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? != GoalBackend::Local {
                return Err(GoalError::UnsupportedContract(
                    "v1 migration transfers a local source Goal only".into(),
                ));
            }
            let source = open_readonly(&a.store)?.get(&a.goal_ref)?;
            if source.revision != a.expected_goal_revision || source.status != GoalStatus::Sealed {
                return Err(GoalError::InvalidInput(
                    "transfer requires the expected sealed source revision".into(),
                ));
            }
            let bundle: shuttle_goal::GoalMigrationBundle =
                serde_json::from_slice(&std::fs::read(&a.bundle)?)?;
            bundle.verify()?;
            if bundle.source_goal.goal_ref != a.goal_ref
                || bundle.source_goal.revision != a.expected_goal_revision
                || bundle.source_goal.status != GoalStatus::Sealed
            {
                return Err(GoalError::InvalidInput(
                    "bundle is not bound to the expected sealed source revision".into(),
                ));
            }
            let target = call_cf(
                &a.store,
                Some(&a.target_goal_ref),
                ExternalOperation::Get,
                json!({"goal_ref":a.target_goal_ref}),
            )?;
            if target["status"] != "active"
                || target["lineage"]["source_goal_ref"] != a.goal_ref
                || target["lineage"]["source_revision"] != a.expected_goal_revision
                || target["lineage"]["export_digest"] != bundle.bundle_digest
            {
                return Err(GoalError::InvalidInput(
                    "target Goal is not the active lineage-bound migration destination".into(),
                ));
            }
            let receipt = format!("{}#{}", a.target_goal_ref, bundle.bundle_digest);
            open(&a.store)?
                .mark_transferred(&a.goal_ref, a.expected_goal_revision, &receipt)
                .and_then(to_value)
        }
        GoalCmd::Evaluate(a) => {
            let evidence = std::fs::read(&a.evidence)?;
            if selected_backend(&a.store, Some(&a.goal_ref))? == GoalBackend::External {
                let evidence: serde_json::Value =
                    serde_json::from_slice(&evidence).map_err(|e| {
                        GoalError::InvalidInput(format!(
                            "external evaluation evidence must be JSON: {e}"
                        ))
                    })?;
                return call_cf(
                    &a.store,
                    Some(&a.goal_ref),
                    ExternalOperation::Evaluate,
                    json!({
                        "goal_ref":a.goal_ref,
                        "gate_id":a.gate_id,
                        "status":ConditionStatus::from(a.status),
                        "evidence":evidence,
                        "expected_goal_revision":a.expected_goal_revision,
                        "acceptance_basis_revision":a.acceptance_basis_revision,
                        "attachment_id":a.attachment_id,
                        "expected_attachment_revision":a.expected_attachment_revision,
                        "fencing_epoch":a.fencing_epoch,
                        "complete_when_satisfied":a.complete_when_satisfied,
                    }),
                );
            }
            let mut s = open(&a.store)?;
            s.evaluate_condition(EvaluateConditionRequest {
                goal_ref: &a.goal_ref,
                gate_id: &a.gate_id,
                status: a.status.into(),
                evidence: &evidence,
                expected_goal_revision: a.expected_goal_revision,
                acceptance_basis_revision: a.acceptance_basis_revision,
                attachment_id: &a.attachment_id,
                expected_attachment_revision: a.expected_attachment_revision,
                fencing_epoch: a.fencing_epoch,
                complete_when_satisfied: a.complete_when_satisfied,
            })
            .map(|(evaluation, goal)| json!({"evaluation": evaluation, "goal": goal}))
        }
        GoalCmd::Step(a) => {
            if selected_backend(&a.store, goal_ref_from_bundle(&a.bundle)?.as_deref())?
                == GoalBackend::External
            {
                return run_step_cf(a);
            }
            run_step(a)
        }
        GoalCmd::Run(a) => {
            if selected_backend(&a.store, goal_ref_from_plan(&a.plan)?.as_deref())?
                == GoalBackend::External
            {
                return run_bounded_cf(a);
            }
            run_bounded(a)
        }
        GoalCmd::ActionStatus(a) => {
            if selected_backend(&a.store, None)? == GoalBackend::External {
                call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Get,
                    json!({"action_id":a.action_id}),
                )
            } else {
                open_readonly(&a.store)
                    .and_then(|s| s.get_action(&a.action_id))
                    .and_then(to_value)
            }
        }
        GoalCmd::Reconcile(a) => {
            if selected_backend(&a.store, None)? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    None,
                    ExternalOperation::Reconcile,
                    json!({
                        "action_id":a.action_id,
                        "attachment_id":a.attachment_id,
                        "expected_attachment_revision":a.expected_attachment_revision,
                        "fencing_epoch":a.fencing_epoch,
                    }),
                );
            }
            let mut s = open(&a.store)?;
            s.reconcile_action(
                &a.action_id,
                &a.attachment_id,
                a.expected_attachment_revision,
                a.fencing_epoch,
            )
            .map(|(action, goal)| json!({"action":action,"goal":goal}))
        }
        GoalCmd::Explain(a) => {
            if selected_backend(&a.store, Some(&a.goal_ref))? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    Some(&a.goal_ref),
                    ExternalOperation::Get,
                    json!({"goal_ref":a.goal_ref,"view":"explain"}),
                );
            }
            let s = open_readonly(&a.store)?;
            let g = s.get(&a.goal_ref)?;
            let caps = s.capabilities();
            let next = if g.status.is_terminal() {
                "none_terminal"
            } else if g.status == GoalStatus::Paused {
                "resume_or_amend"
            } else if g.evidence.pending_effects > 0 {
                "reconcile_pending_effect"
            } else {
                "prepare_committed_action"
            };
            Ok(
                json!({"goal":g,"capabilities":caps,"next_action_reason":next,"generated_at_ms":now_ms()}),
            )
        }
        GoalCmd::Doctor(a) => {
            if selected_backend(&a.store, a.goal_ref.as_deref())? == GoalBackend::External {
                return call_cf(
                    &a.store,
                    a.goal_ref.as_deref(),
                    ExternalOperation::Capabilities,
                    json!({"goal_ref":a.goal_ref,"samples":a.samples}),
                );
            }
            let s = open(&a.store)?;
            s.integrity_check()?;
            let benchmark = match &a.goal_ref {
                Some(goal_ref) => Some(benchmark_status(&s, goal_ref, a.samples)?),
                None => None,
            };
            Ok(
                json!({"status":"healthy","backend":"local","database":s.paths().database,"capabilities":s.capabilities(),"status_backend_latency_ms":benchmark}),
            )
        }
    }
}

fn transition(a: TransitionArgs, status: GoalStatus) -> Result<serde_json::Value, GoalError> {
    if selected_backend(&a.store, Some(&a.goal_ref))? == GoalBackend::External {
        return call_cf(
            &a.store,
            Some(&a.goal_ref),
            ExternalOperation::Transition,
            json!({
                "goal_ref":a.goal_ref,
                "target_status":status,
                "attachment_id":a.attachment_id,
                "expected_goal_revision":a.expected_goal_revision,
                "expected_attachment_revision":a.expected_attachment_revision,
                "fencing_epoch":a.fencing_epoch,
            }),
        );
    }
    let mut s = open(&a.store)?;
    s.transition(
        &a.goal_ref,
        status,
        a.expected_goal_revision,
        &a.attachment_id,
        a.expected_attachment_revision,
        a.fencing_epoch,
    )
    .and_then(to_value)
}

fn selected_backend(args: &StoreArgs, goal_ref: Option<&str>) -> Result<GoalBackend, GoalError> {
    if let Some(goal_ref) = goal_ref {
        let parsed: GoalRef = goal_ref.parse()?;
        if let Some(explicit) = args.backend {
            let explicit = match explicit {
                BackendArg::Local => GoalBackend::Local,
                BackendArg::External => GoalBackend::External,
            };
            if explicit != parsed.backend {
                return Err(GoalError::InvalidInput(
                    "explicit --backend conflicts with the Goal ref authority".into(),
                ));
            }
        }
        return Ok(parsed.backend);
    }
    Ok(match args.backend.unwrap_or(BackendArg::Local) {
        BackendArg::Local => GoalBackend::Local,
        BackendArg::External => GoalBackend::External,
    })
}

fn cf_client(args: &StoreArgs, goal_ref: Option<&str>) -> Result<ExternalGoalClient, GoalError> {
    let facade = args
        .cf_facade
        .clone()
        .or_else(|| std::env::var_os("SHUTTLE_GOAL_CF_FACADE").map(PathBuf::from))
        .ok_or_else(|| {
            GoalError::BackendUnavailable(
                "external backend selected but --cf-facade/SHUTTLE_GOAL_CF_FACADE is not configured"
                    .into(),
            )
        })?;
    let instance_id = match goal_ref {
        Some(goal_ref) => goal_ref.parse::<GoalRef>()?.instance_id,
        None => args.store_id.clone(),
    };
    ExternalGoalClient::new(facade, &instance_id)?
        .with_timeout(Duration::from_millis(args.cf_timeout_ms))
}

fn call_cf(
    args: &StoreArgs,
    goal_ref: Option<&str>,
    operation: ExternalOperation,
    parameters: serde_json::Value,
) -> Result<serde_json::Value, GoalError> {
    let response = cf_client(args, goal_ref)?.call(operation, parameters)?;
    match response {
        ExternalFacadeResponse::Result { result, .. } => Ok(result),
        outcome @ (ExternalFacadeResponse::Conflict { .. }
        | ExternalFacadeResponse::Error { .. }) => {
            Err(GoalError::BackendOutcome(serde_json::to_value(outcome)?))
        }
    }
}

fn goal_ref_from_bundle(path: &std::path::Path) -> Result<Option<String>, GoalError> {
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value
        .get("goal_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

fn goal_ref_from_plan(path: &std::path::Path) -> Result<Option<String>, GoalError> {
    goal_ref_from_bundle(path)
}

fn open(args: &StoreArgs) -> Result<LocalGoalStore, GoalError> {
    if !shuttle_goal::safe_id(&args.store_id) {
        return Err(GoalError::InvalidInput(
            "store id must contain only ASCII letters, digits, '.', '_' or '-'".into(),
        ));
    }
    match &args.state_dir {
        Some(root) => {
            LocalGoalStore::open(StorePaths::new(root.join(&args.store_id)), &args.store_id)
        }
        None => LocalGoalStore::open_default(&args.store_id),
    }
}

fn open_readonly(args: &StoreArgs) -> Result<LocalGoalStore, GoalError> {
    if !shuttle_goal::safe_id(&args.store_id) {
        return Err(GoalError::InvalidInput(
            "store id must contain only ASCII letters, digits, '.', '_' or '-'".into(),
        ));
    }
    match &args.state_dir {
        Some(root) => LocalGoalStore::open_readonly(
            StorePaths::new(root.join(&args.store_id)),
            &args.store_id,
        ),
        None => LocalGoalStore::open_default_readonly(&args.store_id),
    }
}

fn benchmark_status(
    store: &LocalGoalStore,
    goal_ref: &str,
    samples: usize,
) -> Result<serde_json::Value, GoalError> {
    if samples == 0 || samples > 10_000 {
        return Err(GoalError::InvalidInput(
            "doctor samples must be between 1 and 10000".into(),
        ));
    }
    for _ in 0..5 {
        store.get(goal_ref)?;
    }
    let mut latency_ms = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        store.get(goal_ref)?;
        latency_ms.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    latency_ms.sort_by(f64::total_cmp);
    let p50 = latency_ms[(samples - 1) * 50 / 100];
    let p95 = latency_ms[(samples - 1) * 95 / 100];
    Ok(json!({
        "samples": samples,
        "warmup": 5,
        "p50": (p50 * 1000.0).round() / 1000.0,
        "p95": (p95 * 1000.0).round() / 1000.0,
        "max": (latency_ms[samples - 1] * 1000.0).round() / 1000.0,
        "target_p95": 50,
        "pass": p95 <= 50.0,
    }))
}

fn ttl_ms(seconds: u64) -> i64 {
    seconds.saturating_mul(1000).min(i64::MAX as u64) as i64
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn run_step(a: StepArgs) -> Result<serde_json::Value, GoalError> {
    let raw = std::fs::read(&a.bundle)?;
    let bundle = GoalActionBundle::parse(&raw)?;
    run_step_bundle(bundle, &raw, &a.store, a.lease_seconds)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalPreparedAction {
    action: shuttle_goal::ActionProjection,
    goal: shuttle_goal::GoalProjection,
    #[serde(default)]
    result: Option<GoalActionResult>,
    #[serde(default)]
    artifacts: Option<ExternalResultArtifacts>,
    #[serde(default)]
    #[serde(rename = "dispatch_intent")]
    _dispatch_intent: Option<serde_json::Value>,
    dispatch_authorized: bool,
    idempotent_replay: bool,
    #[serde(default)]
    dispatch_claim_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalResultArtifacts {
    stdout: String,
    stderr: String,
    verification_command_evidence: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalActionOutcome {
    action: shuttle_goal::ActionProjection,
    goal: shuttle_goal::GoalProjection,
    #[serde(default)]
    result: Option<GoalActionResult>,
    #[serde(default)]
    artifacts: Option<ExternalResultArtifacts>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalActionView {
    action: shuttle_goal::ActionProjection,
    bundle: GoalActionBundle,
    result: Option<GoalActionResult>,
    artifacts: Option<ExternalResultArtifacts>,
}

fn cf_result<T: DeserializeOwned>(response: ExternalFacadeResponse) -> Result<T, GoalError> {
    match response {
        ExternalFacadeResponse::Result { result, .. } => {
            serde_json::from_value(result).map_err(|error| {
                GoalError::ProtocolViolation(format!("external result payload is invalid: {error}"))
            })
        }
        outcome @ (ExternalFacadeResponse::Conflict { .. }
        | ExternalFacadeResponse::Error { .. }) => {
            Err(GoalError::BackendOutcome(serde_json::to_value(outcome)?))
        }
    }
}

fn call_cf_stable_with_retry(
    client: &ExternalGoalClient,
    operation: ExternalOperation,
    parameters: serde_json::Value,
    request_id: String,
) -> Result<ExternalFacadeResponse, GoalError> {
    match client.call_with_request_id(operation, parameters.clone(), request_id.clone()) {
        Err(GoalError::BackendTimeout(_)) | Err(GoalError::BackendUnavailable(_)) => {
            client.call_with_request_id(operation, parameters, request_id)
        }
        outcome => outcome,
    }
}

fn cf_optional_action(
    client: &ExternalGoalClient,
    action_id: &str,
) -> Result<Option<ExternalActionView>, GoalError> {
    match client.call(
        ExternalOperation::Get,
        json!({"action_id": action_id, "view": "explain"}),
    )? {
        ExternalFacadeResponse::Result { result, .. } => {
            serde_json::from_value(result).map(Some).map_err(|error| {
                GoalError::ProtocolViolation(format!("external action view is invalid: {error}"))
            })
        }
        ExternalFacadeResponse::Error { error, .. } if error.reason_code == "not_found" => Ok(None),
        outcome @ (ExternalFacadeResponse::Conflict { .. }
        | ExternalFacadeResponse::Error { .. }) => {
            Err(GoalError::BackendOutcome(serde_json::to_value(outcome)?))
        }
    }
}

fn stable_cf_request_id(prefix: &str, value: &impl Serialize) -> Result<String, GoalError> {
    let bytes = serde_json::to_vec(value)?;
    let hex = crate::sha256::sha256_hex_prefixed(&bytes)
        .trim_start_matches("sha256:")
        .to_owned();
    Ok(format!("shuttle-{prefix}-{}", &hex[..32]))
}

fn artifact_ref(bytes: &[u8]) -> (String, String) {
    let hex = crate::sha256::sha256_hex_prefixed(bytes)
        .trim_start_matches("sha256:")
        .to_owned();
    (format!("artifact://sha256/{hex}"), format!("sha256:{hex}"))
}

fn bounded_verification_json(records: &[serde_json::Value]) -> Result<Option<Vec<u8>>, GoalError> {
    const LIMIT: usize = 64 * 1024;
    if records.is_empty() {
        return Ok(None);
    }
    let mut bounded = records.to_vec();
    for record in &mut bounded {
        for field in ["stdout", "stderr"] {
            if let Some(text) = record.get(field).and_then(serde_json::Value::as_str) {
                record[field] = serde_json::Value::String(
                    String::from_utf8_lossy(&bounded_redacted_with(text.as_bytes(), &[]))
                        .chars()
                        .rev()
                        .take(2048)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect(),
                );
            }
        }
    }
    let bytes = serde_json::to_vec(&bounded)?;
    if bytes.len() > LIMIT {
        return Err(GoalError::InvalidInput(
            "verification evidence exceeds the 64KiB external contract limit".into(),
        ));
    }
    Ok(Some(bytes))
}

fn run_step_cf(a: StepArgs) -> Result<serde_json::Value, GoalError> {
    let raw = std::fs::read(&a.bundle)?;
    let bundle = GoalActionBundle::parse(&raw)?;
    run_step_bundle_cf(bundle, &a.store, a.lease_seconds)
}

fn run_step_bundle_cf(
    bundle: GoalActionBundle,
    store_args: &StoreArgs,
    lease_seconds: u64,
) -> Result<serde_json::Value, GoalError> {
    let client = cf_client(store_args, Some(&bundle.goal_ref))?;
    let prepare_parameters = json!({"bundle": bundle});
    let prepare_id = stable_cf_request_id("goal-prepare", &prepare_parameters)?;
    let prepared: ExternalPreparedAction = cf_result(call_cf_stable_with_retry(
        &client,
        ExternalOperation::PrepareAction,
        prepare_parameters,
        prepare_id,
    )?)?;
    if prepared.idempotent_replay {
        let mut replay = json!({
            "action": prepared.action, "goal": prepared.goal,
            "result": prepared.result, "artifacts": prepared.artifacts,
            "idempotent_replay": true,
        });
        replay["tokens_used"] = cf_step_tokens(&replay).into();
        return Ok(replay);
    }
    if !prepared.dispatch_authorized {
        return Err(GoalError::ReconcileRequired(bundle.action_id.clone()));
    }
    let claim_token = prepared.dispatch_claim_token.ok_or_else(|| {
        GoalError::ProtocolViolation("dispatch authorization omitted claim token".into())
    })?;
    let lease_ttl_ms = ttl_ms(lease_seconds);
    let mut attachment_revision = bundle.attachment_revision;
    let runtime_nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let runtime = std::env::temp_dir().join(format!(
        "shuttle-goal-cf-{}-{runtime_nonce:x}-{}",
        std::process::id(),
        bundle.action_id
    ));
    let binding_path = runtime.join("binding.json");
    let stdout_path = runtime.join("stdout.log");
    let stderr_path = runtime.join("stderr.log");
    let execution = (|| -> Result<serde_json::Value, GoalError> {
        let mut attachment: shuttle_goal::AttachmentProjection = cf_result(client.call(
            ExternalOperation::Get,
            json!({"attachment_id": bundle.attachment_id}),
        )?)?;
        attachment_revision = attachment.revision;
        std::fs::create_dir(&runtime)?;
        atomic_write(
            &binding_path,
            &serde_json::to_vec_pretty(&bundle.cell_binding)?,
        )?;
        let stdout_file = create_private_file(&stdout_path)?;
        let stderr_file = create_private_file(&stderr_path)?;
        let started = now_ms();
        attachment = cf_result(client.call(
            ExternalOperation::Renew,
            json!({
                "attachment_id": bundle.attachment_id,
                "expected_attachment_revision": attachment.revision,
                "fencing_epoch": bundle.fencing_epoch,
                "lease_ms": lease_ttl_ms,
            }),
        )?)?;
        attachment_revision = attachment.revision;

        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("cell-run")
            .arg("--binding")
            .arg(&binding_path)
            .arg("--prompt")
            .arg(&bundle.prompt)
            .env("SHUTTLE_CELL_EXCLUDE_TOOLS", "edit,write")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        if let Some(model) = &bundle.model {
            cmd.arg("--model").arg(model);
        }
        let mut child = cmd.spawn()?;
        let heartbeat_every =
            Duration::from_millis((lease_ttl_ms.max(3) as u64 / 3).clamp(250, 30_000));
        let deadline_wait =
            Duration::from_millis(bundle.deadline_ms.saturating_sub(now_ms()).max(0) as u64);
        let deadline = Instant::now() + deadline_wait;
        let mut next_heartbeat = Instant::now() + heartbeat_every;
        let mut exit_code = loop {
            if let Some(status) = child.try_wait()? {
                break status.code().unwrap_or(-1);
            }
            let now = Instant::now();
            if now >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break 124;
            }
            if now >= next_heartbeat {
                let renewed = client
                    .call(
                        ExternalOperation::Renew,
                        json!({
                            "attachment_id": bundle.attachment_id,
                            "expected_attachment_revision": attachment.revision,
                            "fencing_epoch": bundle.fencing_epoch,
                            "lease_ms": lease_ttl_ms,
                        }),
                    )
                    .and_then(cf_result::<shuttle_goal::AttachmentProjection>);
                attachment = match renewed {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                };
                attachment_revision = attachment.revision;
                next_heartbeat = Instant::now() + heartbeat_every;
            }
            thread::sleep(Duration::from_millis(50));
        };
        attachment = cf_result(client.call(
            ExternalOperation::Renew,
            json!({
                "attachment_id": bundle.attachment_id,
                "expected_attachment_revision": attachment.revision,
                "fencing_epoch": bundle.fencing_epoch,
                "lease_ms": lease_ttl_ms,
            }),
        )?)?;
        attachment_revision = attachment.revision;
        let settled = now_ms();
        let stdout_raw = bounded_redacted(&std::fs::read(&stdout_path)?);
        let stderr_raw = bounded_redacted(&std::fs::read(&stderr_path)?);
        let mut verification_records = if bundle.role == GoalActionRole::Verifier && exit_code == 0
        {
            run_verification_commands_cf(
                &bundle.cell_binding,
                bundle.deadline_ms,
                &client,
                &bundle.attachment_id,
                &mut attachment,
                bundle.fencing_epoch,
                lease_ttl_ms,
            )?
        } else {
            Vec::new()
        };
        redact_verification_records(&mut verification_records);
        if bundle.role == GoalActionRole::Verifier
            && (verification_records.is_empty()
                || verification_records
                    .iter()
                    .any(|record| record["exit_code"] != 0))
        {
            exit_code = 126;
        }
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
        let stdout_text = String::from_utf8_lossy(&stdout_raw).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr_raw).into_owned();
        let tokens_used = tokens_from_stderr(&stderr_text);
        let (stdout_ref, stdout_digest) = artifact_ref(stdout_text.as_bytes());
        let (stderr_ref, stderr_digest) = artifact_ref(stderr_text.as_bytes());
        let verification_bytes = bounded_verification_json(&verification_records)?;
        let verification_artifact = verification_bytes.as_ref().map(|bytes| artifact_ref(bytes));
        let verification_text = verification_bytes
            .as_ref()
            .map(|bytes| String::from_utf8(bytes.clone()).expect("JSON is UTF-8"));
        let fallback_artifacts = ExternalResultArtifacts {
            stdout: stdout_text.clone(),
            stderr: stderr_text.clone(),
            verification_command_evidence: verification_text.clone(),
        };
        let result = GoalActionResult {
            schema_version: ACTION_RESULT_SCHEMA.into(),
            goal_ref: bundle.goal_ref.clone(),
            action_id: bundle.action_id.clone(),
            role: bundle.role,
            provider_run_id: bundle.idempotency_key.clone(),
            started_at_ms: started,
            settled_at_ms: settled,
            exit_code,
            stdout_ref,
            stdout_digest,
            stderr_ref,
            stderr_digest,
            verification_command_evidence_ref: verification_artifact
                .as_ref()
                .map(|value| value.0.clone()),
            verification_command_evidence_digest: verification_artifact
                .as_ref()
                .map(|value| value.1.clone()),
            proposal: if exit_code == 0 {
                "progress".into()
            } else {
                "blocked".into()
            },
            residual_risks: if exit_code == 0 {
                Vec::new()
            } else {
                vec!["cell-run did not settle green".into()]
            },
        };
        let parameters = json!({
            "action_id": bundle.action_id,
            "dispatch_claim_token": claim_token,
            "result": result,
            "artifacts": {
                "stdout": stdout_text,
                "stderr": stderr_text,
                "verification_command_evidence": verification_text,
            },
            "expected_attachment_revision": attachment_revision,
            "fencing_epoch": bundle.fencing_epoch,
        });
        let result_id = stable_cf_request_id("goal-result", &parameters)?;
        let outcome: ExternalActionOutcome = cf_result(call_cf_stable_with_retry(
            &client,
            ExternalOperation::RecordResult,
            parameters,
            result_id,
        )?)?;
        Ok(json!({
            "action": outcome.action, "attachment": attachment,
            "goal": outcome.goal, "result": outcome.result.unwrap_or(result),
            "artifacts": outcome.artifacts.unwrap_or(fallback_artifacts),
            "idempotent_replay": false, "tokens_used": tokens_used,
        }))
    })();
    let _ = std::fs::remove_file(&binding_path);
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    let _ = std::fs::remove_dir(&runtime);
    match execution {
        Ok(value) => Ok(value),
        Err(error) => {
            let recovery_parameters = json!({"bundle": &bundle});
            let recovery_id = stable_cf_request_id("goal-prepare", &recovery_parameters)?;
            if let Ok(recovered) = call_cf_stable_with_retry(
                &client,
                ExternalOperation::PrepareAction,
                recovery_parameters,
                recovery_id,
            )
            .and_then(cf_result::<ExternalPreparedAction>)
            {
                if recovered.idempotent_replay
                    && recovered.action.status == ActionStatus::Settled
                    && recovered.result.is_some()
                    && recovered.artifacts.is_some()
                {
                    if let Ok(latest) = client
                        .call(
                            ExternalOperation::Get,
                            json!({"attachment_id": bundle.attachment_id}),
                        )
                        .and_then(cf_result::<shuttle_goal::AttachmentProjection>)
                    {
                        return Ok(json!({
                            "action": recovered.action, "attachment": latest,
                            "goal": recovered.goal, "result": recovered.result,
                            "artifacts": recovered.artifacts, "idempotent_replay": true,
                            "recovered_after_response_loss": true,
                        }));
                    }
                }
            }
            if let Ok(latest) = client
                .call(
                    ExternalOperation::Get,
                    json!({"attachment_id": bundle.attachment_id}),
                )
                .and_then(cf_result::<shuttle_goal::AttachmentProjection>)
            {
                attachment_revision = latest.revision;
            }
            let parameters = json!({
                "action_id": bundle.action_id,
                "attachment_id": bundle.attachment_id,
                "expected_attachment_revision": attachment_revision,
                "fencing_epoch": bundle.fencing_epoch,
            });
            let request_id = stable_cf_request_id("goal-reconcile", &parameters)?;
            let persisted = call_cf_stable_with_retry(
                &client,
                ExternalOperation::Reconcile,
                parameters,
                request_id,
            );
            let suffix = match persisted {
                Ok(ExternalFacadeResponse::Result { .. }) => "reconcile_required persisted".into(),
                Ok(_) => "reconcile returned a non-result outcome".into(),
                Err(persist_error) => {
                    format!("failed to persist reconcile_required: {persist_error}")
                }
            };
            Err(GoalError::ReconcileRequired(format!(
                "{}: post-intent execution failed: {error}; {suffix}",
                bundle.action_id
            )))
        }
    }
}

fn run_verification_commands_cf(
    binding: &shuttle_protocol::CellBinding,
    deadline_ms: i64,
    client: &ExternalGoalClient,
    attachment_id: &str,
    attachment: &mut shuttle_goal::AttachmentProjection,
    fencing_epoch: u64,
    lease_ttl_ms: i64,
) -> Result<Vec<serde_json::Value>, GoalError> {
    let remaining = deadline_ms.saturating_sub(now_ms());
    if remaining <= 0 {
        return Ok(vec![json!({
            "schema_version":"shuttle_goal_command_evidence/v1", "argv":[],
            "wall_ms":0, "exit_code":124,
            "assignment_digest":binding.assignment_digest, "base_oid":binding.base_oid,
            "workspace_mode":"disposable_snapshot", "stdout":"",
            "stderr":"verification deadline expired",
        })]);
    }
    let heartbeat_every =
        Duration::from_millis((lease_ttl_ms.max(3) as u64 / 3).clamp(250, 30_000));
    let mut authority_error = None;
    let result =
        crate::cell_run::run_allowed_commands(binding, remaining as u64, heartbeat_every, || {
            match client
                .call(
                    ExternalOperation::Renew,
                    json!({
                        "attachment_id": attachment_id,
                        "expected_attachment_revision": attachment.revision,
                        "fencing_epoch": fencing_epoch,
                        "lease_ms": lease_ttl_ms,
                    }),
                )
                .and_then(cf_result::<shuttle_goal::AttachmentProjection>)
            {
                Ok(renewed) => {
                    *attachment = renewed;
                    Ok(())
                }
                Err(error) => {
                    authority_error = Some(error);
                    Err(anyhow::anyhow!("verifier authority heartbeat failed"))
                }
            }
        });
    match result {
        Ok(records) => Ok(records),
        Err(_) if authority_error.is_some() => Err(authority_error.unwrap()),
        Err(error) => Ok(vec![json!({
            "schema_version":"shuttle_goal_command_evidence/v1", "argv":[],
            "wall_ms":0, "exit_code":126,
            "assignment_digest":binding.assignment_digest, "base_oid":binding.base_oid,
            "workspace_mode":"disposable_snapshot", "stdout":"",
            "stderr":format!("verification command setup failed: {error}"),
        })]),
    }
}

fn verify_cf_replay_matches(
    existing: &ExternalActionView,
    template: &GoalActionTemplate,
    goal_ref: &str,
) -> Result<(), GoalError> {
    let action = &existing.action;
    let stored = &existing.bundle;
    if action.goal_ref != goal_ref
        || action.action_kind != template.action_kind
        || action.idempotency_key != template.idempotency_key
        || stored.prompt != template.prompt
        || stored.role != template.role
        || stored.model != template.model
        || stored.expected_artifacts != template.expected_artifacts
        || stored.satisfies != template.satisfies
        || serde_json::to_value(&stored.cell_binding)?
            != serde_json::to_value(&template.cell_binding)?
    {
        return Err(GoalError::InvalidInput(format!(
            "existing external action does not match run plan: {}",
            template.action_id
        )));
    }
    Ok(())
}

fn tokens_from_stderr(text: &str) -> Option<u64> {
    for line in text.lines().rev() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("exit_code").is_some() && event.get("wall_ms").is_some() {
            return event.get("tokens").and_then(serde_json::Value::as_u64);
        }
    }
    None
}

fn cf_step_tokens(value: &serde_json::Value) -> Option<u64> {
    let text = value
        .pointer("/artifacts/stderr")
        .and_then(serde_json::Value::as_str)?;
    tokens_from_stderr(text)
}

fn run_bounded_cf(a: RunArgs) -> Result<serde_json::Value, GoalError> {
    validate_run_limits(&a)?;
    let raw_plan = std::fs::read(&a.plan)?;
    let plan = GoalRunPlan::parse(&raw_plan)?;
    let client = cf_client(&a.store, Some(&plan.goal_ref))?;
    let started = Instant::now();
    let active_limit = Duration::from_secs(a.max_active_seconds);
    let mut executed = 0usize;
    let mut replayed = 0usize;
    let mut provider_failures = 0usize;
    let mut tokens_used = 0u64;
    let mut token_usage_complete = true;
    let mut consecutive_no_progress = 0usize;
    let mut independent_verifier_settled = false;
    let mut summaries = Vec::new();
    let mut stop_reason = RunStopReason::PlanExhausted;

    for template in &plan.actions {
        if started.elapsed() >= active_limit {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }
        let goal: shuttle_goal::GoalProjection =
            cf_result(client.call(ExternalOperation::Get, json!({"goal_ref": plan.goal_ref}))?)?;
        let attachment: shuttle_goal::AttachmentProjection = cf_result(client.call(
            ExternalOperation::Get,
            json!({"attachment_id": plan.attachment_id}),
        )?)?;
        if attachment.goal_ref != plan.goal_ref {
            return Err(GoalError::InvalidInput(
                "run plan attachment belongs to another Goal".into(),
            ));
        }
        if goal.status != GoalStatus::Active {
            stop_reason = RunStopReason::GoalNotActive;
            break;
        }
        if attachment.role != AttachmentRole::Controller
            || attachment.state != AttachmentState::Active
        {
            stop_reason = RunStopReason::AttachmentNotController;
            break;
        }
        let existing = cf_optional_action(&client, &template.action_id)?;
        let mut resume_bundle = None;
        if let Some(existing) = &existing {
            verify_cf_replay_matches(existing, template, &plan.goal_ref)?;
            if existing.action.status == ActionStatus::ReconcileRequired {
                stop_reason = RunStopReason::ReconcileRequired;
                break;
            }
            if existing.action.status == ActionStatus::Settled {
                replayed += 1;
                consecutive_no_progress += 1;
                let evidence_ref = existing
                    .result
                    .as_ref()
                    .and_then(|result| result.verification_command_evidence_ref.clone());
                if template.role == GoalActionRole::Verifier
                    && existing.action.exit_code == Some(0)
                    && evidence_ref.is_some()
                    && existing
                        .artifacts
                        .as_ref()
                        .is_some_and(|artifacts| artifacts.verification_command_evidence.is_some())
                {
                    independent_verifier_settled = true;
                }
                summaries.push(RunActionSummary {
                    action_id: template.action_id.clone(),
                    role: template.role,
                    idempotent_replay: true,
                    exit_code: existing.action.exit_code.map(i64::from),
                    verification_evidence_ref: evidence_ref,
                    goal_revision: goal.revision,
                });
                if consecutive_no_progress >= a.max_no_progress {
                    stop_reason = RunStopReason::NoProgressBudget;
                    break;
                }
                continue;
            }
            if existing.action.status == ActionStatus::Running {
                resume_bundle = Some(existing.bundle.clone());
            }
        }
        if goal.evidence.pending_effects > 0 && resume_bundle.is_none() {
            stop_reason = RunStopReason::ReconcileRequired;
            break;
        }
        if executed >= a.max_actions {
            stop_reason = RunStopReason::MaxActions;
            break;
        }
        let remaining_ms = active_limit
            .saturating_sub(started.elapsed())
            .as_millis()
            .min(i64::MAX as u128) as i64;
        if remaining_ms <= 0 {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }
        let bundle = resume_bundle.unwrap_or_else(|| {
            bind_template(
                template,
                &goal,
                &attachment,
                template.timeout_ms.min(remaining_ms as u64),
            )
        });
        let value = match run_step_bundle_cf(bundle, &a.store, a.lease_seconds) {
            Err(error) if is_reconcile_required(&error) => {
                stop_reason = RunStopReason::ReconcileRequired;
                break;
            }
            outcome => outcome?,
        };
        executed += 1;
        consecutive_no_progress = 0;
        let exit_code = value["result"]["exit_code"].as_i64();
        let revision = value["goal"]["revision"]
            .as_u64()
            .ok_or_else(|| GoalError::Integrity("external step omitted Goal revision".into()))?;
        if exit_code != Some(0) {
            provider_failures += 1;
        }
        match cf_step_tokens(&value) {
            Some(tokens) => tokens_used = tokens_used.saturating_add(tokens),
            None => token_usage_complete = false,
        }
        let evidence_ref = value["result"]["verification_command_evidence_ref"]
            .as_str()
            .map(str::to_owned);
        if template.role == GoalActionRole::Verifier
            && exit_code == Some(0)
            && evidence_ref.is_some()
        {
            independent_verifier_settled = true;
        }
        summaries.push(RunActionSummary {
            action_id: template.action_id.clone(),
            role: template.role,
            idempotent_replay: false,
            exit_code,
            verification_evidence_ref: evidence_ref,
            goal_revision: revision,
        });
        if started.elapsed() >= active_limit {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }
        if provider_failures >= a.max_provider_failures {
            stop_reason = RunStopReason::ProviderFailureBudget;
            break;
        }
        if !token_usage_complete {
            stop_reason = RunStopReason::TokenUsageUnknown;
            break;
        }
        if tokens_used >= a.max_tokens {
            stop_reason = RunStopReason::TokenBudget;
            break;
        }
    }

    let final_goal: shuttle_goal::GoalProjection =
        cf_result(client.call(ExternalOperation::Get, json!({"goal_ref": plan.goal_ref}))?)?;
    let final_attachment: shuttle_goal::AttachmentProjection = cf_result(client.call(
        ExternalOperation::Get,
        json!({"attachment_id": plan.attachment_id}),
    )?)?;
    to_value(RunResult {
        schema_version: RUN_RESULT_SCHEMA.into(),
        goal_ref: plan.goal_ref,
        stop_reason,
        actions_planned: plan.actions.len(),
        actions_executed: executed,
        actions_replayed: replayed,
        provider_failures,
        tokens_used,
        token_usage_complete,
        no_progress_events: consecutive_no_progress,
        active_ms: started.elapsed().as_millis(),
        independent_verifier_settled,
        actions: summaries,
        final_goal,
        final_attachment,
    })
}

fn is_reconcile_required(error: &GoalError) -> bool {
    match error {
        GoalError::ReconcileRequired(_) => true,
        GoalError::BackendOutcome(value) => {
            value
                .get("conflict")
                .or_else(|| value.get("error"))
                .and_then(|detail| detail.get("reason_code"))
                .and_then(serde_json::Value::as_str)
                == Some("reconcile_required")
        }
        _ => false,
    }
}

fn validate_run_limits(a: &RunArgs) -> Result<(), GoalError> {
    if a.max_actions == 0 || a.max_actions > 1024 {
        return Err(GoalError::InvalidInput(
            "max_actions must be between 1 and 1024".into(),
        ));
    }
    if a.max_active_seconds == 0 || a.max_active_seconds > 86_400 {
        return Err(GoalError::InvalidInput(
            "max_active_seconds must be between 1 and 86400".into(),
        ));
    }
    if a.max_provider_failures == 0 || a.max_provider_failures > 1024 {
        return Err(GoalError::InvalidInput(
            "max_provider_failures must be between 1 and 1024".into(),
        ));
    }
    if a.max_tokens == 0 {
        return Err(GoalError::InvalidInput(
            "max_tokens must be positive".into(),
        ));
    }
    if a.max_no_progress == 0 || a.max_no_progress > 1024 {
        return Err(GoalError::InvalidInput(
            "max_no_progress must be between 1 and 1024".into(),
        ));
    }
    Ok(())
}

fn run_step_bundle(
    bundle: GoalActionBundle,
    raw: &[u8],
    store_args: &StoreArgs,
    lease_seconds: u64,
) -> Result<serde_json::Value, GoalError> {
    let mut store = open(store_args)?;
    let prepared = store.prepare_action(&bundle, raw)?;
    if prepared.status == ActionStatus::Settled {
        let attachment = store.get_attachment(&prepared.attachment_id)?;
        return Ok(
            json!({"action": prepared, "attachment": attachment, "goal": store.get(&bundle.goal_ref)?, "idempotent_replay": true}),
        );
    }
    let mut attachment_revision = bundle.attachment_revision;
    let lease_ttl_ms = ttl_ms(lease_seconds);
    let runtime = store.paths().root.join("runtime").join(&bundle.action_id);
    std::fs::create_dir_all(&runtime)?;
    let binding_path = runtime.join("binding.json");
    atomic_write(
        &binding_path,
        &serde_json::to_vec_pretty(&bundle.cell_binding)?,
    )?;
    let started = now_ms();
    let exe = std::env::current_exe()?;
    let stdout_path = runtime.join("stdout.log");
    let stderr_path = runtime.join("stderr.log");
    let stdout_file = create_private_file(&stdout_path)?;
    let stderr_file = create_private_file(&stderr_path)?;
    let renewed = store.renew(
        &bundle.attachment_id,
        attachment_revision,
        bundle.fencing_epoch,
        lease_ttl_ms,
    )?;
    attachment_revision = renewed.revision;
    store.mark_action_running(&bundle.action_id, attachment_revision, bundle.fencing_epoch)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("cell-run")
        .arg("--binding")
        .arg(&binding_path)
        .arg("--prompt")
        .arg(&bundle.prompt)
        // Goal actions are leaf cells: all writes must pass through the
        // scope-enforced SHUTTLE extension, never Pi's native edit/write tools.
        .env("SHUTTLE_CELL_EXCLUDE_TOOLS", "edit,write")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(model) = &bundle.model {
        cmd.arg("--model").arg(model);
    }
    let execution = (|| -> Result<serde_json::Value, GoalError> {
        let mut child = cmd.spawn()?;
        let heartbeat_every =
            Duration::from_millis((lease_ttl_ms.max(3) as u64 / 3).clamp(250, 30_000));
        let deadline_wait =
            Duration::from_millis(bundle.deadline_ms.saturating_sub(now_ms()).max(0) as u64);
        let deadline = Instant::now() + deadline_wait;
        let mut next_heartbeat = Instant::now() + heartbeat_every;
        let mut exit_code = loop {
            if let Some(status) = child.try_wait()? {
                break status.code().unwrap_or(-1);
            }
            let now = Instant::now();
            if now >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break 124;
            }
            if now >= next_heartbeat {
                match store.renew(
                    &bundle.attachment_id,
                    attachment_revision,
                    bundle.fencing_epoch,
                    lease_ttl_ms,
                ) {
                    Ok(renewed) => attachment_revision = renewed.revision,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                }
                next_heartbeat = Instant::now() + heartbeat_every;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let renewed = store.renew(
            &bundle.attachment_id,
            attachment_revision,
            bundle.fencing_epoch,
            lease_ttl_ms,
        )?;
        attachment_revision = renewed.revision;
        let settled = now_ms();
        let stdout_raw = std::fs::read(&stdout_path)?;
        let stderr_raw = std::fs::read(&stderr_path)?;
        let mut verification_records = if bundle.role == GoalActionRole::Verifier && exit_code == 0
        {
            run_verification_commands(
                &bundle.cell_binding,
                bundle.deadline_ms,
                &mut store,
                &bundle.attachment_id,
                &mut attachment_revision,
                bundle.fencing_epoch,
                lease_ttl_ms,
            )?
        } else {
            Vec::new()
        };
        redact_verification_records(&mut verification_records);
        if bundle.role == GoalActionRole::Verifier
            && (verification_records.is_empty()
                || verification_records
                    .iter()
                    .any(|record| record["exit_code"] != 0))
        {
            exit_code = 126;
        }
        std::fs::remove_file(&stdout_path)?;
        std::fs::remove_file(&stderr_path)?;
        let stdout = store.artifacts()?.put(&bounded_redacted(&stdout_raw))?;
        let stderr = store.artifacts()?.put(&bounded_redacted(&stderr_raw))?;
        let verification_evidence = if verification_records.is_empty() {
            None
        } else {
            Some(
                store
                    .artifacts()?
                    .put(&serde_json::to_vec_pretty(&verification_records)?)?,
            )
        };
        let result = GoalActionResult {
            schema_version: ACTION_RESULT_SCHEMA.into(),
            goal_ref: bundle.goal_ref.clone(),
            action_id: bundle.action_id.clone(),
            role: bundle.role,
            provider_run_id: bundle.idempotency_key.clone(),
            started_at_ms: started,
            settled_at_ms: settled,
            exit_code,
            stdout_ref: stdout.r#ref.clone(),
            stdout_digest: stdout.digest.clone(),
            stderr_ref: stderr.r#ref.clone(),
            stderr_digest: stderr.digest.clone(),
            verification_command_evidence_ref: verification_evidence
                .as_ref()
                .map(|artifact| artifact.r#ref.clone()),
            verification_command_evidence_digest: verification_evidence
                .as_ref()
                .map(|artifact| artifact.digest.clone()),
            proposal: if exit_code == 0 {
                "progress".into()
            } else {
                "blocked".into()
            },
            residual_risks: if exit_code == 0 {
                Vec::new()
            } else {
                vec!["cell-run did not settle green".into()]
            },
        };
        let result_bytes = serde_json::to_vec_pretty(&result)?;
        let result_ref = store.artifacts()?.put(&result_bytes)?;
        let renewed = store.renew(
            &bundle.attachment_id,
            attachment_revision,
            bundle.fencing_epoch,
            lease_ttl_ms,
        )?;
        attachment_revision = renewed.revision;
        let (action, goal) = store.record_action_result(
            &bundle.action_id,
            &result_ref,
            exit_code,
            attachment_revision,
            bundle.fencing_epoch,
        )?;
        let attachment = store.get_attachment(&bundle.attachment_id)?;
        Ok(
            json!({"action":action,"attachment":attachment,"goal":goal,"result":result,"result_ref":result_ref.r#ref,"idempotent_replay":false}),
        )
    })();
    match execution {
        Ok(value) => Ok(value),
        Err(error) => {
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            let persisted = store.reconcile_action(
                &bundle.action_id,
                &bundle.attachment_id,
                attachment_revision,
                bundle.fencing_epoch,
            );
            let suffix = match persisted {
                Ok(_) => "reconcile_required persisted".to_string(),
                Err(persist_error) => {
                    format!("failed to persist reconcile_required: {persist_error}")
                }
            };
            Err(GoalError::ReconcileRequired(format!(
                "{}: post-intent execution failed: {error}; {suffix}",
                bundle.action_id
            )))
        }
    }
}

fn run_bounded(a: RunArgs) -> Result<serde_json::Value, GoalError> {
    validate_run_limits(&a)?;

    let raw_plan = std::fs::read(&a.plan)?;
    let plan = GoalRunPlan::parse(&raw_plan)?;
    let started = Instant::now();
    let active_limit = Duration::from_secs(a.max_active_seconds);
    let mut executed = 0usize;
    let mut replayed = 0usize;
    let mut provider_failures = 0usize;
    let mut tokens_used = 0u64;
    let mut token_usage_complete = true;
    let mut consecutive_no_progress = 0usize;
    let mut independent_verifier_settled = false;
    let mut summaries = Vec::new();
    let mut stop_reason = RunStopReason::PlanExhausted;

    for template in &plan.actions {
        if started.elapsed() >= active_limit {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }

        let store = open(&a.store)?;
        let goal = store.get(&plan.goal_ref)?;
        let attachment = store.get_attachment(&plan.attachment_id)?;
        if attachment.goal_ref != plan.goal_ref {
            return Err(GoalError::InvalidInput(
                "run plan attachment belongs to another Goal".into(),
            ));
        }
        if goal.status != GoalStatus::Active {
            stop_reason = RunStopReason::GoalNotActive;
            break;
        }
        if attachment.role != AttachmentRole::Controller
            || attachment.state != AttachmentState::Active
        {
            stop_reason = RunStopReason::AttachmentNotController;
            break;
        }

        let existing_committed = match store.get_action(&template.action_id) {
            Ok(existing) => {
                verify_replay_matches(&store, &existing, template, &plan.goal_ref)?;
                if existing.status == ActionStatus::Running
                    || existing.status == ActionStatus::ReconcileRequired
                {
                    stop_reason = RunStopReason::ReconcileRequired;
                    break;
                }
                if existing.status != ActionStatus::Settled {
                    Some(existing)
                } else {
                    replayed += 1;
                    consecutive_no_progress += 1;
                    let verification_evidence_ref =
                        verifier_evidence_ref(&store, &existing, template.role)?;
                    if template.role == GoalActionRole::Verifier
                        && existing.exit_code == Some(0)
                        && verification_evidence_ref.is_some()
                    {
                        independent_verifier_settled = true;
                    }
                    summaries.push(RunActionSummary {
                        action_id: template.action_id.clone(),
                        role: template.role,
                        idempotent_replay: true,
                        exit_code: existing.exit_code.map(i64::from),
                        verification_evidence_ref,
                        goal_revision: goal.revision,
                    });
                    if consecutive_no_progress >= a.max_no_progress {
                        stop_reason = RunStopReason::NoProgressBudget;
                        break;
                    }
                    continue;
                }
            }
            Err(GoalError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if goal.evidence.pending_effects > 0
            && (existing_committed.is_none() || goal.evidence.pending_effects != 1)
        {
            stop_reason = RunStopReason::ReconcileRequired;
            break;
        }

        if executed >= a.max_actions {
            stop_reason = RunStopReason::MaxActions;
            break;
        }
        let remaining_ms = active_limit
            .saturating_sub(started.elapsed())
            .as_millis()
            .min(i64::MAX as u128) as i64;
        if remaining_ms <= 0 {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }
        let timeout_ms = template.timeout_ms.min(remaining_ms as u64);
        let (bundle, raw) = if let Some(existing) = existing_committed {
            let raw = store.artifacts()?.get(&existing.bundle_ref)?;
            let bundle = GoalActionBundle::parse(&raw)?;
            (bundle, raw)
        } else {
            let bundle = bind_template(template, &goal, &attachment, timeout_ms);
            let raw = serde_json::to_vec_pretty(&bundle)?;
            (bundle, raw)
        };
        drop(store);
        let value = run_step_bundle(bundle, &raw, &a.store, a.lease_seconds)?;
        executed += 1;
        consecutive_no_progress = 0;
        let exit_code = value["result"]["exit_code"].as_i64();
        let revision = value["goal"]["revision"]
            .as_u64()
            .ok_or_else(|| GoalError::Integrity("step result omitted goal revision".into()))?;
        if exit_code != Some(0) {
            provider_failures += 1;
        }
        let action_tokens = step_tokens(&value, &a.store)?;
        match action_tokens {
            Some(tokens) => tokens_used = tokens_used.saturating_add(tokens),
            None => token_usage_complete = false,
        }
        let verification_evidence_ref = value["result"]["verification_command_evidence_ref"]
            .as_str()
            .map(str::to_owned);
        if template.role == GoalActionRole::Verifier
            && exit_code == Some(0)
            && verification_evidence_ref.is_some()
        {
            independent_verifier_settled = true;
        }
        summaries.push(RunActionSummary {
            action_id: template.action_id.clone(),
            role: template.role,
            idempotent_replay: false,
            exit_code,
            verification_evidence_ref,
            goal_revision: revision,
        });
        if started.elapsed() >= active_limit {
            stop_reason = RunStopReason::MaxActiveTime;
            break;
        }
        if provider_failures >= a.max_provider_failures {
            stop_reason = RunStopReason::ProviderFailureBudget;
            break;
        }
        if !token_usage_complete {
            stop_reason = RunStopReason::TokenUsageUnknown;
            break;
        }
        if tokens_used >= a.max_tokens {
            stop_reason = RunStopReason::TokenBudget;
            break;
        }
    }

    let store = open(&a.store)?;
    let final_goal = store.get(&plan.goal_ref)?;
    let final_attachment = store.get_attachment(&plan.attachment_id)?;
    to_value(RunResult {
        schema_version: RUN_RESULT_SCHEMA.into(),
        goal_ref: plan.goal_ref,
        stop_reason,
        actions_planned: plan.actions.len(),
        actions_executed: executed,
        actions_replayed: replayed,
        provider_failures,
        tokens_used,
        token_usage_complete,
        no_progress_events: consecutive_no_progress,
        active_ms: started.elapsed().as_millis(),
        independent_verifier_settled,
        actions: summaries,
        final_goal,
        final_attachment,
    })
}

fn step_tokens(
    value: &serde_json::Value,
    store_args: &StoreArgs,
) -> Result<Option<u64>, GoalError> {
    let reference = value["result"]["stderr_ref"]
        .as_str()
        .ok_or_else(|| GoalError::Integrity("step result omitted stderr_ref".into()))?;
    let store = open(store_args)?;
    let bytes = store.artifacts()?.get(reference)?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines().rev() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("exit_code").is_some() && event.get("wall_ms").is_some() {
            return Ok(event.get("tokens").and_then(serde_json::Value::as_u64));
        }
    }
    Ok(None)
}

fn bind_template(
    template: &GoalActionTemplate,
    goal: &shuttle_goal::GoalProjection,
    attachment: &shuttle_goal::AttachmentProjection,
    timeout_ms: u64,
) -> GoalActionBundle {
    GoalActionBundle {
        schema_version: ACTION_BUNDLE_SCHEMA.into(),
        goal_ref: goal.goal_ref.clone(),
        goal_revision: goal.revision,
        attachment_id: attachment.attachment_id.clone(),
        attachment_revision: attachment.revision,
        fencing_epoch: attachment.fencing_epoch,
        action_id: template.action_id.clone(),
        action_kind: template.action_kind.clone(),
        role: template.role,
        idempotency_key: template.idempotency_key.clone(),
        cell_binding: template.cell_binding.clone(),
        prompt: template.prompt.clone(),
        model: template.model.clone(),
        expected_artifacts: template.expected_artifacts.clone(),
        satisfies: template.satisfies.clone(),
        deadline_ms: now_ms().saturating_add(timeout_ms.min(i64::MAX as u64) as i64),
    }
}

fn verify_replay_matches(
    store: &LocalGoalStore,
    existing: &shuttle_goal::ActionProjection,
    template: &GoalActionTemplate,
    goal_ref: &str,
) -> Result<(), GoalError> {
    if existing.goal_ref != goal_ref
        || existing.action_kind != template.action_kind
        || existing.idempotency_key != template.idempotency_key
    {
        return Err(GoalError::InvalidInput(format!(
            "existing action identity does not match run plan: {}",
            template.action_id
        )));
    }
    let stored = GoalActionBundle::parse(&store.artifacts()?.get(&existing.bundle_ref)?)?;
    let stored_binding = serde_json::to_value(&stored.cell_binding)?;
    let planned_binding = serde_json::to_value(&template.cell_binding)?;
    if stored.prompt != template.prompt
        || stored.role != template.role
        || stored.model != template.model
        || stored.expected_artifacts != template.expected_artifacts
        || stored_binding != planned_binding
    {
        return Err(GoalError::InvalidInput(format!(
            "existing action payload does not match run plan: {}",
            template.action_id
        )));
    }
    Ok(())
}

fn verifier_evidence_ref(
    store: &LocalGoalStore,
    action: &shuttle_goal::ActionProjection,
    role: GoalActionRole,
) -> Result<Option<String>, GoalError> {
    if role != GoalActionRole::Verifier {
        return Ok(None);
    }
    let result_ref = action.result_ref.as_deref().ok_or_else(|| {
        GoalError::Integrity(format!(
            "settled verifier action has no result artifact: {}",
            action.action_id
        ))
    })?;
    let result: GoalActionResult = serde_json::from_slice(&store.artifacts()?.get(result_ref)?)?;
    if result.role != GoalActionRole::Verifier || result.exit_code != 0 {
        return Ok(None);
    }
    match (
        result.verification_command_evidence_ref,
        result.verification_command_evidence_digest,
    ) {
        (Some(reference), Some(digest)) => {
            let evidence = store.artifacts()?.get(&reference)?;
            let verified = store.artifacts()?.put(&evidence)?;
            if verified.digest != digest {
                return Err(GoalError::Integrity(format!(
                    "verifier command evidence digest mismatch: {}",
                    action.action_id
                )));
            }
            Ok(Some(reference))
        }
        (None, None) => Ok(None),
        _ => Err(GoalError::Integrity(format!(
            "incomplete verifier command evidence reference: {}",
            action.action_id
        ))),
    }
}

fn to_value<T: Serialize>(value: T) -> Result<serde_json::Value, GoalError> {
    serde_json::to_value(value).map_err(GoalError::Serde)
}

fn bounded_redacted(bytes: &[u8]) -> Vec<u8> {
    let secrets: Vec<String> = std::env::vars()
        .filter(|(name, value)| {
            let upper = name.to_ascii_uppercase();
            !value.is_empty()
                && [
                    "TOKEN",
                    "SECRET",
                    "PASSWORD",
                    "CREDENTIAL",
                    "API_KEY",
                    "PROVIDER_KEY",
                ]
                .iter()
                .any(|marker| upper.contains(marker))
        })
        .map(|(_, value)| value)
        .collect();
    bounded_redacted_with(bytes, &secrets)
}

fn redact_verification_records(records: &mut [serde_json::Value]) {
    for record in records {
        for field in ["stdout", "stderr"] {
            let Some(text) = record.get(field).and_then(serde_json::Value::as_str) else {
                continue;
            };
            record[field] = serde_json::Value::String(
                String::from_utf8_lossy(&bounded_redacted(text.as_bytes())).into_owned(),
            );
        }
    }
}

fn run_verification_commands(
    binding: &shuttle_protocol::CellBinding,
    deadline_ms: i64,
    store: &mut LocalGoalStore,
    attachment_id: &str,
    attachment_revision: &mut u64,
    fencing_epoch: u64,
    lease_ttl_ms: i64,
) -> Result<Vec<serde_json::Value>, GoalError> {
    let remaining = deadline_ms.saturating_sub(now_ms());
    if remaining <= 0 {
        return Ok(vec![json!({
            "schema_version":"shuttle_goal_command_evidence/v1",
            "argv":[],
            "wall_ms":0,
            "exit_code":124,
            "assignment_digest":binding.assignment_digest,
            "base_oid":binding.base_oid,
            "workspace_mode":"disposable_snapshot",
            "stdout":"",
            "stderr":"verification deadline expired",
        })]);
    }
    let heartbeat_every =
        Duration::from_millis((lease_ttl_ms.max(3) as u64 / 3).clamp(250, 30_000));
    let mut authority_error = None;
    let result =
        crate::cell_run::run_allowed_commands(binding, remaining as u64, heartbeat_every, || {
            match store.renew(
                attachment_id,
                *attachment_revision,
                fencing_epoch,
                lease_ttl_ms,
            ) {
                Ok(renewed) => {
                    *attachment_revision = renewed.revision;
                    Ok(())
                }
                Err(error) => {
                    authority_error = Some(error);
                    Err(anyhow::anyhow!("verifier authority heartbeat failed"))
                }
            }
        });
    match result {
        Ok(records) => Ok(records),
        Err(_) if authority_error.is_some() => Err(authority_error.unwrap()),
        Err(error) => Ok(vec![json!({
            "schema_version":"shuttle_goal_command_evidence/v1",
            "argv":[],
            "wall_ms":0,
            "exit_code":126,
            "assignment_digest":binding.assignment_digest,
            "base_oid":binding.base_oid,
            "workspace_mode":"disposable_snapshot",
            "stdout":"",
            "stderr":format!("verification command setup failed: {error}"),
        })]),
    }
}

fn bounded_redacted_with(bytes: &[u8], secrets: &[String]) -> Vec<u8> {
    const LIMIT: usize = 64 * 1024;
    let start = bytes.len().saturating_sub(LIMIT);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    for secret in secrets {
        text = text.replace(secret, "[REDACTED]");
    }
    let output = text.into_bytes();
    output[output.len().saturating_sub(LIMIT)..].to_vec()
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), GoalError> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        use std::io::Write;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn create_private_file(path: &std::path::Path) -> Result<std::fs::File, GoalError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn emit_error(error: GoalError) -> anyhow::Result<()> {
    if let GoalError::BackendOutcome(body) = error {
        let code = body
            .get("conflict")
            .or_else(|| body.get("error"))
            .and_then(|detail| detail.get("reason_code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("backend_outcome")
            .to_owned();
        println!("{}", serde_json::to_string_pretty(&body)?);
        anyhow::bail!("goal command failed: {code}");
    }
    let (code, retryable) = match &error {
        GoalError::RevisionConflict { .. } => ("revision_conflict", true),
        GoalError::ControllerHeld(_) => ("controller_held", true),
        GoalError::StaleFence { .. } => ("stale_fence", false),
        GoalError::BackendUnavailable(_) => ("backend_unavailable", true),
        GoalError::BackendTimeout(_) => ("backend_timeout", true),
        GoalError::UnsupportedContract(_) => ("unsupported_contract", false),
        GoalError::ProtocolViolation(_) => ("protocol_violation", false),
        GoalError::BackendOutcome(_) => unreachable!(),
        GoalError::ReconcileRequired(_) => ("reconcile_required", false),
        GoalError::NotFound(_) => ("not_found", false),
        GoalError::Integrity(_) => ("integrity_failure", false),
        GoalError::InvalidInput(_) => ("invalid_input", false),
        GoalError::Io(_) | GoalError::Sql(_) => ("store_error", true),
        GoalError::Serde(_) => ("serialization_error", false),
    };
    let body = json!({"outcome":"error","reason_code":code,"message":error.to_string(),"retryable":retryable});
    println!("{}", serde_json::to_string_pretty(&body)?);
    anyhow::bail!("goal command failed: {code}")
}

#[cfg(test)]
mod tests {
    use super::{bounded_redacted_with, call_cf_stable_with_retry, redact_verification_records};
    use shuttle_goal::{ExternalGoalClient, ExternalOperation};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static RETRY_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(1);

    #[test]
    fn action_logs_are_bounded_and_provider_secrets_are_redacted() {
        let mut input = vec![b'x'; 70 * 1024];
        input.extend_from_slice(b" goal-secret-value tail");
        let output = bounded_redacted_with(&input, &["goal-secret-value".into()]);
        assert!(output.len() <= 64 * 1024);
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("goal-secret-value"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn verifier_evidence_stays_valid_json_after_bounding_and_redaction() {
        std::env::set_var("SHUTTLE_API_KEY", "verifier-secret");
        let mut records = vec![serde_json::json!({
            "stdout": format!("{} verifier-secret", "x".repeat(70 * 1024)),
            "stderr": "verifier-secret",
        })];
        redact_verification_records(&mut records);
        std::env::remove_var("SHUTTLE_API_KEY");
        assert!(records[0]["stdout"].as_str().unwrap().len() <= 64 * 1024);
        assert!(!serde_json::to_string(&records)
            .unwrap()
            .contains("verifier-secret"));
    }

    #[test]
    fn stable_action_retry_reuses_request_id_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        let facade = dir.path().join("goal-control");
        let request_id = format!(
            "retry-test-{}",
            RETRY_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let body = format!(
            r#"#!/bin/sh
set -eu
IFS= read -r request
n=0
[ -f '{count}' ] && n=$(sed -n '1p' '{count}')
n=$((n + 1))
printf '%s\n' "$n" > '{count}'
if [ "$n" -eq 1 ]; then sleep 5; fi
printf '%s\n' '{{"schema_version":"goal_control_facade_response/v1","request_id":"{request_id}","outcome":"result","current_revisions":{{"goal":1,"attachment":1,"action":1}},"result":{{"ok":true}}}}'
"#,
            count = counter.display(),
        );
        std::fs::write(&facade, body).unwrap();
        std::fs::set_permissions(&facade, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = ExternalGoalClient::new(&facade, "test-instance")
            .unwrap()
            .with_timeout(Duration::from_secs(2))
            .unwrap();

        let response = call_cf_stable_with_retry(
            &client,
            ExternalOperation::RecordResult,
            serde_json::json!({"action_id": "a1"}),
            request_id,
        )
        .unwrap();

        assert!(matches!(
            response,
            shuttle_goal::ExternalFacadeResponse::Result { .. }
        ));
        assert_eq!(std::fs::read_to_string(counter).unwrap().trim(), "2");
    }
}
