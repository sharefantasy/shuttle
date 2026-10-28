//! Binary-level CLI integration tests for `shuttle index` — the G7·U2 **time-truth**
//! persistent-index surface .
//! Spawns the REAL `shuttle` binary and drives the content-addressed on-disk symbol
//! index end-to-end over hermetic scratch repos.
//!
//! The iron contract under test:
//!   * `shuttle index` refreshes a shareable on-disk cache under `.shuttle/` — a file, never
//!     a resident service (daemonless). Re-running with no edits re-scans NOTHING
//!     (every file a content-addressed hit); editing exactly one file rebuilds
//!     exactly one entry (FNV-1a `(hash,size)` — a hit is a proof, not an mtime bet).
//!   * `shuttle index --status` is the fast read-only freshness verdict. It reuses T2's
//!     exact-match watermark rule: Fresh iff the recorded `index_base_oid` equals
//!     the current HEAD oid exactly; drift / cold / version-mismatch is
//!     `stale-index` (§7 «绝不瞎猜» — never reason off a stale index).
//!
//! Hermetic: scratch dirs live under the OS temp root (honours TMPDIR). The
//! git-backed watermark cases skip when a scratch `.git/config` cannot be created
//! (some sandboxes permit the `git` binary but block writes to `.git/config`); the
//! non-git content-addressing cases always run. All cases require `universal-ctags`
//! and skip cleanly without it (the symbol scan degrades, never panics).

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

/// A bare `git --version` is not enough: some sandboxes permit the binary but
/// block writes to `.git/config`, in which case the git-backed watermark cases
/// must skip (the freshness rule keys on `git rev-parse HEAD`).
fn can_init_git() -> bool {
    let probe = scratch_dir("initprobe");
    let ok = Command::new("git")
        .args(["init", "-q", "--template="])
        .current_dir(&probe)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && probe.join(".git").join("config").exists();
    let _ = std::fs::remove_dir_all(&probe);
    ok
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_index_it_{tag}_{nanos}"));
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_seed(dir: &std::path::Path) {
    git(dir, &["init", "-q", "--template="]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "seed"]);
}

fn head_oid(dir: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_pure(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut full = vec!["index", "--refresh", "--root", dir.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(shuttle_bin())
        .args(full)
        .output()
        .expect("spawn pure shuttle index")
}

/// `shuttle index --json [--status] --root <ws> --cache-path <p>` → parsed JSON.
/// Asserts exit 0 (the machine contract: the JSON envelope is what a caller
/// routes on; there is no hard-fail path for `shuttle index`).
fn run_index_json(
    ws: &std::path::Path,
    cache: &std::path::Path,
    status: bool,
) -> serde_json::Value {
    let mut args: Vec<String> = vec!["index".into(), "--json".into()];
    if status {
        args.push("--status".into());
    }
    args.push("--root".into());
    args.push(ws.to_str().unwrap().into());
    args.push("--cache-path".into());
    args.push(cache.to_str().unwrap().into());
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle index");
    assert!(
        out.status.success(),
        "shuttle index exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not index JSON: {e}\n---\n{stdout}"))
}

fn seed_files(dir: &std::path::Path) {
    write(dir, "a.py", "def alpha():\n    return 1\n");
    write(dir, "b.py", "def beta():\n    return 2\n");
    write(dir, "pkg/c.py", "def gamma():\n    return 3\n");
}

/// Non-git content addressing: the first refresh rebuilds everything; a second
/// refresh with no edits is ALL hits (re-scans nothing); editing exactly one
/// file rebuilds exactly one entry. This is the core "a hit is a proof" claim,
/// and it holds even without git (the tracked set falls back to a full scan and
/// every file is still content-hashed for the next delta).
#[test]
fn nongit_refresh_is_content_addressed_delta() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping index CLI E2E");
        return;
    }
    let ws = scratch_dir("nongit_delta");
    seed_files(&ws);
    let cache = ws.join(".shuttle").join("symbols.index");

    // Cold → full rebuild, some symbols discovered.
    let first = run_index_json(&ws, &cache, false);
    let total = first["total_files"].as_u64().unwrap();
    assert!(total >= 3, "expected >=3 files, got {total}: {first}");
    assert_eq!(first["cache_hits"].as_u64().unwrap(), 0, "cold has no hits");
    assert_eq!(
        first["rebuilt"].as_u64().unwrap(),
        total,
        "cold rebuilds all"
    );
    assert!(
        first["symbols"].as_u64().unwrap() >= 3,
        "expected >=3 symbols: {first}"
    );

    // Re-run, no edits → every file a content-addressed hit, zero rebuilt.
    let second = run_index_json(&ws, &cache, false);
    assert_eq!(second["total_files"].as_u64().unwrap(), total);
    assert_eq!(
        second["cache_hits"].as_u64().unwrap(),
        total,
        "no-edit refresh must be all hits: {second}"
    );
    assert_eq!(
        second["rebuilt"].as_u64().unwrap(),
        0,
        "no-edit rebuilds nothing"
    );

    // Edit exactly one file → exactly one rebuild, the rest still hits.
    write(&ws, "a.py", "def alpha():\n    return 999\n");
    let third = run_index_json(&ws, &cache, false);
    assert_eq!(third["total_files"].as_u64().unwrap(), total);
    assert_eq!(
        third["rebuilt"].as_u64().unwrap(),
        1,
        "editing one file rebuilds exactly one entry: {third}"
    );
    assert_eq!(
        third["cache_hits"].as_u64().unwrap(),
        total - 1,
        "the untouched files stay hits: {third}"
    );
}

