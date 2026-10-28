//! Binary-level CLI integration tests for `shuttle goal --parallel` — the G8/P1
//! concurrency scheduler . Spawns
//! the REAL `shuttle` binary and exercises the whole outer loop end-to-end with ZERO
//! model calls via the two hermetic seams (`--plan` + `--dry-run`), asserting
//! the concurrency schedule attached to the result.
//!
//! The schedule partitions the topological order into *batches*: within a batch
//! every pair of tasks is proven file- AND symbol-disjoint, so a launcher could
//! run the batch concurrently. Any pair shuttle cannot PROVE independent stays
//! serial (§2.3 / §7) — the tests below pin both the parallel-wins path and the
//! two degrade-to-serial paths:
//!
//!   * disjoint files + disjoint symbols            → one wide batch;
//!   * a proven dependency                          → serialized across batches;
//!   * disjoint files but a SHARED symbol (use↔def) → serialized (§2.3 row 1);
//!   * an unknown footprint (no touched files)      → serialized + `degraded`.
//!
//! Exit is always 0 — the machine result (phase/schedule) is the contract.
//! ctags-gated: skips cleanly when universal-ctags is unavailable.

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
    let dir = std::env::temp_dir().join(format!("shuttle_goal_par_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

fn write_intent(dir: &std::path::Path, json: serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("intent.json");
    std::fs::write(&p, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    p
}

fn init_git_repo(dir: &std::path::Path) -> String {
    for args in [
        vec!["init"],
        vec!["config", "user.email", "shuttle-tests@example.invalid"],
        vec!["config", "user.name", "SHUTTLE Tests"],
        vec!["add", "."],
        vec!["commit", "-m", "fixture"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Run `shuttle goal --parallel --plan <plan> --root <ws> --dry-run --json` (plus any
/// extra args), parse stdout as the GoalResult JSON, assert exit 0.
fn run_goal_parallel(
    ws: &std::path::Path,
    plan: &std::path::Path,
    extra: &[&str],
) -> serde_json::Value {
    let mut args: Vec<String> = vec![
        "goal".into(),
        "--plan".into(),
        plan.to_str().unwrap().into(),
        "--root".into(),
        ws.to_str().unwrap().into(),
        "--dry-run".into(),
        "--parallel".into(),
        "--json".into(),
    ];
    for e in extra {
        args.push((*e).into());
    }
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle goal --parallel");
    assert!(
        out.status.success(),
        "shuttle goal --parallel exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("goal stdout is not the JSON GoalResult: {e}\n---\n{stdout}"))
}

fn batch_tasks(r: &serde_json::Value, idx: usize) -> Vec<String> {
    r["schedule"]["batches"][idx]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn identity(
    root: &std::path::Path,
    head: &str,
    table: &shuttle_symbols::SymbolTable,
    name: &str,
) -> shuttle_protocol::SymbolIdentity {
    let symbol = table
        .symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap();
    let relative = std::path::Path::new(&symbol.path)
        .strip_prefix(root)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| symbol.path.replace('\\', "/"));
    shuttle_protocol::SymbolIdentity::from_signature_bytes(
        "fixture/repo",
        head,
        shuttle_symbols::lang_for_path(&relative).unwrap(),
        relative,
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
}

fn write_exact_footprint(ws: &std::path::Path, head: &str) -> std::path::PathBuf {
    let table = shuttle_symbols::SymbolTable::scan(ws.to_str().unwrap()).unwrap();
    let core = identity(ws, head, &table, "core_fn");
    let wrapper = identity(ws, head, &table, "wrap");
    let footprint = shuttle_protocol::SymbolFootprint::new(
        shuttle_protocol::SymbolFootprintBasis {
            repositories: vec![shuttle_protocol::SymbolRepositoryBase {
                repo_id: "fixture/repo".into(),
                base_oid: head.into(),
            }],
            symbol_index_ref: "artifact://fixture-index".into(),
            symbol_index_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                "sha256:{}",
                "7".repeat(64)
            ))
            .unwrap(),
            analyzer_version: "test".into(),
            coverage: shuttle_protocol::SymbolCoverage::Exact,
            freshness: shuttle_protocol::SymbolIndexFreshness::Fresh,
            truncated: false,
            degraded_reasons: Vec::new(),
        },
        vec![
            shuttle_protocol::TaskSymbolFootprint::new(
                "core",
                shuttle_protocol::PlannedSymbolEvidence::validated(Vec::new(), vec![core.clone()]),
                shuttle_protocol::ObservedSymbolEvidence::not_observed(),
            ),
            shuttle_protocol::TaskSymbolFootprint::new(
                "wrapper",
                shuttle_protocol::PlannedSymbolEvidence::validated(vec![core], vec![wrapper]),
                shuttle_protocol::ObservedSymbolEvidence::not_observed(),
            ),
        ],
    )
    .unwrap();
    let path = ws.join("footprint.json");
    std::fs::write(&path, footprint.canonical_json().unwrap()).unwrap();
    path
}

// ---------------------------------------------------------------------------
// E2E 1 — disjoint files AND disjoint symbols co-schedule into ONE wide batch.
// ---------------------------------------------------------------------------

#[test]
fn parallel_batches_two_independent_tasks() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping parallel disjoint-batch E2E");
        return;
    }
    let ws = scratch_dir("disjoint");
    // No import relation, no shared symbol names — provably independent.
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    write(&ws, "beta.py", "def beta_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "alpha", "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                { "id": "beta",  "intent": "edit beta",  "touched_files_guess": ["beta.py"],  "depends_on_guess": [] }
            ]
        }),
    );

    let r = run_goal_parallel(&ws, &plan, &[]);

    assert_eq!(r["phase"], "landed", "expected landed: {r}");
    let sched = &r["schedule"];
    assert!(!sched.is_null(), "--parallel must attach a schedule: {r}");
    assert_eq!(
        sched["batches"].as_array().unwrap().len(),
        1,
        "disjoint tasks share a single batch: {r}"
    );
    assert_eq!(sched["max_width"], 2, "the one batch is 2 wide: {r}");
    assert_eq!(sched["degraded"], false, "nothing was degraded: {r}");
    let mut b0 = batch_tasks(&r, 0);
    b0.sort();
    assert_eq!(b0, vec!["alpha", "beta"], "both tasks in batch 0: {r}");
    // Every task still executes exactly once (batch-major flatten of the order).
    assert_eq!(r["exec"].as_array().unwrap().len(), 2, "exec: {r}");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn advice_out_projects_the_same_final_schedule_with_digest_bindings() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping advice-out E2E");
        return;
    }
    let ws = scratch_dir("advice_out");
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    write(&ws, "beta.py", "def beta_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "advice demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "alpha", "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                { "id": "beta",  "intent": "edit beta",  "touched_files_guess": ["beta.py"],  "depends_on_guess": [] }
            ]
        }),
    );
    let head = init_git_repo(&ws);
    let advice_path = ws.join("evidence").join("parallel-advice.json");
    let result = run_goal_parallel(&ws, &plan, &["--advice-out", advice_path.to_str().unwrap()]);

    let bytes = std::fs::read(&advice_path).expect("advice artifact written");
    let advice = shuttle_nav::parallel::ParallelAdvice::parse(&bytes).expect("valid advice wire");
    assert_eq!(advice.graph_basis.base_oid, head);
    assert_eq!(
        advice.batches[0].task_ids,
        batch_tasks(&result, 0),
        "advice must project the exact schedule returned on stdout"
    );
    assert_eq!(
        advice.max_width as u64,
        result["schedule"]["max_width"].as_u64().unwrap()
    );
    assert_eq!(
        advice.degraded,
        result["schedule"]["degraded"].as_bool().unwrap()
    );
    assert_eq!(
        advice.source_intent_ref,
        format!(
            "artifact://sha256/{}",
            advice
                .source_intent_digest
                .as_str()
                .strip_prefix("sha256:")
                .unwrap()
        )
    );
    let detail: shuttle_protocol::SplitDetail =
        serde_json::from_value(result["split"]["detail"].clone()).expect("split detail");
    assert_eq!(
        advice.graph_basis.correction_digest,
        shuttle_nav::parallel::correction_digest(&detail).unwrap()
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn advice_auto_selects_v2_with_closed_canonical_footprint_and_preserves_model_edges() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("advice_v2");
    write(&ws, "core.py", "def core_fn():\n    return 1\n");
    write(
        &ws,
        "wrapper.py",
        "from core import core_fn\n\ndef wrap():\n    return core_fn()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "hybrid advice",
            "version": "intent/v1",
            "planning_revision": 7,
            "tasks": [
                { "id": "core", "intent": "edit core", "touched_files_guess": ["core.py"], "depends_on_guess": [] },
                { "id": "wrapper", "intent": "edit wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );
    let head = init_git_repo(&ws);
    let footprint_path = write_exact_footprint(&ws, &head);
    let advice_path = ws.join("hybrid-advice.json");
    let result = run_goal_parallel(
        &ws,
        &plan,
        &[
            "--advice-out",
            advice_path.to_str().unwrap(),
            "--symbol-footprint",
            footprint_path.to_str().unwrap(),
        ],
    );
    assert_eq!(result["phase"], "landed");
    let bytes = std::fs::read(&advice_path).unwrap();
    let advice = shuttle_protocol::HybridParallelAdvice::parse(&bytes).unwrap();
    assert_eq!(advice.schema_version, "shuttle_goal_parallel_advice/v2");
    assert_eq!(advice.planning_basis.planning_revision, "7");
    assert_eq!(
        advice.planning_basis.planning_structure_digest,
        advice.source_intent.digest
    );
    assert_eq!(advice.repository_bases[0].base_oid, head);
    assert!(advice
        .model_edges
        .iter()
        .any(|edge| { edge.from == "core" && edge.to == "wrapper" }));
    assert!(advice
        .proposed_hard_edges
        .iter()
        .any(|edge| { edge.edge.from == "core" && edge.edge.to == "wrapper" }));
    assert_eq!(
        advice.canonical_digest().unwrap(),
        shuttle_protocol::HybridParallelAdvice::parse(&bytes)
            .unwrap()
            .canonical_digest()
            .unwrap()
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn advice_version_contract_is_explicit_and_fail_closed() {
    let cases: &[&[&str]] = &[
        &[
            "goal",
            "--plan",
            "intent.json",
            "--dry-run",
            "--parallel",
            "--advice-out",
            "advice.json",
            "--advice-version",
            "v2",
        ],
        &[
            "goal",
            "--plan",
            "intent.json",
            "--dry-run",
            "--parallel",
            "--advice-out",
            "advice.json",
            "--advice-version",
            "v1",
            "--symbol-footprint",
            "footprint.json",
        ],
    ];
    for args in cases {
        let output = Command::new(shuttle_bin()).args(*args).output().unwrap();
        assert!(
            !output.status.success(),
            "invalid advice flags were accepted"
        );
    }
    let help = Command::new(shuttle_bin())
        .args(["goal", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--advice-version"));
    assert!(help.contains("--symbol-footprint"));
    assert!(help.contains("auto"));
}

#[test]
fn advice_v2_stale_footprint_degrades_to_serial() {
    if !ctags_present() {
        return;
    }
    let ws = scratch_dir("advice_v2_stale");
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    write(&ws, "beta.py", "def beta_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "stale advice",
            "version": "intent/v1",
            "planning_revision": 7,
            "tasks": [
                { "id": "alpha", "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                { "id": "beta", "intent": "edit beta", "touched_files_guess": ["beta.py"], "depends_on_guess": [] }
            ]
        }),
    );
    let head = init_git_repo(&ws);
    let footprint = shuttle_protocol::SymbolFootprint::new(
        shuttle_protocol::SymbolFootprintBasis {
            repositories: vec![shuttle_protocol::SymbolRepositoryBase {
                repo_id: "fixture/repo".into(),
                base_oid: head,
            }],
            symbol_index_ref: "artifact://stale-index".into(),
            symbol_index_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                "sha256:{}",
                "8".repeat(64)
            ))
            .unwrap(),
            analyzer_version: "test".into(),
            coverage: shuttle_protocol::SymbolCoverage::None,
            freshness: shuttle_protocol::SymbolIndexFreshness::Stale,
            truncated: false,
            degraded_reasons: vec!["stale".into()],
        },
        ["alpha", "beta"]
            .into_iter()
            .map(|task_id| {
                shuttle_protocol::TaskSymbolFootprint::new(
                    task_id,
                    shuttle_protocol::PlannedSymbolEvidence::planner_guess(Vec::new()),
                    shuttle_protocol::ObservedSymbolEvidence::not_observed(),
                )
            })
            .collect(),
    )
    .unwrap();
    let footprint_path = ws.join("stale-footprint.json");
    std::fs::write(&footprint_path, footprint.canonical_json().unwrap()).unwrap();
    let advice_path = ws.join("stale-advice.json");
    run_goal_parallel(
        &ws,
        &plan,
        &[
            "--advice-out",
            advice_path.to_str().unwrap(),
            "--advice-version",
            "v2",
            "--symbol-footprint",
            footprint_path.to_str().unwrap(),
        ],
    );
    let advice =
        shuttle_protocol::HybridParallelAdvice::parse(&std::fs::read(advice_path).unwrap())
            .expect("degraded v2 advice remains a valid closed envelope");
    assert!(advice.degraded.value);
    assert!(advice
        .degraded
        .reasons
        .iter()
        .any(|reason| reason.contains("stale")));
    assert_eq!(advice.schedule.max_width, 1);
    assert_eq!(advice.schedule.batches.len(), 2);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn advice_out_requires_parallel_at_cli_boundary() {
    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--goal",
            "demo",
            "--dry-run",
            "--advice-out",
            "advice.json",
        ])
        .output()
        .expect("spawn shuttle goal");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--parallel"),
        "clap must explain the missing --parallel requirement: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn advice_out_rejects_reconcile_mode_instead_of_silently_ignoring_it() {
    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--plan",
            "intent.json",
            "--diff",
            "landed.diff",
            "--reconcile",
            "--parallel",
            "--advice-out",
            "advice.json",
        ])
        .output()
        .expect("spawn shuttle goal");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "clap must reject the incompatible modes: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn advice_out_supports_a_stable_detached_head() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping detached-head advice E2E");
        return;
    }
    let ws = scratch_dir("advice_detached");
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    write(&ws, "beta.py", "def beta_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "detached advice", "version": "intent/v1",
            "tasks": [
                { "id": "alpha", "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                { "id": "beta", "intent": "edit beta", "touched_files_guess": ["beta.py"], "depends_on_guess": [] }
            ]
        }),
    );
    let head = init_git_repo(&ws);
    let detached = Command::new("git")
        .args(["checkout", "--detach", &head])
        .current_dir(&ws)
        .output()
        .unwrap();
    assert!(detached.status.success());
    let advice_path = ws.join("detached-advice.json");
    let result = run_goal_parallel(&ws, &plan, &["--advice-out", advice_path.to_str().unwrap()]);
    assert_eq!(result["phase"], "landed");
    let advice = shuttle_nav::parallel::ParallelAdvice::parse(&std::fs::read(advice_path).unwrap())
        .expect("detached advice parses");
    assert_eq!(advice.graph_basis.base_oid, head);
    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// E2E 2 — a PROVEN dependency (import) serializes across two batches.
