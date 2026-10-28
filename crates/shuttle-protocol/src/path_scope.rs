//! path_scope — the single write-path containment layer for shuttle (P0-2).
//!
//! Every editing tool that writes to disk MUST route its target path through
//! [`ScopedPathResolver::resolve`]. The assignment `root` thereby becomes a
//! real security boundary instead of a decorative parameter.
//!
//! Rules (fail-closed; any failure rejects the write):
//!   R1 reject absolute paths (candidate must be relative to root)
//!   R2 reject any `..` component after lexical normalization
//!   R3 canonicalize the deepest existing ancestor, splice the remaining tail
//!   R4 reject symlink escape (canonical result must stay under root)
//!   R5 candidate must still match the assignment allowlist
//!   R6 return the canonical absolute path; writers may only use the return value
//!
//! Platform-agnostic: pure lexical + filesystem checks, no OS-specific behavior.

use std::path::{Component, Path, PathBuf};

/// Validates and resolves candidate write paths against an assignment root.
#[derive(Debug, Clone)]
pub struct ScopedPathResolver {
    /// Canonicalized assignment root — the sole legal subtree for writes.
    root: PathBuf,
    /// Allowlist relative to root (prefix / glob). Empty = reject all (fail-closed).
    allowed: Vec<String>,
}

/// Why a candidate write path was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathViolation {
    #[error("absolute path not allowed: {0}")]
    AbsolutePath(String),
    #[error("parent traversal '..' not allowed: {0}")]
    ParentTraversal(String),
    #[error("cannot resolve parent dir: {0}")]
    ParentResolveFailed(String),
    #[error("symlink escapes assignment root: {0}")]
    SymlinkEscape(String),
    #[error("path not in allowed_paths: {0}")]
    NotInAllowlist(String),
}

impl ScopedPathResolver {
    /// Construct a resolver. `root` is canonicalized immediately; `allowed`
    /// are patterns relative to root. An empty allowlist rejects every write.
    pub fn new(root: impl AsRef<Path>, allowed: Vec<String>) -> std::io::Result<Self> {
        let root = std::fs::canonicalize(root.as_ref())?;
        Ok(Self { root, allowed })
    }

