use crate::{
    safe_id, AcceptanceContract, ActionProjection, ActionStatus, ArtifactRef, ArtifactStore,
    AttachmentProjection, AttachmentRole, AttachmentState, ConditionEvaluation, ConditionStatus,
    EvaluateConditionRequest, EvidenceProgress, GoalActionBundle, GoalActionResult, GoalBackend,
    GoalError, GoalHealth, GoalMigrationBundle, GoalProjection, GoalRef, GoalStatus,
    MigrationArtifact, MigrationCondition, ProviderCapabilities, ATTACHMENT_SCHEMA,
    CAPABILITY_SCHEMA, CONDITION_SCHEMA, MIGRATION_BUNDLE_SCHEMA, PROJECTION_SCHEMA,
};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct StorePaths {
    pub root: PathBuf,
    pub database: PathBuf,
}

impl StorePaths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let database = root.join("goal.sqlite3");
        Self { root, database }
    }

    pub fn platform_default(store_id: &str) -> Result<Self, GoalError> {
        if let Ok(override_root) = std::env::var("SHUTTLE_GOAL_STATE_DIR") {
            if !override_root.trim().is_empty() {
                return Ok(Self::new(PathBuf::from(override_root).join(store_id)));
            }
        }
        #[cfg(target_os = "macos")]
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join("Library/Application Support"));
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|p| p.join(".local/state"))
            });
        let base = base.ok_or_else(|| {
            GoalError::InvalidInput(
                "cannot resolve platform state directory; set SHUTTLE_GOAL_STATE_DIR".into(),
            )
        })?;
        Ok(Self::new(base.join("shuttle/goals").join(store_id)))
    }
}

pub struct LocalGoalStore {
    conn: Connection,
    paths: StorePaths,
    instance_id: String,
    artifacts: Option<ArtifactStore>,
}

