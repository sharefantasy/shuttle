//! Strict one-request-per-process client for the governed Goal facade.
//!
//! The client deliberately knows nothing about any upstream ledger or reducer.
//! It sends one provider-neutral Goal operation to an explicitly configured
//! `goal-control --json` executable and validates the complete response
//! envelope. There is no local-store fallback and no dual write.

use crate::{safe_id, GoalError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CF_FACADE_REQUEST_SCHEMA: &str = "goal_control_facade_request/v1";
pub const CF_FACADE_RESPONSE_SCHEMA: &str = "goal_control_facade_response/v1";
pub const DEFAULT_CF_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_CF_REQUEST_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_CF_RESPONSE_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_CF_STDERR_LIMIT: usize = 64 * 1024;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalOperation {
    Start,
    Import,
    Activate,
    Capabilities,
    Discover,
    Get,
    Attach,
    Renew,
    Release,
    PrepareAction,
    RecordResult,
    Evaluate,
    Transition,
    Reconcile,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalFacadeRequest {
    pub schema_version: String,
    pub request_id: String,
    pub operation: ExternalOperation,
    pub backend_instance_id: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalCurrentRevisions {
    pub goal: Option<u64>,
    pub attachment: Option<u64>,
    pub action: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalConflict {
    pub reason_code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalFacadeError {
    pub reason_code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalRetry {
    pub retryable: bool,
    pub after_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalResponse<T> {
    Result {
        schema_version: String,
        request_id: String,
        current_revisions: ExternalCurrentRevisions,
        result: T,
    },
    Conflict {
        schema_version: String,
        request_id: String,
        current_revisions: ExternalCurrentRevisions,
        conflict: ExternalConflict,
        retry: Option<ExternalRetry>,
    },
    Error {
        schema_version: String,
        request_id: String,
        current_revisions: ExternalCurrentRevisions,
        error: ExternalFacadeError,
        retry: Option<ExternalRetry>,
    },
}

pub type ExternalFacadeResponse = GoalResponse<Value>;

impl<T> GoalResponse<T> {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Result { request_id, .. }
            | Self::Conflict { request_id, .. }
            | Self::Error { request_id, .. } => request_id,
        }
    }

    pub fn schema_version(&self) -> &str {
        match self {
            Self::Result { schema_version, .. }
            | Self::Conflict { schema_version, .. }
            | Self::Error { schema_version, .. } => schema_version,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExternalGoalClient {
    executable: PathBuf,
    backend_instance_id: String,
    timeout: Duration,
    request_limit: usize,
    response_limit: usize,
    stderr_limit: usize,
}

impl ExternalGoalClient {
    pub fn new(executable: impl AsRef<Path>, backend_instance_id: &str) -> Result<Self, GoalError> {
        if !safe_id(backend_instance_id) {
            return Err(GoalError::InvalidInput(
                "external backend instance id must be a safe identifier".into(),
            ));
        }
        let supplied = executable.as_ref();
        if !supplied.is_absolute() {
            return Err(GoalError::InvalidInput(
                "backend facade path must be absolute".into(),
            ));
        }
        let executable = supplied.canonicalize().map_err(|error| {
            GoalError::BackendUnavailable(format!(
                "cannot resolve backend facade {}: {error}",
                supplied.display()
            ))
        })?;
        let metadata = executable.metadata().map_err(|error| {
            GoalError::BackendUnavailable(format!(
                "cannot inspect backend facade {}: {error}",
                executable.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(GoalError::BackendUnavailable(format!(
                "backend facade is not a regular file: {}",
                executable.display()
            )));
        }
        Ok(Self {
            executable,
            backend_instance_id: backend_instance_id.into(),
            timeout: DEFAULT_CF_TIMEOUT,
            request_limit: DEFAULT_CF_REQUEST_LIMIT,
            response_limit: DEFAULT_CF_RESPONSE_LIMIT,
            stderr_limit: DEFAULT_CF_STDERR_LIMIT,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, GoalError> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(GoalError::InvalidInput(
                "backend facade timeout must be between 1ms and 300s".into(),
            ));
        }
        self.timeout = timeout;
        Ok(self)
    }

    #[doc(hidden)]
    pub fn with_limits(
        mut self,
        request_limit: usize,
        response_limit: usize,
        stderr_limit: usize,
    ) -> Result<Self, GoalError> {
        if request_limit == 0 || response_limit == 0 || stderr_limit == 0 {
            return Err(GoalError::InvalidInput(
                "backend facade byte limits must be positive".into(),
            ));
        }
        self.request_limit = request_limit;
        self.response_limit = response_limit;
        self.stderr_limit = stderr_limit;
        Ok(self)
    }

    pub fn call(
        &self,
        operation: ExternalOperation,
        parameters: Value,
    ) -> Result<ExternalFacadeResponse, GoalError> {
        let request_id = next_request_id();
        self.call_with_request_id(operation, parameters, request_id)
    }

    pub fn call_with_request_id(
        &self,
        operation: ExternalOperation,
        parameters: Value,
        request_id: String,
    ) -> Result<ExternalFacadeResponse, GoalError> {
        let request = ExternalFacadeRequest {
            schema_version: CF_FACADE_REQUEST_SCHEMA.into(),
            request_id: request_id.clone(),
            operation,
            backend_instance_id: self.backend_instance_id.clone(),
            parameters,
        };
        let mut body = serde_json::to_vec(&request)?;
        body.push(b'\n');
        if body.len() > self.request_limit {
            return Err(GoalError::InvalidInput(format!(
                "backend facade request exceeds {} bytes",
                self.request_limit
            )));
        }

        let mut command = Command::new(&self.executable);
        command
            .arg("--json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // The facade is a one-shot process. A separate process group lets
            // timeout handling also stop descendants instead of orphaning a
            // helper after SHUTTLE has failed the request.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().map_err(|error| {
            GoalError::BackendUnavailable(format!(
                "failed to start backend facade {}: {error}",
                self.executable.display()
            ))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            GoalError::BackendUnavailable("backend facade stdin was not created".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            GoalError::BackendUnavailable("backend facade stdout was not created".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            GoalError::BackendUnavailable("backend facade stderr was not created".into())
        })?;

        let writer = thread::spawn(move || -> std::io::Result<()> {
            let mut stdin = stdin;
            stdin.write_all(&body)?;
            stdin.flush()
        });
        let response_limit = self.response_limit;
        let stderr_limit = self.stderr_limit;
        let stdout_reader = thread::spawn(move || read_bounded(stdout, response_limit));
        let stderr_reader = thread::spawn(move || read_bounded(stderr, stderr_limit));

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    kill_process_tree(&mut child);
                    // Do not wait for pipe workers on the deadline path. A
                    // broken or hostile descendant may have inherited an fd;
                    // the request deadline must still bound the caller. The
                    // child is reaped asynchronously because some platforms
                    // can delay wait(2) after killing a shell process group.
                    thread::spawn(move || {
                        let _ = child.wait();
                    });
                    drop(writer);
                    drop(stdout_reader);
                    drop(stderr_reader);
                    return Err(GoalError::BackendTimeout(format!(
                        "backend facade exceeded {}ms",
                        self.timeout.as_millis()
                    )));
                }
                Err(error) => {
                    kill_process_tree(&mut child);
                    thread::spawn(move || {
                        let _ = child.wait();
                    });
                    drop(writer);
                    drop(stdout_reader);
                    drop(stderr_reader);
                    return Err(GoalError::BackendUnavailable(format!(
                        "failed while waiting for backend facade: {error}"
                    )));
                }
            }
        };
        while !(writer.is_finished() && stdout_reader.is_finished() && stderr_reader.is_finished())
        {
            if Instant::now() >= deadline {
                // A one-shot facade must not leave helpers holding inherited
                // pipe fds. Such a request has not completed by its deadline.
                kill_process_group(child.id());
                drop(writer);
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(GoalError::BackendTimeout(format!(
                    "backend facade streams remained open beyond {}ms",
                    self.timeout.as_millis()
                )));
            }
            thread::sleep(Duration::from_millis(5));
        }

        writer
            .join()
            .map_err(|_| {
                GoalError::BackendUnavailable("backend facade stdin writer panicked".into())
            })?
            .map_err(|error| {
                GoalError::BackendUnavailable(format!("failed to write backend request: {error}"))
            })?;
        let stdout = join_reader(stdout_reader, "stdout")?;
        let stderr = join_reader(stderr_reader, "stderr")?;
        if stdout.total > self.response_limit {
            return Err(GoalError::ProtocolViolation(format!(
                "backend facade response exceeds {} bytes",
                self.response_limit
            )));
        }
        if stderr.total > self.stderr_limit {
            return Err(GoalError::ProtocolViolation(format!(
                "backend facade stderr exceeds {} bytes",
                self.stderr_limit
            )));
        }
        let response: ExternalFacadeResponse =
            serde_json::from_slice(&stdout.bytes).map_err(|error| {
                GoalError::ProtocolViolation(format!(
                    "backend facade returned malformed JSON envelope: {error}"
                ))
            })?;
        if response.schema_version() != CF_FACADE_RESPONSE_SCHEMA {
            return Err(GoalError::UnsupportedContract(format!(
                "expected {}, received {}",
                CF_FACADE_RESPONSE_SCHEMA,
                response.schema_version()
            )));
        }
        if response.request_id() != request_id {
            return Err(GoalError::ProtocolViolation(
                "backend facade response request_id does not match request".into(),
            ));
        }
        if matches!(response, ExternalFacadeResponse::Result { .. }) && !status.success() {
            return Err(GoalError::ProtocolViolation(format!(
                "backend facade returned a result envelope with exit status {:?}",
                status.code()
            )));
        }
        Ok(response)
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    total: usize,
}

fn read_bounded(mut reader: impl Read, limit: usize) -> std::io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if bytes.len() < limit {
            let keep = (limit - bytes.len()).min(read);
            bytes.extend_from_slice(&chunk[..keep]);
        }
    }
    Ok(BoundedOutput { bytes, total })
}

fn join_reader(
    handle: thread::JoinHandle<std::io::Result<BoundedOutput>>,
    stream: &str,
) -> Result<BoundedOutput, GoalError> {
    handle
        .join()
        .map_err(|_| {
            GoalError::BackendUnavailable(format!("backend facade {stream} reader panicked"))
        })?
        .map_err(|error| {
            GoalError::BackendUnavailable(format!(
                "failed to read backend facade {stream}: {error}"
            ))
        })
}

fn kill_process_tree(child: &mut std::process::Child) {
    kill_process_group(child.id());
    let _ = child.kill();
}

fn kill_process_group(process_id: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(process_id as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = process_id;
}

fn next_request_id() -> String {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("shuttle-{}-{epoch:x}-{sequence:x}", std::process::id())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const REQUEST_ID: &str = "request-test-1";

    fn fake(body: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("goal-control");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }

    fn client(path: &Path) -> ExternalGoalClient {
        ExternalGoalClient::new(path, "test-instance").unwrap()
    }

    fn result_envelope(schema: &str, request_id: &str) -> String {
        format!(
            r#"{{"schema_version":"{schema}","request_id":"{request_id}","outcome":"result","current_revisions":{{"goal":4,"attachment":2,"action":null}},"result":{{"status":"ok"}}}}"#
        )
    }

    #[test]
    fn result_round_trip_uses_exact_operation_and_allows_bounded_stderr() {
        let response = result_envelope(CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID);
        let script = format!(
            "read request\ncase \"$request\" in *'\"operation\":\"get\"'*) ;; *) exit 19 ;; esac\nprintf '%s\\n' '{}'\nprintf 'diagnostic noise\\n' >&2",
            response.replace('\'', "'\\''")
        );
        let (_dir, path) = fake(&script);
        let response = client(&path)
            .call_with_request_id(
                ExternalOperation::Get,
                serde_json::json!({"goal_ref":"goal:external:test-instance:g1"}),
                REQUEST_ID.into(),
            )
            .unwrap();
        assert!(
            matches!(response, ExternalFacadeResponse::Result { result, .. } if result["status"] == "ok")
        );
    }

    #[test]
    fn conflict_and_error_are_preserved_for_the_caller() {
        let conflict = format!(
            r#"{{"schema_version":"{}","request_id":"{}","outcome":"conflict","current_revisions":{{"goal":7,"attachment":null,"action":null}},"conflict":{{"reason_code":"revision_conflict","message":"stale"}},"retry":{{"retryable":true,"after_ms":5}}}}"#,
            CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID
        );
        let (_dir, path) = fake(&format!("read request\nprintf '%s\\n' '{}'", conflict));
        let response = client(&path)
            .call_with_request_id(ExternalOperation::Attach, Value::Null, REQUEST_ID.into())
            .unwrap();
        assert!(
            matches!(response, ExternalFacadeResponse::Conflict { current_revisions, conflict, .. } if current_revisions.goal == Some(7) && conflict.reason_code == "revision_conflict")
        );

        let error = format!(
            r#"{{"schema_version":"{}","request_id":"{}","outcome":"error","current_revisions":{{"goal":null,"attachment":null,"action":null}},"error":{{"reason_code":"backend_unavailable","message":"offline"}},"retry":{{"retryable":true,"after_ms":100}}}}"#,
            CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID
        );
        let (_dir, path) = fake(&format!(
            "read request\nprintf '%s\\n' '{}'\nexit 75",
            error
        ));
        let response = client(&path)
            .call_with_request_id(
                ExternalOperation::Capabilities,
                Value::Null,
                REQUEST_ID.into(),
            )
            .unwrap();
        assert!(
            matches!(response, ExternalFacadeResponse::Error { error, retry: Some(retry), .. } if error.reason_code == "backend_unavailable" && retry.retryable)
        );
    }

    #[test]
    fn unavailable_timeout_and_protocol_fail_closed() {
        let missing = PathBuf::from("/definitely/missing/goal-control");
        assert!(matches!(
            ExternalGoalClient::new(&missing, "test-instance"),
            Err(GoalError::BackendUnavailable(_))
        ));

        let (_dir, path) = fake("read request\nsleep 2");
        let timeout = client(&path)
            .with_timeout(Duration::from_millis(25))
            .unwrap()
            .call_with_request_id(ExternalOperation::Get, Value::Null, REQUEST_ID.into());
        assert!(matches!(timeout, Err(GoalError::BackendTimeout(_))));

        let (_dir, path) = fake("read request\nprintf '%s\n' 'not-json'");
        let malformed = client(&path).call_with_request_id(
            ExternalOperation::Get,
            Value::Null,
            REQUEST_ID.into(),
        );
        assert!(matches!(malformed, Err(GoalError::ProtocolViolation(_))));
    }

    #[test]
    fn descendant_cannot_hold_response_pipes_open_after_facade_exit() {
        let response = result_envelope(CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID);
        let (_dir, path) = fake(&format!(
            "read request\n(sleep 5) &\nprintf '%s\\n' '{}'",
            response
        ));
        let started = Instant::now();
        let response = client(&path)
            .with_timeout(Duration::from_millis(100))
            .unwrap()
            .call_with_request_id(ExternalOperation::Get, Value::Null, REQUEST_ID.into());
        assert!(matches!(response, Err(GoalError::BackendTimeout(_))));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn future_schema_request_mismatch_and_oversized_streams_fail_closed() {
        let future = result_envelope("goal_control_facade_response/v2", REQUEST_ID);
        let (_dir, path) = fake(&format!("read request\nprintf '%s\\n' '{}'", future));
        let response = client(&path).call_with_request_id(
            ExternalOperation::Get,
            Value::Null,
            REQUEST_ID.into(),
        );
        assert!(matches!(response, Err(GoalError::UnsupportedContract(_))));

        let mismatch = result_envelope(CF_FACADE_RESPONSE_SCHEMA, "some-other-request");
        let (_dir, path) = fake(&format!("read request\nprintf '%s\\n' '{}'", mismatch));
        let response = client(&path).call_with_request_id(
            ExternalOperation::Get,
            Value::Null,
            REQUEST_ID.into(),
        );
        assert!(matches!(response, Err(GoalError::ProtocolViolation(_))));

        let valid = result_envelope(CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID);
        let (_dir, path) = fake(&format!("read request\nprintf '%s\\n' '{}'", valid));
        let response = client(&path)
            .with_limits(DEFAULT_CF_REQUEST_LIMIT, 32, DEFAULT_CF_STDERR_LIMIT)
            .unwrap()
            .call_with_request_id(ExternalOperation::Get, Value::Null, REQUEST_ID.into());
        assert!(matches!(response, Err(GoalError::ProtocolViolation(_))));

        let (_dir, path) = fake(&format!(
            "read request\nprintf 'stderr-too-large' >&2\nprintf '%s\\n' '{}'",
            valid
        ));
        let response = client(&path)
            .with_limits(DEFAULT_CF_REQUEST_LIMIT, DEFAULT_CF_RESPONSE_LIMIT, 4)
            .unwrap()
            .call_with_request_id(ExternalOperation::Get, Value::Null, REQUEST_ID.into());
        assert!(matches!(response, Err(GoalError::ProtocolViolation(_))));
    }

    #[test]
    fn every_v1_operation_has_a_stable_wire_name() {
        let cases = [
            (ExternalOperation::Start, "start"),
            (ExternalOperation::Import, "import"),
            (ExternalOperation::Activate, "activate"),
            (ExternalOperation::Capabilities, "capabilities"),
            (ExternalOperation::Discover, "discover"),
            (ExternalOperation::Get, "get"),
            (ExternalOperation::Attach, "attach"),
            (ExternalOperation::Renew, "renew"),
            (ExternalOperation::Release, "release"),
            (ExternalOperation::PrepareAction, "prepare_action"),
            (ExternalOperation::RecordResult, "record_result"),
            (ExternalOperation::Evaluate, "evaluate"),
            (ExternalOperation::Transition, "transition"),
            (ExternalOperation::Reconcile, "reconcile"),
        ];
        for (operation, expected) in cases {
            assert_eq!(serde_json::to_value(operation).unwrap(), expected);
        }
    }

    #[test]
    fn every_v1_operation_round_trips_through_the_facade() {
        let response = result_envelope(CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID);
        let (_dir, path) = fake(&format!("read request\nprintf '%s\\n' '{}'", response));
        let client = client(&path);
        for operation in [
            ExternalOperation::Start,
            ExternalOperation::Import,
            ExternalOperation::Activate,
            ExternalOperation::Capabilities,
            ExternalOperation::Discover,
            ExternalOperation::Get,
            ExternalOperation::Attach,
            ExternalOperation::Renew,
            ExternalOperation::Release,
            ExternalOperation::PrepareAction,
            ExternalOperation::RecordResult,
            ExternalOperation::Evaluate,
            ExternalOperation::Transition,
            ExternalOperation::Reconcile,
        ] {
            let response = client
                .call_with_request_id(operation, Value::Null, REQUEST_ID.into())
                .unwrap();
            assert!(matches!(response, ExternalFacadeResponse::Result { .. }));
        }
    }

    #[test]
    fn explicit_request_id_is_reused_verbatim_across_retries() {
        let dir = tempfile::tempdir().unwrap();
        let capture = dir.path().join("request-ids.txt");
        let response = result_envelope(CF_FACADE_RESPONSE_SCHEMA, REQUEST_ID);
        let (_guard, path) = fake(&format!(
            "read request\nprintf '%s\\n' \"$request\" >> '{}'\nprintf '%s\\n' '{}'",
            capture.display(),
            response.replace('\'', "'\"'\"'")
        ));
        let client = client(&path);
        for _ in 0..2 {
            client
                .call_with_request_id(
                    ExternalOperation::PrepareAction,
                    serde_json::json!({"bundle": {"action_id": "a1"}}),
                    REQUEST_ID.into(),
                )
                .unwrap();
        }
        let requests: Vec<Value> = std::fs::read_to_string(capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["request_id"], REQUEST_ID);
        assert_eq!(requests[1]["request_id"], REQUEST_ID);
    }
}