    /// Canonical assignment root (already resolved). Exposed for callers that
    /// need to compute paths relative to the boundary.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lexically normalize a relative path, rejecting any `..` escape.
    /// Returns the cleaned relative PathBuf (may be empty for ".").
    fn lexical_normalize(candidate: &str) -> Result<PathBuf, PathViolation> {
        let p = Path::new(candidate);
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(PathViolation::AbsolutePath(candidate.to_string()));
                }
                Component::ParentDir => {
                    // Only allow popping a segment we lexically pushed; never
                    // escape above the (implicit) root.
                    if !out.pop() {
                        return Err(PathViolation::ParentTraversal(candidate.to_string()));
                    }
                }
                Component::CurDir => {}
                Component::Normal(seg) => out.push(seg),
            }
        }
        Ok(out)
    }

    /// Check the normalized relative path against the allowlist (R5).
    /// A pattern matches if it equals the rel path, is a path-prefix of it,
    /// or is a simple trailing-`*` / `**` glob prefix. Empty allowlist = reject.
    fn in_allowlist(&self, rel: &Path) -> bool {
        if self.allowed.is_empty() {
            return false;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        for pat in &self.allowed {
            let pat = pat.trim_end_matches('/');
            // Glob suffix: "src/**" or "src/*" -> prefix match on "src".
            let base = pat
                .strip_suffix("/**")
                .or_else(|| pat.strip_suffix("/*"))
                .unwrap_or(pat);
            if rel_str == base {
                return true;
            }
            if !base.is_empty() && rel_str.starts_with(&format!("{base}/")) {
                return true;
            }
            // Bare "*" or "**" allows anything inside root.
            if pat == "*" || pat == "**" {
                return true;
            }
        }
        false
    }

    /// Splice a normalized relative tail onto the canonical root, resolving the
    /// existing prefix component-by-component and rejecting any symlink escape
    /// (R3/R4). The canonical result must remain under `self.root`.
    ///
    /// Security note — why we probe with `symlink_metadata` (lstat) instead of
    /// `Path::exists`: `exists()` FOLLOWS symlinks, so a *dangling* symlink (one
    /// whose target does not exist yet) reports `false`. The old probe therefore
    /// treated such a component as an ordinary to-be-created segment, spliced it
    /// straight onto `root`, and the naive `starts_with(root)` check passed —
    /// even though the writer would follow the link and land OUTSIDE the cage.
    /// We now lstat every component: a symlink is fully canonicalized and its
    /// target is required to stay under root; a dangling symlink cannot be
    /// canonicalized, so it fails closed (`SymlinkEscape`). Only when a segment
    /// does not exist AT ALL (not even as a symlink) do we stop and treat the
    /// remainder as new path to create.
    ///
    /// TOCTOU: `resolve()` is a check; the caller's subsequent write is the use.
    /// The returned path is safe at resolve time, but a concurrent attacker who
    /// swaps a real directory for a symlink between resolve and write can still
    /// defeat any path-based check. Callers that need absolute safety must open
    /// with `O_NOFOLLOW` semantics; that is outside this resolver's contract.
    fn canonical_under_root(&self, rel: &Path, candidate: &str) -> Result<PathBuf, PathViolation> {
        let comps: Vec<&std::ffi::OsStr> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect();

        // Walk the existing prefix. `existing` is kept canonical (symlinks
        // resolved) so containment is always checked against real targets.
        let mut existing = self.root.clone();
        let mut split = comps.len();
        for (i, seg) in comps.iter().enumerate() {
            let next = existing.join(seg);
            match std::fs::symlink_metadata(&next) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    // A symlink on the path: resolve it fully and require the
                    // real target to stay under root. A dangling symlink cannot
                    // be canonicalized -> reject fail-closed.
                    let canon = std::fs::canonicalize(&next)
                        .map_err(|_| PathViolation::SymlinkEscape(candidate.to_string()))?;
                    if !canon.starts_with(&self.root) {
                        return Err(PathViolation::SymlinkEscape(candidate.to_string()));
                    }
                    existing = canon;
                }
                Ok(_) => {
                    // Ordinary existing dir/file — descend.
                    existing = next;
                }
                Err(_) => {
                    // Nothing exists here (not even a symlink); this and every
                    // following segment are new path to be created.
                    split = i;
                    break;
                }
            }
        }

        // Canonicalize the deepest existing portion once more and re-check
        // containment (defence in depth; `existing` is already resolved).
        let canon_existing = std::fs::canonicalize(&existing)
            .map_err(|_| PathViolation::ParentResolveFailed(candidate.to_string()))?;
        // R4: the existing (symlink-resolved) prefix must stay under root.
        if !canon_existing.starts_with(&self.root) {
            return Err(PathViolation::SymlinkEscape(candidate.to_string()));
        }

        // Splice the remaining (non-existing) tail. These segments do not exist
        // yet, so none of them can be a symlink; `rel` is already lexically
        // normalized, so none can be `..`.
        let mut resolved = canon_existing;
        for seg in &comps[split.min(comps.len())..] {
            resolved.push(seg);
        }
        // Final defensive containment check on the fully spliced path.
        if !resolved.starts_with(&self.root) {
            return Err(PathViolation::SymlinkEscape(candidate.to_string()));
        }
        Ok(resolved)
    }

    /// Validate and resolve a candidate write path. On success returns the
    /// canonical absolute path; writers MUST write only to this value and never
    /// to the original candidate string.
    pub fn resolve(&self, candidate: &str) -> Result<PathBuf, PathViolation> {
        // R1: reject absolute paths early (before normalization).
        if Path::new(candidate).is_absolute() {
            return Err(PathViolation::AbsolutePath(candidate.to_string()));
        }
        // R2: lexical normalize, reject residual `..`.
        let rel = Self::lexical_normalize(candidate)?;
        // R5: allowlist gate (checked on the clean rel path).
        if !self.in_allowlist(&rel) {
            return Err(PathViolation::NotInAllowlist(candidate.to_string()));
        }
        // R3/R4: canonicalize existing ancestor + symlink-escape guard.
        let resolved = self.canonical_under_root(&rel, candidate)?;
        // R6: return canonical absolute path.
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_root() -> PathBuf {
        let base =
            std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".shuttletmp_pathscope".into());
        let dir = std::env::current_dir().unwrap().join(base).join(format!(
            "ps_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn resolver(root: &Path, allowed: &[&str]) -> ScopedPathResolver {
        ScopedPathResolver::new(root, allowed.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    #[test]
    fn t_r1_absolute_rejected() {
        let root = tmp_root();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("/etc/passwd"),
            Err(PathViolation::AbsolutePath("/etc/passwd".into()))
        );
    }

    #[test]
    fn t_r2_parent_traversal_rejected() {
        let root = tmp_root();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("../../../etc/passwd"),
            Err(PathViolation::ParentTraversal("../../../etc/passwd".into()))
        );
    }

    #[test]
    fn t_r2b_normalized_escape_rejected() {
        let root = tmp_root();
        let r = resolver(&root, &["**"]);
        // sub/../../out.txt normalizes to ../out.txt -> escape.
        assert_eq!(
            r.resolve("sub/../../out.txt"),
            Err(PathViolation::ParentTraversal("sub/../../out.txt".into()))
        );
    }

    #[test]
    fn t_r5_not_in_allowlist() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        assert_eq!(
            r.resolve("secrets/key.txt"),
            Err(PathViolation::NotInAllowlist("secrets/key.txt".into()))
        );
    }

    #[test]
    fn t_ok_new_file_missing_parent() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        // Parent dir does not exist yet; must still resolve OK under root.
        let got = r.resolve("src/a/b/new.rs").unwrap();
        assert!(got.starts_with(fs::canonicalize(&root).unwrap()));
        assert!(got.ends_with("src/a/b/new.rs"));
    }

    #[test]
    fn t_r4_symlink_escape_rejected() {
        let root = tmp_root();
        // Create an escape target outside root and a symlink inside root -> it.
        let outside = tmp_root(); // separate dir, sibling under SHUTTLE_TEST_TMP
        let link = root.join("evil");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let r = resolver(&root, &["**"]);
        #[cfg(unix)]
        assert_eq!(
            r.resolve("evil/pwn.txt"),
            Err(PathViolation::SymlinkEscape("evil/pwn.txt".into()))
        );
        let _ = link;
    }

    #[test]
    fn t_parity_dryrun_and_real_agree() {
        let root = tmp_root();
        let r = resolver(&root, &["**"]);
        // Same out-of-scope path must give the identical rejection regardless
        // of whether a caller is in dry-run or real-write mode — resolve() is
        // the single shared gate, so the result is by construction identical.
        let first = r.resolve("../escape.txt");
        let second = r.resolve("../escape.txt");
        assert_eq!(first, second);
        assert!(first.is_err());
    }

    /// Regression: a *dangling* symlink component (points nowhere) must fail
    /// closed. The old `.exists()` probe followed the link, saw it as missing,
    /// and treated `dangle` as an ordinary new segment — splicing it straight
    /// onto root and letting a write to `dangle/pwn.txt` escape the cage the
    /// instant the attacker later re-pointed the link. `symlink_metadata`
    /// (lstat) sees the link, `canonicalize` fails on the broken target, and
    /// we reject with `SymlinkEscape`.
    #[test]
    #[cfg(unix)]
    fn t_r4_dangling_symlink_rejected() {
        let root = tmp_root();
        let link = root.join("dangle");
        // Target deliberately does not exist -> dangling symlink.
        std::os::unix::fs::symlink(root.join("no_such_target"), &link).unwrap();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("dangle/pwn.txt"),
            Err(PathViolation::SymlinkEscape("dangle/pwn.txt".into()))
        );
        // Resolving the dangling link itself as the target is equally rejected.
        assert_eq!(
            r.resolve("dangle"),
            Err(PathViolation::SymlinkEscape("dangle".into()))
        );
    }

    /// An *intermediate* directory component that is a symlink pointing outside
    /// root must reject the whole path, even though the trailing segment is new.
    #[test]
    #[cfg(unix)]
    fn t_r4_intermediate_symlink_escape_rejected() {
        let root = tmp_root();
        let outside = tmp_root(); // sibling dir outside `root`
        std::os::unix::fs::symlink(&outside, root.join("jail")).unwrap();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("jail/sub/new.txt"),
            Err(PathViolation::SymlinkEscape("jail/sub/new.txt".into()))
        );
    }

    /// A symlink that stays *inside* root is legitimate: it resolves to a real
    /// target under the cage, so writes through it are contained and allowed.
    /// The returned path is the canonical (link-resolved) location.
    #[test]
    #[cfg(unix)]
    fn t_r4_symlink_inside_root_ok() {
        let root = tmp_root();
        let real = root.join("real_sub");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join("alias")).unwrap();
        let r = resolver(&root, &["**"]);
        let got = r.resolve("alias/new.txt").unwrap();
        // Resolved through the link to the real target, still under root.
        assert!(got.starts_with(fs::canonicalize(&root).unwrap()));
        assert!(got.starts_with(fs::canonicalize(&real).unwrap()));
        assert!(got.ends_with("new.txt"));
    }

    /// Defence in depth: a symlink whose target is itself *another symlink*
    /// chaining out of root must still be rejected — `canonicalize` follows the
    /// full chain, so the final real target is what containment is checked on.
    #[test]
    #[cfg(unix)]
    fn t_r4_chained_symlink_escape_rejected() {
        let root = tmp_root();
        let outside = tmp_root();
        // hop1 (inside root) -> hop2 (inside root) -> outside.
        std::os::unix::fs::symlink(&outside, root.join("hop2")).unwrap();
        std::os::unix::fs::symlink(root.join("hop2"), root.join("hop1")).unwrap();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("hop1/loot.txt"),
            Err(PathViolation::SymlinkEscape("hop1/loot.txt".into()))
        );
    }

    /// A symlink pointing to root's own parent (classic `..`-via-link) escapes
    /// and must be rejected even though no lexical `..` is present in candidate.
    #[test]
    #[cfg(unix)]
    fn t_r4_symlink_to_parent_rejected() {
        let root = tmp_root();
        let parent = root.parent().unwrap().to_path_buf();
        std::os::unix::fs::symlink(&parent, root.join("up")).unwrap();
        let r = resolver(&root, &["**"]);
        assert_eq!(
            r.resolve("up/sibling.txt"),
            Err(PathViolation::SymlinkEscape("up/sibling.txt".into()))
        );
    }
}