/// Dropping a file drops exactly its manifest entry — content addressing never
/// carries forward a vanished path (no stale tags survive a deletion).
#[test]
fn nongit_drops_vanished_file() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping index CLI E2E");
        return;
    }
    let ws = scratch_dir("nongit_drop");
    seed_files(&ws);
    let cache = ws.join(".shuttle").join("symbols.index");

    let first = run_index_json(&ws, &cache, false);
    let total = first["total_files"].as_u64().unwrap();

    std::fs::remove_file(ws.join("b.py")).unwrap();
    let second = run_index_json(&ws, &cache, false);
    assert_eq!(
        second["total_files"].as_u64().unwrap(),
        total - 1,
        "the removed file leaves the tracked set: {second}"
    );
    assert_eq!(
        second["dropped"].as_u64().unwrap(),
        1,
        "exactly one prior entry dropped: {second}"
    );
}

/// The git-backed exact-match watermark (T2 reuse): after a refresh the status
/// is Fresh for the committed HEAD; a NEW commit drifts the watermark and the
/// SAME index reads back `stale-index` (never silently reused).
#[test]
fn git_status_fresh_then_stale_on_drift() {
    if !ctags_present() {
        eprintln!("universal-ctags unavailable; skipping index CLI E2E");
        return;
    }
    if !can_init_git() {
        eprintln!("git repo init blocked in sandbox; skipping git watermark E2E");
        return;
    }
    let ws = scratch_dir("git_watermark");
    seed_files(&ws);
    git_seed(&ws);
    let cache = ws.join(".shuttle").join("symbols.index");
    let oid1 = head_oid(&ws);

    // Cold status is stale (no watermark yet) but reports the live HEAD.
    let cold = run_index_json(&ws, &cache, true);
    assert_eq!(
        cold["status"].as_str().unwrap(),
        "stale-index",
        "cold: {cold}"
    );
    assert_eq!(cold["head_oid"].as_str().unwrap(), oid1);

    // Refresh stamps the watermark; status is now Fresh for exactly that oid.
    let refreshed = run_index_json(&ws, &cache, false);
    assert_eq!(refreshed["index_base_oid"].as_str().unwrap(), oid1);
    let fresh = run_index_json(&ws, &cache, true);
    assert_eq!(fresh["status"].as_str().unwrap(), "fresh", "fresh: {fresh}");
    assert_eq!(fresh["index_base_oid"].as_str().unwrap(), oid1);
    assert!(fresh["files"].as_u64().unwrap() >= 3);

    // A new commit drifts HEAD; the untouched index is now stale (never reused).
    write(&ws, "d.py", "def delta():\n    return 4\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "drift"]);
    let oid2 = head_oid(&ws);
    assert_ne!(oid1, oid2, "the new commit must move HEAD");

    let stale = run_index_json(&ws, &cache, true);
    assert_eq!(
        stale["status"].as_str().unwrap(),
        "stale-index",
        "drift must read stale: {stale}"
    );
    assert_eq!(
        stale["index_base_oid"].as_str().unwrap(),
        oid1,
        "records old oid"
    );
    assert_eq!(
        stale["head_oid"].as_str().unwrap(),
        oid2,
        "reports live head"
    );

    // Re-refresh re-stamps to the new HEAD and returns to Fresh.
    let re = run_index_json(&ws, &cache, false);
    assert_eq!(re["index_base_oid"].as_str().unwrap(), oid2);
    let fresh2 = run_index_json(&ws, &cache, true);
    assert_eq!(fresh2["status"].as_str().unwrap(), "fresh");
    assert_eq!(fresh2["index_base_oid"].as_str().unwrap(), oid2);
}

/// Human (non-JSON) status output is greppable and honest: cold prints the
/// `stale-index` banner plus the §7 refresh hint (never a false "fresh").
#[test]
fn human_status_cold_is_stale_with_hint() {
    let ws = scratch_dir("human_status");
    seed_files(&ws);
    let cache = ws.join(".shuttle").join("symbols.index");
    let out = Command::new(shuttle_bin())
        .args([
            "index",
            "--status",
            "--root",
            ws.to_str().unwrap(),
            "--cache-path",
            cache.to_str().unwrap(),
        ])
        .output()
        .expect("spawn shuttle index --status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("stale-index"),
        "cold status must be stale-index: {stdout}"
    );
    assert!(
        stdout.contains("shuttle index"),
        "cold status must hint the refresh: {stdout}"
    );
}

#[test]
fn pure_stdout_and_out_are_canonical_and_leave_no_default_cache() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git unavailable; skipping pure index E2E");
        return;
    }
    let ws = scratch_dir("pure_stdout");
    write(&ws, "src/a.ts", "export function alpha() { return 1; }\n");
    git_seed(&ws);
    let head = head_oid(&ws);

    let stdout = run_pure(&ws, &["--head", &head]);
    assert!(
        stdout.status.success(),
        "pure stdout failed: {}",
        String::from_utf8_lossy(&stdout.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&stdout.stdout).unwrap();
    assert_eq!(value["schema"], "shuttle_symbol_index/v1");
    assert_eq!(value["index_base_oid"], head);
    assert!(value["files"].get("src/a.ts").is_some());
    assert!(
        value["files"]
            .as_object()
            .unwrap()
            .keys()
            .all(|path| !path.starts_with('/')),
        "artifact paths must be repository-relative: {value}"
    );
    assert!(
        !ws.join(".shuttle").exists(),
        "pure stdout must not create .shuttle"
    );

    let out_path = ws.parent().unwrap().join(format!(
        "{}.artifact.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    let written = run_pure(&ws, &["--head", &head, "--out", out_path.to_str().unwrap()]);
    assert!(
        written.status.success(),
        "pure --out failed: {}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(
        written.stdout.is_empty(),
        "--out must not duplicate JSON on stdout"
    );
    assert_eq!(std::fs::read(&out_path).unwrap(), stdout.stdout);
    assert!(
        !ws.join(".shuttle").exists(),
        "pure --out must not create .shuttle"
    );
}

#[test]
fn pure_out_failure_propagates_and_cleans_atomic_temporary_file() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git unavailable; skipping pure output failure E2E");
        return;
    }
    let ws = scratch_dir("pure_out_failure");
    write(&ws, "src/a.rs", "pub fn alpha() -> u8 { 1 }\n");
    git_seed(&ws);
    let head = head_oid(&ws);

    // An existing directory cannot be atomically replaced by the completed
    // artifact file. The error must reach the caller, and the temporary file
    // used for atomic publication must be removed.
    let out_path = ws.join("caller-owned-output");
    std::fs::create_dir(&out_path).unwrap();
    let out = run_pure(&ws, &["--head", &head, "--out", out_path.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "an unwritable caller-owned output target must fail"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "the output failure must be visible to the caller"
    );
    let tmp_prefix = ".caller-owned-output.tmp.";
    assert!(
        std::fs::read_dir(&ws).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(tmp_prefix)),
        "failed atomic publication must clean its temporary file"
    );
    assert!(
        !ws.join(".shuttle").exists(),
        "pure failure must not create .shuttle"
    );
}

