//! Binary-level CLI integration tests for the G4 §7「绝不瞎猜」boundary suite
//! .
//!
//! These are the "eval sample expansion" for the four hard topologies §2 #20
//! calls out as under-covered — cycle deps, Python dynamic imports, mixed
//! language, and >100-file large repos — verified end-to-end against the REAL
//! `shuttle` binary. Every case asserts the SAME invariant from a different angle:
//! when the static symbol graph cannot *prove* a directed dependency, the
//! outer loop degrades (weak NameMatch edge / UNKNOWN / escalate) and NEVER
//! fabricates a proven edge. Concretely:
//!
//!   * A1 name-match back-edge — a reference running *against* the import
//!     direction (the callee's file does not import the owner's) degrades to a
//!     weak, undirected NameMatch that contributes to nothing, so no phantom
//!     reverse edge and no false cycle appear. Only the one truly import-proven
//!     edge survives. (§2 #1 / #9)
//!   * A1 proven cycle — mutual imports prove BOTH directions (both AGREE), and
//!     the cycle is caught at topological ordering → escalated, never run.
//!   * A4 mixed language — a Go `helper()` call colliding by NAME with a Python
//!     `def helper` is a cross-language coincidence, never an import. The
//!     directed impact closure excludes it while the *undirected* one shows it,
//!     and `split --correct` never emits a cross-language ADD. (§2 #4)
//!   * A5 dynamic import — an `importlib.import_module(name)` call is a
//!     structural blind spot: the guessed dependency is neither provable nor
//!     disprovable → UNKNOWN → escalate. Explicitly unknowable, never guessed.
//!     (§2 #5)
//!   * B7 large repo — a 120-file import chain adjudicates and topo-orders with
//!     bounded latency and, being fully import-proven, lands with zero review
//!     surface and a correct linear order. (§2 #7)
//!
//! Hermetic & model-free: scratch repos under the OS temp root (honours
//! TMPDIR); the two seams `--plan` (intent DAG without pi) and `--dry-run`
//! (no-op executor) let the whole loop run with ZERO LLM calls. Exit is always
//! 0 — the machine result (phase/state/verdicts) is the contract. ctags-gated:
//! cases skip cleanly without universal-ctags.

