use std::process::Command;

fn shuttle() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

#[test]
fn tools_doctor_json_is_deterministic_and_secret_free_in_legacy_mode() {
    let run = || {
        Command::new(shuttle())
            .args(["tools", "doctor", "--json"])
            .env_remove("SHUTTLE_TOOL_MODE")
            .env("OPENAI_API_KEY", "must-not-leak")
            .output()
            .unwrap()
    };
    let first = run();
    let second = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    let value: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["schema_version"], "shuttle_tools_doctor/v1");
    assert_eq!(value["mode"], "legacy");
    assert!(!String::from_utf8(first.stdout)
        .unwrap()
        .contains("must-not-leak"));
}

#[test]
fn tools_doctor_rejects_incomplete_strict_configuration_without_path_fallback() {
    let output = Command::new(shuttle())
        .args(["tools", "doctor", "--json"])
        .env("SHUTTLE_TOOL_MODE", "strict")
        .env_remove("SHUTTLE_TOOL_BUNDLE_ROOT")
        .env_remove("SHUTTLE_TOOL_MANIFEST")
        .env_remove("SHUTTLE_TOOL_PROFILE")
        .env_remove("SHUTTLE_TOOL_BUNDLE_DIGEST")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("strict mode requires"), "{stderr}");
}
