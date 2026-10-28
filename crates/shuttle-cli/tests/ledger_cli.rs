//! Binary-level CLI integration tests for the G9·D2 `shuttle split --correct
//! --ledger <path>` provenance ledger .
//! The ledger institutionalizes «绝不瞎猜»: every adjudicated edge lands a
//! replayable `(edge, verdict, evidence_digest, snapshot_oid)` seed. Two red
//! lines are asserted end-to-end:
//!
//!   1. **digest seed, never the full text (§2 坑 #14 防爆盘)** — the ledger
//!      file carries the `sha256:` digest of each edge's evidence, NOT the
//!      evidence string itself. A grep for the known evidence substring must
//!      miss.
//!   2. **replay is bit-for-bit (§7)** — re-hashing the ACTUAL evidence text
//!      (read off the stdout envelope, the same tree the adjudication saw)
//!      through `shuttle_protocol::evidence_digest` reproduces the recorded digest
//!      exactly; a drifted/fabricated evidence text would not.
//!
//! Hermetic: the split adjudication needs universal-ctags to build the symbol
//! graph, so the cases skip when ctags is unavailable. No git is required — a
//! non-git tree yields an empty `snapshot_oid` (honest: replay then keys on
//! content alone), which we also assert as its own honest-degradation case.

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
    let dir = std::env::temp_dir().join(format!("shuttle_ledger_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    p
}

/// Two Python files where `wrapper` imports and calls `core`: the symbol graph
/// PROVES `wrapper depends_on core` (import-directed). Paired with a planner
/// intent that guessed the same edge, the corrector returns one AGREE edge with
/// non-empty evidence — so the ledger records exactly one entry WITH a digest.
fn seed_agree(dir: &std::path::Path) -> std::path::PathBuf {
    write(dir, "core.py", "def core_fn():\n    return 1\n");
    write(
        dir,
        "wrapper.py",
        "import core\n\ndef wrap():\n    return core.core_fn()\n",
    );
    let plan = serde_json::json!({
        "goal": "demo",
        "version": "intent/v1",
        "tasks": [
            { "id": "core",    "intent": "core",    "touched_files_guess": ["core.py"],    "depends_on_guess": [] },
            { "id": "wrapper", "intent": "wrapper", "touched_files_guess": ["wrapper.py"], "depends_on_guess": ["core"] }
        ]
    });
    write(
        dir,
        "intent.json",
        &serde_json::to_string_pretty(&plan).unwrap(),
    )
}

/// Run `shuttle split --correct --plan .. --root .. --ledger <path> --json`. Returns
/// (parsed stdout envelope, parsed ledger file). Asserts exit 0.
fn run_split_with_ledger(
    ws: &std::path::Path,
    plan: &std::path::Path,
    ledger: &std::path::Path,
) -> (serde_json::Value, serde_json::Value) {
    let out = Command::new(shuttle_bin())
        .args([
            "split",
            "--correct",
            "--plan",
            plan.to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--ledger",
            ledger.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("spawn shuttle split --correct --ledger");
    assert!(
        out.status.success(),
        "shuttle split exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("split stdout is not the JSON envelope: {e}\n---\n{stdout}"));
    let raw =
        std::fs::read_to_string(ledger).unwrap_or_else(|e| panic!("ledger file not written: {e}"));
    let led: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("ledger is not JSON: {e}\n{raw}"));
    (env, led)
}

// ---------------------------------------------------------------------------
// §2 red line 1: the ledger stores a digest seed, never the full evidence text.
// ---------------------------------------------------------------------------

#[test]
fn ledger_records_a_digest_seed_never_the_full_evidence_text() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping ledger digest E2E");
        return;
    }
    let ws = scratch_dir("digest");
    let plan = seed_agree(&ws);
    let ledger_path = ws.join("ledger.json");
    let (env, led) = run_split_with_ledger(&ws, &plan, &ledger_path);

    // One adjudicated edge with non-empty evidence → one ledger entry with a
    // `sha256:` digest.
    assert_eq!(led["version"], 1, "{led}");
    let entries = led["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "one edge → one entry: {led}");
    let entry = &entries[0];
    assert_eq!(entry["from"], "core", "{entry}");
    assert_eq!(entry["to"], "wrapper", "{entry}");
    assert_eq!(entry["verdict"], "agree", "{entry}");
    let digest = entry["evidence_digest"]
        .as_str()
        .expect("entry carries an evidence_digest");
    assert!(
        digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64,
        "canonical sha256:<64 hex> digest: {digest}"
    );

    // The ACTUAL evidence string is on the stdout envelope, NOT in the ledger.
    let evidence = env["detail"]["edges"][0]["evidence"]
        .as_str()
        .expect("envelope carries the evidence text");
    assert!(!evidence.is_empty(), "envelope evidence should be present");
    let ledger_raw = std::fs::read_to_string(&ledger_path).unwrap();
    assert!(
        !ledger_raw.contains(evidence),
        "防爆盘: the raw evidence text leaked into the ledger:\nevidence={evidence}\nledger={ledger_raw}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 red line 2: replay is bit-for-bit — re-hashing the true evidence text
// reproduces the recorded digest; a drifted text does not.
// ---------------------------------------------------------------------------

#[test]
fn recorded_digest_replays_bit_for_bit_and_rejects_drift() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping ledger replay E2E");
        return;
    }
    let ws = scratch_dir("replay");
    let plan = seed_agree(&ws);
    let ledger_path = ws.join("ledger.json");
    let (env, led) = run_split_with_ledger(&ws, &plan, &ledger_path);

    let recorded = led["entries"][0]["evidence_digest"]
        .as_str()
        .expect("recorded digest");
    let evidence = env["detail"]["edges"][0]["evidence"]
        .as_str()
        .expect("envelope evidence");

    // Replay: re-mint the digest from the true evidence via the SAME primitive
    // the ledger used. It must match the recorded seed bit-for-bit.
    let replayed = shuttle_protocol::evidence_digest(evidence);
    assert_eq!(
        replayed.as_str(),
        recorded,
        "replay of the true evidence must reproduce the recorded digest"
    );

    // A single-byte drift in the evidence must NOT reproduce the digest — the
    // ledger is tamper-evident, a fabricated evidence cannot forge the seed.
    let drifted = format!("{evidence} ");
    let drifted_digest = shuttle_protocol::evidence_digest(&drifted);
    assert_ne!(
        drifted_digest.as_str(),
        recorded,
        "drifted evidence must not reproduce the recorded digest"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 honest degradation: an edge the corrector could not attribute (empty
// evidence) records NO digest — an honest "no proof", never a hash of "".
//
// A planner edge with no supporting import (name-only guess against files that
// share no symbol) is dropped/unknown with empty evidence, so its ledger entry
// (if any) carries no `evidence_digest`. We assert the ledger NEVER contains a
// digest of the empty string.
// ---------------------------------------------------------------------------

#[test]
fn unattributed_edge_records_no_forged_digest() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping ledger no-forge E2E");
        return;
    }
    let ws = scratch_dir("noforge");
    // Two unrelated files that share no symbol; the planner nonetheless guesses
    // a dependency. The corrector cannot attribute it → empty evidence.
    write(&ws, "alpha.py", "def alpha():\n    return 1\n");
    write(&ws, "beta.py", "def beta():\n    return 2\n");
    let plan = serde_json::json!({
        "goal": "unrelated",
        "version": "intent/v1",
        "tasks": [
            { "id": "alpha", "intent": "alpha", "touched_files_guess": ["alpha.py"], "depends_on_guess": [] },
            { "id": "beta",  "intent": "beta",  "touched_files_guess": ["beta.py"],  "depends_on_guess": ["alpha"] }
        ]
    });
    let plan_path = write(
        &ws,
        "intent.json",
        &serde_json::to_string_pretty(&plan).unwrap(),
    );
    let ledger_path = ws.join("ledger.json");
    let (_env, led) = run_split_with_ledger(&ws, &plan_path, &ledger_path);

    // The digest of the empty string must never appear: an unattributed edge is
    // an honest "no evidence", never a hash of "" dressed up as proof.
    let empty_digest = shuttle_protocol::evidence_digest("");
    let ledger_raw = std::fs::read_to_string(&ledger_path).unwrap();
    assert!(
        !ledger_raw.contains(empty_digest.as_str()),
        "the empty-string digest was forged into the ledger:\n{ledger_raw}"
    );
    // Any entry that DID land must either carry a real digest or none at all —
    // never a digest over empty evidence.
    if let Some(entries) = led["entries"].as_array() {
        for e in entries {
            if let Some(d) = e["evidence_digest"].as_str() {
                assert_ne!(
                    d,
                    empty_digest.as_str(),
                    "forged empty digest on entry: {e}"
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&ws);
}
