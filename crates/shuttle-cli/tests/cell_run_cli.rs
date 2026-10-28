//! Binary-level CLI integration tests for `shuttle cell-run` — the intermediate
//! verification layer between the unit tests in `cell_run.rs` and a full
//! 33-task eval DAG.
//!
//! These spawn the REAL `target/.../shuttle` binary (path injected by cargo as
//! `CARGO_BIN_EXE_shuttle`) in **preflight-only** mode: a binding is supplied but no
//! `--prompt`, so `run_cell` validates the envelope, stands up the mandatory
//! `ScopedPathResolver`, prints the `CellPreflight` JSON, and returns WITHOUT
//! calling `run::supervise`. That keeps the test hermetic — no provider, no pi
//! child, no network, no token spend — while still exercising the exact code
//! paths a real cell entry takes up to the cage boundary: process spawn, clap
//! arg dispatch (`Cmd::CellRun`), envelope parse, canonicalization, fail-closed
//! rejection, and the preflight schema. Runs in milliseconds.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

/// Write a binding JSON into `dir` and return its path.
fn write_binding(dir: &Path, json: &str) -> PathBuf {
    write_binding_named(dir, "binding.json", json)
}

fn write_binding_named(dir: &Path, name: &str, json: &str) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    p
}

/// A unique temp dir under the OS temp root (no external tempfile dep).
fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_cellrun_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", shuttle_protocol::sha256_hex(bytes))
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

struct StrictRuntimeFixture {
    workspace: PathBuf,
    state: PathBuf,
    bundle_root: PathBuf,
    manifest_path: PathBuf,
    bundle_digest: String,
    provider: PathBuf,
    extension: PathBuf,
    marker: PathBuf,
    binding_path: PathBuf,
    binding: serde_json::Value,
}

