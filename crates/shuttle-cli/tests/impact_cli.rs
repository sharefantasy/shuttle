//! Binary-level CLI integration test for `shuttle impact` — the M4 blast-radius
//! path . Spawns the REAL `shuttle` binary
//! against a scratch Python repo and asserts the reverse-dependency closure
//! JSON is parseable, that `--tests` narrows the result to test files, and
//! that a missing-ctags environment degrades gracefully (no panic, exit 0).
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
    let dir = std::env::temp_dir().join(format!("shuttle_impact_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

/// A linear dependency chain `core <- wrapper <- test_core`, plus a non-test
/// caller, so `--tests` has something to filter out.
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

#[test]
fn impact_json_is_parseable_and_contains_callers() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping impact CLI E2E");
        return;
    }
    let ws = scratch_dir("json");
    seed_repo(&ws);

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "a.py",
            "--root",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact");
    assert!(
        out.status.success(),
        "shuttle impact exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("impact stdout is not JSON: {e}\n---\n{stdout}"));
    let arr = v.as_array().expect("impact JSON is an array of hits");

    // The seed `core` must appear at distance 0 as a definition, and both the
    // non-test caller `wrapper` and the test caller `test_core` must be reached.
    let symbols: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("symbol").and_then(|s| s.as_str()))
        .collect();
    assert!(symbols.contains(&"core"), "seed core missing: {symbols:?}");
    assert!(
        symbols.contains(&"wrapper"),
        "reverse caller wrapper missing: {symbols:?}"
    );
    assert!(
        symbols.contains(&"test_core"),
        "reverse caller test_core missing: {symbols:?}"
    );

    // Every hit carries the stable schema fields.
    for h in arr {
        assert!(h.get("path").and_then(|p| p.as_str()).is_some());
        assert!(h.get("line").and_then(|l| l.as_u64()).is_some());
        assert!(h.get("confidence").and_then(|c| c.as_str()).is_some());
        assert!(h.get("kind").and_then(|k| k.as_str()).is_some());
    }

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn impact_tests_flag_narrows_to_test_files() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping impact --tests CLI E2E");
        return;
    }
    let ws = scratch_dir("tests");
    seed_repo(&ws);

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "a.py",
            "--root",
            ws.to_str().unwrap(),
            "--tests",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --tests");
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parseable JSON array");

    assert!(!arr.is_empty(), "expected at least the test-file hit");
    for h in &arr {
        let path = h.get("path").and_then(|p| p.as_str()).unwrap_or("");
        assert!(
            path.contains("test"),
            "--tests leaked a non-test hit: {path}"
        );
    }
    // The non-test caller `wrapper` must be gone.
    let symbols: Vec<&str> = arr
        .iter()
        .filter_map(|h| h.get("symbol").and_then(|s| s.as_str()))
        .collect();
    assert!(
        !symbols.contains(&"wrapper"),
        "wrapper is not a test and must be filtered: {symbols:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn impact_empty_changed_yields_empty_array() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("blank");
    // A file that defines nothing => no seeds => empty radius, still valid JSON.
    write(&ws, "blank.py", "# no defs here\n");

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "blank.py",
            "--root",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact on def-less file");
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parseable JSON array");
    assert!(arr.is_empty(), "no seeds -> empty radius: {arr:?}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// A same-package Go pair `store/a.go` <- `store/b.go` (B calls A with NO import
/// statement, because Go's compile unit is the package/directory). Plus a
/// cross-package caller under `svc/` that imports `store`, so the directed
/// closure must reach both the intra-package sibling AND the cross-package
/// importer — the "Go 包→文件粒度" alignment with the external orchestrator packet_plan consumer
/// .
fn seed_go_repo(dir: &std::path::Path) {
    write(dir, "go.mod", "module example.com/impactgo\n\ngo 1.21\n");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    write(
        &store,
        "a.go",
        "package store\n\nfunc Alpha() int {\n\treturn 1\n}\n",
    );
    // Same package, no import of `a.go` — the language rule (same dir = same
    // package) is what certifies the dependency, not an import statement.
    write(
        &store,
        "b.go",
        "package store\n\nfunc Beta() int {\n\treturn Alpha() + 1\n}\n",
    );
    let svc = dir.join("svc");
    std::fs::create_dir_all(&svc).unwrap();
    write(
        &svc,
        "svc.go",
        "package svc\n\nimport \"example.com/impactgo/store\"\n\nfunc Use() int {\n\treturn store.Alpha()\n}\n",
    );
}

#[test]
fn impact_go_directed_reaches_same_package_sibling_at_file_granularity() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping Go directed impact CLI E2E");
        return;
    }
    let ws = scratch_dir("go_directed");
    seed_go_repo(&ws);

    let out = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "store/a.go",
            "--root",
            ws.to_str().unwrap(),
            "--directed",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --directed on Go repo");
    assert!(
        out.status.success(),
        "shuttle impact --directed exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("Go directed impact stdout is not JSON: {e}\n---\n{stdout}"));

    // Collect (symbol, path) pairs for assertions.
    let hits: Vec<(&str, &str)> = arr
        .iter()
        .filter_map(|h| {
            let s = h.get("symbol").and_then(|s| s.as_str())?;
            let p = h.get("path").and_then(|p| p.as_str())?;
            Some((s, p))
        })
        .collect();

    // The changed seed itself.
    assert!(
        hits.iter().any(|(s, _)| *s == "Alpha"),
        "seed Alpha missing: {hits:?}"
    );
    // The same-package sibling `Beta` must be reached at its OWN file path
    // `store/b.go` — file granularity, not merely "package store". Before T4
    // this intra-package reverse-dep was dropped from the directed closure.
    assert!(
        hits.iter()
            .any(|(s, p)| *s == "Beta" && p.replace('\\', "/").ends_with("store/b.go")),
        "same-package sibling Beta@store/b.go missing from directed closure: {hits:?}"
    );
    // The cross-package importer must also be present (already file-grained via
    // the import fan-out), proving both direction sources coexist.
    assert!(
        hits.iter()
            .any(|(s, p)| *s == "Use" && p.replace('\\', "/").ends_with("svc/svc.go")),
        "cross-package importer Use@svc/svc.go missing: {hits:?}"
    );

    // Directed hits carry the stable schema, and every path is file-grained
    // (ends in `.go`, never a bare directory).
    for h in &arr {
        let path = h.get("path").and_then(|p| p.as_str()).unwrap_or("");
        assert!(
            path.ends_with(".go"),
            "directed Go hit path is not file-grained: {path}"
        );
        assert!(h.get("confidence").and_then(|c| c.as_str()).is_some());
        assert!(h.get("kind").and_then(|k| k.as_str()).is_some());
    }

    let _ = std::fs::remove_dir_all(&ws);
}

/// Two callers of a changed Go type: one that *type-depends* on it (a param of
/// type `store.User`) and one that merely *calls* a function from the same
/// package with no type dependency on the changed struct. `--typed` must keep
/// the former, prove-drop the latter, and stay strictly narrower than
/// `--directed` — the G6·S2 narrowing . The §7 «绝不瞎猜» rule means any caller whose type proof is
/// unavailable is KEPT at Medium and surfaced in `notes`, never dropped.
fn seed_go_typed_repo(dir: &std::path::Path) {
    write(dir, "go.mod", "module example.com/typedgo\n\ngo 1.21\n");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    write(
        &store,
        "model.go",
        "package store\n\ntype User struct {\n\tID   int\n\tName string\n}\n\nfunc NewUser(name string) *User {\n\treturn &User{Name: name}\n}\n",
    );
    let svc = dir.join("svc");
    std::fs::create_dir_all(&svc).unwrap();
    // Type-dependent caller: takes a `store.User` parameter.
    write(
        &svc,
        "typed_dep.go",
        "package svc\n\nimport \"example.com/typedgo/store\"\n\nfunc Consume(u store.User) int {\n\treturn u.ID\n}\n",
    );
    // Plain caller: calls a function, no type dependency on the changed struct.
    write(
        &svc,
        "plain_dep.go",
        "package svc\n\nimport \"example.com/typedgo/store\"\n\nfunc Trigger() string {\n\treturn store.NewUser(\"x\").Name\n}\n",
    );
}

#[test]
fn impact_typed_is_strictly_narrower_than_directed_on_go() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping typed impact CLI E2E");
        return;
    }
    let ws = scratch_dir("typed_go");
    seed_go_typed_repo(&ws);

    // Baseline: the trusted DIRECTED closure reaches BOTH callers.
    let directed = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "store/model.go",
            "--root",
            ws.to_str().unwrap(),
            "--directed",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --directed");
    assert!(directed.status.success());
    let d_arr: Vec<serde_json::Value> =
        serde_json::from_str(&String::from_utf8_lossy(&directed.stdout)).expect("directed JSON");
    let d_syms: Vec<&str> = d_arr
        .iter()
        .filter_map(|h| h.get("symbol").and_then(|s| s.as_str()))
        .collect();
    assert!(
        d_syms.contains(&"Consume"),
        "directed must reach type-dep caller Consume: {d_syms:?}"
    );
    assert!(
        d_syms.contains(&"Trigger"),
        "directed must reach plain caller Trigger: {d_syms:?}"
    );

    // Refinement: `--typed` keeps the type-dependent caller and prove-drops the
    // plain one, so its hit set is STRICTLY smaller than `--directed`.
    let typed = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "store/model.go",
            "--root",
            ws.to_str().unwrap(),
            "--typed",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --typed");
    assert!(
        typed.status.success(),
        "shuttle impact --typed exited non-zero: {}",
        String::from_utf8_lossy(&typed.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&typed.stdout)).expect("typed JSON object");

    let hits = v
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    let hit_syms: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("symbol").and_then(|s| s.as_str()))
        .collect();
    // Type-dependent caller kept.
    assert!(
        hit_syms.contains(&"Consume"),
        "typed must keep the type-dependent caller Consume: {hit_syms:?}"
    );
    // Plain caller proven out of hits.
    assert!(
        !hit_syms.contains(&"Trigger"),
        "typed must narrow out the plain caller Trigger: {hit_syms:?}"
    );

    // The prove-drop is surfaced (not silently discarded), with the ONLY reason
    // §7 permits for a drop: `no-type-dependency`.
    let dropped = v
        .get("dropped")
        .and_then(|d| d.as_array())
        .expect("dropped array");
    assert!(
        dropped.iter().any(|d| {
            d.get("symbol").and_then(|s| s.as_str()) == Some("Trigger")
                && d.get("reason").and_then(|r| r.as_str()) == Some("no-type-dependency")
        }),
        "Trigger must appear in dropped with reason no-type-dependency: {dropped:?}"
    );

    // Strictly narrower: fewer hits than the directed closure had entries.
    assert!(
        hits.len() < d_arr.len(),
        "typed ({}) must be strictly narrower than directed ({})",
        hits.len(),
        d_arr.len()
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// A weakly-typed (Python) caller can never be *proven* type-independent, so
/// per §7 «绝不瞎猜» `--typed` must DEGRADE to keeping the directed edge —
/// tagged Medium, flagged `degraded=true`, and explained in `notes` — rather
/// than drop it. This is the "类型推不出时退回 directed" half of E2E #4.
#[test]
fn impact_typed_degrades_to_directed_on_weakly_typed_python() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping typed degrade CLI E2E");
        return;
    }
    let ws = scratch_dir("typed_py");
    // `b.py::wrapper` imports and calls `a.py::core`. Python annotations are
    // optional/sparse, so we cannot prove a type (in)dependency: keep + degrade.
    seed_repo(&ws);

    let typed = Command::new(shuttle_bin())
        .args([
            "impact",
            "--changed",
            "a.py",
            "--root",
            ws.to_str().unwrap(),
            "--typed",
            "--json",
        ])
        .output()
        .expect("spawn shuttle impact --typed on python");
    assert!(typed.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&typed.stdout)).expect("typed JSON object");

    // Nothing is dropped for a weakly-typed language (we never guess absence).
    let dropped = v
        .get("dropped")
        .and_then(|d| d.as_array())
        .expect("dropped array");
    assert!(
        dropped.is_empty(),
        "weakly-typed callers must never be prove-dropped: {dropped:?}"
    );

    // The degrade is flagged and explained.
    assert_eq!(
        v.get("degraded").and_then(|d| d.as_bool()),
        Some(true),
        "a weakly-typed caller must set degraded=true"
    );
    let notes = v.get("notes").and_then(|n| n.as_array()).expect("notes");
    assert!(
        notes.iter().any(|n| n
            .as_str()
            .map(|s| s.contains("weakly-typed"))
            .unwrap_or(false)),
        "notes must explain the weakly-typed degrade: {notes:?}"
    );

    // The directed caller `wrapper` is KEPT (not dropped) and demoted to Medium.
    let hits = v.get("hits").and_then(|h| h.as_array()).expect("hits");
    let wrapper = hits
        .iter()
        .find(|h| h.get("symbol").and_then(|s| s.as_str()) == Some("wrapper"));
    let wrapper = wrapper.expect("degraded caller wrapper must be kept in hits");
    assert_eq!(
        wrapper.get("confidence").and_then(|c| c.as_str()),
        Some("medium"),
        "a degraded kept caller must be tagged Medium"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