#[test]
fn pure_refresh_does_not_mutate_legacy_cache_contract() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git unavailable; skipping legacy isolation E2E");
        return;
    }
    let ws = scratch_dir("pure_legacy_isolation");
    write(&ws, "src/a.py", "def alpha():\n    return 1\n");
    git_seed(&ws);
    let head = head_oid(&ws);

    // Exercise the original command surface exactly: default root and default
    // `.shuttle/symbols.index`, with the pre-existing JSON summary shape.
    let legacy = Command::new(shuttle_bin())
        .args(["index", "--json"])
        .current_dir(&ws)
        .output()
        .unwrap();
    assert!(
        legacy.status.success(),
        "legacy refresh failed: {}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
    assert_eq!(summary["index_base_oid"], head);
    assert_eq!(summary["cache_path"], ".shuttle/symbols.index");
    assert!(summary["total_files"].as_u64().unwrap() >= 1);
    assert!(summary.get("cache_hits").is_some());
    assert!(summary.get("rebuilt").is_some());
    assert!(summary.get("dropped").is_some());
    assert!(summary.get("symbols").is_some());

    let cache_path = ws.join(".shuttle/symbols.index");
    let cache_before = std::fs::read(&cache_path).unwrap();
    let pure = run_pure(&ws, &["--head", &head]);
    assert!(
        pure.status.success(),
        "pure refresh failed: {}",
        String::from_utf8_lossy(&pure.stderr)
    );
    assert_eq!(
        std::fs::read(&cache_path).unwrap(),
        cache_before,
        "pure refresh must neither read through nor rewrite legacy cache state"
    );
}

