//! Durable local registry for the `shuttle cell-*` lifecycle.
//!
//! The registry stores only execution coordinates and references. Provider
//! credentials remain owned by pi and are never serialized here.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use shuttle_protocol::anchor::{ProviderReceipt, ProviderStatus, ReconciliationHint};
use shuttle_protocol::CellBinding;

use crate::sha256;

pub const RUN_RECORD_SCHEMA: &str = "ea_cell_run/v1";
pub const RUN_EVENT_SCHEMA: &str = "ea_cell_runtime_event/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    Ready,
    Active,
    Blocked,
    Done,
    Failed,
    Cancelled,
    Recoverable,
}

impl CellState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellRunRecord {
    pub schema_version: String,
    pub provider_run_id: String,
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohort_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_attempt_id: Option<String>,
    pub state: CellState,
    pub revision: u64,
    pub created_at_ms: u128,
    pub updated_at_ms: u128,
    pub binding_ref: String,
    pub binding_digest: String,
    pub assignment_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_descriptor_digest: Option<String>,
    pub workspace_root: String,
    pub environment_id: String,
    pub base_oid: String,
    pub session_ref: String,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    pub result_ref: Option<String>,
    pub receipt: Option<ProviderReceipt>,
    pub error_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_digest: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: &'static str,
    pub status: &'static str,
    pub registry_root: String,
    pub writable: bool,
    pub pi_binary: String,
    pub pi_available: bool,
    pub pi_version: Option<String>,
    pub pi_session_supported: bool,
    pub provider_credential_ref_present: bool,
    pub credential_source_kind: &'static str,
    pub container_use_binary: String,
    pub container_use_available: bool,
    pub container_use_version: Option<String>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CellRegistry {
    root: PathBuf,
}

impl CellRegistry {
    pub fn open_default() -> anyhow::Result<Self> {
        let root = if let Some(value) = std::env::var_os("SHUTTLE_CELL_STATE_DIR") {
            PathBuf::from(value)
        } else if cfg!(target_os = "macos") {
            home_dir()?.join("Library/Application Support/shuttle/cells")
        } else if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
            PathBuf::from(value).join("shuttle/cells")
        } else {
            home_dir()?.join(".local/state/shuttle/cells")
        };
        Self::open(root)
    }

    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(root.join("runs"))?;
        fs::create_dir_all(root.join("events"))?;
        fs::create_dir_all(root.join("bindings"))?;
        fs::create_dir_all(root.join("sessions"))?;
        fs::create_dir_all(root.join("locks"))?;
        fs::create_dir_all(root.join("usage"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let root = fs::canonicalize(root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn initialize(
        &self,
        provider_run_id: &str,
        attempt_id: Option<String>,
        binding: &CellBinding,
        binding_json: &str,
    ) -> anyhow::Result<CellRunRecord> {
        let run_id = checked_id(provider_run_id)?;
        let _lock = RunLock::acquire(self.lock_path(run_id))?;
        // `atomic_write` terminates text records with one newline; bind the
        // digest to the exact durable bytes so recovery can verify them.
        let durable_binding = format!("{}\n", binding_json.trim_end_matches('\n'));
        let digest = sha256::sha256_hex_prefixed(durable_binding.as_bytes());
        if let Some(existing) = self.read_unlocked(run_id)? {
            if existing.binding_digest != digest
                || existing.assignment_digest != binding.assignment_digest
                || existing.base_oid != binding.base_oid
                || existing.environment_id != binding.environment_id
                || existing.runtime_descriptor_ref != binding.runtime_descriptor_ref
                || existing.runtime_descriptor_digest
                    != binding
                        .runtime_descriptor_digest
                        .as_ref()
                        .map(ToString::to_string)
            {
                anyhow::bail!("provider_run_id is already bound to different run coordinates");
            }
            reject_lineage_drift(
                "SHUTTLE_COST_COHORT_ID",
                existing.cohort_id.as_deref(),
                env_coordinate("SHUTTLE_COST_COHORT_ID").as_deref(),
            )?;
            reject_lineage_drift(
                "SHUTTLE_PARENT_ATTEMPT_ID",
                existing.parent_attempt_id.as_deref(),
                env_coordinate("SHUTTLE_PARENT_ATTEMPT_ID").as_deref(),
            )?;
            return Ok(existing);
        }
        let binding_path = self.binding_path(run_id);
        atomic_write(
            &binding_path,
            durable_binding.trim_end_matches('\n').as_bytes(),
        )?;
        let now = now_ms();
        let record = CellRunRecord {
            schema_version: RUN_RECORD_SCHEMA.into(),
            provider_run_id: run_id.into(),
            attempt_id,
            cohort_id: env_coordinate("SHUTTLE_COST_COHORT_ID"),
            parent_attempt_id: env_coordinate("SHUTTLE_PARENT_ATTEMPT_ID"),
            state: CellState::Ready,
            revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            binding_ref: binding_path.to_string_lossy().into_owned(),
            binding_digest: digest,
            assignment_digest: binding.assignment_digest.clone(),
            runtime_descriptor_ref: binding.runtime_descriptor_ref.clone(),
            runtime_descriptor_digest: binding
                .runtime_descriptor_digest
                .as_ref()
                .map(ToString::to_string),
            workspace_root: binding.workspace_root.clone(),
            environment_id: binding.environment_id.clone(),
            base_oid: binding.base_oid.clone(),
            session_ref: self.session_dir(run_id).to_string_lossy().into_owned(),
            pid: None,
            pgid: None,
            result_ref: None,
            receipt: None,
            error_class: None,
            usage_ref: None,
            usage_digest: None,
        };
        self.write_unlocked(&record)?;
        self.append_event_unlocked(&record, "registered", serde_json::json!({}))?;
        Ok(record)
    }

    pub fn read(&self, provider_run_id: &str) -> anyhow::Result<Option<CellRunRecord>> {
        self.read_unlocked(checked_id(provider_run_id)?)
    }

    pub fn mutate<F>(
        &self,
        provider_run_id: &str,
        kind: &str,
        f: F,
    ) -> anyhow::Result<CellRunRecord>
    where
        F: FnOnce(&mut CellRunRecord) -> anyhow::Result<serde_json::Value>,
    {
        let run_id = checked_id(provider_run_id)?;
        let _lock = RunLock::acquire(self.lock_path(run_id))?;
        let mut record = self
            .read_unlocked(run_id)?
            .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
        let detail = f(&mut record)?;
        record.revision = record.revision.saturating_add(1);
        record.updated_at_ms = now_ms();
        self.write_unlocked(&record)?;
        self.append_event_unlocked(&record, kind, detail)?;
        Ok(record)
    }

    pub fn verify_binding(&self, record: &CellRunRecord) -> anyhow::Result<String> {
        let body = fs::read_to_string(&record.binding_ref)?;
        if sha256::sha256_hex_prefixed(body.as_bytes()) != record.binding_digest {
            anyhow::bail!("durable cell binding digest drift");
        }
        let binding = CellBinding::parse(&body)
            .map_err(|error| anyhow::anyhow!("durable cell binding invalid: {error}"))?;
        if binding.assignment_digest != record.assignment_digest
            || binding.base_oid != record.base_oid
            || binding.environment_id != record.environment_id
            || binding.workspace_root != record.workspace_root
            || binding.runtime_descriptor_ref != record.runtime_descriptor_ref
            || binding
                .runtime_descriptor_digest
                .as_ref()
                .map(ToString::to_string)
                != record.runtime_descriptor_digest
        {
            anyhow::bail!("durable cell binding coordinate drift");
        }
        Ok(body)
    }

    pub fn verify_base_oid(&self, record: &CellRunRecord) -> anyhow::Result<()> {
        let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
        let output = std::process::Command::new(git)
            .args(["-C", &record.workspace_root, "rev-parse", "HEAD"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("cannot verify durable cell base OID");
        }
        let observed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if observed != record.base_oid {
            anyhow::bail!("durable cell base OID drift");
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<usize> {
        let mut count = 0usize;
        for entry in fs::read_dir(self.root.join("runs"))? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let record: CellRunRecord = serde_json::from_slice(&fs::read(&path)?)?;
            if record.schema_version != RUN_RECORD_SCHEMA {
                anyhow::bail!("unsupported durable cell record schema: {}", path.display());
            }
            checked_id(&record.provider_run_id)?;
            self.verify_binding(&record)?;
            count = count.saturating_add(1);
        }
        Ok(count)
    }

    pub fn mark_active(&self, provider_run_id: &str, pid: u32) -> anyhow::Result<CellRunRecord> {
        self.mutate(provider_run_id, "spawned", |record| {
            if !matches!(record.state, CellState::Ready | CellState::Recoverable) {
                anyhow::bail!("cell run cannot spawn from state {:?}", record.state);
            }
            record.state = CellState::Active;
            record.pid = Some(pid);
            record.pgid = Some(pid as i32);
            Ok(serde_json::json!({"pid": pid, "pgid": pid}))
        })
    }

    /// Commit a settled receipt only while the run is still active. This is the
    /// serialization point shared with cancellation: once cancel changes the
    /// state away from `active`, a late waiter can only observe that truth.
    pub fn settle_if_active(
        &self,
        provider_run_id: &str,
        receipt: ProviderReceipt,
        error_class: Option<String>,
        exit_code: i32,
    ) -> anyhow::Result<(CellRunRecord, bool)> {
        let run_id = checked_id(provider_run_id)?;
        let _lock = RunLock::acquire(self.lock_path(run_id))?;
        let mut record = self
            .read_unlocked(run_id)?
            .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
        if record.state == CellState::Blocked
            && record.error_class.as_deref() == Some("shuttle_cell_cancellation_requested")
        {
            record.pid = None;
            record.pgid = None;
            record.state = CellState::Cancelled;
            record.receipt = Some(crate::provider_receipt::cancelled_receipt(run_id));
            record.error_class = Some("shuttle_cell_cancelled".into());
            record.revision = record.revision.saturating_add(1);
            record.updated_at_ms = now_ms();
            self.write_unlocked(&record)?;
            self.append_event_unlocked(
                &record,
                "cancelled_by_supervisor",
                serde_json::json!({"exit_code": exit_code}),
            )?;
            return Ok((record, false));
        }
        if record.state != CellState::Active {
            return Ok((record, false));
        }
        record.pid = None;
        record.pgid = None;
        record.state = match receipt.provider_status {
            ProviderStatus::Completed => CellState::Done,
            ProviderStatus::Cancelled => CellState::Cancelled,
            _ => CellState::Failed,
        };
        record.result_ref = receipt.result_ref.clone();
        record.receipt = Some(receipt);
        record.error_class = error_class;
        record.revision = record.revision.saturating_add(1);
        record.updated_at_ms = now_ms();
        self.write_unlocked(&record)?;
        self.append_event_unlocked(
            &record,
            "settled",
            serde_json::json!({"exit_code": exit_code}),
        )?;
        Ok((record, true))
    }

    /// Finalize a supervisor failure while the attempt is still non-terminal.
    /// A concurrent terminal transition remains authoritative.
    pub fn fail_if_nonterminal(
        &self,
        provider_run_id: &str,
        receipt: ProviderReceipt,
        error_class: String,
        exit_code: i32,
    ) -> anyhow::Result<(CellRunRecord, bool)> {
        let run_id = checked_id(provider_run_id)?;
        let _lock = RunLock::acquire(self.lock_path(run_id))?;
        let mut record = self
            .read_unlocked(run_id)?
            .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
        if record.state.terminal() {
            return Ok((record, false));
        }
        record.state = CellState::Failed;
        record.pid = None;
        record.pgid = None;
        record.receipt = Some(receipt);
        record.error_class = Some(error_class);
        record.revision = record.revision.saturating_add(1);
        record.updated_at_ms = now_ms();
        self.write_unlocked(&record)?;
        self.append_event_unlocked(
            &record,
            "launch_failed",
            serde_json::json!({"exit_code": exit_code}),
        )?;
        Ok((record, true))
    }

    fn read_unlocked(&self, run_id: &str) -> anyhow::Result<Option<CellRunRecord>> {
        let path = self.record_path(run_id);
        if !path.is_file() {
            return Ok(None);
        }
        let record: CellRunRecord = serde_json::from_slice(&fs::read(&path)?)?;
        if record.schema_version != RUN_RECORD_SCHEMA || record.provider_run_id != run_id {
            anyhow::bail!("durable cell record identity mismatch");
        }
        Ok(Some(record))
    }

    fn write_unlocked(&self, record: &CellRunRecord) -> anyhow::Result<()> {
        let body = serde_json::to_vec_pretty(record)?;
        atomic_write(&self.record_path(&record.provider_run_id), &body)
    }

    fn append_event_unlocked(
        &self,
        record: &CellRunRecord,
        kind: &str,
        detail: serde_json::Value,
    ) -> anyhow::Result<()> {
        let event = serde_json::json!({
            "schema_version": RUN_EVENT_SCHEMA,
            "provider_run_id": record.provider_run_id,
            "revision": record.revision,
            "state": record.state,
            "kind": kind,
            "observed_at_ms": record.updated_at_ms,
            "detail": detail,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.event_path(&record.provider_run_id))?;
        writeln!(file, "{}", serde_json::to_string(&event)?)?;
        file.sync_all()?;
        Ok(())
    }

    fn record_path(&self, run_id: &str) -> PathBuf {
        self.root
            .join("runs")
            .join(format!("{}.json", file_id(run_id)))
    }
    fn event_path(&self, run_id: &str) -> PathBuf {
        self.root
            .join("events")
            .join(format!("{}.jsonl", file_id(run_id)))
    }
    fn binding_path(&self, run_id: &str) -> PathBuf {
        self.root
            .join("bindings")
            .join(format!("{}.json", file_id(run_id)))
    }
    pub fn usage_path(&self, run_id: &str) -> PathBuf {
        self.root
            .join("usage")
            .join(format!("{}.usage.json", file_id(run_id)))
    }
    fn lock_path(&self, run_id: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("{}.lock", file_id(run_id)))
    }

    pub fn session_dir(&self, run_id: &str) -> PathBuf {
        self.root.join("sessions").join(file_id(run_id))
    }

    /// Finalize exactly one immutable usage receipt after the run reaches a
    /// terminal state. Replays with identical bytes are idempotent; divergent
    /// replays fail closed.
    pub fn finalize_usage(
        &self,
        provider_run_id: &str,
        telemetry: Option<&crate::run::RunTelemetry>,
    ) -> anyhow::Result<CellRunRecord> {
        let run_id = checked_id(provider_run_id)?;
        let _lock = RunLock::acquire(self.lock_path(run_id))?;
        let mut record = self
            .read_unlocked(run_id)?
            .ok_or_else(|| anyhow::anyhow!("cell run not found: {run_id}"))?;
        if !record.state.terminal() {
            anyhow::bail!("usage receipt is unavailable before terminal state");
        }
        let usage = crate::usage_receipt::EaUsageReceipt::from_terminal(&record, telemetry);
        let path = self.usage_path(run_id);
        let digest = if path.is_file() && record.usage_digest.is_none() {
            let body = fs::read(&path)?;
            let existing: serde_json::Value = serde_json::from_slice(&body)?;
            if existing
                .get("schema_version")
                .and_then(serde_json::Value::as_str)
                != Some(crate::usage_receipt::USAGE_RECEIPT_SCHEMA)
                || existing
                    .get("provider_run_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(record.provider_run_id.as_str())
                || existing.get("attempt_id")
                    != Some(
                        &record
                            .attempt_id
                            .clone()
                            .map_or(serde_json::Value::Null, serde_json::Value::String),
                    )
            {
                anyhow::bail!("orphaned usage receipt coordinate drift");
            }
            sha256::sha256_hex_prefixed(&body)
        } else {
            crate::usage_receipt::write_immutable(&path, &usage)?
        };
        let usage_ref = format!("file://{}", path.display());
        if let Some(existing) = &record.usage_digest {
            if existing != &digest || record.usage_ref.as_deref() != Some(&usage_ref) {
                anyhow::bail!("durable usage receipt binding drift");
            }
            return Ok(record);
        }
        record.usage_ref = Some(usage_ref.clone());
        record.usage_digest = Some(digest.clone());
        record.revision = record.revision.saturating_add(1);
        record.updated_at_ms = now_ms();
        self.write_unlocked(&record)?;
        self.append_event_unlocked(
            &record,
            "usage_finalized",
            serde_json::json!({"usage_ref": usage_ref, "usage_digest": digest}),
        )?;
        Ok(record)
    }
}

fn env_coordinate(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            !value.is_empty() && value.len() <= 1024 && !value.contains(['\n', '\r', '\0'])
        })
}

fn reject_lineage_drift(
    name: &str,
    durable: Option<&str>,
    requested: Option<&str>,
) -> anyhow::Result<()> {
    if requested.is_some() && requested != durable {
        anyhow::bail!("{name} conflicts with durable cell lineage");
    }
    Ok(())
}

pub fn provider_receipt(record: &CellRunRecord) -> ProviderReceipt {
    if let Some(receipt) = &record.receipt {
        return receipt.clone();
    }
    let (status, hint) = match record.state {
        CellState::Ready => (ProviderStatus::Accepted, ReconciliationHint::Reconnect),
        CellState::Active => (ProviderStatus::Running, ReconciliationHint::Reconnect),
        CellState::Blocked | CellState::Recoverable => {
            (ProviderStatus::Unknown, ReconciliationHint::QueryProvider)
        }
        CellState::Done => (ProviderStatus::Completed, ReconciliationHint::QueryProvider),
        CellState::Failed => (ProviderStatus::Failed, ReconciliationHint::Restart),
        CellState::Cancelled => (ProviderStatus::Cancelled, ReconciliationHint::Block),
    };
    ProviderReceipt {
        provider_status: status,
        reconciliation_hint: hint,
        result_ref: record.result_ref.clone(),
        provider_run_id: Some(record.provider_run_id.clone()),
        terminal: record.state.terminal(),
        extra: Default::default(),
    }
}

pub fn process_alive(pgid: Option<i32>) -> bool {
    let Some(pgid) = pgid.filter(|value| *value > 0) else {
        return false;
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(-pgid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    false
}

pub fn terminate_process_group(pgid: i32, grace_ms: u64) -> anyhow::Result<()> {
    if pgid <= 0 {
        anyhow::bail!("invalid process group id");
    }
    #[cfg(unix)]
    {
        let descendants = descendant_pids(pgid)?;
        let group_signal = signal_group(pgid, libc::SIGTERM)?;
        let mut group_signal_available = group_signal == GroupSignalOutcome::Delivered;
        signal_pids(&descendants, libc::SIGTERM);
        if group_signal == GroupSignalOutcome::PermissionDenied {
            signal_pids(&[pgid], libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
        while process_scope_alive(pgid, &descendants, group_signal_available)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if process_scope_alive(pgid, &descendants, group_signal_available) {
            if group_signal_available {
                let outcome = signal_group(pgid, libc::SIGKILL)?;
                group_signal_available = outcome == GroupSignalOutcome::Delivered;
                if outcome == GroupSignalOutcome::PermissionDenied {
                    signal_pids(&[pgid], libc::SIGKILL);
                }
            }
            if group_signal == GroupSignalOutcome::PermissionDenied {
                signal_pids(&[pgid], libc::SIGKILL);
            }
        }
        signal_pids(&descendants, libc::SIGKILL);
        let confirm_deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
        while process_scope_alive(pgid, &descendants, group_signal_available)
            && std::time::Instant::now() < confirm_deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if process_scope_alive(pgid, &descendants, group_signal_available) {
            anyhow::bail!("process group {pgid} or a descendant is still alive after SIGKILL");
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("process-group cancellation is unsupported on this platform");
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupSignalOutcome {
    Delivered,
    Missing,
    PermissionDenied,
}

#[cfg(unix)]
fn signal_group(pgid: i32, signal: i32) -> anyhow::Result<GroupSignalOutcome> {
    unsafe {
        if libc::kill(-pgid, signal) == 0 {
            return Ok(GroupSignalOutcome::Delivered);
        }
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(GroupSignalOutcome::Missing),
        Some(libc::EPERM) => Ok(GroupSignalOutcome::PermissionDenied),
        _ => Err(error.into()),
    }
}

#[cfg(unix)]
fn process_scope_alive(pgid: i32, descendants: &[i32], group_signal_available: bool) -> bool {
    if group_signal_available {
        process_alive(Some(pgid))
    } else {
        pid_alive(pgid) || descendants.iter().any(|pid| pid_alive(*pid))
    }
}

#[cfg(unix)]
fn descendant_pids(root_pid: i32) -> anyhow::Result<Vec<i32>> {
    let ps = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Ps)?;
    let output = match std::process::Command::new(ps)
        .args(["-axo", "pid=,ppid="])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            #[cfg(target_os = "macos")]
            {
                if error.raw_os_error() == Some(libc::EPERM) {
                    return Ok(macos_descendant_pids(root_pid));
                }
            }
            return Err(error.into());
        }
    };
    if !output.status.success() {
        #[cfg(target_os = "macos")]
        {
            return Ok(macos_descendant_pids(root_pid));
        }
        #[cfg(not(target_os = "macos"))]
        anyhow::bail!("cannot enumerate descendant processes");
    }
    let mut pairs = Vec::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let (Ok(pid), Ok(ppid)) = (pid.parse::<i32>(), ppid.parse::<i32>()) {
            pairs.push((pid, ppid));
        }
    }
    let mut result = Vec::new();
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &pairs {
            if *ppid == parent && !result.contains(pid) {
                result.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn macos_descendant_pids(root_pid: i32) -> Vec<i32> {
    let mut result = Vec::new();
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        // libproc returns a byte count and permits a null buffer size probe.
        let needed = unsafe { libc::proc_listchildpids(parent, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            continue;
        }
        let slots = needed as usize / size_of::<libc::pid_t>();
        let mut children = vec![0 as libc::pid_t; slots.saturating_add(16)];
        let bytes = unsafe {
            libc::proc_listchildpids(
                parent,
                children.as_mut_ptr().cast(),
                (children.len() * size_of::<libc::pid_t>()) as libc::c_int,
            )
        };
        if bytes <= 0 {
            continue;
        }
        children.truncate(bytes as usize / size_of::<libc::pid_t>());
        for child in children.into_iter().filter(|pid| *pid > 0) {
            if !result.contains(&child) {
                result.push(child);
                frontier.push(child);
            }
        }
    }
    result
}

#[cfg(unix)]
fn signal_pids(pids: &[i32], signal: i32) {
    for pid in pids.iter().rev() {
        unsafe {
            libc::kill(*pid, signal);
        }
    }
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn doctor(registry: &CellRegistry) -> DoctorReport {
    let pi = std::env::var("PI_BIN").unwrap_or_else(|_| "pi".into());
    let pi_version = command_version(&pi, &["--version"]);
    let doctor_pi_dir = registry.root().join("doctor-pi");
    let _ = fs::create_dir_all(&doctor_pi_dir);
    let pi_help = command_output(&pi, &["--help"], false, Some(&doctor_pi_dir));
    let cu = std::env::var("CONTAINER_USE_BIN").unwrap_or_else(|_| "container-use".into());
    let cu_version = command_version(&cu, &["version"]);
    let credential_env_present = [
        "SHUTTLE_PROVIDER_KEY",
        "SHUTTLE_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some());
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().ok().map(|path| path.join(".pi/agent")));
    let auth_ref_present = agent_dir
        .as_ref()
        .map(|path| path.join("auth.json"))
        .and_then(|path| fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.len() > 2);
    let models_ref_present = agent_dir
        .as_ref()
        .map(|path| path.join("models.json"))
        .is_some_and(|path| models_has_api_key_reference(&path));
    let credential_source_kind = if credential_env_present {
        "environment"
    } else if auth_ref_present {
        "auth_store"
    } else if models_ref_present {
        "models_config"
    } else {
        "absent"
    };
    let credential_ref_present = credential_source_kind != "absent";
    let writable = probe_writable(registry.root());
    let mut issues = Vec::new();
    if !writable {
        issues.push("registry_not_writable".into());
    }
    if registry.validate().is_err() {
        issues.push("registry_integrity_failed".into());
    }
    if pi_version.is_none() {
        issues.push("pi_unavailable".into());
    }
    let session_supported = pi_help
        .as_deref()
        .is_some_and(|help| help.contains("--session-id") && help.contains("--session-dir"));
    if !session_supported {
        issues.push("pi_durable_session_unsupported".into());
    }
    if !credential_ref_present {
        issues.push("provider_credential_ref_absent".into());
    }
    if cu_version.is_none() {
        issues.push("container_use_unavailable".into());
    }
    DoctorReport {
        schema_version: "ea_cell_doctor/v1",
        status: if issues.is_empty() {
            "ready"
        } else {
            "blocked"
        },
        registry_root: registry.root().display().to_string(),
        writable,
        pi_binary: pi,
        pi_available: pi_version.is_some(),
        pi_version,
        pi_session_supported: session_supported,
        provider_credential_ref_present: credential_ref_present,
        credential_source_kind,
        container_use_binary: cu,
        container_use_available: cu_version.is_some(),
        container_use_version: cu_version,
        issues,
    }
}

fn models_has_api_key_reference(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    root.get("providers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|providers| {
            providers.values().any(|provider| {
                provider
                    .get("apiKey")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
            })
        })
}

fn command_version(bin: &str, args: &[&str]) -> Option<String> {
    command_output(bin, args, true, None)
}

fn command_output(
    bin: &str,
    args: &[&str],
    require_success: bool,
    pi_agent_dir: Option<&Path>,
) -> Option<String> {
    let mut command = std::process::Command::new(bin);
    command.args(args);
    if let Some(path) = pi_agent_dir {
        command.env("PI_CODING_AGENT_DIR", path);
    }
    let output = command.output().ok()?;
    if require_success && !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stderr}\n{stdout}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => return None,
    };
    Some(combined)
}

fn probe_writable(root: &Path) -> bool {
    let path = root.join(format!(".doctor-{}", std::process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let ok = file.write_all(b"ok").is_ok() && file.sync_all().is_ok();
            let _ = fs::remove_file(path);
            ok
        }
        Err(_) => false,
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is unavailable; set SHUTTLE_CELL_STATE_DIR"))
}

fn checked_id(value: &str) -> anyhow::Result<&str> {
    if value.trim().is_empty() || value.len() > 1024 || value.contains(['\n', '\r', '\0']) {
        anyhow::bail!("provider_run_id must be non-empty and contain no control separators");
    }
    Ok(value)
}

fn file_id(value: &str) -> String {
    sha256::sha256_hex_prefixed(value.as_bytes())
        .trim_start_matches("sha256:")
        .to_string()
}

pub fn pi_session_id(provider_run_id: &str) -> String {
    let hex = file_id(provider_run_id);
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        now_ms()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    FileSync::sync_dir(parent)?;
    Ok(())
}

struct FileSync;
impl FileSync {
    fn sync_dir(path: &Path) -> anyhow::Result<()> {
        OpenOptions::new().read(true).open(path)?.sync_all()?;
        Ok(())
    }
}

struct RunLock {
    file: fs::File,
}
impl RunLock {
    fn acquire(path: PathBuf) -> anyhow::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        #[cfg(unix)]
        unsafe {
            if libc::flock(file.as_raw_fd(), libc::LOCK_EX) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self { file })
    }
}
impl Drop for RunLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
#[cfg(unix)]
use std::os::fd::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shuttle-cell-registry-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn binding(root: &Path) -> CellBinding {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "workspace_root": root,
            "allowed_paths": ["src/**"],
            "allowed_commands": [],
            "base_oid": "oid-1",
            "assignment_digest": "sha256:abc",
            "environment_id": "env-1",
            "provider_run_id": "provider/run:1",
            "attempt_id": "attempt-1"
        }))
        .unwrap()
    }

    #[test]
    fn registry_is_idempotent_and_rejects_coordinate_drift() {
        let root = scratch("idem");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let binding = binding(&root);
        let json = serde_json::to_string(&binding).unwrap();
        let first = registry
            .initialize("provider/run:1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        let replay = registry
            .initialize("provider/run:1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        assert_eq!(first.revision, replay.revision);
        let mut drifted = binding.clone();
        drifted.base_oid = "oid-2".into();
        let error = registry
            .initialize(
                "provider/run:1",
                Some("attempt-1".into()),
                &drifted,
                &serde_json::to_string(&drifted).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("different run coordinates"));
        assert!(registry.event_path("provider/run:1").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_record_preserves_runtime_descriptor_coordinates() {
        let root = scratch("runtime_descriptor");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let mut binding = binding(&root);
        binding.runtime_descriptor_ref = Some("artifact://runtime/one".into());
        binding.runtime_descriptor_digest = Some(
            shuttle_protocol::anchor::Sha256Digest::parse(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
        );
        binding.runtime_descriptor = Some(shuttle_protocol::EaRuntimeDescriptor {
            schema_version: shuttle_protocol::SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA.into(),
            mode: shuttle_protocol::RuntimeMode::HostBundle,
            core: shuttle_protocol::RuntimeCore {
                version: "1".into(),
                runtime_abi: "shuttle-runtime/v1".into(),
                executable_path: "/bin/shuttle".into(),
                digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                    "sha256:{}",
                    "b".repeat(64)
                ))
                .unwrap(),
            },
            tools: shuttle_protocol::RuntimeTools {
                source: "file:///tools".into(),
                distribution_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                    "sha256:{}",
                    "c".repeat(64)
                ))
                .unwrap(),
                manifest_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                    "sha256:{}",
                    "d".repeat(64)
                ))
                .unwrap(),
                root: "/opt/shuttle-tools".into(),
                profiles: vec!["source-analysis".into()],
            },
            provider: shuttle_protocol::ProviderRuntime {
                kind: "pi".into(),
                version: "1".into(),
                binary: "/bin/pi".into(),
                binary_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                    "sha256:{}",
                    "f".repeat(64)
                ))
                .unwrap(),
                extension_path: "/opt/shuttle-provider/shuttle-tools.ts".into(),
                extension_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                    "sha256:{}",
                    "e".repeat(64)
                ))
                .unwrap(),
            },
            composed_image_digest: None,
            mount_receipt: None,
        });
        let digest = binding
            .runtime_descriptor
            .as_ref()
            .unwrap()
            .digest()
            .unwrap();
        binding.runtime_descriptor_digest = Some(digest.clone());
        let json = serde_json::to_string(&binding).unwrap();
        let record = registry
            .initialize("runtime-one", None, &binding, &json)
            .unwrap();
        assert_eq!(
            record.runtime_descriptor_ref.as_deref(),
            Some("artifact://runtime/one")
        );
        assert_eq!(
            record.runtime_descriptor_digest.as_deref(),
            Some(digest.as_str())
        );
        registry.verify_binding(&record).unwrap();

