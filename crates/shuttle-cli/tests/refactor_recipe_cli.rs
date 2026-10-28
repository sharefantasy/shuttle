//! Binary-level CLI integration test for `shuttle refactor recipe` — the §G2
//! composite-recipe path. Spawns the REAL `shuttle` binary against scratch Python
//! files, runs built-in recipes, and asserts each recipe expanded into a
//! BatchSpec and landed the composed result through the same transactional gate
//! as `shuttle refactor batch`. Hermetic: disk under the OS temp root (honours
//! TMPDIR).

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
    let dir = std::env::temp_dir().join(format!("shuttle_recipe_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

/// A sync function with two hardcoded tunables (500 batch, 100 watermark) that
/// the recipe promotes to parameters over two composed `introduce-parameter`
/// steps.
const SEED: &str = "\
def push_rows(client, rows):
    buf = []
    for r in rows:
        buf.append(r)
        if len(buf) >= 500:
            client.flush(buf)
            buf = []
    if len(buf) >= 100:
        client.flush(buf)
    return len(rows)

push_rows(c, data)
";

/// A second recipe shape for R1: remove an unused trailing parameter, then
/// reorder the remaining parameters to match a new API contract.
const RESEQUENCE_SEED: &str = "\
def normalize_pair(left, right, unused):
    return f\"{left}:{right}\"\n\
\n\
normalize_pair(\"L\", \"R\", 0)\n\
normalize_pair(\"A\", \"B\", 1)\n";

#[test]
fn recipe_write_lands_composed_result() {
    let ws = scratch_dir("commit");
    let file = write(&ws, "sync.py", SEED);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=push_rows",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "recipe --write should succeed; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = std::fs::read_to_string(&file).unwrap();
    // Both tunables promoted; signature grew both params (step 2 saw step 1's edit).
    assert!(
        got.contains("def push_rows(client, rows, flush_at, high_watermark):"),
        "composed signature; got:\n{got}"
    );
    // The literals became parameter references in the body.
    assert!(
        got.contains("if len(buf) >= flush_at:"),
        "flush_at bound; got:\n{got}"
    );
    assert!(
        got.contains("if len(buf) >= high_watermark:"),
        "watermark bound; got:\n{got}"
    );
    // Call site forwards the original literals (defaults folded in).
    assert!(
        got.contains("push_rows(c, data, 500, 100)"),
        "call composed; got:\n{got}"
    );
}

#[test]
fn recipe_dry_run_leaves_disk_untouched() {
    let ws = scratch_dir("dry");
    let file = write(&ws, "sync.py", SEED);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=push_rows",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, SEED, "dry-run must not touch disk");
}

#[test]
fn recipe_supplied_params_override_defaults() {
    let ws = scratch_dir("params");
    let file = write(&ws, "sync.py", SEED);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=push_rows",
            "--set",
            "batch_size=500",
            "--set",
            "watermark=100",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "recipe with explicit params should succeed; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = std::fs::read_to_string(&file).unwrap();
    assert!(got.contains("push_rows(c, data, 500, 100)"), "got:\n{got}");
}

#[test]
fn second_recipe_write_lands_composed_result() {
    let ws = scratch_dir("resequence");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "api.remove-unused-tail-resequence",
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=normalize_pair",
            "--set",
            "removed_param=unused",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "second recipe --write should succeed; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = std::fs::read_to_string(&file).unwrap();
    assert!(
        got.contains("def normalize_pair(right, left):"),
        "composed signature; got:\n{got}"
    );
    assert!(
        got.contains("return f\"{left}:{right}\""),
        "body should stay unchanged; got:\n{got}"
    );
    assert!(
        got.contains("normalize_pair(\"R\", \"L\")"),
        "first call composed; got:\n{got}"
    );
    assert!(
        got.contains("normalize_pair(\"B\", \"A\")"),
        "second call composed; got:\n{got}"
    );
    assert!(!got.contains("unused"), "unused param dropped; got:\n{got}");
}

#[test]
fn recipe_missing_required_param_is_rejected() {
    let ws = scratch_dir("missing");
    let file = write(&ws, "sync.py", SEED);

    // No --set func=... : the required param is missing → hard fail, no write.
    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--file",
            file.to_str().unwrap(),
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "missing required param must be refused"
    );
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, SEED, "rejected recipe must not touch disk");
}

#[test]
fn recipe_expect_version_mismatch_is_rejected() {
    let ws = scratch_dir("ver");
    let file = write(&ws, "sync.py", SEED);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=push_rows",
            "--expect-version",
            "2",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "version mismatch must be refused");
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, SEED, "version-mismatched recipe must not touch disk");
}