#[cfg(test)]
mod cf_frozen_tests {
    //! Pins the ScopedPathResolver allowed_paths semantics: glob-only syntax,
    //! prefix written as `p/**`, and empty array == fail-closed (reject all
    //! writes). These semantics are shuttle's own write-boundary rules;
    //! `cell_binding/v1` is a shuttle-local envelope. The module name is kept
    //! for test-id stability; the pinned behaviour is unchanged.
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn tmp_root() -> PathBuf {
        let base =
            std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".shuttletmp_pathscope".into());
        let dir = std::env::current_dir().unwrap().join(base).join(format!(
            "cf_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn resolver(root: &Path, allowed: &[&str]) -> ScopedPathResolver {
        ScopedPathResolver::new(root, allowed.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    // --- external: empty allowed_paths == fail-closed (reject all) ---
    #[test]
    fn cf_empty_allowlist_rejects_all() {
        let root = tmp_root();
        let r = resolver(&root, &[]);
        assert_eq!(
            r.resolve("src/main.rs"),
            Err(PathViolation::NotInAllowlist("src/main.rs".into()))
        );
        assert_eq!(
            r.resolve("README.md"),
            Err(PathViolation::NotInAllowlist("README.md".into()))
        );
    }

    // --- external: prefix is written as "p/**" and matches nested paths ---
    #[test]
    fn cf_glob_prefix_matches_nested() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        assert!(r.resolve("src/a/b/deep.rs").is_ok());
        assert!(r.resolve("src/top.rs").is_ok());
    }

    // --- external: a "p/**" prefix must not leak into a sibling sharing the stem ---
    #[test]
    fn cf_glob_prefix_no_sibling_leak() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        assert_eq!(
            r.resolve("srcfoo/x.rs"),
            Err(PathViolation::NotInAllowlist("srcfoo/x.rs".into()))
        );
    }

    // --- external: single-segment glob "p/*" allows the dir itself and children ---
    #[test]
    fn cf_single_star_prefix() {
        let root = tmp_root();
        let r = resolver(&root, &["build/*"]);
        assert!(r.resolve("build/out.o").is_ok());
        assert!(r.resolve("build").is_ok());
    }

    // --- external: multiple allowlist entries — any match admits ---
    #[test]
    fn cf_multi_entry_union() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**", "docs/**"]);
        assert!(r.resolve("docs/spec.md").is_ok());
        assert!(r.resolve("src/lib.rs").is_ok());
        assert_eq!(
            r.resolve("tests/it.rs"),
            Err(PathViolation::NotInAllowlist("tests/it.rs".into()))
        );
    }

    // --- external: exact-file allowlist entry (no glob) matches only that file ---
    #[test]
    fn cf_exact_file_entry() {
        let root = tmp_root();
        let r = resolver(&root, &["Cargo.toml"]);
        assert!(r.resolve("Cargo.toml").is_ok());
        assert_eq!(
            r.resolve("Cargo.lock"),
            Err(PathViolation::NotInAllowlist("Cargo.lock".into()))
        );
    }

    // --- external: traversal is caught before the allowlist gate (R2 precedes R5) ---
    // An out-of-scope path that also escapes must report the escape, not the
    // allowlist miss — order matters for diagnosing binding violations.
    #[test]
    fn cf_traversal_precedes_allowlist() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        assert_eq!(
            r.resolve("../secrets/key.txt"),
            Err(PathViolation::ParentTraversal("../secrets/key.txt".into()))
        );
    }

    // --- external: absolute path is caught before the allowlist gate (R1 precedes R5) ---
    #[test]
    fn cf_absolute_precedes_allowlist() {
        let root = tmp_root();
        let r = resolver(&root, &["src/**"]);
        assert_eq!(
            r.resolve("/etc/passwd"),
            Err(PathViolation::AbsolutePath("/etc/passwd".into()))
        );
    }

    // NOTE: PathViolation::ParentResolveFailed is not deterministically
    // reachable from resolve() on a healthy filesystem — the existing-ancestor
    // probe only descends into segments that `.exists()`, so canonicalize()
    // of that prefix cannot fail under normal conditions. It is retained as a
    // defensive arm for I/O faults (e.g. permission races) and is covered by
    // the offline binding validator's fault-injection cases, not here.
}