impl LocalGoalStore {
    pub fn open(paths: StorePaths, instance_id: impl Into<String>) -> Result<Self, GoalError> {
        let instance_id = instance_id.into();
        if !safe_id(&instance_id) {
            return Err(GoalError::InvalidInput(
                "store id must contain only ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        std::fs::create_dir_all(&paths.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&paths.root, std::fs::Permissions::from_mode(0o700))?;
        }
        let canonical_root = std::fs::canonicalize(&paths.root)?;
        let paths = StorePaths::new(canonical_root);
        let is_new = !paths.database.exists();
        if paths.database.exists()
            && std::fs::symlink_metadata(&paths.database)?
                .file_type()
                .is_symlink()
        {
            return Err(GoalError::Integrity(format!(
                "database is a symlink: {}",
                paths.database.display()
            )));
        }
        let conn = Connection::open(&paths.database)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let artifacts = ArtifactStore::new(&paths.root)?;
        let mut s = Self {
            conn,
            paths,
            instance_id,
            artifacts: Some(artifacts),
        };
        if is_new {
            s.conn.pragma_update(None, "journal_mode", "WAL")?;
            s.migrate()?;
            s.integrity_check()?;
        } else {
            s.validate_schema()?;
        }
        Ok(s)
    }

    pub fn open_default(store_id: &str) -> Result<Self, GoalError> {
        Self::open(StorePaths::platform_default(store_id)?, store_id)
    }

    pub fn open_readonly(
        paths: StorePaths,
        instance_id: impl Into<String>,
    ) -> Result<Self, GoalError> {
        let instance_id = instance_id.into();
        if !safe_id(&instance_id) {
            return Err(GoalError::InvalidInput(
                "store id must contain only ASCII letters, digits, '.', '_' or '-'".into(),
            ));
        }
        let canonical_root = std::fs::canonicalize(&paths.root)?;
        let paths = StorePaths::new(canonical_root);
        let meta = std::fs::symlink_metadata(&paths.database)?;
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return Err(GoalError::Integrity(format!(
                "database is not a real file: {}",
                paths.database.display()
            )));
        }
        let conn = Connection::open_with_flags(&paths.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_millis(250))?;
        let store = Self {
            conn,
            paths,
            instance_id,
            artifacts: None,
        };
        store.validate_schema()?;
        Ok(store)
    }

    pub fn open_default_readonly(store_id: &str) -> Result<Self, GoalError> {
        Self::open_readonly(StorePaths::platform_default(store_id)?, store_id)
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }
    pub fn artifacts(&self) -> Result<&ArtifactStore, GoalError> {
        self.artifacts.as_ref().ok_or_else(|| {
            GoalError::InvalidInput("artifact access requires a writable Goal store".into())
        })
    }

    fn migrate(&mut self) -> Result<(), GoalError> {
        self.conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS goals(
               goal_id TEXT PRIMARY KEY, revision INTEGER NOT NULL, repository_identity TEXT NOT NULL,
               objective_ref TEXT NOT NULL, objective_digest TEXT NOT NULL, acceptance_ref TEXT NOT NULL,
               acceptance_digest TEXT NOT NULL, acceptance_basis_revision INTEGER NOT NULL, status TEXT NOT NULL, health TEXT NOT NULL,
               required_conditions INTEGER NOT NULL DEFAULT 0, satisfied_conditions INTEGER NOT NULL DEFAULT 0,
               unsatisfied_conditions INTEGER NOT NULL DEFAULT 0, unknown_conditions INTEGER NOT NULL DEFAULT 0,
               pending_effects INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS goals_repository ON goals(repository_identity, updated_at_ms DESC);
             CREATE TABLE IF NOT EXISTS events(
               seq INTEGER PRIMARY KEY AUTOINCREMENT, goal_id TEXT NOT NULL REFERENCES goals(goal_id),
               goal_revision INTEGER NOT NULL, event_kind TEXT NOT NULL, idempotency_key TEXT NOT NULL UNIQUE,
               payload_json TEXT NOT NULL, occurred_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS attachments(
               attachment_id TEXT PRIMARY KEY, goal_id TEXT NOT NULL REFERENCES goals(goal_id),
               revision INTEGER NOT NULL, role TEXT NOT NULL, driver_id TEXT NOT NULL, generation INTEGER NOT NULL,
               fencing_epoch INTEGER NOT NULL, lease_expires_at_ms INTEGER NOT NULL, state TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS one_active_controller ON attachments(goal_id)
               WHERE role='controller' AND state='active';
             CREATE TABLE IF NOT EXISTS conditions(
               goal_id TEXT NOT NULL REFERENCES goals(goal_id), gate_id TEXT NOT NULL, status TEXT NOT NULL,
               evidence_ref TEXT NOT NULL, evidence_digest TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
               basis_goal_revision INTEGER NOT NULL, PRIMARY KEY(goal_id, gate_id)
             );
             CREATE TABLE IF NOT EXISTS actions(
               action_id TEXT PRIMARY KEY, goal_id TEXT NOT NULL REFERENCES goals(goal_id),
               basis_goal_revision INTEGER NOT NULL, committed_goal_revision INTEGER NOT NULL,
               attachment_id TEXT NOT NULL, attachment_revision INTEGER NOT NULL, fencing_epoch INTEGER NOT NULL,
               action_kind TEXT NOT NULL, idempotency_key TEXT NOT NULL UNIQUE, bundle_ref TEXT NOT NULL,
               bundle_digest TEXT NOT NULL, status TEXT NOT NULL, result_ref TEXT, result_digest TEXT,
               exit_code INTEGER, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             PRAGMA user_version=1; COMMIT;"
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(key,value) VALUES('store_id',?1)",
            [&self.instance_id],
        )?;
        let actual: String =
            self.conn
                .query_row("SELECT value FROM meta WHERE key='store_id'", [], |r| {
                    r.get(0)
                })?;
        if actual != self.instance_id {
            return Err(GoalError::Integrity(format!(
                "store identity mismatch: expected {}, found {actual}",
                self.instance_id
            )));
        }
        Ok(())
    }

    fn validate_schema(&self) -> Result<(), GoalError> {
        let version: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 1 {
            return Err(GoalError::Integrity(format!(
                "unsupported local Goal schema version: {version}"
            )));
        }
        let actual: String =
            self.conn
                .query_row("SELECT value FROM meta WHERE key='store_id'", [], |r| {
                    r.get(0)
                })?;
        if actual != self.instance_id {
            return Err(GoalError::Integrity(format!(
                "store identity mismatch: expected {}, found {actual}",
                self.instance_id
            )));
        }
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<(), GoalError> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if result != "ok" {
            return Err(GoalError::Integrity(result));
        }
        Ok(())
    }

    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            schema_version: CAPABILITY_SCHEMA.into(),
            backend: GoalBackend::Local,
            backend_instance_id: self.instance_id.clone(),
            read_status: true,
            attach_observer: true,
            attach_controller: true,
            cross_session_reconnect: true,
            provider_cas: true,
            conditional_update: true,
            action_effect_lookup: true,
            bounded_continuation: true,
            graph_advice: false,
            independent_verifier: true,
        }
    }

    pub fn start_goal(
        &mut self,
        repository_identity: &str,
        objective: &str,
        acceptance: &[u8],
        driver_id: &str,
        lease_ttl_ms: i64,
    ) -> Result<(GoalProjection, AttachmentProjection), GoalError> {
        require_nonempty("repository_identity", repository_identity)?;
        require_nonempty("objective", objective)?;
        require_nonempty("driver_id", driver_id)?;
        let acceptance_contract = AcceptanceContract::parse(acceptance)?;
        if lease_ttl_ms <= 0 {
            return Err(GoalError::InvalidInput(
                "lease_ttl_ms must be positive".into(),
            ));
        }
        let now = now_ms();
        let goal_id = new_id("g");
        let attachment_id = new_id("att");
        let objective_artifact = self.artifacts()?.put(objective.as_bytes())?;
        let acceptance_artifact = self.artifacts()?.put(acceptance)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO goals(goal_id,revision,repository_identity,objective_ref,objective_digest,acceptance_ref,acceptance_digest,acceptance_basis_revision,status,health,required_conditions,unknown_conditions,created_at_ms,updated_at_ms) VALUES(?1,1,?2,?3,?4,?5,?6,1,'active','healthy',?7,?7,?8,?8)", params![goal_id, repository_identity, objective_artifact.r#ref, objective_artifact.digest, acceptance_artifact.r#ref, acceptance_artifact.digest, acceptance_contract.required_gates.len() as u32, now])?;
        for gate in &acceptance_contract.required_gates {
            tx.execute("INSERT INTO conditions(goal_id,gate_id,status,evidence_ref,evidence_digest,observed_at_ms,basis_goal_revision) VALUES(?1,?2,'unknown','','',?3,1)", params![goal_id,gate,now])?;
        }
        tx.execute("INSERT INTO attachments(attachment_id,goal_id,revision,role,driver_id,generation,fencing_epoch,lease_expires_at_ms,state,created_at_ms,updated_at_ms) VALUES(?1,?2,1,'controller',?3,1,1,?4,'active',?5,?5)", params![attachment_id, goal_id, driver_id, now.saturating_add(lease_ttl_ms), now])?;
        append_event(
            &tx,
            &goal_id,
            1,
            "goal_started",
            &format!("start:{goal_id}"),
            &json!({"repository_identity":repository_identity,"attachment_id":attachment_id}),
            now,
        )?;
        tx.commit()?;
        Ok((
            self.get_goal_by_id(&goal_id)?,
            self.get_attachment(&attachment_id)?,
        ))
    }

    pub fn get(&self, goal_ref: &str) -> Result<GoalProjection, GoalError> {
        let parsed: GoalRef = goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        self.get_goal_by_id(&parsed.goal_id)
    }

    pub fn discover(&self, repository_identity: &str) -> Result<Vec<GoalProjection>, GoalError> {
        let mut stmt = self.conn.prepare("SELECT goal_id FROM goals WHERE repository_identity=?1 ORDER BY updated_at_ms DESC, goal_id")?;
        let ids = stmt
            .query_map([repository_identity], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| self.get_goal_by_id(id)).collect()
    }

    pub fn attach(
        &mut self,
        goal_ref: &str,
        driver_id: &str,
        role: AttachmentRole,
        expected_goal_revision: u64,
        lease_ttl_ms: i64,
    ) -> Result<AttachmentProjection, GoalError> {
        let parsed: GoalRef = goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = goal_revision(&tx, &parsed.goal_id)?;
        if current != expected_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_goal_revision,
                current,
            });
        }
        let lifecycle: String = tx.query_row(
            "SELECT status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |row| row.get(0),
        )?;
        if matches!(
            lifecycle.as_str(),
            "sealed" | "transferred" | "complete" | "cancelled"
        ) {
            return Err(GoalError::InvalidInput(format!(
                "Goal lifecycle does not accept attachments: {lifecycle}"
            )));
        }
        expire_controllers(&tx, &parsed.goal_id, now)?;
        if role == AttachmentRole::Controller {
            if let Some(holder) = active_controller(&tx, &parsed.goal_id, now)? {
                return Err(GoalError::ControllerHeld(holder));
            }
        }
        let next_generation: u64 = tx.query_row(
            "SELECT COALESCE(MAX(generation),0)+1 FROM attachments WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| r.get(0),
        )?;
        let next_fence: u64 = tx.query_row(
            "SELECT COALESCE(MAX(fencing_epoch),0)+1 FROM attachments WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| r.get(0),
        )?;
        let id = new_id("att");
        tx.execute("INSERT INTO attachments(attachment_id,goal_id,revision,role,driver_id,generation,fencing_epoch,lease_expires_at_ms,state,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?7,'active',?8,?8)", params![id, parsed.goal_id, role_db(role), driver_id, next_generation, next_fence, now.saturating_add(lease_ttl_ms), now])?;
        append_event(
            &tx,
            &parsed.goal_id,
            current,
            "attachment_acquired",
            &format!("attach:{id}"),
            &json!({"attachment_id":id,"role":role_db(role),"fencing_epoch":next_fence}),
            now,
        )?;
        tx.commit()?;
        self.get_attachment(&id)
    }

    pub fn renew(
        &mut self,
        attachment_id: &str,
        expected_attachment_revision: u64,
        fencing_epoch: u64,
        lease_ttl_ms: i64,
    ) -> Result<AttachmentProjection, GoalError> {
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, fence, state, lease_expires_at_ms): (u64, u64, String, i64) = tx
            .query_row(
                "SELECT revision,fencing_epoch,state,lease_expires_at_ms FROM attachments WHERE attachment_id=?1",
                [attachment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| GoalError::NotFound(attachment_id.into()))?;
        if current != expected_attachment_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_attachment_revision,
                current,
            });
        }
        if fence != fencing_epoch {
            return Err(GoalError::StaleFence {
                expected: fencing_epoch,
                current: fence,
            });
        }
        if state != "active" || lease_expires_at_ms <= now {
            return Err(GoalError::InvalidInput(format!(
                "attachment is not renewable: state={state}, lease_expires_at_ms={lease_expires_at_ms}"
            )));
        }
        tx.execute("UPDATE attachments SET revision=revision+1,lease_expires_at_ms=?2,updated_at_ms=?3 WHERE attachment_id=?1", params![attachment_id,now.saturating_add(lease_ttl_ms),now])?;
        tx.commit()?;
        self.get_attachment(attachment_id)
    }

    pub fn release(
        &mut self,
        attachment_id: &str,
        expected_attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<AttachmentProjection, GoalError> {
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (goal_id, current, fence): (String, u64, u64) = tx
            .query_row(
                "SELECT goal_id,revision,fencing_epoch FROM attachments WHERE attachment_id=?1",
                [attachment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| GoalError::NotFound(attachment_id.into()))?;
        if current != expected_attachment_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_attachment_revision,
                current,
            });
        }
        if fence != fencing_epoch {
            return Err(GoalError::StaleFence {
                expected: fencing_epoch,
                current: fence,
            });
        }
        tx.execute("UPDATE attachments SET revision=revision+1,state='released',updated_at_ms=?2 WHERE attachment_id=?1", params![attachment_id,now])?;
        let gr = goal_revision(&tx, &goal_id)?;
        append_event(
            &tx,
            &goal_id,
            gr,
            "attachment_released",
            &format!("release:{attachment_id}:{current}"),
            &json!({"attachment_id":attachment_id}),
            now,
        )?;
        tx.commit()?;
        self.get_attachment(attachment_id)
    }

    pub fn transition(
        &mut self,
        goal_ref: &str,
        target: GoalStatus,
        expected_goal_revision: u64,
        attachment_id: &str,
        expected_attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<GoalProjection, GoalError> {
        let parsed: GoalRef = goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = goal_revision(&tx, &parsed.goal_id)?;
        if current != expected_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_goal_revision,
                current,
            });
        }
        verify_controller(
            &tx,
            &parsed.goal_id,
            attachment_id,
            expected_attachment_revision,
            fencing_epoch,
            now,
        )?;
        let old: String = tx.query_row(
            "SELECT status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| r.get(0),
        )?;
        let old_status = parse_goal_status(&old)?;
        if !valid_transition(old_status, target) {
            return Err(GoalError::InvalidInput(format!(
                "invalid goal transition: {old:?} -> {}",
                goal_status_db(target)
            )));
        }
        if target == GoalStatus::Complete {
            let (required,satisfied,unsatisfied,unknown,pending):(u32,u32,u32,u32,u32)=tx.query_row("SELECT required_conditions,satisfied_conditions,unsatisfied_conditions,unknown_conditions,pending_effects FROM goals WHERE goal_id=?1", [&parsed.goal_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            if required == 0
                || satisfied != required
                || unsatisfied != 0
                || unknown != 0
                || pending != 0
            {
                return Err(GoalError::InvalidInput("completion requires all required conditions satisfied on the current basis and no pending effects".into()));
            }
        }
        let next = current + 1;
        tx.execute("UPDATE goals SET revision=?2,status=?3,updated_at_ms=?4 WHERE goal_id=?1 AND revision=?5", params![parsed.goal_id,next,goal_status_db(target),now,current])?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "goal_transitioned",
            &format!("transition:{}:{current}:{next}", parsed.goal_id),
            &json!({"from":old,"to":goal_status_db(target),"attachment_id":attachment_id,"fencing_epoch":fencing_epoch}),
            now,
        )?;
        tx.commit()?;
        self.get_goal_by_id(&parsed.goal_id)
    }

    pub fn seal_for_migration(
        &mut self,
        goal_ref: &str,
        expected_goal_revision: u64,
        attachment_id: &str,
        expected_attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<GoalProjection, GoalError> {
        let parsed: GoalRef = goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, status): (u64, String) = tx.query_row(
            "SELECT revision,status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if status == "sealed" && current == expected_goal_revision + 1 {
            let payload: String = tx.query_row(
                "SELECT payload_json FROM events WHERE goal_id=?1 AND event_kind='goal_sealed' ORDER BY seq DESC LIMIT 1",
                [&parsed.goal_id],
                |row| row.get(0),
            )?;
            let payload: serde_json::Value = serde_json::from_str(&payload)?;
            if payload["attachment_id"] == attachment_id
                && payload["fencing_epoch"] == fencing_epoch
            {
                drop(tx);
                return self.get_goal_by_id(&parsed.goal_id);
            }
        }
        if current != expected_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_goal_revision,
                current,
            });
        }
        verify_controller(
            &tx,
            &parsed.goal_id,
            attachment_id,
            expected_attachment_revision,
            fencing_epoch,
            now,
        )?;
        let (status, pending): (String, u32) = tx.query_row(
            "SELECT status,pending_effects FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !matches!(status.as_str(), "active" | "paused") || pending != 0 {
            return Err(GoalError::InvalidInput(
                "migration seal requires an active/paused Goal with no pending effects".into(),
            ));
        }
        let next = current + 1;
        tx.execute(
            "UPDATE goals SET revision=?2,status='sealed',updated_at_ms=?3 WHERE goal_id=?1 AND revision=?4",
            params![parsed.goal_id, next, now, current],
        )?;
        tx.execute(
            "UPDATE attachments SET revision=revision+1,state='released',updated_at_ms=?2 WHERE attachment_id=?1",
            params![attachment_id, now],
        )?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "goal_sealed",
            &format!("seal:{}:{next}", parsed.goal_id),
            &json!({"attachment_id":attachment_id,"fencing_epoch":fencing_epoch}),
            now,
        )?;
        tx.commit()?;
        self.get_goal_by_id(&parsed.goal_id)
    }

    pub fn export_migration(&self, goal_ref: &str) -> Result<GoalMigrationBundle, GoalError> {
        let goal = self.get(goal_ref)?;
        if goal.status != GoalStatus::Sealed {
            return Err(GoalError::InvalidInput(
                "Goal must be sealed before migration export".into(),
            ));
        }
        if goal.evidence.pending_effects != 0 {
            return Err(GoalError::InvalidInput(
                "Goal with pending effects cannot be exported".into(),
            ));
        }
        let artifacts = self.artifacts()?;
        let objective_bytes = artifacts.get(&goal.objective_ref)?;
        let acceptance_bytes = artifacts.get(&goal.acceptance_ref)?;
        let objective = MigrationArtifact {
            r#ref: goal.objective_ref.clone(),
            digest: goal.objective_digest.clone(),
            bytes: objective_bytes,
        };
        let acceptance = MigrationArtifact {
            r#ref: goal.acceptance_ref.clone(),
            digest: goal.acceptance_digest.clone(),
            bytes: acceptance_bytes,
        };
        let parsed: GoalRef = goal_ref.parse()?;
        let mut stmt = self.conn.prepare("SELECT gate_id,status,evidence_ref,evidence_digest,observed_at_ms,basis_goal_revision FROM conditions WHERE goal_id=?1 ORDER BY gate_id")?;
        let conditions = stmt
            .query_map([&parsed.goal_id], |row| {
                let status: String = row.get(1)?;
                Ok(MigrationCondition {
                    gate_id: row.get(0)?,
                    status: match status.as_str() {
                        "satisfied" => ConditionStatus::Satisfied,
                        "unsatisfied" => ConditionStatus::Unsatisfied,
                        "unknown" => ConditionStatus::Unknown,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    },
                    evidence_ref: row.get(2)?,
                    evidence_digest: row.get(3)?,
                    observed_at_ms: row.get(4)?,
                    basis_goal_revision: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut action_stmt = self.conn.prepare(
            "SELECT action_id FROM actions WHERE goal_id=?1 ORDER BY created_at_ms,action_id",
        )?;
        let actions = action_stmt
            .query_map([&parsed.goal_id], |row| row.get::<_, String>(0))?
            .map(|row| {
                row.map_err(GoalError::Sql)
                    .and_then(|id| self.get_action(&id))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut artifact_refs = std::collections::BTreeSet::new();
        for action in &actions {
            artifact_refs.insert(action.bundle_ref.clone());
            if let Some(reference) = &action.result_ref {
                artifact_refs.insert(reference.clone());
                let result_bytes = artifacts.get(reference)?;
                let result: GoalActionResult = serde_json::from_slice(&result_bytes)?;
                artifact_refs.insert(result.stdout_ref);
                artifact_refs.insert(result.stderr_ref);
                if let Some(reference) = result.verification_command_evidence_ref {
                    artifact_refs.insert(reference);
                }
            }
        }
        for condition in &conditions {
            if !condition.evidence_ref.is_empty() {
                artifact_refs.insert(condition.evidence_ref.clone());
            }
        }
        let mut migration_artifacts = Vec::new();
        for reference in artifact_refs {
            let bytes = artifacts.get(&reference)?;
            let stored = artifacts.put(&bytes)?;
            migration_artifacts.push(MigrationArtifact {
                r#ref: stored.r#ref,
                digest: stored.digest,
                bytes,
            });
        }
        // Bind export identity to the sealed revision rather than wall time so
        // a lost response can be retried without minting a second bundle.
        let exported_at_ms = goal.updated_at_ms;
        let core = serde_json::json!({
            "schema_version":MIGRATION_BUNDLE_SCHEMA,
            "source_goal":goal,
            "objective":objective,
            "acceptance":acceptance,
            "conditions":conditions,
            "actions":actions,
            "artifacts":migration_artifacts,
            "exported_at_ms":exported_at_ms,
        });
        let bundle_digest = canonical_json_digest(&core)?;
        Ok(GoalMigrationBundle {
            schema_version: MIGRATION_BUNDLE_SCHEMA.into(),
            source_goal: serde_json::from_value(core["source_goal"].clone())?,
            objective: serde_json::from_value(core["objective"].clone())?,
            acceptance: serde_json::from_value(core["acceptance"].clone())?,
            conditions: serde_json::from_value(core["conditions"].clone())?,
            actions: serde_json::from_value(core["actions"].clone())?,
            artifacts: serde_json::from_value(core["artifacts"].clone())?,
            exported_at_ms,
            bundle_digest,
        })
    }

    pub fn mark_transferred(
        &mut self,
        goal_ref: &str,
        expected_goal_revision: u64,
        import_receipt: &str,
    ) -> Result<GoalProjection, GoalError> {
        require_nonempty("import_receipt", import_receipt)?;
        let parsed: GoalRef = goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, status): (u64, String) = tx.query_row(
            "SELECT revision,status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if status == "transferred" && current == expected_goal_revision + 1 {
            let payload: String = tx.query_row(
                "SELECT payload_json FROM events WHERE goal_id=?1 AND event_kind='goal_transferred' ORDER BY seq DESC LIMIT 1",
                [&parsed.goal_id],
                |row| row.get(0),
            )?;
            if serde_json::from_str::<serde_json::Value>(&payload)?["import_receipt"]
                == import_receipt
            {
                drop(tx);
                return self.get_goal_by_id(&parsed.goal_id);
            }
        }
        if current != expected_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: expected_goal_revision,
                current,
            });
        }
        if status != "sealed" {
            return Err(GoalError::InvalidInput(
                "only a sealed source Goal can become transferred".into(),
            ));
        }
        let next = current + 1;
        tx.execute(
            "UPDATE goals SET revision=?2,status='transferred',updated_at_ms=?3 WHERE goal_id=?1 AND revision=?4",
            params![parsed.goal_id, next, now, current],
        )?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "goal_transferred",
            &format!("transfer:{}:{next}", parsed.goal_id),
            &json!({"import_receipt":import_receipt}),
            now,
        )?;
        tx.commit()?;
        self.get_goal_by_id(&parsed.goal_id)
    }

    #[cfg(test)]
    pub fn import_migration_local(
        &mut self,
        bundle: &GoalMigrationBundle,
    ) -> Result<GoalProjection, GoalError> {
        bundle.verify()?;
        let objective = self.artifacts()?.put(&bundle.objective.bytes)?;
        let acceptance = self.artifacts()?.put(&bundle.acceptance.bytes)?;
        if objective.r#ref != bundle.objective.r#ref
            || objective.digest != bundle.objective.digest
            || acceptance.r#ref != bundle.acceptance.r#ref
            || acceptance.digest != bundle.acceptance.digest
        {
            return Err(GoalError::Integrity(
                "migration artifact digest does not verify".into(),
            ));
        }
        let goal_id = format!("import-{}", &bundle.bundle_digest[7..31]);
        if let Ok(existing) = self.get_goal_by_id(&goal_id) {
            return Ok(existing);
        }
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO goals(goal_id,revision,repository_identity,objective_ref,objective_digest,acceptance_ref,acceptance_digest,acceptance_basis_revision,status,health,required_conditions,satisfied_conditions,unsatisfied_conditions,unknown_conditions,pending_effects,created_at_ms,updated_at_ms) VALUES(?1,1,?2,?3,?4,?5,?6,?7,'paused',?8,?9,?10,?11,?12,0,?13,?13)", params![goal_id,bundle.source_goal.repository_identity,objective.r#ref,objective.digest,acceptance.r#ref,acceptance.digest,bundle.source_goal.acceptance_basis_revision,health_db(bundle.source_goal.health),bundle.source_goal.evidence.required_conditions,bundle.source_goal.evidence.satisfied_conditions,bundle.source_goal.evidence.unsatisfied_conditions,bundle.source_goal.evidence.unknown_conditions,now])?;
        for condition in &bundle.conditions {
            tx.execute("INSERT INTO conditions(goal_id,gate_id,status,evidence_ref,evidence_digest,observed_at_ms,basis_goal_revision) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![goal_id,condition.gate_id,condition_db(condition.status),condition.evidence_ref,condition.evidence_digest,condition.observed_at_ms,condition.basis_goal_revision])?;
        }
        for action in &bundle.actions {
            tx.execute("INSERT INTO actions(action_id,goal_id,basis_goal_revision,committed_goal_revision,attachment_id,attachment_revision,fencing_epoch,action_kind,idempotency_key,bundle_ref,bundle_digest,status,result_ref,result_digest,exit_code,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",params![action.action_id,goal_id,action.basis_goal_revision,action.committed_goal_revision,action.attachment_id,action.attachment_revision,action.fencing_epoch,action.action_kind,action.idempotency_key,action.bundle_ref,action.bundle_digest,action_status_db(action.status),action.result_ref,action.result_digest,action.exit_code,action.created_at_ms,action.updated_at_ms])?;
        }
        append_event(
            &tx,
            &goal_id,
            1,
            "goal_imported",
            &format!("import:{}", bundle.bundle_digest),
            &json!({
                "source_goal_ref":bundle.source_goal.goal_ref,
                "source_revision":bundle.source_goal.revision,
                "export_digest":bundle.bundle_digest,
            }),
            now,
        )?;
        tx.commit()?;
        self.get_goal_by_id(&goal_id)
    }

    pub fn evaluate_condition(
        &mut self,
        request: EvaluateConditionRequest<'_>,
    ) -> Result<(ConditionEvaluation, GoalProjection), GoalError> {
        let parsed: GoalRef = request.goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        require_nonempty("gate_id", request.gate_id)?;
        if request.evidence.is_empty() {
            return Err(GoalError::InvalidInput(
                "condition evidence must not be empty".into(),
            ));
        }
        let artifact = self.artifacts()?.put(request.evidence)?;
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = goal_revision(&tx, &parsed.goal_id)?;
        if current != request.expected_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: request.expected_goal_revision,
                current,
            });
        }
        verify_controller(
            &tx,
            &parsed.goal_id,
            request.attachment_id,
            request.expected_attachment_revision,
            request.fencing_epoch,
            now,
        )?;
        let basis: u64 = tx.query_row(
            "SELECT acceptance_basis_revision FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| r.get(0),
        )?;
        if basis != request.acceptance_basis_revision {
            return Err(GoalError::RevisionConflict {
                expected: request.acceptance_basis_revision,
                current: basis,
            });
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conditions WHERE goal_id=?1 AND gate_id=?2)",
            params![parsed.goal_id, request.gate_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(GoalError::InvalidInput(format!(
                "gate is not required by acceptance: {}",
                request.gate_id
            )));
        }
        tx.execute("UPDATE conditions SET status=?3,evidence_ref=?4,evidence_digest=?5,observed_at_ms=?6,basis_goal_revision=?7 WHERE goal_id=?1 AND gate_id=?2",params![parsed.goal_id,request.gate_id,condition_db(request.status),artifact.r#ref,artifact.digest,now,basis])?;
        let (required,satisfied,unsatisfied,unknown):(u32,u32,u32,u32)=tx.query_row("SELECT COUNT(*),SUM(CASE WHEN status='satisfied' THEN 1 ELSE 0 END),SUM(CASE WHEN status='unsatisfied' THEN 1 ELSE 0 END),SUM(CASE WHEN status='unknown' THEN 1 ELSE 0 END) FROM conditions WHERE goal_id=?1 AND basis_goal_revision=?2",params![parsed.goal_id,basis],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let (pending, current_status): (u32, String) = tx.query_row(
            "SELECT pending_effects,status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let current_lifecycle = parse_goal_status(&current_status)?;
        if current_lifecycle.is_terminal() {
            return Err(GoalError::InvalidInput(format!(
                "terminal Goal cannot accept new condition evidence: {current_status}"
            )));
        }
        let completes = request.complete_when_satisfied
            && required > 0
            && required == satisfied
            && unsatisfied == 0
            && unknown == 0
            && pending == 0;
        let next = current + 1;
        let goal_status = if completes {
            "complete"
        } else {
            current_status.as_str()
        };
        tx.execute("UPDATE goals SET revision=?2,status=?3,required_conditions=?4,satisfied_conditions=?5,unsatisfied_conditions=?6,unknown_conditions=?7,updated_at_ms=?8 WHERE goal_id=?1 AND revision=?9",params![parsed.goal_id,next,goal_status,required,satisfied,unsatisfied,unknown,now,current])?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "condition_evaluated",
            &format!("condition:{}:{}:{next}", parsed.goal_id, request.gate_id),
            &json!({"gate_id":request.gate_id,"status":condition_db(request.status),"evidence_ref":artifact.r#ref,"basis":basis}),
            now,
        )?;
        if completes {
            append_event(
                &tx,
                &parsed.goal_id,
                next,
                "goal_completed",
                &format!("complete:{}:{next}", parsed.goal_id),
                &json!({"acceptance_basis_revision":basis}),
                now,
            )?;
        }
        tx.commit()?;
        let eval = ConditionEvaluation {
            schema_version: CONDITION_SCHEMA.into(),
            goal_ref: request.goal_ref.into(),
            acceptance_basis_revision: basis,
            gate_id: request.gate_id.into(),
            status: request.status,
            evidence_ref: artifact.r#ref,
            evidence_digest: artifact.digest,
            observed_at_ms: now,
        };
        Ok((eval, self.get_goal_by_id(&parsed.goal_id)?))
    }

    pub fn prepare_action(
        &mut self,
        bundle: &GoalActionBundle,
        raw_bundle: &[u8],
    ) -> Result<ActionProjection, GoalError> {
        let parsed: GoalRef = bundle.goal_ref.parse()?;
        self.check_local_ref(&parsed)?;
        let now = now_ms();
        let artifact = self.artifacts()?.put(raw_bundle)?;
        if let Some(existing) = self.action_by_idempotency(&bundle.idempotency_key)? {
            if existing.action_id != bundle.action_id {
                return Err(GoalError::InvalidInput(
                    "idempotency key already belongs to another action".into(),
                ));
            }
            let stored = GoalActionBundle::parse(&self.artifacts()?.get(&existing.bundle_ref)?)?;
            if action_semantic_value(&stored) != action_semantic_value(bundle) {
                return Err(GoalError::InvalidInput(
                    "idempotency key replay changed the action bundle".into(),
                ));
            }
            return match existing.status {
                ActionStatus::Running | ActionStatus::ReconcileRequired => {
                    Err(GoalError::ReconcileRequired(existing.action_id))
                }
                _ => Ok(existing),
            };
        }
        if bundle.deadline_ms <= now {
            return Err(GoalError::InvalidInput(
                "action deadline has expired".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = goal_revision(&tx, &parsed.goal_id)?;
        if current != bundle.goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: bundle.goal_revision,
                current,
            });
        }
        verify_controller(
            &tx,
            &parsed.goal_id,
            &bundle.attachment_id,
            bundle.attachment_revision,
            bundle.fencing_epoch,
            now,
        )?;
        let status: String = tx.query_row(
            "SELECT status FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |r| r.get(0),
        )?;
        if status != "active" {
            return Err(GoalError::InvalidInput(format!(
                "Goal must be active to prepare action; current={status}"
            )));
        }
        let next = current + 1;
        tx.execute("INSERT INTO actions(action_id,goal_id,basis_goal_revision,committed_goal_revision,attachment_id,attachment_revision,fencing_epoch,action_kind,idempotency_key,bundle_ref,bundle_digest,status,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'committed',?12,?12)",params![bundle.action_id,parsed.goal_id,current,next,bundle.attachment_id,bundle.attachment_revision,bundle.fencing_epoch,bundle.action_kind,bundle.idempotency_key,artifact.r#ref,artifact.digest,now])?;
        tx.execute("UPDATE goals SET revision=?2,pending_effects=pending_effects+1,updated_at_ms=?3 WHERE goal_id=?1 AND revision=?4",params![parsed.goal_id,next,now,current])?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "action_committed",
            &bundle.idempotency_key,
            &json!({"action_id":bundle.action_id,"bundle_ref":artifact.r#ref,"fencing_epoch":bundle.fencing_epoch}),
            now,
        )?;
        tx.commit()?;
        self.get_action(&bundle.action_id)
    }

    pub fn mark_action_running(
        &mut self,
        action_id: &str,
        attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<ActionProjection, GoalError> {
        let now = now_ms();
        let instance_id = self.instance_id.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let action = load_action(&tx, &instance_id, action_id)?;
        if action.status != ActionStatus::Committed {
            return Err(GoalError::ReconcileRequired(action_id.into()));
        }
        let parsed: GoalRef = action.goal_ref.parse()?;
        verify_controller(
            &tx,
            &parsed.goal_id,
            &action.attachment_id,
            attachment_revision,
            fencing_epoch,
            now,
        )?;
        tx.execute("UPDATE actions SET status='running',attachment_revision=?2,fencing_epoch=?3,updated_at_ms=?4 WHERE action_id=?1 AND status='committed'",params![action_id,attachment_revision,fencing_epoch,now])?;
        append_event(
            &tx,
            &parsed.goal_id,
            action.committed_goal_revision,
            "action_running",
            &format!("running:{action_id}"),
            &json!({"action_id":action_id}),
            now,
        )?;
        tx.commit()?;
        self.get_action(action_id)
    }

    pub fn record_action_result(
        &mut self,
        action_id: &str,
        result: &ArtifactRef,
        exit_code: i32,
        attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<(ActionProjection, GoalProjection), GoalError> {
        let existing = self.get_action(action_id)?;
        if existing.status == ActionStatus::Settled {
            let goal_id = existing.goal_ref.parse::<GoalRef>()?.goal_id;
            return Ok((existing, self.get_goal_by_id(&goal_id)?));
        }
        let bundle = GoalActionBundle::parse(&self.artifacts()?.get(&existing.bundle_ref)?)?;
        let result_value: serde_json::Value =
            serde_json::from_slice(&self.artifacts()?.get(&result.r#ref)?)?;
        let now = now_ms();
        let instance_id = self.instance_id.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let action = load_action(&tx, &instance_id, action_id)?;
        if action.status != ActionStatus::Running {
            return Err(GoalError::ReconcileRequired(action_id.into()));
        }
        let parsed: GoalRef = action.goal_ref.parse()?;
        verify_controller(
            &tx,
            &parsed.goal_id,
            &action.attachment_id,
            attachment_revision,
            fencing_epoch,
            now,
        )?;
        let current = goal_revision(&tx, &parsed.goal_id)?;
        if current != action.committed_goal_revision {
            return Err(GoalError::RevisionConflict {
                expected: action.committed_goal_revision,
                current,
            });
        }
        let next = current + 1;
        tx.execute("UPDATE actions SET status='settled',result_ref=?2,result_digest=?3,exit_code=?4,updated_at_ms=?5 WHERE action_id=?1",params![action_id,result.r#ref,result.digest,exit_code,now])?;
        if exit_code == 0 {
            let verification_ref = result_value
                .get("verification_command_evidence_ref")
                .and_then(serde_json::Value::as_str);
            for gate in &bundle.satisfies {
                let known: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM conditions WHERE goal_id=?1 AND gate_id=?2)",
                    params![parsed.goal_id, gate],
                    |row| row.get(0),
                )?;
                if !known {
                    return Err(GoalError::InvalidInput(format!(
                        "action result references an unknown acceptance gate: {gate}"
                    )));
                }
                if gate.starts_with("verification") && verification_ref.is_none() {
                    return Err(GoalError::InvalidInput(
                        "verification gate requires independent command evidence".into(),
                    ));
                }
                tx.execute("UPDATE conditions SET status='satisfied',evidence_ref=?3,evidence_digest=?4,observed_at_ms=?5,basis_goal_revision=(SELECT acceptance_basis_revision FROM goals WHERE goal_id=?1) WHERE goal_id=?1 AND gate_id=?2",params![parsed.goal_id,gate,result.r#ref,result.digest,now])?;
            }
        }
        let (required,satisfied,unsatisfied,unknown):(u32,u32,u32,u32)=tx.query_row("SELECT COUNT(*),SUM(CASE WHEN status='satisfied' THEN 1 ELSE 0 END),SUM(CASE WHEN status='unsatisfied' THEN 1 ELSE 0 END),SUM(CASE WHEN status='unknown' THEN 1 ELSE 0 END) FROM conditions WHERE goal_id=?1",[&parsed.goal_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        let pending_before: u32 = tx.query_row(
            "SELECT pending_effects FROM goals WHERE goal_id=?1",
            [&parsed.goal_id],
            |row| row.get(0),
        )?;
        let completes = !bundle.satisfies.is_empty()
            && required > 0
            && required == satisfied
            && unsatisfied == 0
            && unknown == 0
            && pending_before == 1;
        tx.execute("UPDATE goals SET revision=?2,pending_effects=CASE WHEN pending_effects>0 THEN pending_effects-1 ELSE 0 END,health=?3,status=CASE WHEN ?4 THEN 'complete' ELSE status END,required_conditions=?5,satisfied_conditions=?6,unsatisfied_conditions=?7,unknown_conditions=?8,updated_at_ms=?9 WHERE goal_id=?1 AND revision=?10",params![parsed.goal_id,next,if exit_code==0{"healthy"}else{"needs_attention"},completes,required,satisfied,unsatisfied,unknown,now,current])?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "action_settled",
            &format!("result:{action_id}"),
            &json!({"action_id":action_id,"result_ref":result.r#ref,"exit_code":exit_code}),
            now,
        )?;
        tx.commit()?;
        Ok((
            self.get_action(action_id)?,
            self.get_goal_by_id(&parsed.goal_id)?,
        ))
    }

    pub fn get_action(&self, id: &str) -> Result<ActionProjection, GoalError> {
        load_action(&self.conn, &self.instance_id, id)
    }
    pub fn reconcile_action(
        &mut self,
        action_id: &str,
        controller_attachment_id: &str,
        attachment_revision: u64,
        fencing_epoch: u64,
    ) -> Result<(ActionProjection, GoalProjection), GoalError> {
        let now = now_ms();
        let instance_id = self.instance_id.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let action = load_action(&tx, &instance_id, action_id)?;
        let parsed: GoalRef = action.goal_ref.parse()?;
        verify_controller(
            &tx,
            &parsed.goal_id,
            controller_attachment_id,
            attachment_revision,
            fencing_epoch,
            now,
        )?;
        if action.status == ActionStatus::Settled {
            return Err(GoalError::InvalidInput(
                "settled action does not require reconciliation".into(),
            ));
        }
        if action.status != ActionStatus::Running
            && action.status != ActionStatus::ReconcileRequired
        {
            return Err(GoalError::InvalidInput(
                "only running/unknown actions can enter reconcile_required".into(),
            ));
        }
        let current = goal_revision(&tx, &parsed.goal_id)?;
        let next = current + 1;
        tx.execute(
            "UPDATE actions SET status='reconcile_required',updated_at_ms=?2 WHERE action_id=?1",
            params![action_id, now],
        )?;
        tx.execute("UPDATE goals SET revision=?2,health='needs_attention',updated_at_ms=?3 WHERE goal_id=?1 AND revision=?4",params![parsed.goal_id,next,now,current])?;
        append_event(
            &tx,
            &parsed.goal_id,
            next,
            "action_reconcile_required",
            &format!("reconcile:{action_id}"),
            &json!({"action_id":action_id}),
            now,
        )?;
        tx.commit()?;
        Ok((
            self.get_action(action_id)?,
            self.get_goal_by_id(&parsed.goal_id)?,
        ))
    }
    fn action_by_idempotency(&self, key: &str) -> Result<Option<ActionProjection>, GoalError> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT action_id FROM actions WHERE idempotency_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|v| self.get_action(&v)).transpose()
    }

    pub fn get_attachment(&self, id: &str) -> Result<AttachmentProjection, GoalError> {
        self.conn.query_row("SELECT attachment_id,goal_id,revision,role,driver_id,generation,fencing_epoch,lease_expires_at_ms,state,created_at_ms,updated_at_ms FROM attachments WHERE attachment_id=?1", [id], |r| {
            let goal_id:String=r.get(1)?; let role:String=r.get(3)?; let state:String=r.get(8)?;
            Ok(AttachmentProjection { schema_version:ATTACHMENT_SCHEMA.into(), attachment_id:r.get(0)?, goal_ref:GoalRef{backend:GoalBackend::Local,instance_id:self.instance_id.clone(),goal_id}.to_string(), revision:r.get(2)?, role:parse_role(&role).map_err(sql_conv)?, driver_id:r.get(4)?, generation:r.get(5)?, fencing_epoch:r.get(6)?, lease_expires_at_ms:r.get(7)?, state:parse_attachment_state(&state).map_err(sql_conv)?, created_at_ms:r.get(9)?, updated_at_ms:r.get(10)? })
        }).optional()?.ok_or_else(|| GoalError::NotFound(id.into()))
    }

    fn get_goal_by_id(&self, id: &str) -> Result<GoalProjection, GoalError> {
        self.conn.query_row("SELECT goal_id,revision,repository_identity,objective_ref,objective_digest,acceptance_ref,acceptance_digest,acceptance_basis_revision,status,health,required_conditions,satisfied_conditions,unsatisfied_conditions,unknown_conditions,pending_effects,created_at_ms,updated_at_ms FROM goals WHERE goal_id=?1", [id], |r| {
            let goal_id:String=r.get(0)?; let status:String=r.get(8)?; let health:String=r.get(9)?;
            Ok(GoalProjection { schema_version:PROJECTION_SCHEMA.into(), goal_ref:GoalRef{backend:GoalBackend::Local,instance_id:self.instance_id.clone(),goal_id:goal_id.clone()}.to_string(), goal_id, backend:GoalBackend::Local, backend_instance_id:self.instance_id.clone(), revision:r.get(1)?, repository_identity:r.get(2)?, objective_ref:r.get(3)?, objective_digest:r.get(4)?, acceptance_ref:r.get(5)?, acceptance_digest:r.get(6)?, acceptance_basis_revision:r.get(7)?, status:parse_goal_status(&status).map_err(sql_conv)?, health:parse_health(&health).map_err(sql_conv)?, evidence:EvidenceProgress { required_conditions:r.get(10)?, satisfied_conditions:r.get(11)?, unsatisfied_conditions:r.get(12)?, unknown_conditions:r.get(13)?, pending_effects:r.get(14)? }, lineage:None, created_at_ms:r.get(15)?, updated_at_ms:r.get(16)? })
        }).optional()?.ok_or_else(|| GoalError::NotFound(id.into()))
    }

    fn check_local_ref(&self, r: &GoalRef) -> Result<(), GoalError> {
        if r.backend != GoalBackend::Local {
            return Err(GoalError::BackendUnavailable(
                "external backend adapter is not installed".into(),
            ));
        }
        if r.instance_id != self.instance_id {
            return Err(GoalError::BackendUnavailable(format!(
                "Goal belongs to local store {}, current store is {}",
                r.instance_id, self.instance_id
            )));
        }
        Ok(())
    }
}

fn append_event(
    tx: &Transaction<'_>,
    goal_id: &str,
    revision: u64,
    kind: &str,
    idem: &str,
    payload: &serde_json::Value,
    now: i64,
) -> Result<(), GoalError> {
    tx.execute("INSERT INTO events(goal_id,goal_revision,event_kind,idempotency_key,payload_json,occurred_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![goal_id,revision,kind,idem,serde_json::to_string(payload)?,now])?;
    Ok(())
}
fn load_action(
    conn: &Connection,
    instance_id: &str,
    id: &str,
) -> Result<ActionProjection, GoalError> {
    conn.query_row("SELECT action_id,goal_id,basis_goal_revision,committed_goal_revision,attachment_id,attachment_revision,fencing_epoch,action_kind,idempotency_key,bundle_ref,bundle_digest,status,result_ref,result_digest,exit_code,created_at_ms,updated_at_ms FROM actions WHERE action_id=?1",[id],|r|{let goal_id:String=r.get(1)?;let status:String=r.get(11)?;Ok(ActionProjection{action_id:r.get(0)?,goal_ref:GoalRef{backend:GoalBackend::Local,instance_id:instance_id.to_string(),goal_id}.to_string(),basis_goal_revision:r.get(2)?,committed_goal_revision:r.get(3)?,attachment_id:r.get(4)?,attachment_revision:r.get(5)?,fencing_epoch:r.get(6)?,action_kind:r.get(7)?,idempotency_key:r.get(8)?,bundle_ref:r.get(9)?,bundle_digest:r.get(10)?,status:parse_action_status(&status).map_err(sql_conv)?,result_ref:r.get(12)?,result_digest:r.get(13)?,exit_code:r.get(14)?,created_at_ms:r.get(15)?,updated_at_ms:r.get(16)?})}).optional()?.ok_or_else(||GoalError::NotFound(id.into()))
}
fn goal_revision(tx: &Transaction<'_>, id: &str) -> Result<u64, GoalError> {
    tx.query_row("SELECT revision FROM goals WHERE goal_id=?1", [id], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| GoalError::NotFound(id.into()))
}
fn active_controller(
    tx: &Transaction<'_>,
    goal_id: &str,
    now: i64,
) -> Result<Option<String>, GoalError> {
    Ok(tx.query_row("SELECT driver_id FROM attachments WHERE goal_id=?1 AND role='controller' AND state='active' AND lease_expires_at_ms>?2",params![goal_id,now],|r|r.get(0)).optional()?)
}
fn expire_controllers(tx: &Transaction<'_>, goal_id: &str, now: i64) -> Result<(), GoalError> {
    tx.execute("UPDATE attachments SET state='expired',revision=revision+1,updated_at_ms=?2 WHERE goal_id=?1 AND role='controller' AND state='active' AND lease_expires_at_ms<=?2",params![goal_id,now])?;
    Ok(())
}
fn verify_controller(
    tx: &Transaction<'_>,
    goal_id: &str,
    id: &str,
    revision: u64,
    fence: u64,
    now: i64,
) -> Result<(), GoalError> {
    let row:(String,u64,u64,String,String,i64)=tx.query_row("SELECT goal_id,revision,fencing_epoch,role,state,lease_expires_at_ms FROM attachments WHERE attachment_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?.ok_or_else(||GoalError::NotFound(id.into()))?;
    if row.0 != goal_id {
        return Err(GoalError::InvalidInput(
            "attachment belongs to another Goal".into(),
        ));
    }
    if row.1 != revision {
        return Err(GoalError::RevisionConflict {
            expected: revision,
            current: row.1,
        });
    }
    if row.2 != fence {
        return Err(GoalError::StaleFence {
            expected: fence,
            current: row.2,
        });
    }
    if row.3 != "controller" || row.4 != "active" || row.5 <= now {
        return Err(GoalError::InvalidInput(
            "active, unexpired controller attachment required".into(),
        ));
    }
    Ok(())
}
fn valid_transition(from: GoalStatus, to: GoalStatus) -> bool {
    if from.is_terminal() {
        return false;
    }
    matches!(
        (from, to),
        (GoalStatus::Active, GoalStatus::Paused)
            | (GoalStatus::Paused, GoalStatus::Active)
            | (GoalStatus::Waiting, GoalStatus::Active)
            | (GoalStatus::Blocked, GoalStatus::Active)
            | (_, GoalStatus::Cancelled)
            | (_, GoalStatus::Complete)
    )
}
fn require_nonempty(name: &str, v: &str) -> Result<(), GoalError> {
    if v.trim().is_empty() {
        Err(GoalError::InvalidInput(format!("{name} must not be empty")))
    } else {
        Ok(())
    }
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn new_id(prefix: &str) -> String {
    let now = now_ms();
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now:x}-{:x}-{n:x}", std::process::id())
}
fn role_db(v: AttachmentRole) -> &'static str {
    match v {
        AttachmentRole::Controller => "controller",
        AttachmentRole::Observer => "observer",
    }
}
fn goal_status_db(v: GoalStatus) -> &'static str {
    match v {
        GoalStatus::Draft => "draft",
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Waiting => "waiting",
        GoalStatus::Blocked => "blocked",
        GoalStatus::Complete => "complete",
        GoalStatus::Cancelled => "cancelled",
        GoalStatus::Transferred => "transferred",
        GoalStatus::Sealed => "sealed",
    }
}
fn parse_goal_status(v: &str) -> Result<GoalStatus, GoalError> {
    match v {
        "draft" => Ok(GoalStatus::Draft),
        "active" => Ok(GoalStatus::Active),
        "paused" => Ok(GoalStatus::Paused),
        "waiting" => Ok(GoalStatus::Waiting),
        "blocked" => Ok(GoalStatus::Blocked),
        "complete" => Ok(GoalStatus::Complete),
        "cancelled" => Ok(GoalStatus::Cancelled),
        "transferred" => Ok(GoalStatus::Transferred),
        "sealed" => Ok(GoalStatus::Sealed),
        _ => Err(GoalError::Integrity(format!("unknown goal status: {v}"))),
    }
}

fn canonical_json_digest(value: &serde_json::Value) -> Result<String, GoalError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn action_semantic_value(bundle: &GoalActionBundle) -> serde_json::Value {
    json!({
        "schema_version":bundle.schema_version,
        "goal_ref":bundle.goal_ref,
        "action_id":bundle.action_id,
        "action_kind":bundle.action_kind,
        "role":bundle.role,
        "idempotency_key":bundle.idempotency_key,
        "cell_binding":bundle.cell_binding,
        "prompt":bundle.prompt,
        "model":bundle.model,
        "expected_artifacts":bundle.expected_artifacts,
        "satisfies":bundle.satisfies,
    })
}
fn parse_health(v: &str) -> Result<GoalHealth, GoalError> {
    match v {
        "healthy" => Ok(GoalHealth::Healthy),
        "degraded" => Ok(GoalHealth::Degraded),
        "needs_attention" => Ok(GoalHealth::NeedsAttention),
        _ => Err(GoalError::Integrity(format!("unknown goal health: {v}"))),
    }
}
#[cfg(test)]
fn health_db(v: GoalHealth) -> &'static str {
    match v {
        GoalHealth::Healthy => "healthy",
        GoalHealth::Degraded => "degraded",
        GoalHealth::NeedsAttention => "needs_attention",
    }
}
fn parse_role(v: &str) -> Result<AttachmentRole, GoalError> {
    match v {
        "controller" => Ok(AttachmentRole::Controller),
        "observer" => Ok(AttachmentRole::Observer),
        _ => Err(GoalError::Integrity(format!(
            "unknown attachment role: {v}"
        ))),
    }
}
fn parse_attachment_state(v: &str) -> Result<AttachmentState, GoalError> {
    match v {
        "active" => Ok(AttachmentState::Active),
        "released" => Ok(AttachmentState::Released),
        "expired" => Ok(AttachmentState::Expired),
        _ => Err(GoalError::Integrity(format!(
            "unknown attachment state: {v}"
        ))),
    }
}
fn condition_db(v: ConditionStatus) -> &'static str {
    match v {
        ConditionStatus::Satisfied => "satisfied",
        ConditionStatus::Unsatisfied => "unsatisfied",
        ConditionStatus::Unknown => "unknown",
    }
}
fn parse_action_status(v: &str) -> Result<ActionStatus, GoalError> {
    match v {
        "committed" => Ok(ActionStatus::Committed),
        "running" => Ok(ActionStatus::Running),
        "settled" => Ok(ActionStatus::Settled),
        "reconcile_required" => Ok(ActionStatus::ReconcileRequired),
        _ => Err(GoalError::Integrity(format!("unknown action status: {v}"))),
    }
}
#[cfg(test)]
fn action_status_db(v: ActionStatus) -> &'static str {
    match v {
        ActionStatus::Committed => "committed",
        ActionStatus::Running => "running",
        ActionStatus::Settled => "settled",
        ActionStatus::ReconcileRequired => "reconcile_required",
    }
}
fn sql_conv(e: GoalError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn acceptance(gates: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&json!({"schema_version":"goal_acceptance/v1","required_gates":gates}))
            .unwrap()
    }
    fn store() -> (tempfile::TempDir, LocalGoalStore) {
        let d = tempfile::tempdir().unwrap();
        let s = LocalGoalStore::open(StorePaths::new(d.path()), "test").unwrap();
        (d, s)
    }
    #[test]
    fn start_is_durable_and_discoverable() {
        let (d, mut s) = store();
        let (g, a) = s
            .start_goal(
                "repo:x",
                "ship it",
                &acceptance(&["tests"]),
                "driver-a",
                90_000,
            )
            .unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.evidence.required_conditions, 1);
        assert_eq!(g.evidence.unknown_conditions, 1);
        assert_eq!(a.fencing_epoch, 1);
        drop(s);
        let s = LocalGoalStore::open(StorePaths::new(d.path()), "test").unwrap();
        assert_eq!(s.get(&g.goal_ref).unwrap(), g);
        assert_eq!(s.discover("repo:x").unwrap().len(), 1);
    }
    #[test]
    fn goal_revision_cas_rejects_stale_writer() {
        let (_d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "d", 90_000)
            .unwrap();
        let p = s
            .transition(
                &g.goal_ref,
                GoalStatus::Paused,
                g.revision,
                &a.attachment_id,
                a.revision,
                a.fencing_epoch,
            )
            .unwrap();
        assert_eq!(p.revision, 2);
        assert!(matches!(
            s.transition(
                &g.goal_ref,
                GoalStatus::Active,
                1,
                &a.attachment_id,
                a.revision,
                a.fencing_epoch
            ),
            Err(GoalError::RevisionConflict { .. })
        ));
    }
    #[test]
    fn one_controller_and_takeover_fences_old_writer() {
        let (_d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 90_000)
            .unwrap();
        assert!(matches!(
            s.attach(
                &g.goal_ref,
                "b",
                AttachmentRole::Controller,
                g.revision,
                90_000
            ),
            Err(GoalError::ControllerHeld(_))
        ));
        let released = s
            .release(&a.attachment_id, a.revision, a.fencing_epoch)
            .unwrap();
        assert_eq!(released.state, AttachmentState::Released);
        let b = s
            .attach(
                &g.goal_ref,
                "b",
                AttachmentRole::Controller,
                g.revision,
                90_000,
            )
            .unwrap();
        assert!(b.fencing_epoch > a.fencing_epoch);
        assert!(matches!(
            s.release(&a.attachment_id, a.revision, a.fencing_epoch),
            Err(GoalError::RevisionConflict { .. }) | Err(GoalError::InvalidInput(_))
        ));
    }
    #[test]
    fn observer_does_not_block_controller() {
        let (_d, mut s) = store();
        let (g, _a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let o = s
            .attach(
                &g.goal_ref,
                "observer",
                AttachmentRole::Observer,
                g.revision,
                90_000,
            )
            .unwrap();
        let c = s
            .attach(
                &g.goal_ref,
                "controller",
                AttachmentRole::Controller,
                g.revision,
                90_000,
            )
            .unwrap();
        assert_eq!(o.role, AttachmentRole::Observer);
        assert_eq!(c.role, AttachmentRole::Controller);
    }
    #[test]
    fn completion_is_atomic_and_evidence_driven() {
        let (_d, mut s) = store();
        let (g, a) = s
            .start_goal(
                "repo:x",
                "ship",
                &acceptance(&["tests", "scope"]),
                "a",
                90_000,
            )
            .unwrap();
        let (_, g2) = s
            .evaluate_condition(EvaluateConditionRequest {
                goal_ref: &g.goal_ref,
                gate_id: "tests",
                status: ConditionStatus::Satisfied,
                evidence: b"tests ok",
                expected_goal_revision: 1,
                acceptance_basis_revision: 1,
                attachment_id: &a.attachment_id,
                expected_attachment_revision: 1,
                fencing_epoch: 1,
                complete_when_satisfied: true,
            })
            .unwrap();
        assert_eq!(g2.status, GoalStatus::Active);
        let (_, g3) = s
            .evaluate_condition(EvaluateConditionRequest {
                goal_ref: &g.goal_ref,
                gate_id: "scope",
                status: ConditionStatus::Satisfied,
                evidence: b"scope ok",
                expected_goal_revision: 2,
                acceptance_basis_revision: 1,
                attachment_id: &a.attachment_id,
                expected_attachment_revision: 1,
                fencing_epoch: 1,
                complete_when_satisfied: true,
            })
            .unwrap();
        assert_eq!(g3.status, GoalStatus::Complete);
        assert_eq!(g3.evidence.satisfied_conditions, 2);
    }
    #[test]
    fn running_action_requires_reconcile_instead_of_blind_redispatch() {
        let (d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 90_000)
            .unwrap();
        let root = d.path().canonicalize().unwrap();
        let bundle = GoalActionBundle {
            schema_version: crate::ACTION_BUNDLE_SCHEMA.into(),
            goal_ref: g.goal_ref.clone(),
            goal_revision: 1,
            attachment_id: a.attachment_id.clone(),
            attachment_revision: 1,
            fencing_epoch: 1,
            action_id: "action-1".into(),
            action_kind: "verify".into(),
            role: crate::GoalActionRole::Executor,
            idempotency_key: "idem-1".into(),
            cell_binding: shuttle_protocol::CellBinding {
                version: 1,
                workspace_root: root.display().to_string(),
                allowed_paths: vec![],
                allowed_commands: vec![],
                base_oid: "oid".into(),
                assignment_digest: format!("sha256:{}", "a".repeat(64)),
                runtime_descriptor_ref: None,
                runtime_descriptor_digest: None,
                runtime_descriptor: None,
                shuttle_binary_digest: None,
                environment_id: "env".into(),
                provider_run_id: None,
                attempt_id: None,
                runtime_id: None,
                package_id: None,
                profile_ref: None,
                session_file: None,
                repo_set: None,
                project: Default::default(),
            },
            prompt: "verify".into(),
            model: None,
            expected_artifacts: vec![],
            satisfies: vec![],
            deadline_ms: now_ms() + 60_000,
        };
        let raw = serde_json::to_vec(&bundle).unwrap();
        s.prepare_action(&bundle, &raw).unwrap();
        s.mark_action_running(&bundle.action_id, 1, 1).unwrap();
        assert!(matches!(
            s.prepare_action(&bundle, &raw),
            Err(GoalError::ReconcileRequired(_))
        ));
        let expired_at = now_ms().saturating_sub(1);
        s.conn
            .execute(
                "UPDATE attachments SET lease_expires_at_ms=?2,updated_at_ms=?2 WHERE attachment_id=?1",
                rusqlite::params![a.attachment_id, expired_at],
            )
            .unwrap();
        assert!(
            s.get_attachment(&a.attachment_id)
                .unwrap()
                .lease_expires_at_ms
                <= now_ms()
        );
        let next = s
            .attach(
                &g.goal_ref,
                "recovery",
                AttachmentRole::Controller,
                2,
                90_000,
            )
            .unwrap();
        assert_eq!(
            s.get_attachment(&a.attachment_id).unwrap().state,
            AttachmentState::Expired
        );
        let (action, goal) = s
            .reconcile_action(
                &bundle.action_id,
                &next.attachment_id,
                next.revision,
                next.fencing_epoch,
            )
            .unwrap();
        assert_eq!(action.status, ActionStatus::ReconcileRequired);
        assert_eq!(goal.health, GoalHealth::NeedsAttention);
        assert_eq!(goal.evidence.pending_effects, 1);
    }
    #[test]
    fn idempotency_replay_cannot_change_bundle() {
        let (d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 90_000)
            .unwrap();
        let root = d.path().canonicalize().unwrap();
        let mut bundle = GoalActionBundle {
            schema_version: crate::ACTION_BUNDLE_SCHEMA.into(),
            goal_ref: g.goal_ref.clone(),
            goal_revision: 1,
            attachment_id: a.attachment_id.clone(),
            attachment_revision: 1,
            fencing_epoch: 1,
            action_id: "action-1".into(),
            action_kind: "verify".into(),
            role: crate::GoalActionRole::Executor,
            idempotency_key: "idem-1".into(),
            cell_binding: shuttle_protocol::CellBinding {
                version: 1,
                workspace_root: root.display().to_string(),
                allowed_paths: vec![],
                allowed_commands: vec![],
                base_oid: "oid".into(),
                assignment_digest: format!("sha256:{}", "a".repeat(64)),
                runtime_descriptor_ref: None,
                runtime_descriptor_digest: None,
                runtime_descriptor: None,
                shuttle_binary_digest: None,
                environment_id: "env".into(),
                provider_run_id: None,
                attempt_id: None,
                runtime_id: None,
                package_id: None,
                profile_ref: None,
                session_file: None,
                repo_set: None,
                project: Default::default(),
            },
            prompt: "verify".into(),
            model: None,
            expected_artifacts: vec![],
            satisfies: vec![],
            deadline_ms: now_ms() + 60_000,
        };
        let raw = serde_json::to_vec(&bundle).unwrap();
        s.prepare_action(&bundle, &raw).unwrap();
        bundle.prompt = "different".into();
        let changed = serde_json::to_vec(&bundle).unwrap();
        assert!(matches!(
            s.prepare_action(&bundle, &changed),
            Err(GoalError::InvalidInput(_))
        ));
    }
    #[test]
    fn settled_action_replay_is_valid_after_original_deadline() {
        let (d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 90_000)
            .unwrap();
        let root = d.path().canonicalize().unwrap();
        let bundle = GoalActionBundle {
            schema_version: crate::ACTION_BUNDLE_SCHEMA.into(),
            goal_ref: g.goal_ref.clone(),
            goal_revision: 1,
            attachment_id: a.attachment_id.clone(),
            attachment_revision: 1,
            fencing_epoch: 1,
            action_id: "action-replay".into(),
            action_kind: "verify".into(),
            role: crate::GoalActionRole::Executor,
            idempotency_key: "idem-replay".into(),
            cell_binding: shuttle_protocol::CellBinding {
                version: 1,
                workspace_root: root.display().to_string(),
                allowed_paths: vec![],
                allowed_commands: vec![],
                base_oid: "oid".into(),
                assignment_digest: format!("sha256:{}", "a".repeat(64)),
                runtime_descriptor_ref: None,
                runtime_descriptor_digest: None,
                runtime_descriptor: None,
                shuttle_binary_digest: None,
                environment_id: "env".into(),
                runtime_id: None,
                package_id: None,
                profile_ref: None,
                session_file: None,
                repo_set: None,
                provider_run_id: None,
                attempt_id: None,
                project: Default::default(),
            },
            prompt: "verify".into(),
            model: None,
            expected_artifacts: vec![],
            satisfies: vec![],
            deadline_ms: now_ms() + 200,
        };
        let raw = serde_json::to_vec(&bundle).unwrap();
        s.prepare_action(&bundle, &raw).unwrap();
        s.mark_action_running(&bundle.action_id, 1, 1).unwrap();
        let stdout = s.artifacts().unwrap().put(b"ok").unwrap();
        let stderr = s.artifacts().unwrap().put(b"").unwrap();
        let result_value = GoalActionResult {
            schema_version: crate::ACTION_RESULT_SCHEMA.into(),
            goal_ref: g.goal_ref.clone(),
            action_id: bundle.action_id.clone(),
            role: crate::GoalActionRole::Executor,
            provider_run_id: "provider-replay".into(),
            started_at_ms: now_ms(),
            settled_at_ms: now_ms(),
            exit_code: 0,
            stdout_ref: stdout.r#ref.clone(),
            stdout_digest: stdout.digest.clone(),
            stderr_ref: stderr.r#ref.clone(),
            stderr_digest: stderr.digest.clone(),
            verification_command_evidence_ref: None,
            verification_command_evidence_digest: None,
            proposal: "progress".into(),
            residual_risks: vec![],
        };
        let result = s
            .artifacts()
            .unwrap()
            .put(&serde_json::to_vec_pretty(&result_value).unwrap())
            .unwrap();
        s.record_action_result(&bundle.action_id, &result, 0, 1, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let replay = s.prepare_action(&bundle, &raw).unwrap();
        assert_eq!(replay.status, ActionStatus::Settled);
        let sealed = s
            .seal_for_migration(&g.goal_ref, 3, &a.attachment_id, 1, a.fencing_epoch)
            .unwrap();
        let migration = s.export_migration(&g.goal_ref).unwrap();
        assert_eq!(sealed.status, GoalStatus::Sealed);
        let refs: std::collections::BTreeSet<_> = migration
            .artifacts
            .iter()
            .map(|artifact| artifact.r#ref.as_str())
            .collect();
        assert!(!refs.contains(bundle.action_id.as_str()));
        assert!(refs.contains(replay.bundle_ref.as_str()));
        assert!(refs.contains(result.r#ref.as_str()));
        assert!(refs.contains(stdout.r#ref.as_str()));
        assert!(refs.contains(stderr.r#ref.as_str()));
    }
    #[test]
    fn successful_action_can_satisfy_declared_gate_atomically() {
        let (d, mut s) = store();
        let (g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 90_000)
            .unwrap();
        let root = d.path().canonicalize().unwrap();
        let bundle = GoalActionBundle {
            schema_version: crate::ACTION_BUNDLE_SCHEMA.into(),
            goal_ref: g.goal_ref.clone(),
            goal_revision: 1,
            attachment_id: a.attachment_id.clone(),
            attachment_revision: 1,
            fencing_epoch: 1,
            action_id: "auto-condition".into(),
            action_kind: "verify".into(),
            role: crate::GoalActionRole::Verifier,
            idempotency_key: "auto-condition-v1".into(),
            cell_binding: shuttle_protocol::CellBinding {
                version: 1,
                workspace_root: root.display().to_string(),
                allowed_paths: vec![],
                allowed_commands: vec![shuttle_protocol::AllowedCommand {
                    program: "true".into(),
                    argv_prefix: Some(vec![]),
                }],
                base_oid: "oid".into(),
                assignment_digest: format!("sha256:{}", "a".repeat(64)),
                runtime_descriptor_ref: None,
                runtime_descriptor_digest: None,
                runtime_descriptor: None,
                shuttle_binary_digest: None,
                environment_id: "env".into(),
                provider_run_id: None,
                attempt_id: None,
                runtime_id: None,
                package_id: None,
                profile_ref: None,
                session_file: None,
                repo_set: None,
                project: Default::default(),
            },
            prompt: "verify".into(),
            model: None,
            expected_artifacts: vec!["verification_result".into()],
            satisfies: vec!["tests".into()],
            deadline_ms: now_ms() + 60_000,
        };
        let raw = serde_json::to_vec(&bundle).unwrap();
        s.prepare_action(&bundle, &raw).unwrap();
        s.mark_action_running(&bundle.action_id, 1, 1).unwrap();
        let result_bytes = serde_json::to_vec(&json!({
            "verification_command_evidence_ref":format!("artifact://sha256/{}", "b".repeat(64))
        }))
        .unwrap();
        let result = s.artifacts().unwrap().put(&result_bytes).unwrap();
        let (_, goal) = s
            .record_action_result(&bundle.action_id, &result, 0, 1, 1)
            .unwrap();
        assert_eq!(goal.status, GoalStatus::Complete);
        assert_eq!(goal.evidence.satisfied_conditions, 1);
        assert_eq!(goal.evidence.pending_effects, 0);
    }
    #[test]
    fn concurrent_controller_acquire_has_exactly_one_winner() {
        let (d, mut seed) = store();
        let (g, _a) = seed
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "seed", 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        drop(seed);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut joins = Vec::new();
        for name in ["a", "b"] {
            let p = StorePaths::new(d.path());
            let goal_ref = g.goal_ref.clone();
            let b = barrier.clone();
            joins.push(std::thread::spawn(move || {
                let mut s = LocalGoalStore::open(p, "test").unwrap();
                b.wait();
                s.attach(&goal_ref, name, AttachmentRole::Controller, 1, 90_000)
            }));
        }
        barrier.wait();
        let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(GoalError::ControllerHeld(_))))
                .count(),
            1
        );
    }
    #[test]
    fn expired_controller_cannot_self_renew() {
        let (_d, mut s) = store();
        let (_g, a) = s
            .start_goal("repo:x", "ship", &acceptance(&["tests"]), "a", 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(matches!(
            s.renew(&a.attachment_id, a.revision, a.fencing_epoch, 90_000),
            Err(GoalError::InvalidInput(_))
        ));
    }

    #[test]
    fn sealed_export_is_digest_bound_and_source_becomes_permanently_transferred() {
        let (_d, mut s) = store();
        let (goal, attachment) = s
            .start_goal(
                "repo:migrate",
                "move authority",
                &acceptance(&["ready"]),
                "driver",
                90_000,
            )
            .unwrap();
        let sealed = s
            .seal_for_migration(
                &goal.goal_ref,
                goal.revision,
                &attachment.attachment_id,
                attachment.revision,
                attachment.fencing_epoch,
            )
            .unwrap();
        assert_eq!(sealed.status, GoalStatus::Sealed);
        assert_eq!(
            s.seal_for_migration(
                &goal.goal_ref,
                goal.revision,
                &attachment.attachment_id,
                attachment.revision,
                attachment.fencing_epoch,
            )
            .unwrap(),
            sealed
        );
        let bundle = s.export_migration(&goal.goal_ref).unwrap();
        assert_eq!(s.export_migration(&goal.goal_ref).unwrap(), bundle);
        assert_eq!(bundle.schema_version, MIGRATION_BUNDLE_SCHEMA);
        assert_eq!(bundle.source_goal.status, GoalStatus::Sealed);
        assert_eq!(bundle.objective.digest, goal.objective_digest);
        let (_target_dir, mut target) = store();
        let imported = target.import_migration_local(&bundle).unwrap();
        assert_eq!(imported.status, GoalStatus::Paused);
        assert_ne!(imported.goal_ref, goal.goal_ref);
        assert_eq!(
            target.import_migration_local(&bundle).unwrap().goal_ref,
            imported.goal_ref
        );
        let transferred = s
            .mark_transferred(&goal.goal_ref, sealed.revision, "goal-import:receipt")
            .unwrap();
        assert_eq!(transferred.status, GoalStatus::Transferred);
        assert_eq!(
            s.mark_transferred(&goal.goal_ref, sealed.revision, "goal-import:receipt")
                .unwrap(),
            transferred
        );
        assert!(s
            .attach(
                &goal.goal_ref,
                "late-writer",
                AttachmentRole::Controller,
                transferred.revision,
                90_000,
            )
            .is_err());
    }
}
