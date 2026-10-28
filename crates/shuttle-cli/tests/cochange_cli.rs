//! Binary-level CLI integration tests for `shuttle impact --cochange` — the G7·U1
//! **time-truth** co-change HINT surface
//! . Spawns the REAL `shuttle`
//! binary and drives the file-level historical-coupling hint end-to-end over
//! hermetic git repos.
//!
//! The iron contract under test (§7 «绝不瞎猜» / the U1 red line): co-change is a
//! naturally weak signal — always file-level, always undirected, and NEVER folded
//! into the mechanical re-verify / blast-radius set. It surfaces ONLY as a review
//! hint (its own JSON envelope, structurally separate from the closure). Two files
//! repeatedly committed together surface as partners carrying weight + commit
//! evidence; a file never co-edited surfaces no partners (an honest "no hint", not
//! a failure); a shallow / squashed history reports `insufficient_history` rather
//! than extrapolate.
//!
//! The bad-flag guards (`--cochange` composes ONLY with `--changed`, never with
//! `--contract` / `--typed` / `--base` / `--head`) are the one hard `anyhow::bail!`
//! and run without git.
//!
//! Hermetic: scratch dirs live under the OS temp root (honours TMPDIR). The
//! git-backed derivation cases skip when a scratch `.git/config` cannot be created
//! (some sandboxes permit the `git` binary but block writes to `.git/config`); the
//! bad-flag guard cases always run.

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

/// A bare `git --version` is not enough: some sandboxes permit the binary but
/// block writes to `.git/config`. The co-change walk reads `git log --numstat`,
/// so the derivation cases need a real repo.
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
    let dir = std::env::temp_dir().join(format!("shuttle_cochange_it_{tag}_{nanos}"));
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

