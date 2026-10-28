//! Binary-level CLI integration test for SIGPIPE handling (all subcommands'
//! shared `println!` path):
//!
//!   * before the fix, Rust ignores SIGPIPE, so a downstream reader closing
//!     early (`shuttle … | head`) turned the next `println!` into a panic
//!     (exit 101);
//!   * after the fix, `main()` restores SIGPIPE's default disposition and the
//!     process dies by signal 13 — the conventional CLI behavior.
//!
//! Determinism: `symbols --root <workspace> --json` emits ~12 MB, far beyond
//! the 64 KiB pipe buffer, so the child is guaranteed to still be writing when
//! the read end drops.

use std::path::Path;
use std::process::{Command, Stdio};

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

#[test]
#[cfg(unix)]
fn sigpipe_downstream_close_kills_by_signal_not_panic() {
    if !ctags_present() {
        eprintln!("skip: ctags not present");
        return;
    }
    // crates/shuttle-cli/tests → workspace root (output volume guarantee).
    let ws_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let mut child = Command::new(shuttle_bin())
        .arg("symbols")
        .arg("--root")
        .arg(ws_root)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn shuttle");
    // Drop the read end immediately: the child's first write past the pipe
    // buffer must terminate it via SIGPIPE (signal 13), not panic (exit 101).
    drop(child.stdout.take());
    let status = child.wait().expect("wait shuttle");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(13),
        "expected death by SIGPIPE, got {status:?}"
    );
}
