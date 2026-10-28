//! H1 drift-guard: the workspace has ONE centralised lint policy
//! (`[workspace.lints]` in the root manifest) and every member crate must opt
//! into it via `[lints] workspace = true`. Without this test a newly added
//! crate could silently skip the shared policy and reintroduce the per-crate
//! `#![deny(...)]` drift that H1 removed. It also pins the scaffolding
//! guardrails (`dbg!`/`todo!`/`unimplemented!` = deny) so they cannot be
//! quietly downgraded.
//!
//! This is a pure manifest-parsing test: no build, no network, no ctags.

use std::fs;
use std::path::{Path, PathBuf};

/// Repo root = two levels up from this crate's manifest dir (crates/shuttle-cli).
fn workspace_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent() // crates/
        .and_then(Path::parent) // repo root
        .expect("crate lives under <root>/crates/shuttle-cli")
        .to_path_buf()
}

/// Extremely small TOML slice helper: returns the text of the section that
/// starts at `[header]` up to (but not including) the next top-level `[...]`
/// header. Good enough for the flat manifests in this workspace; we do NOT
/// pull in a TOML dependency just for a guard test.
fn section<'a>(toml: &'a str, header: &str) -> Option<&'a str> {
    let start = toml.find(header)?;
    let after = &toml[start + header.len()..];
    let end = after.find("\n[").map(|i| i + 1).unwrap_or(after.len());
    Some(&after[..end])
}

fn member_dirs(root: &Path) -> Vec<PathBuf> {
    let root_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let members = section(&root_toml, "members = [").expect("root manifest has members = [");
    let mut dirs = Vec::new();
    for line in members.lines() {
        let line = line.trim().trim_end_matches(',');
        if let Some(rel) = line.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            if rel.starts_with("crates/") {
                dirs.push(root.join(rel));
            }
        }
    }
    dirs
}

#[test]
fn every_member_opts_into_shared_lints() {
    let root = workspace_root();
    let members = member_dirs(&root);
    assert!(
        members.len() >= 12,
        "expected all workspace crates to be discovered, found {}",
        members.len()
    );
    for dir in members {
        let manifest = dir.join("Cargo.toml");
        let toml = fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        let lints = section(&toml, "[lints]").unwrap_or_else(|| {
            panic!(
                "{} is missing a [lints] section — every member must opt into \
                 the workspace lint policy (add `[lints]\\nworkspace = true`)",
                manifest.display()
            )
        });
        assert!(
            lints.contains("workspace = true"),
            "{} has a [lints] section but does not set `workspace = true`",
            manifest.display()
        );
    }
}

#[test]
fn scaffolding_guardrails_are_denied() {
    let root = workspace_root();
    let root_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let clippy = section(&root_toml, "[workspace.lints.clippy]")
        .expect("root manifest declares [workspace.lints.clippy]");
    for lint in ["dbg_macro", "todo", "unimplemented"] {
        let needle = format!("{lint} = \"deny\"");
        assert!(
            clippy.contains(&needle),
            "workspace lint policy must keep `{needle}` — scaffolding guardrails \
             cannot be downgraded"
        );
    }
}