impl StrictRuntimeFixture {
    fn new(tag: &str, run_id: &str) -> Self {
        let workspace = scratch_dir(tag);
        let state = workspace.join("state");
        let bundle_root = workspace.join("tools");
        let bin = bundle_root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo fake-git-1 ;;;\n  --help) exit 0 ;;;\n  -C) echo base-oid-test ;;;\n  *) exit 0 ;;;\nesac\n".replace(";;;", ";;"),
        )
        .unwrap();
        make_executable(&git);
        std::fs::create_dir_all(bundle_root.join("licenses")).unwrap();
        std::fs::write(bundle_root.join("licenses/NOTICE"), "test only\n").unwrap();
        std::fs::write(
            bundle_root.join("sbom.cdx.json"),
            r#"{"bomFormat":"CycloneDX","specVersion":"1.5"}"#,
        )
        .unwrap();
        let payloads = ["bin/git", "licenses/NOTICE", "sbom.cdx.json"];
        let checksums = payloads
            .iter()
            .map(|name| {
                let digest = sha256(&std::fs::read(bundle_root.join(name)).unwrap());
                format!("{}  {name}\n", digest.trim_start_matches("sha256:"))
            })
            .collect::<String>();
        std::fs::write(bundle_root.join("checksums.sha256"), &checksums).unwrap();
        let platform = serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "libc": if cfg!(target_env = "musl") {
                "musl"
            } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
                "glibc"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "unknown"
            },
        });
        let manifest = serde_json::json!({
            "schema_version": "ea_external_tools_bundle/v1",
            "bundle_id": "cell-run-provider-test",
            "bundle_version": "1.0.0",
            "platform": platform,
            "runtime_abis": ["shuttle-runtime/v1"],
            "profiles": {"cell-supervisor": {"required_tools": ["git"]}},
            "tools": {"git": {
                "path": "bin/git",
                "version": "fake-git-1",
                "sha256": sha256(&std::fs::read(&git).unwrap()),
                "probes": [
                    {"kind": "version", "args": ["--version"], "expected_exit": 0, "stdout_contains": "fake-git-1"},
                    {"kind": "behavior", "args": ["--help"], "expected_exit": 0}
                ]
            }},
            "payload_manifest": {
                "path": "checksums.sha256",
                "sha256": sha256(checksums.as_bytes())
            },
            "source_archives": [],
            "binary_archives": [],
            "license_notices": "licenses/NOTICE",
            "sbom_ref": "sbom.cdx.json"
        });
        let manifest_path = bundle_root.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let marker = workspace.join("provider-spawned");
        let provider = workspace.join("pi");
        std::fs::write(
            &provider,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'pi 1.2.3'; exit 0; fi\nprintf spawned > '{}'\nprintf '%s\\n' '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"usage\":{{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2}}}}}}' '{{\"type\":\"agent_settled\"}}'\n",
                marker.display()
            ),
        )
        .unwrap();
        make_executable(&provider);
        let extension = workspace.join("shuttle-tools.ts");
        std::fs::write(&extension, "export const fixture = true;\n").unwrap();
        let bundle_digest = format!("sha256:{}", "b".repeat(64));
        let descriptor = serde_json::json!({
            "schema_version": "ea_runtime_descriptor/v1",
            "mode": "host_bundle",
            "core": {
                "version": env!("CARGO_PKG_VERSION"),
                "runtime_abi": "shuttle-runtime/v1",
                "executable_path": shuttle_bin(),
                "digest": sha256(&std::fs::read(shuttle_bin()).unwrap())
            },
            "tools": {
                "source": "fixture://tools",
                "distribution_digest": bundle_digest,
                "manifest_digest": sha256(&std::fs::read(&manifest_path).unwrap()),
                "root": bundle_root.display().to_string(),
                "profiles": ["cell-supervisor"]
            },
            "provider": {
                "kind": "pi",
                "version": "1.2.3",
                "binary": provider.display().to_string(),
                "binary_digest": sha256(&std::fs::read(&provider).unwrap()),
                "extension_path": extension.display().to_string(),
                "extension_digest": sha256(&std::fs::read(&extension).unwrap())
            }
        });
        let binding = serde_json::json!({
            "version": 1,
            "workspace_root": workspace.display().to_string(),
            "allowed_paths": [],
            "allowed_commands": [],
            "base_oid": "base-oid-test",
            "assignment_digest": format!("sha256:{}", "a".repeat(64)),
            "runtime_descriptor_ref": "fixture://runtime",
            "runtime_descriptor_digest": "",
            "runtime_descriptor": descriptor,
            "environment_id": format!("env-{tag}"),
            "provider_run_id": run_id,
            "attempt_id": format!("attempt-{tag}")
        });
        let binding_path = workspace.join("binding.json");
        let mut fixture = Self {
            workspace,
            state,
            bundle_root,
            manifest_path,
            bundle_digest,
            provider,
            extension,
            marker,
            binding_path,
            binding,
        };
        fixture.rewrite_binding();
        fixture
    }

    fn rewrite_binding(&mut self) {
        let descriptor: shuttle_protocol::EaRuntimeDescriptor =
            serde_json::from_value(self.binding["runtime_descriptor"].clone()).unwrap();
        self.binding["runtime_descriptor_digest"] =
            serde_json::Value::String(descriptor.digest().unwrap().to_string());
        std::fs::write(
            &self.binding_path,
            serde_json::to_vec_pretty(&self.binding).unwrap(),
        )
        .unwrap();
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(shuttle_bin());
        command
            .arg(subcommand)
            .env("HOME", self.workspace.join("home"))
            .env("SHUTTLE_CELL_STATE_DIR", &self.state)
            .env("SHUTTLE_TOOL_MODE", "strict")
            .env("SHUTTLE_TOOL_BUNDLE_ROOT", &self.bundle_root)
            .env("SHUTTLE_TOOL_MANIFEST", &self.manifest_path)
            .env("SHUTTLE_TOOL_PROFILE", "cell-supervisor")
            .env("SHUTTLE_TOOL_BUNDLE_DIGEST", &self.bundle_digest);
        command
    }

    fn persist_recoverable(&mut self, run_id: &str) -> PathBuf {
        self.rewrite_binding();
        let binding_body = format!(
            "{}\n",
            std::fs::read_to_string(&self.binding_path)
                .unwrap()
                .trim_end_matches('\n')
        );
        std::fs::write(&self.binding_path, &binding_body).unwrap();
        for name in ["runs", "events", "bindings", "sessions", "locks", "usage"] {
            std::fs::create_dir_all(self.state.join(name)).unwrap();
        }
        let file_id = shuttle_protocol::sha256_hex(run_id.as_bytes());
        let events = self.state.join("events").join(format!("{file_id}.jsonl"));
        let record = serde_json::json!({
            "schema_version": "ea_cell_run/v1",
            "provider_run_id": run_id,
            "attempt_id": self.binding["attempt_id"],
            "state": "recoverable",
            "revision": 1,
            "created_at_ms": 1,
            "updated_at_ms": 1,
            "binding_ref": self.binding_path.display().to_string(),
            "binding_digest": sha256(binding_body.as_bytes()),
            "assignment_digest": self.binding["assignment_digest"],
            "runtime_descriptor_ref": self.binding["runtime_descriptor_ref"],
            "runtime_descriptor_digest": self.binding["runtime_descriptor_digest"],
            "workspace_root": self.workspace.display().to_string(),
            "environment_id": self.binding["environment_id"],
            "base_oid": "base-oid-test",
            "session_ref": self.state.join("sessions").join(&file_id).display().to_string(),
            "pid": null,
            "pgid": null,
            "result_ref": null,
            "receipt": null,
            "error_class": null
        });
        std::fs::write(
            self.state.join("runs").join(format!("{file_id}.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        events
    }
}

#[test]
fn preflight_valid_binding_emits_contract_and_boundary() {
    let ws = scratch_dir("valid");
    // workspace_root must exist to canonicalize; allowed_paths are non-empty so
    // writes are NOT fully fail-closed.
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": ["src/**", "docs/**"],
            "allowed_commands": [{{"program": "git", "argv_prefix": ["status"]}}],
            "base_oid": "abc123",
            "assignment_digest": "sha256:deadbeef",
            "environment_id": "env-it-1"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);

    let out = Command::new(shuttle_bin())
        .args(["cell-run", "--binding", bp.to_str().unwrap()])
        .output()
        .expect("spawn shuttle cell-run");

    assert!(
        out.status.success(),
        "preflight-only run must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout must be the CellPreflight JSON");

    assert_eq!(json["contract"], "cell_binding/v1");
    assert_eq!(json["allowed_command_count"], 1);
    assert_eq!(json["base_oid"], "abc123");
    assert_eq!(json["assignment_digest"], "sha256:deadbeef");
    assert_eq!(json["project_mode"], "legacy_unbound");
    assert!(json.get("project_id").is_none());
    assert!(json.get("project_binding_digest").is_none());
    assert_eq!(json["environment_id"], "env-it-1");
    // Non-empty allowlist ⇒ writes are constrained but not fully fail-closed.
    assert_eq!(json["write_fail_closed"], false);
    // shuttle self-fills its own binary digest (shuttle-local) when the binding left a placeholder.
    assert!(json["shuttle_binary_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    // workspace_root is canonicalized (starts from the real temp root).
    assert!(json["workspace_root"]
        .as_str()
        .unwrap()
        .contains("shuttle_cellrun_it_valid"));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn preflight_empty_allowlist_is_write_fail_closed() {
    let ws = scratch_dir("failclosed");
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": [],
            "allowed_commands": [{{"program": "git", "argv_prefix": ["status"]}}],
            "base_oid": "oid0",
            "assignment_digest": "sha256:00",
            "environment_id": "env-fc"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);

    let out = Command::new(shuttle_bin())
        .args(["cell-run", "--binding", bp.to_str().unwrap()])
        .output()
        .expect("spawn shuttle cell-run");
    assert!(out.status.success());
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    // Empty allowlist ⇒ every write is forbidden by design (shuttle's own write-boundary rule).
    assert_eq!(json["write_fail_closed"], true);
    assert_eq!(json["allowed_paths"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn strict_tool_mode_without_descriptor_fails_before_provider_spawn() {
    let ws = scratch_dir("strict-no-descriptor");
    let marker = ws.join("provider-spawned");
    let pi = ws.join("fake-pi");
    std::fs::write(&pi, format!("#!/bin/sh\nprintf spawned > {:?}\n", marker)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": [],
            "allowed_commands": [],
            "base_oid": "oid0",
            "assignment_digest": "sha256:00",
            "environment_id": "env-strict-no-descriptor"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);

    let out = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "must not spawn",
        ])
        .env("SHUTTLE_TOOL_MODE", "strict")
        .env("PI_BIN", &pi)
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert!(
        !marker.exists(),
        "provider must not spawn before strict preflight"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("runtime_dependency_mismatch"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn explicit_legacy_tool_mode_preserves_descriptor_free_preflight() {
    let ws = scratch_dir("legacy-no-descriptor");
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": [],
            "allowed_commands": [],
            "base_oid": "oid0",
            "assignment_digest": "sha256:00",
            "environment_id": "env-legacy-no-descriptor"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let out = Command::new(shuttle_bin())
        .args(["cell-run", "--binding", bp.to_str().unwrap()])
        .env("SHUTTLE_TOOL_MODE", "legacy")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn strict_runtime_rejects_provider_and_extension_drift_before_provider_spawn() {
    for drift in [
        "binary_path",
        "binary_digest",
        "version",
        "extension_path",
        "extension_digest",
    ] {
        let mut fixture = StrictRuntimeFixture::new(
            &format!("provider-drift-{drift}"),
            &format!("run-provider-drift-{drift}"),
        );
        match drift {
            "binary_path" => {}
            "binary_digest" => {
                std::fs::write(&fixture.provider, "#!/bin/sh\necho tampered\n").unwrap()
            }
            "version" => {
                fixture.binding["runtime_descriptor"]["provider"]["version"] =
                    serde_json::Value::String("9.9.9".into());
                fixture.rewrite_binding();
            }
            "extension_path" => {
                let conflicting = fixture.workspace.join("ambient-extension.ts");
                std::fs::write(&conflicting, "export const ambient = true;\n").unwrap();
                std::fs::write(
                    fixture.workspace.join("shuttle.toml"),
                    format!("extension = {:?}\n", conflicting.display().to_string()),
                )
                .unwrap();
            }
            "extension_digest" => {
                std::fs::write(&fixture.extension, "export const tampered = true;\n").unwrap();
            }
            _ => unreachable!(),
        }
        let mut command = fixture.command("cell-run");
        command.args([
            "--binding",
            fixture.binding_path.to_str().unwrap(),
            "--prompt",
            "must not spawn",
        ]);
        if drift == "binary_path" {
            command.env("PI_BIN", fixture.workspace.join("different-pi"));
        } else {
            command.env("PI_BIN", &fixture.provider);
        }
        let out = command.output().unwrap();
        assert!(
            !out.status.success(),
            "{drift} unexpectedly passed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("runtime_dependency_mismatch"),
            "{drift} did not report runtime mismatch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!fixture.marker.exists(), "{drift} spawned the provider");
        let _ = std::fs::remove_dir_all(&fixture.workspace);
    }
}

#[test]
fn recovery_revalidates_extension_digest_before_provider_spawn() {
    let run_id = "run-recovery-extension-drift";
    let mut fixture = StrictRuntimeFixture::new("recovery-extension-drift", run_id);
    let events = fixture.persist_recoverable(run_id);
    std::fs::write(&fixture.extension, "export const tampered = true;\n").unwrap();

    let out = fixture
        .command("cell-recover")
        .args(["--run-id", run_id, "--prompt", "must not spawn"])
        .env("PI_BIN", &fixture.provider)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("provider extension digest drift"));
    assert!(
        !fixture.marker.exists(),
        "recovery spawned the drifted provider"
    );
    assert!(
        std::fs::read_to_string(events)
            .unwrap()
            .contains("\"kind\":\"recovery_started\""),
        "recovery path did not record its durable marker"
    );
    let _ = std::fs::remove_dir_all(&fixture.workspace);
}

#[test]
fn wrong_version_binding_is_rejected_nonzero() {
    let ws = scratch_dir("badver");
    // version 2 is not cell_binding/v1 → parse must reject fail-closed.
    let binding = format!(
        r#"{{
            "version": 2,
            "workspace_root": {root:?},
            "allowed_paths": ["src/**"],
            "allowed_commands": [{{"program": "git", "argv_prefix": ["status"]}}],
            "base_oid": "x",
            "assignment_digest": "y",
            "environment_id": "z"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);

    let out = Command::new(shuttle_bin())
        .args(["cell-run", "--binding", bp.to_str().unwrap()])
        .output()
        .expect("spawn shuttle cell-run");
    assert!(
        !out.status.success(),
        "a non-v1 envelope must abort before any workspace access"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cell_binding rejected"),
        "expected fail-closed rejection message, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn durable_lifecycle_observe_collect_and_terminal_replay() {
    let ws = scratch_dir("lifecycle");
    let state = ws.join("state");
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": [],
            "allowed_commands": [],
            "base_oid": "oid0",
            "assignment_digest": "sha256:00",
            "environment_id": "env-life",
            "provider_run_id": "run/life:1",
            "attempt_id": "attempt-life"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":10,\"output\":4,\"cacheRead\":3,\"cacheWrite\":2,\"reasoning\":1,\"totalTokens\":20,\"cost\":{\"input\":0.1,\"output\":0.2,\"cacheRead\":0.01,\"cacheWrite\":0.02,\"total\":0.33}}}}' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let run = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "finish",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", &pi)
        .env("SHUTTLE_COST_COHORT_ID", "cohort-life")
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let observed = Command::new(shuttle_bin())
        .args(["cell-observe", "--run-id", "run/life:1"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(observed.status.success());
    let state_json: serde_json::Value = serde_json::from_slice(&observed.stdout).unwrap();
    assert_eq!(state_json["state"], "done");
    assert_eq!(state_json["attempt_id"], "attempt-life");
    assert!(state_json["session_ref"]
        .as_str()
        .unwrap()
        .contains("sessions"));

    let collected = Command::new(shuttle_bin())
        .args(["cell-collect", "--run-id", "run/life:1"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&collected.stdout).unwrap();
    assert_eq!(receipt["provider_status"], "completed");
    assert_eq!(receipt["provider_run_id"], "run/life:1");
    assert_eq!(receipt.as_object().unwrap().len(), 5);

    let usage = Command::new(shuttle_bin())
        .args(["cell-usage", "--run-id", "run/life:1"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        usage.status.success(),
        "{}",
        String::from_utf8_lossy(&usage.stderr)
    );
    let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["schema_version"], "ea_usage_receipt/v1");
    assert_eq!(usage["provider_run_id"], "run/life:1");
    assert_eq!(usage["attempt_id"], "attempt-life");
    assert_eq!(usage["cohort_id"], "cohort-life");
    assert_eq!(usage["parent_attempt_id"], serde_json::Value::Null);
    assert_eq!(usage["terminal"]["status"], "completed");
    assert_eq!(usage["terminal"]["reason_code"], "settled");
    assert_eq!(usage["usage"]["input_tokens"], 10);
    assert_eq!(usage["usage"]["total_tokens"], 20);
    assert_eq!(usage["usage"]["reasoning_tokens"], 1);
    assert_eq!(usage["provider_cost_observation"]["total"], 0.33);
    assert_eq!(usage["completeness"]["status"], "complete");
    assert_eq!(usage["provenance"]["event_count"], 2);
    assert_eq!(receipt.as_object().unwrap().len(), 5);

    let replay = Command::new(shuttle_bin())
        .args([
            "cell-recover",
            "--run-id",
            "run/life:1",
            "--prompt",
            "must not rerun",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", ws.join("missing-pi"))
        .output()
        .unwrap();
    assert!(replay.status.success());
    let replay_receipt: serde_json::Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay_receipt, receipt);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn spawn_failure_is_terminal_and_has_unavailable_usage() {
    let ws = scratch_dir("spawn-failure");
    let state = ws.join("state");
    let binding = format!(
        r#"{{"version":1,"workspace_root":{root:?},"allowed_paths":[],"allowed_commands":[],"base_oid":"oid0","assignment_digest":"sha256:00","environment_id":"env-spawn","provider_run_id":"run-spawn","attempt_id":"attempt-spawn"}}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let run = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "x",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", ws.join("missing-pi"))
        .output()
        .unwrap();
    assert!(!run.status.success());
    let collected = Command::new(shuttle_bin())
        .args(["cell-collect", "--run-id", "run-spawn"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    let provider: serde_json::Value = serde_json::from_slice(&collected.stdout).unwrap();
    assert_eq!(provider["provider_status"], "failed");
    assert_eq!(provider["terminal"], true);
    assert_eq!(provider.as_object().unwrap().len(), 5);
    let usage = Command::new(shuttle_bin())
        .args(["cell-usage", "--run-id", "run-spawn"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(usage.status.success());
    let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["terminal"]["status"], "failed");
    assert_eq!(usage["usage"]["total_tokens"], serde_json::Value::Null);
    assert_eq!(usage["completeness"]["status"], "unavailable");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn soft_timeout_is_terminal_and_never_forges_usage() {
    let ws = scratch_dir("timeout");
    let state = ws.join("state");
    let binding = format!(
        r#"{{"version":1,"workspace_root":{root:?},"allowed_paths":[],"allowed_commands":[],"base_oid":"oid0","assignment_digest":"sha256:00","environment_id":"env-timeout","provider_run_id":"run-timeout","attempt_id":"attempt-timeout"}}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nwhile :; do printf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":5,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":6}}}'; sleep 0.05; done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let run = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "x",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("SHUTTLE_SOFT_TIMEOUT_MS", "2000")
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(124));
    let usage = Command::new(shuttle_bin())
        .args(["cell-usage", "--run-id", "run-timeout"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(usage.status.success());
    let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["terminal"]["status"], "timed_out");
    assert_eq!(usage["terminal"]["exit_code"], 124);
    assert_eq!(usage["terminal"]["reason_code"], "soft_timeout");
    let observed = &usage["usage"]["total_tokens"];
    assert!(observed.is_null() || observed.as_u64().is_some_and(|value| value > 0));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn provider_exit_124_is_failure_not_supervisor_timeout() {
    let ws = scratch_dir("provider-exit-124");
    let state = ws.join("state");
    let binding = format!(
        r#"{{"version":1,"workspace_root":{root:?},"allowed_paths":[],"allowed_commands":[],"base_oid":"oid0","assignment_digest":"sha256:00","environment_id":"env-child-124","provider_run_id":"run-child-124","attempt_id":"attempt-child-124"}}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let pi = ws.join("fake-pi");
    std::fs::write(&pi, "#!/bin/sh\nexit 124\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let run = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "x",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(124));
    let usage = Command::new(shuttle_bin())
        .args(["cell-usage", "--run-id", "run-child-124"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(usage.status.success());
    let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["terminal"]["status"], "failed");
    assert_eq!(usage["terminal"]["exit_code"], 124);
    assert_eq!(usage["terminal"]["reason_code"], "child_exit");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn retry_attempts_persist_independent_receipts_and_exact_parent_link() {
    let ws = scratch_dir("retry-lineage");
    let state = ws.join("state");
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":2,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"reasoning\":1,\"totalTokens\":3}}}' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let run_attempt = |run_id: &str, attempt_id: &str, parent: Option<&str>| {
        let binding = format!(
            r#"{{"version":1,"workspace_root":{root:?},"allowed_paths":[],"allowed_commands":[],"base_oid":"oid0","assignment_digest":"sha256:00","environment_id":"env-retry","provider_run_id":{run_id:?},"attempt_id":{attempt_id:?}}}"#,
            root = ws.display().to_string()
        );
        let bp = write_binding_named(&ws, &format!("{attempt_id}.json"), &binding);
        let mut command = Command::new(shuttle_bin());
        command
            .args([
                "cell-run",
                "--binding",
                bp.to_str().unwrap(),
                "--prompt",
                "x",
            ])
            .env("SHUTTLE_CELL_STATE_DIR", &state)
            .env("SHUTTLE_COST_COHORT_ID", "cohort-retry")
            .env("PI_BIN", &pi);
        if let Some(parent) = parent {
            command.env("SHUTTLE_PARENT_ATTEMPT_ID", parent);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run_attempt("run-retry-1", "attempt-retry-1", None);
    run_attempt("run-retry-2", "attempt-retry-2", Some("attempt-retry-1"));

    let read_usage = |run_id: &str| {
        let output = Command::new(shuttle_bin())
            .args(["cell-usage", "--run-id", run_id])
            .env("SHUTTLE_CELL_STATE_DIR", &state)
            .env("SHUTTLE_COST_COHORT_ID", "must-not-affect-readback")
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let first = read_usage("run-retry-1");
    let second = read_usage("run-retry-2");
    assert_eq!(first["receipt_id"], "shuttle-usage:run-retry-1");
    assert_eq!(second["receipt_id"], "shuttle-usage:run-retry-2");
    assert_eq!(first["cohort_id"], "cohort-retry");
    assert_eq!(second["cohort_id"], "cohort-retry");
    assert_eq!(first["parent_attempt_id"], serde_json::Value::Null);
    assert_eq!(second["parent_attempt_id"], "attempt-retry-1");
    assert_eq!(first["attempt_id"], "attempt-retry-1");
    assert_eq!(second["attempt_id"], "attempt-retry-2");

    let conflicting = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            ws.join("attempt-retry-2.json").to_str().unwrap(),
            "--prompt",
            "must not replay",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("SHUTTLE_COST_COHORT_ID", "cohort-retry")
        .env("SHUTTLE_PARENT_ATTEMPT_ID", "wrong-parent")
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    assert!(!conflicting.status.success());
    assert!(String::from_utf8_lossy(&conflicting.stderr).contains("durable cell lineage"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cancel_is_idempotent_and_persists_cancelled_receipt() {
    let ws = scratch_dir("cancel");
    let state = ws.join("state");
    let binding = format!(
        r#"{{
            "version": 1,
            "workspace_root": {root:?},
            "allowed_paths": [],
            "allowed_commands": [],
            "base_oid": "oid0",
            "assignment_digest": "sha256:00",
            "environment_id": "env-cancel",
            "provider_run_id": "run-cancel",
            "attempt_id": "attempt-cancel"
        }}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let pi = ws.join("fake-pi");
    let late_marker = ws.join("late-marker");
    std::fs::write(
        &pi,
        "#!/bin/sh\n(sleep 2; printf late > \"$SHUTTLE_LATE_MARKER\") &\nwait\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut run = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "wait",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", &pi)
        .env("SHUTTLE_LATE_MARKER", &late_marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut active = false;
    // The supervisor's preflight (binding digest + shuttle binary digest) can take
    // seconds under parallel test load; poll well past the 2s pi lifetime.
    for _ in 0..300 {
        let observed = Command::new(shuttle_bin())
            .args(["cell-observe", "--run-id", "run-cancel"])
            .env("SHUTTLE_CELL_STATE_DIR", &state)
            .output()
            .unwrap();
        if observed.status.success() {
            let value: serde_json::Value = serde_json::from_slice(&observed.stdout).unwrap();
            if value["state"] == "active" {
                active = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(active, "run never reached active state");
    let cancel = |state: &Path| {
        Command::new(shuttle_bin())
            .args(["cell-cancel", "--run-id", "run-cancel", "--grace-ms", "20"])
            .env("SHUTTLE_CELL_STATE_DIR", state)
            .output()
            .unwrap()
    };
    let before = Command::new(shuttle_bin())
        .args(["cell-observe", "--run-id", "run-cancel"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    let before: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    let pgid = before["pgid"].as_i64().unwrap() as i32;
    let first = cancel(&state);
    let second = cancel(&state);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(second.status.success());
    let a: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(a, b);
    assert_eq!(a["provider_status"], "cancelled");
    assert_eq!(a["terminal"], true);
    let _ = run.wait();
    let usage = Command::new(shuttle_bin())
        .args(["cell-usage", "--run-id", "run-cancel"])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(usage.status.success());
    let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["terminal"]["status"], "cancelled");
    assert_eq!(usage["usage"]["total_tokens"], serde_json::Value::Null);
    assert_eq!(usage["completeness"]["status"], "unavailable");
    std::thread::sleep(std::time::Duration::from_millis(2200));
    assert!(!late_marker.exists(), "descendant wrote after cancellation");
    #[cfg(unix)]
    unsafe {
        assert_ne!(
            libc::kill(-pgid, 0),
            0,
            "process group survived cancellation"
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn doctor_is_machine_readable_and_never_emits_credentials() {
    let ws = scratch_dir("doctor");
    let output = Command::new(shuttle_bin())
        .arg("cell-doctor")
        .env("SHUTTLE_CELL_STATE_DIR", ws.join("state"))
        .env("SHUTTLE_PROVIDER_KEY", "do-not-print-this")
        .output()
        .unwrap();
    assert!(output.status.success());
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(!raw.contains("do-not-print-this"));
    let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(report["schema_version"], "ea_cell_doctor/v1");
    assert_eq!(report["provider_credential_ref_present"], true);
    assert_eq!(report["credential_source_kind"], "environment");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn doctor_detects_models_config_without_printing_key_or_provider() {
    let ws = scratch_dir("doctor-models");
    let agent_dir = ws.join("pi-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{"providers":{"private-provider":{"apiKey":"secret-reference"}}}"#,
    )
    .unwrap();
    std::fs::write(agent_dir.join("auth.json"), "{}").unwrap();
    let output = Command::new(shuttle_bin())
        .arg("cell-doctor")
        .env("SHUTTLE_CELL_STATE_DIR", ws.join("state"))
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env_remove("SHUTTLE_PROVIDER_KEY")
        .env_remove("SHUTTLE_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let raw = String::from_utf8(output.stdout).unwrap();
    assert!(!raw.contains("secret-reference"));
    assert!(!raw.contains("private-provider"));
    let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(report["provider_credential_ref_present"], true);
    assert_eq!(report["credential_source_kind"], "models_config");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn durable_cell_run_inherits_pi_coding_agent_dir() {
    let ws = scratch_dir("pi-agent-dir");
    let state = ws.join("state");
    let agent_dir = ws.join("pi-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let observed = ws.join("observed-agent-dir");
    let binding = format!(
        r#"{{"version":1,"workspace_root":{root:?},"allowed_paths":[],"allowed_commands":[],"base_oid":"oid0","assignment_digest":"sha256:00","environment_id":"env-inherit","provider_run_id":"run-inherit"}}"#,
        root = ws.display().to_string()
    );
    let bp = write_binding(&ws, &binding);
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf '%s' \"$PI_CODING_AGENT_DIR\" > \"$SHUTTLE_TEST_OBSERVED\"\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--binding",
            bp.to_str().unwrap(),
            "--prompt",
            "x",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", &state)
        .env("PI_BIN", &pi)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("SHUTTLE_TEST_OBSERVED", &observed)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(&observed).unwrap(),
        agent_dir.display().to_string()
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn external_launch_bundle_opt_in_exports_coding_attempt_result() {
    let ws = scratch_dir("coding-result");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "SHUTTLE Test"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "shuttle@example.invalid"])
        .current_dir(&ws)
        .status()
        .unwrap();
    std::fs::write(
        ws.join(".gitignore"),
        ".shuttle/\nstate/\nfake-pi\nbundle.json\n",
    )
    .unwrap();
    std::fs::write(ws.join("base.txt"), "base\n").unwrap();
    Command::new("git")
        .args(["add", "base.txt", ".gitignore"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"])
        .current_dir(&ws)
        .status()
        .unwrap();
    let base = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::fs::write(ws.join("result.txt"), "result\n").unwrap();
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let bundle = ws.join("bundle.json");
    std::fs::write(&bundle, format!(r#"{{"version":1,"workspace_root":{root:?},"environment_id":"env-export","launch_intent":{{"launch_intent_id":"li","attempt_id":"attempt-export","runtime_id":"runtime","package_id":"delivery-export","idempotency_key":"run-export","agent_type":"shuttle","execution_profile_ref":"profile://shuttle/cell","context_manifest_ref":"cm","resource_claim_refs":["claim"],"deadline":"2099-01-01T00:00:00Z","requested_by":"test"}},"context_manifest":{{"context_manifest_id":"cm","context_manifest_digest":"sha256:{digest}","tool_allowlist":[],"source_versions":[{{"source_id":"repo","digest":"{base}"}}]}},"resource_claim":{{"resource_type":"file_write_scope","resource_keys":["result.txt"],"claim_mode":"exclusive_write","lease_epoch":1}},"prompt":"finish","coding_result_export":{{"delivery_id":"delivery-export","workspace_ref":"workspace://delivery-export","repo_id":"repo","change_id":"change-export","generated_tests":[],"unresolved_items":[]}}}}"#, root=ws.display().to_string(), digest="a".repeat(64))).unwrap();
    let output = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--launch-bundle",
            bundle.to_str().unwrap(),
            "--receipt-out",
            "-",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", ws.join("state"))
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    let result_path = PathBuf::from(
        receipt["result_ref"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "missing result_ref: {receipt:?}; stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
            .trim_start_matches("file://"),
    );
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    assert_eq!(result["schema_version"], "coding_attempt_result/v1");
    assert_eq!(result["changed_files"], serde_json::json!(["result.txt"]));
    assert_eq!(result["commits"], serde_json::json!([]));
    assert_eq!(
        result["provider_terminal_receipt"]["provider_status"],
        "completed"
    );
    // Schema-exact coding_attempt_result/v1 no longer carries shuttle-local
    // runtime_descriptor_* fields (N4c): assert they are absent, not null.
    assert!(!result
        .as_object()
        .unwrap()
        .contains_key("runtime_descriptor_ref"));
    assert!(!result
        .as_object()
        .unwrap()
        .contains_key("runtime_descriptor_digest"));
    assert_eq!(result.as_object().unwrap().len(), 14);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn external_launch_bundle_exports_from_non_git_workspace_snapshot() {
    let ws = scratch_dir("coding-result-non-git");
    std::fs::write(
        ws.join(".gitignore"),
        ".shuttle/\nstate/\nfake-pi\nbundle.json\n",
    )
    .unwrap();
    std::fs::write(ws.join("base.txt"), "base\n").unwrap();
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf 'result\n' > result.txt\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let bundle = ws.join("bundle.json");
    std::fs::write(&bundle, format!(r#"{{"version":1,"workspace_root":{root:?},"environment_id":"env-export-non-git","launch_intent":{{"launch_intent_id":"li","attempt_id":"attempt-export-non-git","runtime_id":"runtime","package_id":"delivery-export","idempotency_key":"run-export-non-git","agent_type":"shuttle","execution_profile_ref":"profile://shuttle/cell","context_manifest_ref":"cm","resource_claim_refs":["claim"],"deadline":"2099-01-01T00:00:00Z","requested_by":"test"}},"context_manifest":{{"context_manifest_id":"cm","context_manifest_digest":"sha256:{digest}","tool_allowlist":[],"source_versions":[{{"source_id":"repo","digest":"{base}"}}]}},"resource_claim":{{"resource_type":"file_write_scope","resource_keys":["result.txt"],"claim_mode":"exclusive_write","lease_epoch":1}},"prompt":"finish","coding_result_export":{{"delivery_id":"delivery-export","workspace_ref":"workspace://delivery-export","repo_id":"repo","change_id":"change-export","generated_tests":[],"unresolved_items":[]}}}}"#, root=ws.display().to_string(), digest="a".repeat(64), base="b".repeat(40))).unwrap();
    let output = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--launch-bundle",
            bundle.to_str().unwrap(),
            "--receipt-out",
            "-",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", ws.join("state"))
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        receipt["provider_status"],
        "completed",
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result_path = PathBuf::from(
        receipt["result_ref"]
            .as_str()
            .unwrap()
            .trim_start_matches("file://"),
    );
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    assert_eq!(result["changed_files"], serde_json::json!(["result.txt"]));
    assert_eq!(result["commits"], serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn external_launch_bundle_rejects_uncommitted_out_of_scope_change() {
    let ws = scratch_dir("coding-result-scope-violation");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "SHUTTLE Test"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "shuttle@example.invalid"])
        .current_dir(&ws)
        .status()
        .unwrap();
    std::fs::write(
        ws.join(".gitignore"),
        ".shuttle/\nstate/\nfake-pi\nbundle.json\n",
    )
    .unwrap();
    std::fs::write(ws.join("base.txt"), "base\n").unwrap();
    Command::new("git")
        .args(["add", "base.txt", ".gitignore"])
        .current_dir(&ws)
        .status()
        .unwrap();
    Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"])
        .current_dir(&ws)
        .status()
        .unwrap();
    let base = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ws)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::fs::write(ws.join("outside.txt"), "escape\n").unwrap();
    let pi = ws.join("fake-pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\nprintf '%s\n' '{\"type\":\"agent_settled\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let bundle = ws.join("bundle.json");
    std::fs::write(&bundle, format!(r#"{{"version":1,"workspace_root":{root:?},"environment_id":"env-export-scope","launch_intent":{{"launch_intent_id":"li","attempt_id":"attempt-export-scope","runtime_id":"runtime","package_id":"delivery-export","idempotency_key":"run-export-scope","agent_type":"shuttle","execution_profile_ref":"profile://shuttle/cell","context_manifest_ref":"cm","resource_claim_refs":["claim"],"deadline":"2099-01-01T00:00:00Z","requested_by":"test"}},"context_manifest":{{"context_manifest_id":"cm","context_manifest_digest":"sha256:{digest}","tool_allowlist":[],"source_versions":[{{"source_id":"repo","digest":"{base}"}}]}},"resource_claim":{{"resource_type":"file_write_scope","resource_keys":["result.txt"],"claim_mode":"exclusive_write","lease_epoch":1}},"prompt":"finish","coding_result_export":{{"delivery_id":"delivery-export","workspace_ref":"workspace://delivery-export","repo_id":"repo","change_id":"change-export","generated_tests":[],"unresolved_items":[]}}}}"#, root=ws.display().to_string(), digest="a".repeat(64))).unwrap();
    let output = Command::new(shuttle_bin())
        .args([
            "cell-run",
            "--launch-bundle",
            bundle.to_str().unwrap(),
            "--receipt-out",
            "-",
        ])
        .env("SHUTTLE_CELL_STATE_DIR", ws.join("state"))
        .env("PI_BIN", &pi)
        .output()
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["provider_status"], "failed");
    assert!(receipt["result_ref"].is_null());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "coding_result_export_failed: coding result export found write-scope violations: outside.txt"
    ));
    assert!(
        !ws.join(".shuttle/receipts/run-export-scope.coding-attempt-result.json")
            .exists(),
        "scope violation must not land a coding result artifact"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
