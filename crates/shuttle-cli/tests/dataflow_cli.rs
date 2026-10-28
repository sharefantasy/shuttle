//! Binary-level CLI integration test for `shuttle dataflow` — the G6 · S3 provable
//! forward-dataflow path .
//! Spawns the REAL `shuttle` binary against a scratch single file and asserts the
//! DataFlow JSON is parseable and that every §7 «绝不瞎猜» boundary holds:
//! proven copy chains appear, derived values do NOT, call/return sinks are
//! recorded (never followed), and subscript/attribute stores surface as
//! unprovable frontiers rather than tainting a field. No ctags / git needed —
//! the analysis is a single-file tree-sitter walk. Hermetic: disk under the OS
//! temp root (honours TMPDIR).

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_dataflow_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

/// Run `shuttle dataflow --seed <seed> --path <path> --json` and parse stdout.
fn run_dataflow(seed: &str, path: &std::path::Path) -> serde_json::Value {
    let out = Command::new(shuttle_bin())
        .args(["dataflow", "--seed", seed, "--path"])
        .arg(path)
        .arg("--json")
        .output()
        .expect("spawn shuttle");
    assert!(
        out.status.success(),
        "shuttle dataflow exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse dataflow json")
}

fn node_names(v: &serde_json::Value) -> Vec<String> {
    v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn python_copy_chain_proven_derived_value_absent() {
    let dir = scratch_dir("py_copy");
    let f = write(
        &dir,
        "h.py",
        "def h(raw):\n    x = raw\n    y = x\n    z = y + 1\n    return z\n",
    );
    let v = run_dataflow("raw", &f);
    let names = node_names(&v);
    assert!(names.contains(&"raw".to_string()), "{names:?}");
    assert!(names.contains(&"x".to_string()), "{names:?}");
    assert!(names.contains(&"y".to_string()), "{names:?}");
    // z = y + 1 is a DERIVED value — must never be tainted (§7).
    assert!(!names.contains(&"z".to_string()), "{names:?}");
    // Weakly-typed language: edges are Low, never High/Medium.
    for n in v["nodes"].as_array().unwrap() {
        assert_eq!(n["confidence"].as_str().unwrap(), "low", "node={n}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn python_call_sink_and_unprovable_stores_surface() {
    let dir = scratch_dir("py_sink");
    let f = write(
        &dir,
        "h.py",
        "def h(raw):\n    x = raw\n    save(x)\n    data[0] = x\n    self.field = x\n    return 0\n",
    );
    let v = run_dataflow("raw", &f);
    // save(x) is a sink — value left the function; callee recorded, not followed.
    let sinks = v["sinks"].as_array().unwrap();
    assert!(
        sinks
            .iter()
            .any(|s| s["value"] == "x" && s["callee"] == "save"),
        "sinks={sinks:?}"
    );
    // Subscript + attribute stores are unprovable frontiers (§7), not taint.
    let unresolved = v["unresolved"].as_array().unwrap();
    assert!(
        unresolved.iter().any(|u| u["kind"] == "subscript"),
        "unresolved={unresolved:?}"
    );
    assert!(
        unresolved.iter().any(|u| u["kind"] == "attribute"),
        "unresolved={unresolved:?}"
    );
    // `data`/`field` never appear as tainted nodes.
    let names = node_names(&v);
    assert!(!names.contains(&"data".to_string()), "{names:?}");
    assert!(!names.contains(&"field".to_string()), "{names:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn go_dataflow_is_medium_with_return_sink() {
    let dir = scratch_dir("go_flow");
    let f = write(
        &dir,
        "s.go",
        "package s\n\nfunc H(raw string) string {\n\tx := raw\n\ty := x\n\tsink(y)\n\treturn x\n}\n",
    );
    let v = run_dataflow("raw", &f);
    assert_eq!(v["function"].as_str().unwrap(), "H");
    // Strongly-typed language: copy edges are Medium, never Low.
    for n in v["nodes"].as_array().unwrap() {
        assert_eq!(n["confidence"].as_str().unwrap(), "medium", "node={n}");
    }
    let names = node_names(&v);
    assert!(names.contains(&"x".to_string()) && names.contains(&"y".to_string()));
    let sinks = v["sinks"].as_array().unwrap();
    assert!(
        sinks
            .iter()
            .any(|s| s["callee"] == "<return>" && s["value"] == "x"),
        "sinks={sinks:?}"
    );
    assert!(
        sinks
            .iter()
            .any(|s| s["callee"] == "sink" && s["value"] == "y"),
        "sinks={sinks:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_seed_degrades_and_exits_zero() {
    let dir = scratch_dir("missing");
    let f = write(&dir, "h.py", "def h(raw):\n    return raw\n");
    let out = Command::new(shuttle_bin())
        .args(["dataflow", "--seed", "ghost", "--path"])
        .arg(&f)
        .arg("--json")
        .output()
        .expect("spawn shuttle");
    // Envelope is the machine contract — a degrade is exit 0, not a crash.
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["degraded"].as_bool().unwrap());
    assert!(v["nodes"].as_array().unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
