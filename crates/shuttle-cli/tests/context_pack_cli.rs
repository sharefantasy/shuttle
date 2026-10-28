//! Binary-level CLI integration test for `shuttle context-pack` graph-impact
//! folding . Confirms the backward-compat
//! contract: with the SHUTTLE_GRAPH_IMPACT switch OFF the `graph_impact` key is
//! entirely ABSENT (old consumers unaffected); with it ON the key appears and
//! its `symbol_edges` are self-consistent (every endpoint is a known symbol).
//! Hermetic: disk under the OS temp root (honours TMPDIR).

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn ctags_present() -> bool {
    Command::new("ctags")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_ctxpack_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_seed(dir: &std::path::Path) -> String {
    git(dir, &["init", "-q", "--template="]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "seed"]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn pure_index(dir: &std::path::Path, head: &str, out: &std::path::Path) {
    let result = Command::new(shuttle_bin())
        .args([
            "index",
            "--refresh",
            "--root",
            dir.to_str().unwrap(),
            "--head",
            head,
            "--out",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "pure index failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn run_symbol_pack(
    ws: &std::path::Path,
    task_id: &str,
    index: &std::path::Path,
    index_head: Option<&str>,
) -> serde_json::Value {
    let tasks_dir = ws.join("tasks");
    let mut args = vec![
        "context-pack",
        "--tasks-dir",
        tasks_dir.to_str().unwrap(),
        "--task-id",
        task_id,
        "--symbol-index",
        index.to_str().unwrap(),
        "--root",
        ws.to_str().unwrap(),
    ];
    if let Some(head) = index_head {
        args.extend(["--index-head", head]);
    }
    let result = Command::new(shuttle_bin())
        .args(args)
        .env("SHUTTLE_REPO_ID", "test/repository")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "symbol context-pack failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn seed_repo(dir: &std::path::Path) {
    write(dir, "a.py", "def core():\n    return 1\n");
    write(
        dir,
        "b.py",
        "from a import core\n\ndef wrapper():\n    return core()\n",
    );
    write(
        dir,
        "test_a.py",
        "from a import core\n\ndef test_core():\n    assert core() == 1\n",
    );
}

fn run_pack(ws: &std::path::Path, graph_impact: bool) -> serde_json::Value {
    let mut cmd = Command::new(shuttle_bin());
    cmd.args([
        "context-pack",
        "--touch-hint",
        "a.py",
        "--root",
        ws.to_str().unwrap(),
    ]);
    if graph_impact {
        cmd.env("SHUTTLE_GRAPH_IMPACT", "1");
    } else {
        cmd.env_remove("SHUTTLE_GRAPH_IMPACT");
    }
    let out = cmd.output().expect("spawn shuttle context-pack");
    assert!(
        out.status.success(),
        "context-pack exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("context-pack stdout not JSON: {e}\n---\n{stdout}"))
}

#[test]
fn graph_impact_absent_by_default_present_when_enabled() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping context-pack CLI E2E");
        return;
    }
    let ws = scratch_dir("gate");
    seed_repo(&ws);

    // OFF: the graph_impact key must be entirely absent (byte-for-byte compat).
    let off = run_pack(&ws, false);
    assert!(
        off.get("graph_impact").is_none(),
        "graph_impact must be absent when SHUTTLE_GRAPH_IMPACT is unset: {off}"
    );
    // The legacy fields are still all present.
    for k in ["summary", "source", "focus", "contract_symbols"] {
        assert!(off.get(k).is_some(), "legacy field {k} missing");
    }
    for k in [
        "read_symbols",
        "write_symbols",
        "handoff_symbols",
        "index_status",
    ] {
        assert!(off.get(k).is_none(), "legacy mode must omit {k}: {off}");
    }

    // ON: the key appears with seeds/impacted/edges/derived_touch_hint.
    let on = run_pack(&ws, true);
    let gi = on
        .get("graph_impact")
        .expect("graph_impact present when enabled");
    let seeds: Vec<&str> = gi["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(seeds.contains(&"core"), "seed core missing: {seeds:?}");

    // Every symbol_edge endpoint must live in seeds ∪ impacted (self-consistent).
    let mut node_set: std::collections::HashSet<String> =
        seeds.iter().map(|s| s.to_string()).collect();
    for h in gi["impacted"].as_array().unwrap() {
        node_set.insert(h["symbol"].as_str().unwrap().to_string());
    }
    let edges = gi["symbol_edges"].as_array().unwrap();
    for e in edges {
        let from = e["from"].as_str().unwrap();
        let to = e["to"].as_str().unwrap();
        assert!(
            node_set.contains(from) && node_set.contains(to),
            "dangling edge {from} -> {to}"
        );
    }
    // wrapper -> core is the real reference edge folded in.
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "wrapper" && e["to"] == "core"),
        "expected wrapper -> core edge among {edges:?}"
    );
    // Derived touch hint widened the focus with the caller files.
    let derived: Vec<&str> = gi["derived_touch_hint"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str())
        .collect();
    assert!(
        derived.iter().any(|f| f.ends_with("b.py")),
        "derived touch hint should include b.py: {derived:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn fresh_index_emits_verified_read_write_and_handoff_symbols() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping symbol context E2E");
        return;
    }
    let ws = scratch_dir("symbol_handoff");
    write(&ws, "a.py", "def foo():\n    return 1\n");
    write(
        &ws,
        "b.py",
        "from a import foo\n\ndef bar():\n    return foo()\n",
    );
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [a.py]\nwrite_symbols: [foo@a.py]\n",
    );
    write(
        &ws,
        "tasks/task-b.yaml",
        "depends_on: [task-a]\ntouch_hint: [b.py]\nwrite_symbols: [bar@b.py]\n",
    );
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);
    let pack = run_symbol_pack(&ws, "task-b", &artifact, Some(&head));

    assert_eq!(pack["source"], "symbol_index");
    assert_eq!(pack["index_status"], "fresh");
    assert_eq!(pack["write_symbols"][0]["name"], "bar");
    assert_eq!(pack["write_symbols"][0]["repo_id"], "test/repository");
    assert_eq!(pack["write_symbols"][0]["base_oid"], head);
    assert_eq!(pack["write_symbols"][0]["language"], "python");
    assert_eq!(pack["write_symbols"][0]["normalized_path"], "b.py");
    assert!(pack["write_symbols"][0]["symbol_id"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(pack["write_symbols"][0]["line"].as_u64().unwrap() > 0);
    assert!(pack["write_symbols"][0]["signature_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(pack["write_symbols"][0].get("path").is_none());
    assert!(pack["write_symbols"][0].get("definition_anchor").is_none());
    assert!(pack["read_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|symbol| symbol["name"] == "foo"));
    let handoff = pack["handoff_symbols"].as_array().unwrap();
    assert_eq!(handoff.len(), 1, "pack: {pack}");
    assert_eq!(handoff[0]["from_sym"]["name"], "foo");
    assert_eq!(handoff[0]["to_sym"]["name"], "bar");
    assert_eq!(handoff[0]["kind"], "import_directed");

    let footprint = &pack["symbol_footprint"];
    assert_eq!(footprint["schema_version"], "shuttle_symbol_footprint/v1");
    assert_eq!(
        footprint["basis"]["repositories"][0]["repo_id"],
        "test/repository"
    );
    assert_eq!(footprint["basis"]["repositories"][0]["base_oid"], head);
    assert!(footprint["basis"]["symbol_index_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(footprint["basis"]["coverage"], "exact");
    assert_eq!(footprint["basis"]["freshness"], "fresh");
    assert_eq!(footprint["basis"]["truncated"], false);
    assert_eq!(footprint["tasks"][0]["task_id"], "task-b");
    assert_eq!(
        footprint["tasks"][0]["planned"]["evidence_source"],
        "planner_declared_validated"
    );
    assert_eq!(footprint["tasks"][0]["grants_path_authority"], false);
    assert_eq!(
        footprint["tasks"][0]["observed"]["evidence_source"],
        "not_observed"
    );
    assert!(footprint["tasks"][0]["observed"]["observed_write_symbols"]
        .as_array()
        .unwrap()
        .is_empty());
    let canonical = serde_json::to_vec(footprint).unwrap();
    shuttle_protocol::SymbolFootprint::parse(&canonical)
        .expect("context-pack must emit the canonical closed footprint");

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn fresh_index_does_not_run_a_second_ctags_scan() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("fresh_no_rescan");
    write(&ws, "a.py", "def foo():\n    return 1\n");
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [a.py]\nwrite_symbols: [foo@a.py]\n",
    );
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);

    let fake_ctags = ws.parent().unwrap().join(format!(
        "{}.must-not-run-ctags",
        ws.file_name().unwrap().to_string_lossy()
    ));
    let marker = ws.parent().unwrap().join(format!(
        "{}.ctags-invoked",
        ws.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &fake_ctags,
        format!(
            "#!/bin/sh\nprintf '%s\n' invoked >> '{}'\nif [ \"$1\" = \"--version\" ]; then echo 'Universal Ctags 6.2'; exit 0; fi\nexit 97\n",
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_ctags, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let result = Command::new(shuttle_bin())
        .args([
            "context-pack",
            "--tasks-dir",
            ws.join("tasks").to_str().unwrap(),
            "--task-id",
            "task-a",
            "--symbol-index",
            artifact.to_str().unwrap(),
            "--index-head",
            &head,
            "--root",
            ws.to_str().unwrap(),
        ])
        .env("SHUTTLE_REPO_ID", "test/repository")
        .env("SHUTTLE_CTAGS_BIN", &fake_ctags)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "fresh-index context-pack failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let pack: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(pack["source"], "symbol_index");
    assert_eq!(pack["index_status"], "fresh");
    assert!(
        !marker.exists(),
        "fresh caller-owned index must bypass every live ctags invocation"
    );

    let _ = std::fs::remove_file(fake_ctags);
    let _ = std::fs::remove_file(marker);
    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn touch_hint_never_becomes_exact_write_evidence() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("touch_is_not_write");
    write(&ws, "a.py", "def foo():\n    return 1\n");
    write(&ws, "tasks/task-a.yaml", "touch_hint: [a.py]\n");
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);
    let pack = run_symbol_pack(&ws, "task-a", &artifact, Some(&head));

    assert_eq!(pack["index_status"], "fresh");
    assert!(pack.get("write_symbols").is_none(), "pack: {pack}");
    assert!(
        pack["symbol_footprint"]["tasks"][0]["planned"]["declared_write_symbols"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        pack["symbol_footprint"]["tasks"][0]["grants_path_authority"],
        false
    );

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn ambiguous_declaration_is_unresolved_and_emits_no_exact_symbols() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("ambiguous_symbol");
    write(
        &ws,
        "both.py",
        "class A:\n    def same(self):\n        return 1\n\nclass B:\n    def same(self):\n        return 2\n",
    );
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [both.py]\nwrite_symbols: [same@both.py]\n",
    );
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);
    let pack = run_symbol_pack(&ws, "task-a", &artifact, Some(&head));

    assert_eq!(pack["index_status"], "ambiguous");
    assert!(pack.get("write_symbols").is_none(), "pack: {pack}");
    assert!(pack.get("read_symbols").is_none(), "pack: {pack}");
    assert!(pack.get("handoff_symbols").is_none(), "pack: {pack}");
    assert_eq!(
        pack["symbol_footprint"]["tasks"][0]["planned"]["unresolved"][0]["reason"],
        "ambiguous"
    );
    assert_eq!(
        pack["symbol_footprint"]["tasks"][0]["planned"]["evidence_source"],
        "planner_declared_unresolved"
    );

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn caller_index_ref_propagates_fail_closed_degraded_states() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("degraded_refs");
    write(&ws, "a.py", "def foo():\n    return 1\n");
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [a.py]\nwrite_symbols: [foo@a.py]\n",
    );
    let head = git_seed(&ws);

    for status in ["timeout", "truncated", "unsupported"] {
        let index_ref = ws.join(format!("{status}.index-ref.json"));
        write(
            &ws,
            &format!("{status}.index-ref.json"),
            &format!(
                "{{\"schema_version\":\"shuttle_symbol_index_ref/v1\",\"status\":\"{status}\",\"symbol_index_digest\":\"sha256:{}\",\"base_oid\":\"{head}\"}}",
                "a".repeat(64)
            ),
        );
        let pack = run_symbol_pack(&ws, "task-a", &index_ref, Some(&head));
        assert_eq!(pack["index_status"], status, "pack: {pack}");
        assert!(pack.get("write_symbols").is_none(), "pack: {pack}");
        assert!(pack.get("read_symbols").is_none(), "pack: {pack}");
        assert!(pack.get("handoff_symbols").is_none(), "pack: {pack}");
        assert_eq!(
            pack["symbol_footprint"]["basis"]["truncated"],
            status == "truncated"
        );
        assert_eq!(
            pack["symbol_footprint"]["tasks"][0]["grants_path_authority"],
            false
        );
        assert_eq!(
            pack["symbol_footprint"]["tasks"][0]["planned"]["evidence_source"],
            "planner_guess"
        );
        shuttle_protocol::SymbolFootprint::parse(
            &serde_json::to_vec(&pack["symbol_footprint"]).unwrap(),
        )
        .expect("degraded footprint must remain a valid closed contract");
    }

    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn same_file_disjoint_symbols_keep_file_seam_but_no_symbol_handoff() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("same_file_disjoint");
    write(
        &ws,
        "both.py",
        "def foo():\n    return 1\n\ndef bar():\n    return 2\n",
    );
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [both.py]\nwrite_symbols: [foo@both.py]\n",
    );
    write(
        &ws,
        "tasks/task-b.yaml",
        "depends_on: [task-a]\ntouch_hint: [both.py]\nwrite_symbols: [bar@both.py]\n",
    );
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);
    let pack = run_symbol_pack(&ws, "task-b", &artifact, Some(&head));
    assert_eq!(pack["shared_handoff_files"], serde_json::json!(["both.py"]));
    assert!(pack.get("handoff_symbols").is_none(), "pack: {pack}");

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn stale_and_absent_indexes_degrade_without_symbol_claims() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("symbol_degrade");
    write(&ws, "a.py", "def foo():\n    return 1\n");
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [a.py]\nwrite_symbols: [foo@a.py]\n",
    );
    let base = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &base, &artifact);
    write(&ws, "note.txt", "drift\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "drift"]);
    let stale = run_symbol_pack(&ws, "task-a", &artifact, None);
    assert_eq!(stale["index_status"], "stale");
    assert!(stale.get("write_symbols").is_none());
    assert!(stale.get("read_symbols").is_none());
    assert!(stale.get("handoff_symbols").is_none());
    assert!(stale["contract_symbols"].is_array());

    // A dirty tracked file does not move HEAD, but it still invalidates the
    // content-addressed artifact and must never be treated as fresh.
    git(&ws, &["reset", "--hard", &base]);
    write(&ws, "a.py", "def foo():\n    return 99\n");
    let dirty = run_symbol_pack(&ws, "task-a", &artifact, Some(&base));
    assert_eq!(dirty["index_status"], "stale");
    assert!(dirty.get("write_symbols").is_none());
    git(&ws, &["reset", "--hard", &base]);

    let legacy = run_pack(&ws, false);
    let missing = ws.join("missing.index");
    let absent = run_symbol_pack(&ws, "task-a", &missing, None);
    assert_eq!(absent["index_status"], "absent");
    assert!(absent.get("write_symbols").is_none());
    assert!(absent.get("read_symbols").is_none());
    assert!(absent.get("handoff_symbols").is_none());
    for field in [
        "predecessors",
        "shared_handoff_files",
        "upstream_only_files",
        "focus",
        "contract_symbols",
    ] {
        assert_eq!(
            absent[field], legacy[field],
            "absent-index fallback must preserve legacy field {field}"
        );
    }

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn unsupported_language_degrades_to_absent_without_symbol_claims() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("unsupported_symbol_language");
    write(&ws, "schema.proto", "message Thing { string name = 1; }\n");
    write(
        &ws,
        "tasks/task-a.yaml",
        "touch_hint: [schema.proto]\nwrite_symbols: [Thing@schema.proto]\n",
    );
    let head = git_seed(&ws);
    let artifact = ws.parent().unwrap().join(format!(
        "{}.symbols.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    pure_index(&ws, &head, &artifact);
    let pack = run_symbol_pack(&ws, "task-a", &artifact, Some(&head));
    assert_eq!(pack["index_status"], "unsupported");
    assert_ne!(pack["source"], "symbol_index");
    assert!(pack.get("write_symbols").is_none());
    assert!(pack.get("read_symbols").is_none());
    assert!(pack.get("handoff_symbols").is_none());
    assert!(pack["contract_symbols"].is_array());

    let _ = std::fs::remove_file(artifact);
    let _ = std::fs::remove_dir_all(ws);
}
