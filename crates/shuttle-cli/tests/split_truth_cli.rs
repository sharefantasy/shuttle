//! Binary-level acceptance tests for G10 split truth.
//!
//! These tests drive the real `shuttle index` and `shuttle split --derive-edges` commands
//! over disposable git repositories. They prove that a caller-owned fresh
//! artifact can produce advisory task edges and bounded touch hints, while a
//! weak, stale, or absent input never authorizes hard scheduling.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn ctags_present() -> bool {
    Command::new("ctags")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_split_truth_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_git(root: &Path) -> String {
    git(root, &["init", "-q", "--template="]);
    git(root, &["config", "user.email", "g10@test"]);
    git(root, &["config", "user.name", "G10 Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "seed"]);
    head(root)
}

fn head(root: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn artifact_path(root: &Path) -> PathBuf {
    root.parent().unwrap().join(format!(
        "{}.symbols.json",
        root.file_name().unwrap().to_string_lossy()
    ))
}

fn build_index(root: &Path, revision: &str, artifact: &Path) {
    let output = Command::new(shuttle_bin())
        .args([
            "index",
            "--refresh",
            "--root",
            root.to_str().unwrap(),
            "--head",
            revision,
            "--out",
            artifact.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn derive(root: &Path, artifact: &Path, revision: &str, extra: &[&str]) -> serde_json::Value {
    derive_plan(root, &root.join("plan.json"), artifact, revision, extra)
}

fn derive_plan(
    root: &Path,
    plan_path: &Path,
    artifact: &Path,
    revision: &str,
    extra: &[&str],
) -> serde_json::Value {
    let mut args = vec![
        "split",
        "--derive-edges",
        "--plan",
        plan_path.to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
        "--symbol-index",
        artifact.to_str().unwrap(),
        "--index-head",
        revision,
        "--json",
    ];
    args.extend_from_slice(extra);
    let output = Command::new(shuttle_bin()).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "derive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "derive output is not JSON: {error}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn task<'a>(result: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    result["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == id)
        .unwrap()
}

fn missing_or_empty_array(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .is_none_or(|field| field.as_array().is_some_and(Vec::is_empty))
}

fn seed_chain(root: &Path) {
    write(root, "core.py", "def core():\n    return 1\n");
    write(
        root,
        "wrapper.py",
        "from core import core\n\ndef wrapper():\n    return core()\n",
    );
    write(
        root,
        "check.py",
        "from wrapper import wrapper\n\ndef check():\n    return wrapper()\n",
    );
    write(
        root,
        "plan.json",
        r#"{
  "version": "split-derive/v1",
  "goal": "chain",
  "tasks": [
    {"id": "core", "write_symbols": [{"name": "core", "path": "core.py"}]},
    {"id": "wrapper", "write_symbols": [{"name": "wrapper", "path": "wrapper.py"}]},
    {"id": "check", "write_symbols": [{"name": "check", "path": "check.py"}]}
  ]
}"#,
    );
}

#[test]
fn derive_edges_cli_emits_exact_reverse_closure_without_mutating_plan() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping split truth CLI E2E");
        return;
    }
    let root = scratch("chain");
    seed_chain(&root);
    let revision = seed_git(&root);
    let artifact = artifact_path(&root);
    build_index(&root, &revision, &artifact);
    let plan_before = std::fs::read(root.join("plan.json")).unwrap();

    let first = derive(&root, &artifact, &revision, &[]);
    let second = derive(&root, &artifact, &revision, &[]);
    assert_eq!(first, second, "derivation must be deterministic");
    assert_eq!(first["index_status"], "fresh");
    assert_eq!(first["fallback_required"], false);
    assert_eq!(first["acyclic"], true);
    let edges: BTreeSet<(String, String, String)> = first["hard_edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                edge["from"].as_str().unwrap().to_string(),
                edge["to"].as_str().unwrap().to_string(),
                edge["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        BTreeSet::from([
            ("core".into(), "wrapper".into(), "import_directed".into()),
            ("wrapper".into(), "check".into(), "import_directed".into()),
        ])
    );
    assert_eq!(
        task(&first, "core")["touch_hint"],
        serde_json::json!(["check.py", "core.py", "wrapper.py"])
    );
    assert_eq!(
        std::fs::read(root.join("plan.json")).unwrap(),
        plan_before,
        "derive mode is advisory and must not rewrite its input"
    );
    assert!(
        !root.join(".shuttle").exists(),
        "derive must not own cache paths"
    );

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn derive_edges_cli_bounds_fanout_and_keeps_weak_edges_advisory() {
    if !ctags_present() {
        return;
    }
    let root = scratch("bounded_weak");
    write(root.as_path(), "core.py", "def core():\n    return 1\n");
    write(
        root.as_path(),
        "a.py",
        "from core import core\ndef a(): return core()\n",
    );
    write(
        root.as_path(),
        "b.py",
        "from core import core\ndef b(): return core()\n",
    );
    write(root.as_path(), "weak.py", "def weak(): return core()\n");
    write(
        root.as_path(),
        "plan.json",
        r#"{
  "version": "split-derive/v1",
  "goal": "bounded weak",
  "tasks": [
    {"id": "core", "write_symbols": [{"name": "core", "path": "core.py"}]},
    {"id": "weak", "write_symbols": [{"name": "weak", "path": "weak.py"}]}
  ]
}"#,
    );
    let revision = seed_git(&root);
    let artifact = artifact_path(&root);
    build_index(&root, &revision, &artifact);
    let result = derive(&root, &artifact, &revision, &["--max-fanout", "1"]);

    assert_eq!(result["truncated"], true);
    assert_eq!(result["fallback_required"], true);
    assert_eq!(result["degraded_reason"], "closure-truncated");
    assert!(result["truncation_reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("max_fanout")));
    assert!(missing_or_empty_array(&result, "hard_edges"));
    let weak = result["weak_hints"].as_array().unwrap();
    assert_eq!(weak.len(), 1, "weak edge should remain visible: {result}");
    assert_eq!(weak[0]["from"], "core");
    assert_eq!(weak[0]["to"], "weak");
    assert_eq!(weak[0]["kind"], "name_match");
    assert!(
        task(&result, "core")["touch_hint"]
            .as_array()
            .unwrap()
            .len()
            <= 2
    );

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn derive_edges_cli_stale_and_absent_preserve_only_explicit_file_fallback() {
    if !ctags_present() {
        return;
    }
    let root = scratch("degrade");
    write(root.as_path(), "core.py", "def core():\n    return 1\n");
    write(
        root.as_path(),
        "plan.json",
        r#"{
  "version": "split-derive/v1",
  "goal": "degrade",
  "tasks": [
    {"id": "core", "write_files": ["./core.py"], "write_symbols": [{"name": "core", "path": "core.py"}]}
  ]
}"#,
    );
    let old_revision = seed_git(&root);
    let artifact = artifact_path(&root);
    build_index(&root, &old_revision, &artifact);

    write(root.as_path(), "core.py", "def core():\n    return 2\n");
    git(&root, &["add", "core.py"]);
    git(&root, &["commit", "-q", "-m", "drift"]);
    let new_revision = head(&root);
    let stale = derive(&root, &artifact, &new_revision, &[]);
    assert_eq!(stale["index_status"], "stale");
    assert_eq!(stale["fallback_required"], true);
    assert!(missing_or_empty_array(&stale, "hard_edges"));
    assert_eq!(
        task(&stale, "core")["touch_hint"],
        serde_json::json!(["core.py"])
    );
    assert!(task(&stale, "core").get("write_symbols").is_none());

    let missing = artifact.with_extension("missing");
    let absent = derive(&root, &missing, &new_revision, &[]);
    assert_eq!(absent["index_status"], "absent");
    assert_eq!(absent["fallback_required"], true);
    assert_eq!(
        task(&absent, "core")["touch_hint"],
        serde_json::json!(["core.py"])
    );
    assert!(missing_or_empty_array(&absent, "hard_edges"));

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn derive_edges_cli_unsupported_language_degrades_without_hard_edges() {
    if !ctags_present() {
        return;
    }
    let root = scratch("unsupported");
    write(
        root.as_path(),
        "schema.proto",
        "message Thing { string name = 1; }\n",
    );
    write(
        root.as_path(),
        "plan.json",
        r#"{
  "version": "split-derive/v1",
  "goal": "unsupported language",
  "tasks": [
    {"id": "schema", "write_files": ["schema.proto"], "write_symbols": [{"name": "Thing", "path": "schema.proto"}]}
  ]
}"#,
    );
    let revision = seed_git(&root);
    let artifact = artifact_path(&root);
    build_index(&root, &revision, &artifact);
    let result = derive(&root, &artifact, &revision, &[]);
    assert_eq!(result["index_status"], "absent");
    assert_eq!(result["fallback_required"], true);
    assert_eq!(result["degraded_reason"], "index-absent");
    assert!(missing_or_empty_array(&result, "hard_edges"));
    assert!(task(&result, "schema").get("write_symbols").is_none());
    assert_eq!(
        task(&result, "schema")["touch_hint"],
        serde_json::json!(["schema.proto"])
    );

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn split_help_advertises_the_product_derive_surface() {
    let output = Command::new(shuttle_bin())
        .args(["split", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--derive-edges",
        "--symbol-index",
        "--index-head",
        "--max-depth",
        "--max-fanout",
    ] {
        assert!(help.contains(flag), "split help omitted {flag}: {help}");
    }
}

#[test]
fn derive_edges_rejects_unknown_fields_and_conflicting_modes() {
    let root = scratch("strict_input");
    write(
        &root,
        "plan.json",
        r#"{
  "version": "split-derive/v1",
  "goal": "strict",
  "tasks": [{"id": "a", "write_files": ["a.py"], "surprise": true}]
}"#,
    );
    let output = Command::new(shuttle_bin())
        .args([
            "split",
            "--derive-edges",
            "--plan",
            root.join("plan.json").to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--symbol-index",
            root.join("missing.index").to_str().unwrap(),
            "--index-head",
            &"a".repeat(40),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field"));

    let conflict = Command::new(shuttle_bin())
        .args([
            "split",
            "--correct",
            "--derive-edges",
            "--plan",
            root.join("plan.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn derive_edges_accepts_only_snapshot_bound_canonical_v2_identities() {
    if !ctags_present() {
        return;
    }
    let root = scratch("typed");
    write(&root, "core.py", "def core():\n    return 1\n");
    write(
        &root,
        "wrapper.py",
        "from core import core\n\ndef wrapper():\n    return core()\n",
    );
    let revision = seed_git(&root);
    let artifact = artifact_path(&root);
    build_index(&root, &revision, &artifact);
    let table = match shuttle_symbols::index::read_artifact_for_revision(
        root.to_str().unwrap(),
        artifact.to_str().unwrap(),
        &revision,
    ) {
        shuttle_symbols::index::ArtifactRead::Fresh(table) => table,
        other => panic!("expected fresh index, got {other:?}"),
    };
    let identity = |name: &str| {
        let symbol = table
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap();
        shuttle_protocol::SymbolIdentity::from_signature_bytes(
            "fixture/repo",
            revision.clone(),
            "python",
            symbol.path.clone(),
            symbol.name.clone(),
            serde_json::to_value(&symbol.kind)
                .unwrap()
                .as_str()
                .unwrap(),
            symbol.scope.clone(),
            symbol.line as u64,
            symbol.signature.as_deref().unwrap_or_default().as_bytes(),
        )
        .unwrap()
    };
    let plan = artifact.with_extension("typed-plan.json");
    std::fs::write(
        &plan,
        serde_json::to_vec(&serde_json::json!({
            "version": "split-derive/v2",
            "goal": "typed",
            "tasks": [
                {"id": "core", "write_symbols": [identity("core")]},
                {"id": "wrapper", "write_symbols": [identity("wrapper")]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let result = derive_plan(&root, &plan, &artifact, &revision, &[]);
    assert_eq!(result["index_status"], "fresh");
    assert_eq!(result["fallback_required"], false);
    assert_eq!(result["hard_edges"][0]["kind"], "import_directed");

    let mut tampered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    tampered["tasks"][0]["write_symbols"][0]["name"] = serde_json::json!("not-core");
    std::fs::write(&plan, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let output = Command::new(shuttle_bin())
        .args([
            "split",
            "--derive-edges",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
            "--symbol-index",
            artifact.to_str().unwrap(),
            "--index-head",
            &revision,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symbol_id"));

    let _ = std::fs::remove_file(plan);
    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(root);
}
