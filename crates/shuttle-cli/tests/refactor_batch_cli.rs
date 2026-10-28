//! Binary-level CLI integration test for `shuttle refactor batch` — the §8
//! transactional composition path. Spawns the REAL `shuttle` binary (path injected
//! by cargo as `CARGO_BIN_EXE_shuttle`) against a scratch Python file and a JSON
//! batch spec, then asserts the all-or-nothing commit landed exactly the
//! composed result (step 1's edit is visible to step 2). Hermetic: no network,
//! no provider dependency — just disk under the OS temp root (honours TMPDIR).

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
    let dir = std::env::temp_dir().join(format!("shuttle_batch_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

#[test]
fn batch_write_commits_composed_result_atomically() {
    let ws = scratch_dir("commit");
    // f has an unused trailing param; step 1 drops it, step 2 reorders (a,b)->(b,a).
    let src = "def f(a, b, unused):\n    return a - b\n\nf(1, 2, 9)\nf(3, 4, 8)\n";
    let file = write(&ws, "m.py", src);
    let spec = format!(
        r#"{{ "steps": [
            {{ "op": "remove-parameter", "file": {file:?}, "func": "f", "param": "unused" }},
            {{ "op": "change-signature", "file": {file:?}, "func": "f", "params": "keep:1,keep:0" }}
        ] }}"#,
        file = file.display().to_string()
    );
    let sp = write(&ws, "batch.json", &spec);

    let out = Command::new(shuttle_bin())
        .args(["refactor", "batch", sp.to_str().unwrap(), "--write"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "batch --write should succeed; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = std::fs::read_to_string(&file).unwrap();
    assert!(
        got.contains("def f(b, a):"),
        "composed signature; got:\n{got}"
    );
    assert!(got.contains("f(2, 1)"), "first call composed; got:\n{got}");
    assert!(got.contains("f(4, 3)"), "second call composed; got:\n{got}");
    assert!(!got.contains("unused"), "unused param dropped; got:\n{got}");
}

#[test]
fn batch_dry_run_leaves_disk_untouched() {
    let ws = scratch_dir("dry");
    let src = "def f(a, b, unused):\n    return a - b\n\nf(1, 2, 9)\n";
    let file = write(&ws, "m.py", src);
    let spec = format!(
        r#"{{ "steps": [
            {{ "op": "remove-parameter", "file": {file:?}, "func": "f", "param": "unused" }}
        ] }}"#,
        file = file.display().to_string()
    );
    let sp = write(&ws, "batch.json", &spec);

    let out = Command::new(shuttle_bin())
        .args(["refactor", "batch", sp.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    // No --write: disk must be byte-for-byte identical.
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, src, "dry-run must not touch disk");
}

#[test]
fn batch_unbatchable_op_is_rejected() {
    let ws = scratch_dir("reject");
    let file = write(&ws, "m.py", "x = 1\n");
    let spec = format!(
        r#"{{ "steps": [ {{ "op": "inline-variable", "file": {file:?} }} ] }}"#,
        file = file.display().to_string()
    );
    let sp = write(&ws, "batch.json", &spec);

    let out = Command::new(shuttle_bin())
        .args(["refactor", "batch", sp.to_str().unwrap(), "--write"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "cursor-based op must be refused");
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, "x = 1\n", "rejected batch must not touch disk");
}

#[test]
fn lsp_actions_lowers_a_plan_to_a_workspace_edit() {
    let ws = scratch_dir("lsp");
    let src = "def f(a, unused):\n    return a\n\nf(1, 9)\n";
    let file = write(&ws, "m.py", src);

    // 1. Produce a RefactorPlan JSON via the normal dry-run --json path.
    let plan = Command::new(shuttle_bin())
        .args([
            "refactor",
            "remove-parameter",
            file.to_str().unwrap(),
            "--func",
            "f",
            "--name",
            "unused",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "remove-parameter --json failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );

    // 2. Pipe that plan into `lsp-actions --file <path>`.
    let mut child = Command::new(shuttle_bin())
        .args(["refactor", "lsp-actions", "--file", file.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&plan.stdout).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "lsp-actions failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"kind\":\"refactor.rewrite\""),
        "got: {json}"
    );
    assert!(json.contains("\"newText\""), "got: {json}");
    assert!(json.contains("\"range\""), "got: {json}");
    assert!(
        json.contains("m.py"),
        "workspace edit keyed by path; got: {json}"
    );
}