// ---------------------------------------------------------------------------

#[test]
fn parallel_proven_dependency_serializes_into_two_batches() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping parallel dependency E2E");
        return;
    }
    let ws = scratch_dir("dep");
    // wrapper imports + calls core → symbol graph PROVES core -> wrapper.
    write(
        &ws,
        "core.py",
        "def core_fn():\n    return 1\n\nclass Core:\n    pass\n",
    );
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "edit core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
                { "id": "wrapper", "intent": "edit wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    let r = run_goal_parallel(&ws, &plan, &[]);

    assert_eq!(r["phase"], "landed", "expected landed: {r}");
    let sched = &r["schedule"];
    assert_eq!(
        sched["batches"].as_array().unwrap().len(),
        2,
        "a prereq forces two batches: {r}"
    );
    assert_eq!(sched["max_width"], 1, "each batch is serial: {r}");
    assert_eq!(batch_tasks(&r, 0), vec!["core"], "core runs first: {r}");
    assert_eq!(batch_tasks(&r, 1), vec!["wrapper"], "wrapper second: {r}");
    // The flatten preserves the topo order.
    let order: Vec<&str> = r["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(order, vec!["core", "wrapper"], "order: {r}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 degrade A — disjoint FILES but a SHARED symbol (use↔def, no import) still
// serializes: the symbol-coupling oracle sees the coupling the file oracle
// cannot (design §2.3 row 1). The name-match is a weak/undirected edge so the
// adjudicator still lands Green — the coupling shows up only in the schedule.
// ---------------------------------------------------------------------------

#[test]
fn parallel_symbol_coupling_serializes_disjoint_files() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping parallel symbol-coupling E2E");
        return;
    }
    let ws = scratch_dir("symcouple");
    // defs.py DEFINES shared_helper; uses.py CALLS it by name (no import — so no
    // provable directed edge, but a real use↔def coupling).
    write(&ws, "defs.py", "def shared_helper():\n    return 1\n");
    write(
        &ws,
        "uses.py",
        "def consumer():\n    return shared_helper()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "defs", "intent": "edit defs", "touched_files_guess": ["defs.py"], "depends_on_guess": [] },
                { "id": "uses", "intent": "edit uses", "touched_files_guess": ["uses.py"], "depends_on_guess": [] }
            ]
        }),
    );

    let r = run_goal_parallel(&ws, &plan, &[]);

    // It still LANDS (the coupling is not a proven dependency edge)…
    assert_eq!(r["phase"], "landed", "must still land Green: {r}");
    let sched = &r["schedule"];
    // …but the two tasks do NOT co-schedule: the shared symbol serializes them.
    assert_eq!(
        sched["batches"].as_array().unwrap().len(),
        2,
        "shared symbol must serialize disjoint files: {r}"
    );
    assert_eq!(sched["max_width"], 1, "no batch is wider than one: {r}");
    // The serialize reason names the symbol coupling (mechanical evidence).
    let reasons: Vec<String> = sched["serialized"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| s["reason"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        reasons.iter().any(|m| m.contains("symbol-coupling")),
        "a serialize reason must cite symbol-coupling: {reasons:?} / {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 degrade B — an UNKNOWN footprint (a task with no touched files) cannot be
// proven independent, so it is serialized and the schedule is flagged
// `degraded` (design §2.3 row 2 / §7 — never an optimistic parallel guess).
// ---------------------------------------------------------------------------

#[test]
fn parallel_unknown_footprint_degrades_to_serial() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping parallel unknown-footprint E2E");
        return;
    }
    let ws = scratch_dir("unknown");
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "alpha",  "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                // No touched_files_guess: shuttle cannot prove what this task touches.
                { "id": "mystery", "intent": "do something", "touched_files_guess": [], "depends_on_guess": [] }
            ]
        }),
    );

    let r = run_goal_parallel(&ws, &plan, &[]);

    assert_eq!(r["phase"], "landed", "still lands, just serially: {r}");
    let sched = &r["schedule"];
    assert_eq!(
        sched["degraded"], true,
        "unknown footprint must flag the schedule degraded: {r}"
    );
    assert_eq!(
        sched["max_width"], 1,
        "degrade means no batch runs concurrently: {r}"
    );
    assert_eq!(
        sched["batches"].as_array().unwrap().len(),
        2,
        "the two tasks are split across serial batches: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// Contract — WITHOUT --parallel, no schedule is computed or serialized (the
// serial path is byte-for-byte the pre-G8 behaviour; the hint is opt-in).
// ---------------------------------------------------------------------------

#[test]
fn no_parallel_flag_attaches_no_schedule() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping no-parallel contract E2E");
        return;
    }
    let ws = scratch_dir("serial");
    write(&ws, "alpha.py", "def alpha_fn():\n    return 1\n");
    write(&ws, "beta.py", "def beta_fn():\n    return 2\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "alpha", "intent": "edit alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
                { "id": "beta",  "intent": "edit beta",  "touched_files_guess": ["beta.py"],  "depends_on_guess": [] }
            ]
        }),
    );

    // Same runner, but no --parallel flag.
    let out = Command::new(shuttle_bin())
        .args([
            "goal",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .expect("spawn shuttle goal (serial)");
    assert!(out.status.success(), "serial goal exited non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let r: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(r["phase"], "landed", "expected landed: {r}");
    assert!(
        r.get("schedule").is_none(),
        "serial path must not carry a schedule (skip_serializing_if None): {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