#[test]
fn pure_argument_matrix_rejects_incomplete_or_legacy_combinations() {
    let cases: &[&[&str]] = &[
        &["index", "--refresh"],
        &["index", "--head", "HEAD"],
        &[
            "index",
            "--refresh",
            "--head",
            "HEAD",
            "--merge",
            "old.json",
        ],
        &["index", "--refresh", "--head", "HEAD", "--base", "HEAD"],
        &["index", "--refresh", "--head", "HEAD", "--status"],
        &["index", "--refresh", "--head", "HEAD", "--json"],
        &["index", "--timeout-ms", "10"],
    ];
    for args in cases {
        let out = Command::new(shuttle_bin()).args(*args).output().unwrap();
        assert!(
            !out.status.success(),
            "invalid argument combination unexpectedly succeeded: {args:?}"
        );
    }
}

#[test]
fn pure_merge_is_strict_and_matches_cold_with_reverse_dependents() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git unavailable; skipping pure merge E2E");
        return;
    }
    let ws = scratch_dir("pure_merge");
    write(&ws, "src/a.ts", "export function alpha() { return 1; }\n");
    write(
        &ws,
        "src/b.ts",
        "import { alpha } from './a';\nexport function beta() { return alpha(); }\n",
    );
    write(&ws, "src/c.ts", "export function gamma() { return 3; }\n");
    git_seed(&ws);
    let base = head_oid(&ws);
    let base_path = ws.parent().unwrap().join(format!(
        "{}.base.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    let base_run = run_pure(
        &ws,
        &["--head", &base, "--out", base_path.to_str().unwrap()],
    );
    assert!(base_run.status.success());

    write(&ws, "src/a.ts", "export function alpha() { return 2; }\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "change alpha"]);
    let head = head_oid(&ws);

    let cold = run_pure(&ws, &["--head", &head]);
    assert!(cold.status.success());
    let merged = run_pure(
        &ws,
        &[
            "--head",
            &head,
            "--merge",
            base_path.to_str().unwrap(),
            "--base",
            &base,
        ],
    );
    assert!(
        merged.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert_eq!(
        merged.stdout, cold.stdout,
        "merge and cold artifacts differ"
    );
    let telemetry = String::from_utf8_lossy(&merged.stderr);
    assert!(
        telemetry.contains("rebuilt=2"),
        "changed unit + reverse dependent: {telemetry}"
    );
    assert!(
        telemetry.contains("reused=1"),
        "unrelated unit should be reused: {telemetry}"
    );
    assert!(
        telemetry.contains(r#"rebuilt_paths=["src/a.ts","src/b.ts"]"#),
        "rebuilt set must name the changed unit and its reverse dependent: {telemetry}"
    );

    let wrong_base = run_pure(
        &ws,
        &[
            "--head",
            &head,
            "--merge",
            base_path.to_str().unwrap(),
            "--base",
            &head,
        ],
    );
    assert!(
        !wrong_base.status.success(),
        "base mismatch must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&wrong_base.stderr).contains("merge base mismatch"),
        "unexpected mismatch error: {}",
        String::from_utf8_lossy(&wrong_base.stderr)
    );
}

#[test]
fn pure_artifact_is_identical_across_worktrees() {
    if !ctags_present() || !can_init_git() {
        eprintln!("ctags/git unavailable; skipping cross-worktree E2E");
        return;
    }
    let ws = scratch_dir("pure_origin");
    write(&ws, "src/a.py", "def alpha():\n    return 1\n");
    git_seed(&ws);
    let head = head_oid(&ws);
    let clone = scratch_dir("pure_clone_parent").join("clone");
    git(
        clone.parent().unwrap(),
        &["clone", "-q", ws.to_str().unwrap(), clone.to_str().unwrap()],
    );

    let first = run_pure(&ws, &["--head", &head]);
    let second = run_pure(&clone, &["--head", &head]);
    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn pure_refresh_timeout_fails_before_replacing_caller_artifact() {
    if !ctags_present() || !can_init_git() {
        return;
    }
    let ws = scratch_dir("pure_timeout");
    write(&ws, "src/a.py", "def alpha():\n    return 1\n");
    git_seed(&ws);
    let base = head_oid(&ws);
    let out_path = ws.parent().unwrap().join(format!(
        "{}.timeout.json",
        ws.file_name().unwrap().to_string_lossy()
    ));
    let baseline = run_pure(&ws, &["--head", &base, "--out", out_path.to_str().unwrap()]);
    assert!(baseline.status.success());
    let old_artifact = std::fs::read(&out_path).unwrap();
    write(&ws, "src/a.py", "def alpha():\n    return 2\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "new head"]);
    let head = head_oid(&ws);
    let fake_ctags = ws.parent().unwrap().join(format!(
        "{}.slow-ctags",
        ws.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &fake_ctags,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'Universal Ctags 6.2'; exit 0; fi\nexec sleep 5\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_ctags, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let started = std::time::Instant::now();
    let result = Command::new(shuttle_bin())
        .env("SHUTTLE_CTAGS_BIN", &fake_ctags)
        .args([
            "index",
            "--refresh",
            "--root",
            ws.to_str().unwrap(),
            "--head",
            &head,
            "--out",
            out_path.to_str().unwrap(),
            "--timeout-ms",
            "10",
        ])
        .output()
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "timeout did not terminate the scanner promptly: {:?}",
        started.elapsed()
    );
    assert!(!result.status.success(), "timeout must fail closed");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("ctags scan timed out"),
        "unexpected timeout error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        old_artifact,
        "a timed-out refresh must not replace the caller's prior artifact"
    );
    write(
        &ws,
        "plan.json",
        r#"{"version":"split-derive/v1","tasks":[{"id":"a","write_files":["src/a.py"]}]}"#,
    );
    let stale = Command::new(shuttle_bin())
        .args([
            "split",
            "--derive-edges",
            "--plan",
            ws.join("plan.json").to_str().unwrap(),
            "--root",
            ws.to_str().unwrap(),
            "--symbol-index",
            out_path.to_str().unwrap(),
            "--index-head",
            &head,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(stale.status.success());
    let stale: serde_json::Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(stale["index_status"], "stale");
    assert_eq!(stale["fallback_required"], true);

    let _ = std::fs::remove_file(&fake_ctags);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_dir_all(&ws);
}