use std::io::Write;
use std::process::Command;
use std::time::Instant;

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
    let dir = std::env::temp_dir().join(format!("shuttle_g4_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

fn write_intent(dir: &std::path::Path, json: serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("intent.json");
    std::fs::write(&p, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    p
}

/// Run `shuttle goal --plan … --dry-run --json`, parse stdout as GoalResult. Exit 0.
fn run_goal_json(ws: &std::path::Path, plan: &std::path::Path) -> serde_json::Value {
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
        .expect("spawn shuttle goal");
    assert!(
        out.status.success(),
        "shuttle goal exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("goal stdout is not JSON GoalResult: {e}\n---\n{stdout}"))
}

/// Run `shuttle split --correct --plan … --json`, parse stdout as the ToolOutcome
/// envelope. Exit is always 0 (green or blocked=needs-review is a machine
/// result the caller routes on, not a process failure).
fn run_split_json(ws: &std::path::Path, plan: &std::path::Path) -> serde_json::Value {
    let out = Command::new(shuttle_bin())
        .args([
            "split",
            "--correct",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle split");
    assert!(
        out.status.success(),
        "shuttle split exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("split stdout is not JSON ToolOutcome: {e}\n---\n{stdout}"))
}

/// True if any impacted path ends with `name` on a `/` boundary (or equals it).
/// Impact reports paths rooted at `--root` (an absolute scratch dir here), so we
/// compare by trailing file component rather than by string equality.
fn impact_has(paths: &std::collections::BTreeSet<String>, name: &str) -> bool {
    paths
        .iter()
        .any(|p| p == name || p.ends_with(&format!("/{name}")))
}

/// Run `shuttle impact --changed <file> [--directed] --json`, parse the ImpactHit
/// array. Exit 0. Returns the set of impacted file paths (deduped).
fn run_impact_paths(
    ws: &std::path::Path,
    changed: &str,
    directed: bool,
) -> std::collections::BTreeSet<String> {
    let mut args: Vec<String> = vec![
        "impact".into(),
        "--changed".into(),
        changed.into(),
        "--root".into(),
        ws.to_str().unwrap().into(),
        "--json".into(),
    ];
    if directed {
        args.push("--directed".into());
    }
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle impact");
    assert!(
        out.status.success(),
        "shuttle impact exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("impact stdout is not JSON: {e}\n---\n{stdout}"));
    hits.as_array()
        .expect("impact returns a JSON array of hits")
        .iter()
        .filter_map(|h| h.get("path").and_then(|p| p.as_str()).map(str::to_string))
        .collect()
}

/// Every `SplitEdge` in the split envelope's typed detail.
fn split_edges(env: &serde_json::Value) -> Vec<serde_json::Value> {
    env["detail"]["edges"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

// ===========================================================================
// A1 — name-match back-edge: a reference against the import direction degrades
// to a weak NameMatch that contributes to nothing. No phantom reverse edge, no
// false cycle; only the one import-proven edge survives. (§2 #1 / #9)
// ===========================================================================

#[test]
fn a1_name_match_back_edge_does_not_fabricate_reverse_edge() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping A1 back-edge E2E");
        return;
    }
    let ws = scratch_dir("a1_backedge");
    // a.py imports b (so a depends on b — the ONE provable direction).
    // b.py references `helper` (defined in a.py) but does NOT import a: this
    // back-reference runs against the import direction → weak NameMatch,
    // proves nothing. The corrector must NOT emit an a→b (b depends on a) edge.
    write(
        &ws,
        "a.py",
        "import b\n\ndef helper():\n    return b.thing()\n",
    );
    write(&ws, "b.py", "def thing():\n    return helper()\n");
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "back-edge",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "b", "touched_files_guess": ["b.py"], "depends_on_guess": [] }
            ]
        }),
    );

    let env = run_split_json(&ws, &plan);
    let edges = split_edges(&env);
    // Exactly one proven edge: b→a (a imports b ⇒ a depends on b ⇒ prereq=b).
    assert_eq!(edges.len(), 1, "expected exactly one proven edge: {env}");
    assert_eq!(edges[0]["from"], "b", "prereq must be b: {env}");
    assert_eq!(edges[0]["to"], "a", "dependent must be a: {env}");
    assert_eq!(edges[0]["verdict"], "add", "{env}");
    // Crucially, NO reverse a→b edge was fabricated from the back-reference.
    assert!(
        !edges.iter().any(|e| e["from"] == "a" && e["to"] == "b"),
        "back-reference must NOT fabricate a reverse edge: {env}"
    );
    assert_eq!(
        env["detail"]["review_surface"], 0,
        "no review needed: {env}"
    );

    // And the goal loop lands with an acyclic topo order prereq-first.
    let r = run_goal_json(&ws, &plan);
    assert_eq!(r["phase"], "landed", "back-edge must land acyclic: {r}");
    assert_eq!(r["escalated"], false, "{r}");
    let order: Vec<&str> = r["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(order, vec!["b", "a"], "topo order prereq-first: {r}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ===========================================================================
// A1 — proven cycle: mutual imports prove BOTH directions, so both intent
// edges are AGREE, but the two tasks form a cycle topo ordering must reject.
// (§2 #1 / #9 — the *provable* cycle, distinct from the phantom one above.)
// ===========================================================================

#[test]
fn a1_proven_cycle_escalates_and_never_executes() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping A1 proven-cycle E2E");
        return;
    }
    let ws = scratch_dir("a1_cycle");
    write(
        &ws,
        "core.py",
        "import wrapper\n\ndef core_fn():\n    return wrapper.wrap()\n",
    );
    write(
        &ws,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "cyclic",
            "version": "intent/v1",
            "tasks": [
                { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": ["wrapper"] },
                { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
            ]
        }),
    );

    // split sees both directions as import-proven → both AGREE, zero review.
    let env = run_split_json(&ws, &plan);
    let edges = split_edges(&env);
    assert_eq!(edges.len(), 2, "mutual imports prove both edges: {env}");
    assert!(
        edges.iter().all(|e| e["verdict"] == "agree"),
        "both directions are AGREE: {env}"
    );

    // The goal loop catches the cycle at topo ordering → escalate, never run.
    let r = run_goal_json(&ws, &plan);
    assert_eq!(r["phase"], "escalated", "proven cycle must escalate: {r}");
    assert_eq!(r["escalated"], true, "{r}");
    assert!(
        r["review"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.as_str().unwrap_or("").contains("cycle")),
        "review must report the dependency cycle: {r}"
    );
    assert!(
        r["exec"].as_array().map(|a| a.is_empty()).unwrap_or(true),
        "cycle must not execute: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ===========================================================================
// A4 — mixed language: a Go `helper()` call colliding by NAME with a Python
// `def helper` is a cross-language coincidence, never an import. The directed
// impact closure excludes it; the undirected one shows it; and split never
// emits a cross-language ADD. (§2 #4)
// ===========================================================================

#[test]
fn a4_mixed_language_name_collision_excluded_when_directed() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping A4 mixed-language E2E");
        return;
    }
    let ws = scratch_dir("a4_mixed");
    write(&ws, "util.py", "def helper():\n    return 1\n");
    write(
        &ws,
        "app.py",
        "from util import helper\n\ndef run():\n    return helper()\n",
    );
    // Go files that call a same-named `helper()` — a NAME collision, not an
    // import of the Python symbol. server/handler are same Go package.
    write(
        &ws,
        "server.go",
        "package main\n\nfunc Serve() {\n\thelper()\n}\n",
    );
    write(
        &ws,
        "handler.go",
        "package main\n\nfunc Handle() {\n\thelper()\n\tServe()\n}\n",
    );

    // Undirected impact of util.py's `helper` DOES surface the cross-language
    // name collisions (server.go / handler.go) — that is the weak, undirected
    // view by construction.
    let undirected = run_impact_paths(&ws, "util.py", false);
    assert!(
        impact_has(&undirected, "server.go") || impact_has(&undirected, "handler.go"),
        "undirected impact should surface the name collision: {undirected:?}"
    );
    // Directed impact PROVES nothing cross-language: Go cannot import a Python
    // def, so the collision is pruned — only the true Python dependent remains.
    let directed = run_impact_paths(&ws, "util.py", true);
    assert!(
        impact_has(&directed, "util.py"),
        "directed impact keeps the changed file: {directed:?}"
    );
    assert!(
        !impact_has(&directed, "server.go") && !impact_has(&directed, "handler.go"),
        "directed impact must exclude cross-language name collisions: {directed:?}"
    );

    // split --correct must never emit a cross-language ADD (util→server/handler).
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "mixed language",
            "version": "intent/v1",
            "tasks": [
                { "id": "util",    "intent": "py util",   "touched_files_guess": ["util.py"],    "depends_on_guess": [] },
                { "id": "app",     "intent": "py app",    "touched_files_guess": ["app.py"],     "depends_on_guess": [] },
                { "id": "server",  "intent": "go server", "touched_files_guess": ["server.go"],  "depends_on_guess": [] },
                { "id": "handler", "intent": "go handler","touched_files_guess": ["handler.go"], "depends_on_guess": [] }
            ]
        }),
    );
    let env = run_split_json(&ws, &plan);
    let edges = split_edges(&env);
    // No edge may cross the language boundary via the `helper` name collision.
    for e in &edges {
        let from = e["from"].as_str().unwrap_or("");
        let to = e["to"].as_str().unwrap_or("");
        assert!(
            !(from == "util" && (to == "server" || to == "handler")),
            "cross-language ADD fabricated (util→{to}): {env}"
        );
    }
    // The real same-language proofs DO appear: util→app (Python import) and
    // server→handler (Go same-package). Both ADD, both from the symbol graph.
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "util" && e["to"] == "app" && e["verdict"] == "add"),
        "real Python import util→app must be proven: {env}"
    );
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "server" && e["to"] == "handler" && e["verdict"] == "add"),
        "real Go same-package server→handler must be proven: {env}"
    );
    assert_eq!(
        env["detail"]["review_surface"], 0,
        "all edges mechanically decidable, no review: {env}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ===========================================================================
// A5 — dynamic import: importlib.import_module(name) is a structural blind
// spot. The guessed plugin→loader dependency is neither provable nor
// disprovable → UNKNOWN → escalate. Explicitly unknowable, never guessed.
// (§2 #5)
// ===========================================================================

#[test]
fn a5_dynamic_import_is_unknown_and_escalates() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping A5 dynamic-import E2E");
        return;
    }
    let ws = scratch_dir("a5_dyn");
    write(&ws, "plugin.py", "def run():\n    return 1\n");
    // loader dynamically imports by string — no static import edge exists.
    write(
        &ws,
        "loader.py",
        "import importlib\n\ndef boot(name):\n    mod = importlib.import_module(name)\n    return mod.run()\n",
    );
    let plan = write_intent(
        &ws,
        serde_json::json!({
            "goal": "dynamic import",
            "version": "intent/v1",
            "tasks": [
                { "id": "plugin", "intent": "plugin", "touched_files_guess": ["plugin.py"], "depends_on_guess": [] },
                // A GUESSED dependency that dynamic import makes statically invisible.
                { "id": "loader", "intent": "loader", "touched_files_guess": ["loader.py"], "depends_on_guess": ["plugin"] }
            ]
        }),
    );

    // split: the guessed edge is UNKNOWN (no import corroboration), review=1,
    // and the envelope is blocked=needs-review — never a fabricated ADD/AGREE.
    let env = run_split_json(&ws, &plan);
    assert_eq!(env["state"], "blocked", "unknown edge blocks: {env}");
    assert_eq!(env["reason_code"], "needs-review", "{env}");
    let edges = split_edges(&env);
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "plugin" && e["to"] == "loader" && e["verdict"] == "unknown"),
        "dynamic-import edge must be UNKNOWN, never guessed: {env}"
    );
    assert_eq!(env["detail"]["added"], 0, "nothing may be added: {env}");

    // goal: the unprovable edge escalates to the review gate; nothing executes.
    let r = run_goal_json(&ws, &plan);
    assert_eq!(r["phase"], "escalated", "dynamic import must escalate: {r}");
    assert_eq!(r["escalated"], true, "{r}");
    assert!(
        r["review"].as_array().unwrap().iter().any(|m| {
            let s = m.as_str().unwrap_or("");
            s.contains("plugin") && s.contains("loader")
        }),
        "review must name the unprovable plugin→loader edge: {r}"
    );
    assert!(
        r["exec"].as_array().map(|a| a.is_empty()).unwrap_or(true),
        "must not execute when escalated: {r}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ===========================================================================
// B7 — large repo: a 120-file import chain adjudicates and topo-orders with
// bounded latency, lands with zero review surface (fully import-proven), and
// yields a correct linear order. (§2 #7 — O(tasks²×files²) latency guard.)
// ===========================================================================

#[test]
fn b7_large_repo_chain_lands_in_bounded_time() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping B7 large-repo E2E");
        return;
    }
    let ws = scratch_dir("b7_big");
    const N: usize = 120;
    // mod{i} imports mod{i-1}; task t{i} depends_on t{i-1}. A pure linear chain
    // whose every edge is import-proven ⇒ all AGREE, acyclic.
    let mut tasks: Vec<serde_json::Value> = Vec::with_capacity(N);
    for i in 0..N {
        let fname = format!("mod{i}.py");
        let (body, dep) = if i == 0 {
            (format!("def f{i}():\n    return {i}\n"), vec![])
        } else {
            (
                format!(
                    "import mod{prev}\n\ndef f{i}():\n    return mod{prev}.f{prev}()\n",
                    prev = i - 1
                ),
                vec![format!("t{prev}", prev = i - 1)],
            )
        };
        write(&ws, &fname, &body);
        tasks.push(serde_json::json!({
            "id": format!("t{i}"),
            "intent": format!("mod{i}"),
            "touched_files_guess": [fname],
            "depends_on_guess": dep,
        }));
    }
    let plan = write_intent(
        &ws,
        serde_json::json!({ "goal": "big", "version": "intent/v1", "tasks": tasks }),
    );

    let start = Instant::now();
    let r = run_goal_json(&ws, &plan);
    let elapsed = start.elapsed();

    assert_eq!(r["phase"], "landed", "linear chain must land: {r}");
    assert_eq!(r["escalated"], false, "no escalation on proven chain: {r}");
    let order: Vec<&str> = r["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(order.len(), N, "all {N} tasks ordered: {r}");
    assert_eq!(order.first(), Some(&"t0"), "chain root first: {r}");
    assert_eq!(
        order.last(),
        Some(&format!("t{}", N - 1).as_str()),
        "chain tail last: {r}"
    );
    // Prereq strictly precedes dependent for every consecutive pair.
    for i in 1..N {
        let prev = format!("t{}", i - 1);
        let cur = format!("t{i}");
        let ip = order.iter().position(|&x| x == prev).unwrap();
        let ic = order.iter().position(|&x| x == cur).unwrap();
        assert!(ip < ic, "t{} must precede t{i} in order: {r}", i - 1);
    }
    assert_eq!(
        r["feedback"]["review_surface"], 0,
        "fully proven chain has zero review surface: {r}"
    );
    // Latency guard: a 120-node chain must adjudicate + order + dry-run land in
    // well under the process-spawn-dominated ceiling. Generous to stay robust
    // on loaded CI, but bounded — a super-linear blowup would trip it.
    assert!(
        elapsed.as_secs() < 30,
        "120-file goal loop took too long ({elapsed:?}) — latency regression"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
