use serde_json::Value;
use shuttle_goal::{GoalActionBundle, LocalGoalStore, StorePaths};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());
fn process_test_lock() -> MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn shuttle() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}
fn run(args: &[&str]) -> std::process::Output {
    Command::new(shuttle())
        .args(args)
        .output()
        .expect("spawn shuttle")
}
fn json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}
fn scratch(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("shuttle_goal_{tag}_"))
        .tempdir()
        .unwrap()
}
fn init_snapshot_workspace(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    let out = Command::new("git")
        .current_dir(path)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(out.status.success());
    fs::write(path.join("fixture.txt"), "fixture\n").unwrap();
    let out = Command::new("git")
        .current_dir(path)
        .args(["add", "fixture.txt"])
        .output()
        .unwrap();
    assert!(out.status.success());
}
fn start(dir: &std::path::Path, lease: &str) -> Value {
    let acceptance = dir.join("acceptance.json");
    fs::write(
        &acceptance,
        r#"{"schema_version":"goal_acceptance/v1","required_gates":["tests","scope"]}"#,
    )
    .unwrap();
    let out = run(&[
        "goal",
        "start",
        "ship fixture",
        "--acceptance",
        acceptance.to_str().unwrap(),
        "--repository-identity",
        "repo:test",
        "--driver-id",
        "driver-a",
        "--lease-seconds",
        lease,
        "--state-dir",
        dir.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    json(&out)
}

#[test]
fn cold_resume_status_and_discover_without_cf_or_python() {
    let d = scratch("resume");
    let created = start(d.path(), "90");
    let goal_ref = created["goal"]["goal_ref"].as_str().unwrap();
    let status = run(&[
        "goal",
        "status",
        goal_ref,
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(status.status.success());
    let s = json(&status);
    assert!(s["objective_ref"]
        .as_str()
        .unwrap()
        .starts_with("artifact://sha256/"));
    assert_eq!(s["status"], "active");
    let found = run(&[
        "goal",
        "discover",
        "--repository-identity",
        "repo:test",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(found.status.success());
    assert_eq!(json(&found).as_array().unwrap().len(), 1);
    let doctor = run(&[
        "goal",
        "doctor",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(doctor.status.success());
    assert_eq!(json(&doctor)["status"], "healthy");
}

#[test]
fn local_migration_commands_seal_export_and_transfer() {
    let d = scratch("migration");
    let created = start(d.path(), "90");
    let goal = &created["goal"];
    let attachment = &created["attachment"];
    let sealed = run(&[
        "goal",
        "seal",
        goal["goal_ref"].as_str().unwrap(),
        "--attachment-id",
        attachment["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        "1",
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(sealed.status.success());
    assert_eq!(json(&sealed)["status"], "sealed");
    let bundle = d.path().join("migration.json");
    let exported = run(&[
        "goal",
        "export",
        goal["goal_ref"].as_str().unwrap(),
        "--output",
        bundle.to_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(exported.status.success());
    let migration: shuttle_goal::GoalMigrationBundle =
        serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
    migration.verify().unwrap();
    let transferred = run(&[
        "goal",
        "transfer",
        goal["goal_ref"].as_str().unwrap(),
        "--expected-goal-revision",
        "2",
        "--target-goal-ref",
        "goal:external:target",
        "--bundle",
        bundle.to_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!transferred.status.success());
    assert_eq!(json(&transferred)["reason_code"], "backend_unavailable");
    let status = run(&[
        "goal",
        "status",
        goal["goal_ref"].as_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(json(&status)["status"], "sealed");
}

#[test]
fn controller_contention_then_takeover_rejects_stale_authority() {
    let d = scratch("fence");
    let created = start(d.path(), "6");
    let goal_ref = created["goal"]["goal_ref"].as_str().unwrap();
    let revision = created["goal"]["revision"].as_u64().unwrap();
    let first = &created["attachment"];
    let held = run(&[
        "goal",
        "attach",
        goal_ref,
        "--driver-id",
        "driver-b",
        "--role",
        "controller",
        "--expected-goal-revision",
        &revision.to_string(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!held.status.success());
    assert_eq!(json(&held)["reason_code"], "controller_held");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let expires_at = first["lease_expires_at_ms"].as_i64().unwrap();
    let wait_ms = expires_at.saturating_sub(now_ms).saturating_add(500);
    thread::sleep(Duration::from_millis(wait_ms.max(0) as u64));
    let takeover = run(&[
        "goal",
        "attach",
        goal_ref,
        "--driver-id",
        "driver-b",
        "--role",
        "controller",
        "--expected-goal-revision",
        &revision.to_string(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(
        takeover.status.success(),
        "{}",
        String::from_utf8_lossy(&takeover.stderr)
    );
    let second = json(&takeover);
    assert!(second["fencing_epoch"].as_u64().unwrap() > first["fencing_epoch"].as_u64().unwrap());
    let stale = run(&[
        "goal",
        "pause",
        goal_ref,
        "--attachment-id",
        first["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        &revision.to_string(),
        "--expected-attachment-revision",
        &first["revision"].as_u64().unwrap().to_string(),
        "--fencing-epoch",
        &first["fencing_epoch"].as_u64().unwrap().to_string(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!stale.status.success());
}

#[test]
fn pause_resume_and_revision_cas_are_durable() {
    let d = scratch("cas");
    let created = start(d.path(), "90");
    let goal_ref = created["goal"]["goal_ref"].as_str().unwrap();
    let a = &created["attachment"];
    let pause = run(&[
        "goal",
        "pause",
        goal_ref,
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        "1",
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(
        pause.status.success(),
        "{}",
        String::from_utf8_lossy(&pause.stderr)
    );
    assert_eq!(json(&pause)["status"], "paused");
    let stale = run(&[
        "goal",
        "resume",
        goal_ref,
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        "1",
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!stale.status.success());
    assert_eq!(json(&stale)["reason_code"], "revision_conflict");
    let resume = run(&[
        "goal",
        "resume",
        goal_ref,
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        "2",
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(resume.status.success());
    assert_eq!(json(&resume)["status"], "active");
}

#[test]
fn typed_evidence_is_required_for_atomic_completion() {
    let d = scratch("complete");
    let created = start(d.path(), "90");
    let goal_ref = created["goal"]["goal_ref"].as_str().unwrap();
    let a = &created["attachment"];
    let evidence = d.path().join("evidence.json");
    fs::write(&evidence, r#"{"passed":true}"#).unwrap();
    let first = run(&[
        "goal",
        "evaluate",
        goal_ref,
        "--gate-id",
        "tests",
        "--status",
        "satisfied",
        "--evidence",
        evidence.to_str().unwrap(),
        "--expected-goal-revision",
        "1",
        "--acceptance-basis-revision",
        "1",
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--complete-when-satisfied",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(json(&first)["goal"]["status"], "active");
    let second = run(&[
        "goal",
        "evaluate",
        goal_ref,
        "--gate-id",
        "scope",
        "--status",
        "satisfied",
        "--evidence",
        evidence.to_str().unwrap(),
        "--expected-goal-revision",
        "2",
        "--acceptance-basis-revision",
        "1",
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--complete-when-satisfied",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let v = json(&second);
    assert_eq!(v["goal"]["status"], "complete");
    assert_eq!(v["goal"]["evidence"]["satisfied_conditions"], 2);
}

#[test]
#[cfg(unix)]
fn step_commits_intent_runs_caged_cell_and_records_artifact_without_completing() {
    let _guard = process_test_lock();
    let d = scratch("step");
    let created = start(d.path(), "90");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("fake-pi.sh");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let bundle = d.path().join("action.json");
    let body = serde_json::json!({
      "schema_version":"shuttle_goal_action_bundle/v1","goal_ref":g["goal_ref"],"goal_revision":1,
      "attachment_id":a["attachment_id"],"attachment_revision":1,"fencing_epoch":1,
      "action_id":"action-verify-1","action_kind":"verify","idempotency_key":"goal-step:test:v1",
      "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[],"base_oid":"oid-test","assignment_digest":format!("sha256:{}","a".repeat(64)),"environment_id":"goal-step-env"},
      "prompt":"verify the fixture","model":null,"expected_artifacts":["verification_result"],"deadline_ms":deadline
    });
    fs::write(&bundle, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "step",
            "--bundle",
            bundle.to_str().unwrap(),
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["action"]["status"], "settled");
    assert_eq!(v["result"]["exit_code"], 0);
    assert_eq!(v["goal"]["status"], "active");
    assert_eq!(v["goal"]["evidence"]["pending_effects"], 0);
    assert!(v["attachment"]["revision"].as_u64().unwrap() > 1);
    assert!(v["result_ref"]
        .as_str()
        .unwrap()
        .starts_with("artifact://sha256/"));
    let replay = Command::new(shuttle())
        .args([
            "goal",
            "step",
            "--bundle",
            bundle.to_str().unwrap(),
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(replay.status.success());
    assert_eq!(json(&replay)["idempotent_replay"], true);
}

#[test]
fn observed_cell_failure_settles_blocked_without_unknown_effect() {
    let _guard = process_test_lock();
    let d = scratch("spawnfail");
    let created = start(d.path(), "90");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let bundle = d.path().join("action-fail.json");
    let body = serde_json::json!({
      "schema_version":"shuttle_goal_action_bundle/v1","goal_ref":g["goal_ref"],"goal_revision":1,
      "attachment_id":a["attachment_id"],"attachment_revision":1,"fencing_epoch":1,
      "action_id":"action-fail-1","action_kind":"verify","idempotency_key":"goal-step:fail:v1",
      "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[],"base_oid":"oid-test","assignment_digest":format!("sha256:{}","a".repeat(64)),"environment_id":"goal-step-env"},
      "prompt":"verify the fixture","model":null,"expected_artifacts":[],"deadline_ms":deadline
    });
    fs::write(&bundle, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    let failed = Command::new(shuttle())
        .args([
            "goal",
            "step",
            "--bundle",
            bundle.to_str().unwrap(),
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", d.path().join("missing-pi"))
        .output()
        .unwrap();
    assert!(failed.status.success());
    let failed_json = json(&failed);
    assert_eq!(failed_json["action"]["status"], "settled");
    assert_ne!(failed_json["result"]["exit_code"], 0);
    assert_eq!(failed_json["result"]["proposal"], "blocked");
    let action = run(&[
        "goal",
        "action-status",
        "action-fail-1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(action.status.success());
    assert_eq!(json(&action)["status"], "settled");
    let status = run(&[
        "goal",
        "status",
        g["goal_ref"].as_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(json(&status)["evidence"]["pending_effects"], 0);
    assert_eq!(json(&status)["health"], "needs_attention");
}

#[test]
fn unsafe_store_id_is_rejected_before_path_creation() {
    let d = scratch("storeid");
    let escaped = d.path().parent().unwrap().join("escaped-goal-store");
    let _ = fs::remove_dir_all(&escaped);
    let out = run(&[
        "goal",
        "doctor",
        "--store-id",
        "../escaped-goal-store",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success());
    assert_eq!(json(&out)["reason_code"], "invalid_input");
    assert!(!escaped.exists());
}

#[test]
#[cfg(unix)]
fn step_heartbeats_controller_lease_during_long_action() {
    let _guard = process_test_lock();
    let d = scratch("heartbeat");
    let created = start(d.path(), "3");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("slow-pi.sh");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nsleep 8\nprintf '%s\\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 10_000;
    let bundle = d.path().join("heartbeat.json");
    let body = serde_json::json!({
      "schema_version":"shuttle_goal_action_bundle/v1","goal_ref":g["goal_ref"],"goal_revision":1,
      "attachment_id":a["attachment_id"],"attachment_revision":1,"fencing_epoch":1,
      "action_id":"action-heartbeat-1","action_kind":"verify","idempotency_key":"goal-step:heartbeat:v1",
      "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[],"base_oid":"oid-test","assignment_digest":format!("sha256:{}","a".repeat(64)),"environment_id":"goal-step-env"},
      "prompt":"verify slowly","model":null,"expected_artifacts":[],"deadline_ms":deadline
    });
    fs::write(&bundle, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "step",
            "--bundle",
            bundle.to_str().unwrap(),
            "--lease-seconds",
            "6",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json(&out)["action"]["status"], "settled");
}

#[test]
#[cfg(unix)]
fn step_deadline_kills_child_and_records_timeout() {
    let _guard = process_test_lock();
    let d = scratch("deadline");
    let created = start(d.path(), "30");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("hung-pi.sh");
    fs::write(&fake_pi, "#!/bin/sh\nsleep 5\n").unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 2_000;
    let bundle = d.path().join("deadline.json");
    let body = serde_json::json!({
      "schema_version":"shuttle_goal_action_bundle/v1","goal_ref":g["goal_ref"],"goal_revision":1,
      "attachment_id":a["attachment_id"],"attachment_revision":1,"fencing_epoch":1,
      "action_id":"action-deadline-1","action_kind":"verify","idempotency_key":"goal-step:deadline:v1",
      "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[],"base_oid":"oid-test","assignment_digest":format!("sha256:{}","a".repeat(64)),"environment_id":"goal-step-env"},
      "prompt":"hang","model":null,"expected_artifacts":[],"deadline_ms":deadline
    });
    fs::write(&bundle, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    let started = std::time::Instant::now();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "step",
            "--bundle",
            bundle.to_str().unwrap(),
            "--lease-seconds",
            "30",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(started.elapsed() < Duration::from_secs(4));
    let value = json(&out);
    assert_eq!(value["result"]["exit_code"], 124);
    assert_eq!(value["result"]["proposal"], "blocked");
}

#[test]
#[cfg(unix)]
fn bounded_run_rebinds_authority_and_finishes_with_readonly_verifier() {
    let _guard = process_test_lock();
    let d = scratch("bounded_run");
    let created = start(d.path(), "90");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    init_snapshot_workspace(&workspace);
    fs::create_dir_all(workspace.join("src")).unwrap();
    let count = d.path().join("provider-count.txt");
    let fake_pi = d.path().join("token-pi.sh");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nn=0\n[ -f \"$GOAL_COUNT_FILE\" ] && n=$(sed -n '1p' \"$GOAL_COUNT_FILE\")\nn=$((n + 1))\nprintf '%s\n' \"$n\" > \"$GOAL_COUNT_FILE\"\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":7}}}'\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let binding = |write: bool| {
        serde_json::json!({
          "version":1,"workspace_root":workspace,
          "allowed_paths":if write { vec!["src/**"] } else { Vec::<&str>::new() },
          "allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid-test",
          "assignment_digest":format!("sha256:{}","a".repeat(64)),
          "environment_id":"goal-run-env"
        })
    };
    let plan = d.path().join("run-plan.json");
    fs::write(
        &plan,
        serde_json::to_vec_pretty(&serde_json::json!({
          "schema_version":"shuttle_goal_run_plan/v1",
          "goal_ref":g["goal_ref"],"attachment_id":a["attachment_id"],
          "actions":[
            {"action_id":"run-inspect","action_kind":"inspect","role":"executor","idempotency_key":"run:inspect:v1","cell_binding":binding(false),"prompt":"inspect","expected_artifacts":[],"timeout_ms":10000},
            {"action_id":"run-repair","action_kind":"repair","role":"executor","idempotency_key":"run:repair:v1","cell_binding":binding(true),"prompt":"repair","expected_artifacts":[],"timeout_ms":10000},
            {"action_id":"run-verify","action_kind":"verify","role":"verifier","idempotency_key":"run:verify:v1","cell_binding":binding(false),"prompt":"verify independently","expected_artifacts":["verification_result"],"timeout_ms":10000}
          ]
        }))
        .unwrap(),
    )
    .unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--max-actions",
            "3",
            "--max-tokens",
            "100",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .env("GOAL_COUNT_FILE", &count)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value = json(&out);
    assert_eq!(value["schema_version"], "shuttle_goal_run_result/v1");
    assert_eq!(value["stop_reason"], "plan_exhausted");
    assert_eq!(value["actions_executed"], 3);
    assert_eq!(value["tokens_used"], 21);
    assert_eq!(value["token_usage_complete"], true);
    assert_eq!(value["independent_verifier_settled"], true);
    assert_eq!(value["actions"][2]["exit_code"], 0);
    assert!(value["actions"][2]["verification_evidence_ref"]
        .as_str()
        .unwrap()
        .starts_with("artifact://sha256/"));
    assert_eq!(value["final_goal"]["revision"], 7);
    assert_eq!(fs::read_to_string(&count).unwrap().trim(), "3");
}

#[test]
#[cfg(unix)]
fn bounded_run_stops_before_dispatch_at_action_budget() {
    let _guard = process_test_lock();
    let d = scratch("action_budget");
    let created = start(d.path(), "90");
    let g = &created["goal"];
    let a = &created["attachment"];
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("token-pi.sh");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":1}}}'\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let read_only = serde_json::json!({
      "version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],
      "base_oid":"oid-test","assignment_digest":format!("sha256:{}","b".repeat(64)),"environment_id":"goal-budget-env"
    });
    let plan = d.path().join("run-plan.json");
    fs::write(
        &plan,
        serde_json::to_vec_pretty(&serde_json::json!({
          "schema_version":"shuttle_goal_run_plan/v1","goal_ref":g["goal_ref"],"attachment_id":a["attachment_id"],
          "actions":[
            {"action_id":"budget-one","action_kind":"inspect","role":"executor","idempotency_key":"budget:one:v1","cell_binding":read_only,"prompt":"one","timeout_ms":10000},
            {"action_id":"budget-verify","action_kind":"verify","role":"verifier","idempotency_key":"budget:verify:v1","cell_binding":read_only,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}
          ]
        }))
        .unwrap(),
    )
    .unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--max-actions",
            "1",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "max_actions");
    assert_eq!(value["actions_executed"], 1);
    assert_eq!(value["independent_verifier_settled"], false);
}

#[test]
fn bounded_run_rejects_a_verifier_with_write_authority() {
    let d = scratch("verifier_write");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let plan = d.path().join("bad-run-plan.json");
    fs::write(
        &plan,
        serde_json::to_vec_pretty(&serde_json::json!({
          "schema_version":"shuttle_goal_run_plan/v1",
          "goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
          "actions":[{"action_id":"bad-verifier","action_kind":"verify","role":"verifier","idempotency_key":"bad:verify:v1",
            "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":["src/**"],"allowed_commands":[],"base_oid":"oid","assignment_digest":format!("sha256:{}","c".repeat(64)),"environment_id":"bad"},
            "prompt":"verify","timeout_ms":10000}]
        }))
        .unwrap(),
    )
    .unwrap();
    let out = run(&[
        "goal",
        "run",
        "--plan",
        plan.to_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success());
    let value = json(&out);
    assert_eq!(value["reason_code"], "invalid_input");
    assert!(value["message"].as_str().unwrap().contains("read-only"));
}

#[test]
fn bounded_run_rejects_shell_based_verification() {
    let d = scratch("verifier_shell");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let plan = d.path().join("bad-shell-plan.json");
    fs::write(
        &plan,
        serde_json::to_vec_pretty(&serde_json::json!({
          "schema_version":"shuttle_goal_run_plan/v1",
          "goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
          "actions":[{"action_id":"shell-verifier","action_kind":"verify","role":"verifier","idempotency_key":"shell:verify:v1",
            "cell_binding":{"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"sh","argv_prefix":["-c","true"]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","c".repeat(64)),"environment_id":"bad-shell"},
            "prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}]
        }))
        .unwrap(),
    )
    .unwrap();
    let out = run(&[
        "goal",
        "run",
        "--plan",
        plan.to_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success());
    let value = json(&out);
    assert_eq!(value["reason_code"], "invalid_input");
    assert!(value["message"].as_str().unwrap().contains("shell"));
}

#[test]
#[cfg(unix)]
fn bounded_run_stops_on_token_budget() {
    let _guard = process_test_lock();
    let d = scratch("token_budget");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("token-pi.sh");
    fs::write(&fake_pi, "#!/bin/sh\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":7}}}'\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n").unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","d".repeat(64)),"environment_id":"token-budget"});
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
      "actions":[
        {"action_id":"token-one","action_kind":"inspect","role":"executor","idempotency_key":"token:one:v1","cell_binding":binding,"prompt":"one","timeout_ms":10000},
        {"action_id":"token-verify","action_kind":"verify","role":"verifier","idempotency_key":"token:verify:v1","cell_binding":binding,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}
      ]})).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--max-tokens",
            "7",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "token_budget");
    assert_eq!(value["actions_executed"], 1);
    assert_eq!(value["tokens_used"], 7);
}

#[test]
#[cfg(unix)]
fn bounded_run_stops_when_token_usage_is_unknown() {
    let _guard = process_test_lock();
    let d = scratch("unknown_tokens");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("no-token-pi.sh");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","e".repeat(64)),"environment_id":"unknown-token"});
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
      "actions":[
        {"action_id":"unknown-one","action_kind":"inspect","role":"executor","idempotency_key":"unknown:one:v1","cell_binding":binding,"prompt":"one","timeout_ms":10000},
        {"action_id":"unknown-verify","action_kind":"verify","role":"verifier","idempotency_key":"unknown:verify:v1","cell_binding":binding,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}
      ]})).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "token_usage_unknown");
    assert_eq!(value["actions_executed"], 1);
    assert_eq!(value["token_usage_complete"], false);
}

#[test]
#[cfg(unix)]
fn bounded_run_stops_on_provider_failure_budget() {
    let _guard = process_test_lock();
    let d = scratch("provider_budget");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("failed-pi.sh");
    fs::write(&fake_pi, "#!/bin/sh\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":2}}}'\nexit 2\n").unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","f".repeat(64)),"environment_id":"provider-budget"});
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
      "actions":[
        {"action_id":"failure-one","action_kind":"inspect","role":"executor","idempotency_key":"failure:one:v1","cell_binding":binding,"prompt":"fail","timeout_ms":10000},
        {"action_id":"failure-verify","action_kind":"verify","role":"verifier","idempotency_key":"failure:verify:v1","cell_binding":binding,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}
      ]})).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--max-provider-failures",
            "1",
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "provider_failure_budget");
    assert_eq!(value["provider_failures"], 1);
    assert_eq!(value["actions_executed"], 1);
}

#[test]
#[cfg(unix)]
fn bounded_run_resumes_a_committed_pre_spawn_action_and_replay_is_bounded() {
    let _guard = process_test_lock();
    let d = scratch("pre_spawn_resume");
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    init_snapshot_workspace(&workspace);
    let count = d.path().join("provider-count.txt");
    let fake_pi = d.path().join("token-pi.sh");
    fs::write(&fake_pi, "#!/bin/sh\nn=0\n[ -f \"$GOAL_COUNT_FILE\" ] && n=$(sed -n '1p' \"$GOAL_COUNT_FILE\")\nn=$((n + 1))\nprintf '%s\n' \"$n\" > \"$GOAL_COUNT_FILE\"\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":3}}}'\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n").unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","1".repeat(64)),"environment_id":"pre-spawn"});
    let committed_json = serde_json::json!({
      "schema_version":"shuttle_goal_action_bundle/v1","goal_ref":created["goal"]["goal_ref"],"goal_revision":1,
      "attachment_id":created["attachment"]["attachment_id"],"attachment_revision":1,"fencing_epoch":1,
      "action_id":"resume-inspect","action_kind":"inspect","role":"executor","idempotency_key":"resume:inspect:v1",
      "cell_binding":binding,"prompt":"resume committed action","expected_artifacts":[],"deadline_ms":i64::MAX
    });
    let raw = serde_json::to_vec_pretty(&committed_json).unwrap();
    let bundle = GoalActionBundle::parse(&raw).unwrap();
    let mut store =
        LocalGoalStore::open(StorePaths::new(d.path().join("default")), "default").unwrap();
    assert_eq!(
        store.prepare_action(&bundle, &raw).unwrap().status,
        shuttle_goal::ActionStatus::Committed
    );
    drop(store);
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
      "actions":[
        {"action_id":"resume-inspect","action_kind":"inspect","role":"executor","idempotency_key":"resume:inspect:v1","cell_binding":binding,"prompt":"resume committed action","expected_artifacts":[],"timeout_ms":10000},
        {"action_id":"resume-verify","action_kind":"verify","role":"verifier","idempotency_key":"resume:verify:v1","cell_binding":binding,"prompt":"verify independently","expected_artifacts":["verification_result"],"timeout_ms":10000}
      ]})).unwrap()).unwrap();
    let invoke = || {
        Command::new(shuttle())
            .args([
                "goal",
                "run",
                "--plan",
                plan.to_str().unwrap(),
                "--max-no-progress",
                "1",
                "--state-dir",
                d.path().to_str().unwrap(),
                "--json",
            ])
            .env("PI_BIN", &fake_pi)
            .env("GOAL_COUNT_FILE", &count)
            .output()
            .unwrap()
    };
    let first = invoke();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_value = json(&first);
    assert_eq!(first_value["stop_reason"], "plan_exhausted");
    assert_eq!(first_value["actions_executed"], 2);
    assert_eq!(first_value["independent_verifier_settled"], true);
    assert!(first_value["actions"][1]["verification_evidence_ref"]
        .as_str()
        .unwrap()
        .starts_with("artifact://sha256/"));
    assert_eq!(fs::read_to_string(&count).unwrap().trim(), "2");
    let replay = invoke();
    assert!(replay.status.success());
    let replay_value = json(&replay);
    assert_eq!(replay_value["stop_reason"], "no_progress_budget");
    assert_eq!(replay_value["actions_executed"], 0);
    assert_eq!(replay_value["actions_replayed"], 1);
    assert_eq!(fs::read_to_string(&count).unwrap().trim(), "2");
}

#[test]
fn bounded_run_observes_a_manual_pause_before_dispatch() {
    let d = scratch("manual_pause");
    let created = start(d.path(), "90");
    let g = &created["goal"];
    let a = &created["attachment"];
    let paused = run(&[
        "goal",
        "pause",
        g["goal_ref"].as_str().unwrap(),
        "--attachment-id",
        a["attachment_id"].as_str().unwrap(),
        "--expected-goal-revision",
        "1",
        "--expected-attachment-revision",
        "1",
        "--fencing-epoch",
        "1",
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(paused.status.success());
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"true","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","2".repeat(64)),"environment_id":"paused"});
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":g["goal_ref"],"attachment_id":a["attachment_id"],
      "actions":[{"action_id":"paused-verify","action_kind":"verify","role":"verifier","idempotency_key":"paused:verify:v1","cell_binding":binding,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}]})).unwrap()).unwrap();
    let out = run(&[
        "goal",
        "run",
        "--plan",
        plan.to_str().unwrap(),
        "--state-dir",
        d.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "goal_not_active");
    assert_eq!(value["actions_executed"], 0);
}

#[test]
#[cfg(unix)]
fn verifier_failed_exact_command_blocks_even_when_pi_exits_zero() {
    let d = scratch("verifier_evidence");
    let _guard = process_test_lock();
    let created = start(d.path(), "90");
    let workspace = d.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let fake_pi = d.path().join("prose-only-pi.sh");
    fs::write(&fake_pi, "#!/bin/sh\nprintf '%s\n' '{\"type\":\"tool_execution_end\",\"toolName\":\"shuttle_run_allowed_command\",\"isError\":false}'\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"totalTokens\":2}}}'\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n").unwrap();
    let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_pi, perms).unwrap();
    init_snapshot_workspace(&workspace);
    let binding = serde_json::json!({"version":1,"workspace_root":workspace,"allowed_paths":[],"allowed_commands":[{"program":"false","argv_prefix":[]}],"base_oid":"oid","assignment_digest":format!("sha256:{}","3".repeat(64)),"environment_id":"verifier-evidence"});
    let plan = d.path().join("plan.json");
    fs::write(&plan, serde_json::to_vec_pretty(&serde_json::json!({
      "schema_version":"shuttle_goal_run_plan/v1","goal_ref":created["goal"]["goal_ref"],"attachment_id":created["attachment"]["attachment_id"],
      "actions":[{"action_id":"evidence-verify","action_kind":"verify","role":"verifier","idempotency_key":"evidence:verify:v1","cell_binding":binding,"prompt":"verify","expected_artifacts":["verification_result"],"timeout_ms":10000}]})).unwrap()).unwrap();
    let out = Command::new(shuttle())
        .args([
            "goal",
            "run",
            "--plan",
            plan.to_str().unwrap(),
            "--state-dir",
            d.path().to_str().unwrap(),
            "--json",
        ])
        .env("PI_BIN", &fake_pi)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = json(&out);
    assert_eq!(value["stop_reason"], "provider_failure_budget");
    assert_eq!(value["actions"][0]["exit_code"], 126);
    assert_eq!(value["independent_verifier_settled"], false);
}