#[test]
fn recipe_list_json_reports_registry() {
    let out = Command::new(shuttle_bin())
        .args(["refactor", "recipe", "--list", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "list --json should succeed");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("perf.promote-sync-tunables"), "got: {json}");
    assert!(
        json.contains("api.remove-unused-tail-resequence"),
        "got: {json}"
    );
    assert!(
        json.contains("\"required\": true"),
        "func required flagged; got: {json}"
    );
}

#[test]
fn recipe_unknown_name_is_rejected() {
    let out = Command::new(shuttle_bin())
        .args(["refactor", "recipe", "nope.not-real", "--file", "x.py"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown recipe must be refused");
}

/// An out-of-tree `*.recipe.json` mirroring the built-in resequence recipe.
/// Proves the external path expands + lands through the identical gate.
const EXTERNAL_RECIPE_JSON: &str = r#"{
    "name": "custom.tidy-tail",
    "version": 1,
    "description": "external recipe: drop unused tail param then resequence",
    "params": [
        { "name": "func", "required": true },
        { "name": "removed_param", "required": true },
        { "name": "new_params", "required": false, "default": "keep:1,keep:0" }
    ],
    "steps": [
        { "op": "remove-parameter", "label": "drop tail", "bind": { "func": "{{func}}", "param": "{{removed_param}}" } },
        { "op": "change-signature", "bind": { "func": "{{func}}", "params": "{{new_params}}" } }
    ]
}"#;

#[test]
fn recipe_from_file_lands_composed_result() {
    let ws = scratch_dir("fromfile");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);
    let recipe = write(&ws, "tidy.recipe.json", EXTERNAL_RECIPE_JSON);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "--from-file",
            recipe.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=normalize_pair",
            "--set",
            "removed_param=unused",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "recipe --from-file --write should succeed; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let got = std::fs::read_to_string(&file).unwrap();
    // Same composed result as the built-in resequence recipe: tail dropped,
    // remaining params reordered, call sites rewritten — through the same gate.
    assert!(
        got.contains("def normalize_pair(right, left):"),
        "composed signature; got:\n{got}"
    );
    assert!(
        got.contains("normalize_pair(\"R\", \"L\")"),
        "first call; got:\n{got}"
    );
    assert!(
        got.contains("normalize_pair(\"B\", \"A\")"),
        "second call; got:\n{got}"
    );
    assert!(!got.contains("unused"), "unused param dropped; got:\n{got}");
}

#[test]
fn recipe_from_file_dry_run_leaves_disk_untouched() {
    let ws = scratch_dir("fromfile_dry");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);
    let recipe = write(&ws, "tidy.recipe.json", EXTERNAL_RECIPE_JSON);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "--from-file",
            recipe.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=normalize_pair",
            "--set",
            "removed_param=unused",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, RESEQUENCE_SEED, "dry-run must not touch disk");
}

#[test]
fn recipe_from_file_missing_required_is_rejected() {
    let ws = scratch_dir("fromfile_missing");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);
    let recipe = write(&ws, "tidy.recipe.json", EXTERNAL_RECIPE_JSON);

    // removed_param not supplied → the SAME honest hard-error as built-in path.
    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "--from-file",
            recipe.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
            "--set",
            "func=normalize_pair",
            "--write",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "missing required param must be refused"
    );
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, RESEQUENCE_SEED, "rejected recipe must not touch disk");
}

#[test]
fn recipe_from_file_rejects_non_batchable_op() {
    let ws = scratch_dir("fromfile_badop");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);
    // An off-whitelist op must be rejected by the shared lint at load time.
    let bad = r#"{ "name": "custom.bad", "steps": [ { "op": "inline-variable", "bind": {} } ] }"#;
    let recipe = write(&ws, "bad.recipe.json", bad);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "--from-file",
            recipe.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
            "--write",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "non-batchable op must be refused");
    let got = std::fs::read_to_string(&file).unwrap();
    assert_eq!(got, RESEQUENCE_SEED, "rejected recipe must not touch disk");
}

#[test]
fn recipe_from_file_and_name_are_mutually_exclusive() {
    let ws = scratch_dir("fromfile_both");
    let file = write(&ws, "api.py", RESEQUENCE_SEED);
    let recipe = write(&ws, "tidy.recipe.json", EXTERNAL_RECIPE_JSON);

    let out = Command::new(shuttle_bin())
        .args([
            "refactor",
            "recipe",
            "perf.promote-sync-tunables",
            "--from-file",
            recipe.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "passing both a name and --from-file must be refused"
    );
}