fn git_init(dir: &std::path::Path) {
    git(dir, &["init", "-q", "--template="]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// Amend both files then commit — a single commit touching the pair, so it
/// accrues co-change weight.
fn commit_pair(dir: &std::path::Path, a_body: &str, b_body: &str, msg: &str) {
    write(dir, "a.py", a_body);
    write(dir, "b.py", b_body);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// `shuttle impact --cochange --json --changed <files...> --root <ws>` → parsed JSON.
/// Asserts exit 0 (the machine contract: the hint envelope is what a caller
/// skims; a shallow history is not an error).
fn run_cochange_json(ws: &std::path::Path, changed: &[&str]) -> serde_json::Value {
    let mut args: Vec<String> = vec![
        "impact".into(),
        "--cochange".into(),
        "--json".into(),
        "--root".into(),
        ws.to_str().unwrap().into(),
        "--changed".into(),
    ];
    for c in changed {
        args.push((*c).into());
    }
    let out = Command::new(shuttle_bin())
        .args(&args)
        .output()
        .expect("spawn shuttle impact --cochange");
    assert!(
        out.status.success(),
        "shuttle impact --cochange exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not CoChangeHints JSON: {e}\n---\n{stdout}"))
}

/// Locate the hint block for a given changed file in the envelope.
fn file_hint<'a>(hints: &'a serde_json::Value, changed: &str) -> &'a serde_json::Value {
    hints["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|f| f["changed"].as_str() == Some(changed))
        .unwrap_or_else(|| panic!("no hint block for {changed}: {hints}"))
}

/// Two files committed together across several commits surface as co-change
/// partners carrying weight + commit-count evidence — while a third file that
/// was only ever committed alone surfaces no partners (honest "no hint").
#[test]
fn coupled_pair_surfaces_partner_solo_file_does_not() {
    if !can_init_git() {
        eprintln!("git repo init blocked in sandbox; skipping co-change derivation E2E");
        return;
    }
    let ws = scratch_dir("coupled");
    git_init(&ws);
    // Three commits each touching a.py AND b.py → strong file-level coupling.
    commit_pair(
        &ws,
        "def a():\n    return 1\n",
        "def b():\n    return 1\n",
        "c1",
    );
    commit_pair(
        &ws,
        "def a():\n    return 2\n",
        "def b():\n    return 2\n",
        "c2",
    );
    commit_pair(
        &ws,
        "def a():\n    return 3\n",
        "def b():\n    return 3\n",
        "c3",
    );
    // A later commit that touches ONLY solo.py → no coupling for it.
    write(&ws, "solo.py", "def s():\n    return 0\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "solo"]);

    // a.py's partners include b.py with real evidence.
    let hints = run_cochange_json(&ws, &["a.py", "solo.py"]);
    assert!(
        !hints["insufficient_history"].as_bool().unwrap(),
        "an established multi-commit history is sufficient: {hints}"
    );
    let a = file_hint(&hints, "a.py");
    let partners = a["partners"].as_array().unwrap();
    let b = partners
        .iter()
        .find(|p| p["file"].as_str() == Some("b.py"))
        .unwrap_or_else(|| panic!("a.py should surface b.py as a partner: {a}"));
    assert!(
        b["commits"].as_u64().unwrap() >= 2,
        "b.py must carry the co-change commit count: {b}"
    );
    assert!(
        b["weight"].as_f64().unwrap() > 0.0,
        "b.py must carry a positive decayed weight: {b}"
    );

    // solo.py was never co-edited → an empty partner list, present but honest.
    let solo = file_hint(&hints, "solo.py");
    assert!(
        solo["partners"].as_array().unwrap().is_empty(),
        "solo.py has no historical coupling: {solo}"
    );
}

/// A brand-new file with no commit history surfaces no partners — an honest
/// "no hint", never a fabricated coupling.
#[test]
fn brand_new_file_has_no_partners() {
    if !can_init_git() {
        eprintln!("git repo init blocked in sandbox; skipping co-change derivation E2E");
        return;
    }
    let ws = scratch_dir("newfile");
    git_init(&ws);
    commit_pair(
        &ws,
        "def a():\n    return 1\n",
        "def b():\n    return 1\n",
        "c1",
    );
    commit_pair(
        &ws,
        "def a():\n    return 2\n",
        "def b():\n    return 2\n",
        "c2",
    );

    // `fresh.py` exists on disk but was never committed.
    write(&ws, "fresh.py", "def fresh():\n    return 42\n");
    let hints = run_cochange_json(&ws, &["fresh.py"]);
    let fresh = file_hint(&hints, "fresh.py");
    assert!(
        fresh["partners"].as_array().unwrap().is_empty(),
        "an uncommitted file cannot have co-change partners: {fresh}"
    );
}

/// The co-change hint is a distinct truth axis: it may NOT be combined with the
/// mechanical closure flags. Each bad combo is a hard `anyhow::bail!` (exit != 0,
/// message on stderr) so a caller can never confuse a hint for a proven impact.
/// These guards run without git.
#[test]
fn cochange_rejects_incompatible_flag_combos() {
    let cases: &[&[&str]] = &[
        &["impact", "--cochange", "--contract", "--changed", "a.py"],
        &["impact", "--cochange", "--typed", "--changed", "a.py"],
        &[
            "impact",
            "--cochange",
            "--base",
            "HEAD~1",
            "--head",
            "HEAD",
            "--changed",
            "a.py",
        ],
    ];
    for args in cases {
        let out = Command::new(shuttle_bin())
            .args(*args)
            .output()
            .expect("spawn shuttle impact");
        assert!(
            !out.status.success(),
            "bad combo {args:?} must fail, but exited 0"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cochange"),
            "the guard must name --cochange: {stderr}"
        );
    }
}

/// `--cochange` without `--changed` is a usage error (there is nothing to hint
/// about) — a hard fail, not an empty envelope. Runs without git.
#[test]
fn cochange_without_changed_is_a_usage_error() {
    let out = Command::new(shuttle_bin())
        .args(["impact", "--cochange"])
        .output()
        .expect("spawn shuttle impact --cochange");
    assert!(!out.status.success(), "missing --changed must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--changed"),
        "the error must point at --changed: {stderr}"
    );
}
