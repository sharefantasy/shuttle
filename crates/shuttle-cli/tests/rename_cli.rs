//! Binary-level CLI integration tests for `shuttle rename` — the
//! reconciliation contract (rename-weak-type-safety SC-1/SC-2/SC-3):
//!
//!   * dry-run prints a `planned N / known M sites` reconciliation header and
//!     names every excluded reference site (never a silent partial plan);
//!   * `--write` on a plan with reference gaps is REFUSED — non-zero exit,
//!     zero disk change — and the error names the gaps plus the exact rerun;
//!   * `--write --include-refs` folds reference sites in and lands the full
//!     rename (all-or-nothing via the shared editcore gate);
//!   * a gap-free target (def only, no references) writes directly;
//!   * `--help` states that `--force` does NOT imply `--write`;
//!   * a strict-subdirectory `--root` prints the scope warning (SC-3).
//!
//! Tier-agnostic: assertions do not depend on whether ast-grep is present
//! (`--force` covers the text-tier case), matching the §7 never-guess stance.

use std::path::PathBuf;
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

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_rename_cli_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-file repo whose symbol has a def site plus one reference site —
/// the minimal gap shape every SC-1 assertion revolves around.
fn gap_repo(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::write(
        dir.join("mod.rs"),
        concat!(
            "pub fn target_fn(x: u32) -> u32 {\n",
            "    x + 1\n",
            "}\n",
            "\n",
            "pub fn caller(x: u32) -> u32 {\n",
            "    target_fn(x)\n",
            "}\n",
        ),
    )
    .unwrap();
    dir
}

fn run_rename(dir: &PathBuf, extra: &[&str]) -> (bool, String, String) {
    let out = Command::new(shuttle_bin())
        .args([
            "rename",
            "--from",
            "target_fn",
            "--to",
            "renamed_fn",
            "--root",
        ])
        .arg(dir)
        .args(extra)
        .output()
        .expect("spawn shuttle");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn dry_run_prints_reconciliation_and_excluded_site() {
    if !ctags_present() {
        eprintln!("ctags unavailable; skipping");
        return;
    }
    let dir = gap_repo("dryrun");
    let (ok, stdout, _) = run_rename(&dir, &[]);
    assert!(ok, "dry-run must exit 0");
    assert!(
        stdout.contains("planned 1 / known 2 sites"),
        "reconciliation header missing:\n{stdout}"
    );
    assert!(
        stdout.contains("--include-refs"),
        "blocked note must name the escape flag:\n{stdout}"
    );
    assert!(
        stdout.contains("(excluded)"),
        "excluded site must be listed, not hidden:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_with_gap_refused_and_disk_untouched() {
    if !ctags_present() {
        eprintln!("ctags unavailable; skipping");
        return;
    }
    let dir = gap_repo("refuse");
    let before = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    let (ok, _, stderr) = run_rename(&dir, &["--write"]);
    assert!(!ok, "partial rename must be refused, stderr:\n{stderr}");
    assert!(
        stderr.contains("refusing to write a partial rename"),
        "refusal reason missing:\n{stderr}"
    );
    assert!(
        stderr.contains("--include-refs"),
        "refusal must print the covering rerun:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("mod.rs")).unwrap(),
        before,
        "refused write must leave the file byte-identical"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_include_refs_lands_full_rename() {
    if !ctags_present() {
        eprintln!("ctags unavailable; skipping");
        return;
    }
    let dir = gap_repo("full");
    // `--force` keeps the assertion tier-agnostic: with ast-grep the reference
    // is High and --force is a no-op; without it the site is Low and --force
    // is required. Either way exactly two sites must land.
    let (ok, _, stderr) = run_rename(&dir, &["--write", "--include-refs", "--force"]);
    assert!(ok, "full rename must succeed, stderr:\n{stderr}");
    let after = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(
        !after.contains("target_fn"),
        "old name must be gone:\n{after}"
    );
    assert_eq!(
        after.matches("renamed_fn").count(),
        2,
        "def + call site both renamed:\n{after}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gap_free_target_writes_directly() {
    if !ctags_present() {
        eprintln!("ctags unavailable; skipping");
        return;
    }
    let dir = scratch_dir("gapfree");
    std::fs::write(dir.join("solo.rs"), "pub fn target_fn() {}\n").unwrap();
    let (ok, _, stderr) = run_rename(&dir, &["--write"]);
    assert!(ok, "no refs -> no gap -> direct write, stderr:\n{stderr}");
    let after = std::fs::read_to_string(dir.join("solo.rs")).unwrap();
    assert!(
        after.contains("renamed_fn") && !after.contains("target_fn"),
        "direct def-only write must land:\n{after}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_states_force_does_not_imply_write() {
    let out = Command::new(shuttle_bin())
        .args(["rename", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        help.contains("--include-refs"),
        "new flag must be documented:\n{help}"
    );
    assert!(
        help.contains("does NOT imply"),
        "flag independence must be explicit (D2):\n{help}"
    );
}

#[test]
fn narrow_root_prints_scope_warning() {
    if !ctags_present() {
        eprintln!("ctags unavailable; skipping");
        return;
    }
    let dir = scratch_dir("narrow");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/b.rs"), "pub fn scoped_fn() {}\n").unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !git(&["init", "-q", "--template="]) || !git(&["add", "sub/b.rs"]) {
        eprintln!("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let sub = dir.join("sub");
    let out = Command::new(shuttle_bin())
        .args([
            "rename",
            "--from",
            "scoped_fn",
            "--to",
            "other_fn",
            "--root",
        ])
        .arg(&sub)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("scope:"),
        "subdir root must warn about coverage:\n{stdout}"
    );
    assert!(
        stdout.contains("NOT covered"),
        "warning must say sites outside root are not covered:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