        let mut ref_drift = record.clone();
        ref_drift.runtime_descriptor_ref = Some("artifact://runtime/two".into());
        assert!(registry
            .verify_binding(&ref_drift)
            .unwrap_err()
            .to_string()
            .contains("coordinate drift"));

        let mut digest_drift = record;
        digest_drift.runtime_descriptor_digest = Some(format!("sha256:{}", "f".repeat(64)));
        assert!(registry
            .verify_binding(&digest_drift)
            .unwrap_err()
            .to_string()
            .contains("coordinate drift"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binding_digest_drift_blocks_recovery() {
        let root = scratch("drift");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let binding = binding(&root);
        let json = serde_json::to_string(&binding).unwrap();
        let record = registry
            .initialize("run-1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        fs::write(&record.binding_ref, "{}").unwrap();
        let error = registry.verify_binding(&record).unwrap_err();
        assert!(error.to_string().contains("digest drift"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn record_contains_references_not_credentials() {
        let root = scratch("secret");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let binding = binding(&root);
        let json = serde_json::to_string(&binding).unwrap();
        registry
            .initialize("run-1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        let serialized = serde_json::to_string(&registry.read("run-1").unwrap()).unwrap();
        assert!(!serialized.contains("api_key"));
        assert!(!serialized.contains("credential"));
        assert!(serialized.contains("session_ref"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pi_session_id_is_stable_uuid_shape() {
        let id = pi_session_id("provider/run:1");
        assert_eq!(id, pi_session_id("provider/run:1"));
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
    }

    #[test]
    fn cancelled_state_wins_over_late_settlement() {
        let root = scratch("settle-cancel");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let binding = binding(&root);
        let json = serde_json::to_string(&binding).unwrap();
        registry
            .initialize("run-1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        registry.mark_active("run-1", 123).unwrap();
        registry
            .mutate("run-1", "cancelled", |record| {
                record.state = CellState::Cancelled;
                record.pid = None;
                record.pgid = None;
                record.receipt = Some(crate::provider_receipt::cancelled_receipt("run-1"));
                Ok(serde_json::json!({}))
            })
            .unwrap();
        let late = crate::provider_receipt::build_receipt(
            &crate::run::RunTelemetry {
                termination: crate::run::RunTermination::Settled,
                limit_reason: None,
                wall_ms: 1,
                tokens: Some(1),
                usage: crate::run::UsageBreakdown {
                    total_tokens: Some(1),
                    ..Default::default()
                },
                provider_cost: None,
                provider: None,
                model: None,
                event_count: 1,
                stdout_bytes: 1,
                tool_calls: 1,
                accept_rate: Some(1.0),
                apply_total: 1,
                apply_ok: 1,
                exit_code: 0,
                ttft_ms: Some(1),
                resources: None,
            },
            "run-1",
            Some("file:///late".into()),
        );
        let (record, committed) = registry.settle_if_active("run-1", late, None, 0).unwrap();
        assert!(!committed);
        assert_eq!(record.state, CellState::Cancelled);
        assert_eq!(
            provider_receipt(&record).provider_status,
            ProviderStatus::Cancelled
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recoverable_run_reenters_active_before_settlement() {
        let root = scratch("recover-transition");
        let registry = CellRegistry::open(root.join("state")).unwrap();
        let binding = binding(&root);
        let json = serde_json::to_string(&binding).unwrap();
        registry
            .initialize("run-1", Some("attempt-1".into()), &binding, &json)
            .unwrap();
        registry
            .mutate("run-1", "process_missing", |record| {
                record.state = CellState::Recoverable;
                Ok(serde_json::json!({}))
            })
            .unwrap();
        assert_eq!(
            registry.mark_active("run-1", 456).unwrap().state,
            CellState::Active
        );
        let receipt = crate::provider_receipt::build_receipt(
            &crate::run::RunTelemetry {
                termination: crate::run::RunTermination::Settled,
                limit_reason: None,
                wall_ms: 1,
                tokens: Some(1),
                usage: crate::run::UsageBreakdown {
                    total_tokens: Some(1),
                    ..Default::default()
                },
                provider_cost: None,
                provider: None,
                model: None,
                event_count: 1,
                stdout_bytes: 1,
                tool_calls: 1,
                accept_rate: Some(1.0),
                apply_total: 1,
                apply_ok: 1,
                exit_code: 0,
                ttft_ms: Some(1),
                resources: None,
            },
            "run-1",
            Some("file:///result".into()),
        );
        let (record, committed) = registry
            .settle_if_active("run-1", receipt, None, 0)
            .unwrap();
        assert!(committed);
        assert_eq!(record.state, CellState::Done);
        fs::remove_dir_all(root).unwrap();
    }
}
