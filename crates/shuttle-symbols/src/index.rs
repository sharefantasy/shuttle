//! G7 · U2 — the **content-addressed persistent symbol index** (design
//! ).
//!
//! # What this is
//! The symbol table is cheap on a small repo but a >10k-file monorepo pays a
//! real cold-scan cost on *every* one-shot invocation. This module keeps a
//! shareable **on-disk index** of the per-file symbol tags, keyed by the
//! **content hash of each file**, so that between two commits only the files
//! whose bytes actually changed are re-scanned (the *delta*); every unchanged
//! file is a cache hit. The result is byte-identical to a cold scan — the index
//! is a *speed* optimization layered over a *correctness* invariant, never a
//! substitute for it.
//!
//! # daemonless (the hard line)
//! This is an **on-disk cache file**, read fresh by each one-shot `shuttle`
//! invocation. It is **not** a resident process and **listens on no port** —
//! cold start stays `0`; we merely read the disk instead of re-running ctags on
//! files git says are unchanged. (Contrast a language server, which we
//! deliberately do not run.)
//!
//! # Staleness watermark (§3, reused from T2 `reverify`)
//! The index records the git commit oid it was refreshed at
//! ([`PersistentIndex::index_base_oid`]). A **read-only** consumer that trusts
//! the manifest without re-hashing (the fast path) must first check that the
//! index reflects the commit it is reasoning about: [`load_status`] returns
//! [`IndexFreshness::Stale`] whenever `index_base_oid != head_oid`, using the
//! **same exact-match rule** T2's re-verify set uses (`index_base_oid ==
//! head_oid`). A stale index never silently backs an impact set — the caller
//! refreshes first, exactly as T2 blocks with `stale-index` rather than walking
//! a stale closure (§7 「绝不瞎猜」: absence of a fresh index is not license to
//! guess).
//!
//! The [`refresh`] path, by contrast, is *always correct*: it re-hashes every
//! tracked file against the manifest, so a hit can only happen when the bytes
//! are genuinely identical. Content-addressing means a hit is a proof, not a
//! bet — even if git mtimes lie or a file was touched-but-unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::imports::ImportGraph;
use crate::{
    scan_files, scan_files_with_timeout, validate_scan_files, Symbol, SymbolError, SymbolKind,
    SymbolTable,
};

/// On-disk index schema version. Bump to force a cold rebuild when the layout
/// changes (an old manifest then misses the version check and is discarded).
const INDEX_VERSION: u32 = 1;

/// Wire identity for the caller-owned, pure index artifact.  This is kept
/// separate from the legacy `.shuttle/symbols.index` cache even though both contain
/// per-file symbol data: a cache is best-effort local state, while an artifact
/// is a strict, portable value that may be handed to another worktree.
const ARTIFACT_SCHEMA: &str = "shuttle_symbol_index/v1";
const MAX_INDEX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_IMPORT_EDGES: usize = 250_000;
const MAX_INDEX_GRAPH_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INDEX_GRAPH_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GIT_METADATA_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_GIT_PATH_OUTPUT_BYTES: u64 = 512 * 1024;

/// FNV-1a 64-bit content hash — dependency-free and stable across runs and
/// machines (unlike `DefaultHasher`, whose seed is unspecified). Used purely for
/// *content addressing*: two byte-identical files hash equal, so a cache hit is
/// a proof the tags are still valid. Not a security primitive; collision-safe
/// enough for change detection when paired with the byte length.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3); // FNV prime
    }
    h
}

fn content_signature(path: &Path) -> Result<(u64, u64), SymbolError> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        for byte in &buffer[..read] {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    Ok((hash, size))
}

/// One indexed file: its content signature (`hash` + `size`, both must match for
/// a cache hit) and the symbol tags ctags produced for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedFile {
    /// FNV-1a hash of the file's bytes at index time.
    content_hash: u64,
    /// Byte length at index time (belt-and-suspenders against hash collision).
    size: u64,
    /// The symbols ctags reported for this file.
    symbols: Vec<Symbol>,
}

/// Canonical, caller-owned index value emitted by [`refresh_artifact`].
///
/// All map keys and every [`Symbol::path`] are repository-relative `/`-style
/// paths.  `BTreeMap` plus explicit symbol sorting makes compact serde JSON a
/// canonical encoding: the same commit scanned in different worktrees emits
/// byte-identical bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexArtifact {
    schema: String,
    version: u32,
    index_base_oid: String,
    files: BTreeMap<String, IndexedFile>,
}

impl IndexArtifact {
    fn into_table(self) -> SymbolTable {
        let mut symbols = Vec::new();
        for entry in self.files.into_values() {
            symbols.extend(entry.symbols);
        }
        symbols.sort_by(symbol_cmp);
        SymbolTable { symbols }
    }
}

/// Read-only verdict for a caller-owned pure artifact. Missing, corrupt, or
/// incompatible artifacts are deliberately indistinguishable as `Absent`: all
/// three mean the optimization is unavailable and the consumer must degrade.
#[derive(Debug, Clone)]
pub enum ArtifactRead {
    /// The artifact is valid and exactly bound to the requested commit.
    Fresh(SymbolTable),
    /// The artifact is valid but belongs to a different commit.
    Stale {
        /// Commit recorded in the artifact.
        index_base_oid: String,
        /// Commit requested by the consumer.
        head_oid: String,
    },
    /// Missing, corrupt, unknown-version, or structurally invalid artifact.
    Absent,
}

/// The content-addressed persistent index: per-file tags keyed by repo path,
/// plus the commit oid the index was refreshed at (the staleness watermark).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistentIndex {
    /// Schema version; a mismatch discards the manifest (cold rebuild).
    version: u32,
    /// The git commit oid this index reflects. Empty when built outside git.
    /// Compared exactly against a query's head to detect staleness (§3, T2).
    index_base_oid: String,
    /// path → indexed tags. Sorted (BTreeMap) for a deterministic on-disk form.
    files: BTreeMap<String, IndexedFile>,
}

impl PersistentIndex {
    /// Flatten every indexed file's tags back into a whole-repo [`SymbolTable`].
    fn into_table(self) -> SymbolTable {
        let mut symbols = Vec::new();
        for entry in self.files.into_values() {
            symbols.extend(entry.symbols);
        }
        SymbolTable { symbols }
    }

    /// The commit oid this index reflects (empty when built outside git).
    pub fn index_base_oid(&self) -> &str {
        &self.index_base_oid
    }

    /// Number of files carried in the index.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the index carries no files.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Outcome of a [`refresh`]: the fresh whole-repo table plus the delta stats
/// (how much work the content-addressing actually saved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStats {
    /// Tracked files considered this refresh.
    pub total_files: usize,
    /// Files served straight from the on-disk index (unchanged content).
    pub cache_hits: usize,
    /// Files re-scanned because their content changed / they are new.
    pub rebuilt: usize,
    /// Manifest entries dropped because the path is no longer tracked / present.
    pub dropped: usize,
    /// The commit oid stamped into the refreshed index.
    pub index_base_oid: String,
}

/// The freshness verdict for a read-only consumer of the on-disk index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexFreshness {
    /// The manifest reflects `head_oid` exactly — safe to trust without re-hash.
    Fresh {
        /// The oid the index reflects (== the queried head).
        index_base_oid: String,
        /// Number of files in the trusted index.
        files: usize,
    },
    /// The manifest does not reflect `head_oid` (drift, cold, or version
    /// mismatch): a consumer must [`refresh`] before reasoning off it. Mirrors
    /// T2's `stale-index` block — never guess off a stale index (§7).
    Stale {
        /// The oid the on-disk index reflects (empty when cold/absent).
        index_base_oid: String,
        /// The oid the caller is reasoning about.
        head_oid: String,
    },
}

impl IndexFreshness {
    /// Whether the index may be trusted as-is for `head_oid`.
    pub fn is_fresh(&self) -> bool {
        matches!(self, IndexFreshness::Fresh { .. })
    }
}

/// The delta a refresh must apply, computed **purely** from the previous
/// manifest and the freshly-hashed current tracked set — no I/O, so the
/// content-addressing logic is unit-testable without git or ctags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Delta {
    /// Paths whose content signature is unchanged: reuse the cached tags.
    hits: Vec<String>,
    /// Paths whose content changed or which are new: must be re-scanned.
    rescan: Vec<String>,
    /// Count of prior manifest entries not carried forward (vanished/untracked).
    dropped: usize,
}

/// Pure delta planner. `prev` is the loaded manifest; `current` is the current
/// tracked set as `(path, content_hash, size)`. A path hits the cache iff the
/// previous entry has the identical `(hash, size)`; otherwise it is re-scanned.
/// Any prior path absent from `current` is counted as dropped (it vanished or
/// is no longer tracked). Deterministic: `current` order is preserved and the
/// input is expected pre-sorted by the caller.
fn plan_delta(prev: &BTreeMap<String, IndexedFile>, current: &[(String, u64, u64)]) -> Delta {
    let mut hits = Vec::new();
    let mut rescan = Vec::new();
    let present: BTreeSet<&str> = current.iter().map(|(p, _, _)| p.as_str()).collect();
    for (path, hash, size) in current {
        match prev.get(path) {
            Some(e) if e.content_hash == *hash && e.size == *size => hits.push(path.clone()),
            _ => rescan.push(path.clone()),
        }
    }
    let dropped = prev
        .keys()
        .filter(|p| !present.contains(p.as_str()))
        .count();
    Delta {
        hits,
        rescan,
        dropped,
    }
}

/// Load the on-disk index; any read / parse failure or version mismatch yields
/// an empty (cold) index, so a corrupt cache degrades to a full rebuild rather
/// than an error (the index is an optimization, never a correctness dependency).
fn load_index(cache_path: &str) -> PersistentIndex {
    match read_file_bounded(cache_path, MAX_INDEX_ARTIFACT_BYTES, "index_cache_bytes") {
        Ok(bytes) => {
            let idx: PersistentIndex = serde_json::from_slice(&bytes).unwrap_or_default();
            if idx.version == INDEX_VERSION {
                idx
            } else {
                PersistentIndex::default()
            }
        }
        Err(_) => PersistentIndex::default(),
    }
}

/// Persist the index next to its path (creating `.shuttle/` if needed). Best-effort:
/// a write failure is swallowed — a missed write only costs the next run a
/// rebuild, it never corrupts a result.
fn store_index(cache_path: &str, idx: &PersistentIndex) {
    let json = match serde_json::to_string(idx) {
        Ok(j) => j,
        Err(_) => return,
    };
    if let Some(parent) = Path::new(cache_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(cache_path, json);
}

/// Resolve `root`'s current `HEAD` commit oid. `None` outside a git repo (or on
/// an unborn branch) — the index then records an empty watermark and every
/// freshness check degrades to `Stale` (we never claim freshness we cannot
/// prove).
pub fn git_head_oid(root: &str) -> Option<String> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git).ok()?;
    Command::new(binary)
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Statistics for one pure artifact refresh.  These are deliberately returned
/// out-of-band rather than serialized into [`IndexArtifact`], otherwise a cold
/// and an incremental build of the same commit would not be byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactStats {
    /// Files represented by the resulting artifact.
    pub total_files: usize,
    /// Files whose prior entries were carried forward byte-for-byte.
    pub reused: usize,
    /// Compilation units actually handed to ctags this invocation.
    pub rescanned: usize,
    /// Exact repository-relative units handed to ctags, in stable order.
    pub rescanned_paths: Vec<String>,
}

/// Build a canonical, caller-owned symbol-index artifact for `head`.
///
/// This is the pure counterpart to [`refresh`]. It never reads or writes the
/// default `.shuttle` cache. If `merge_path` is present, `base` is mandatory and
/// must resolve exactly to the commit recorded by that artifact. The worktree
/// must be clean and checked out at `head`, which makes the artifact's commit
/// binding a proof instead of caller-supplied decoration.
pub fn refresh_artifact(
    root: &str,
    head: &str,
    merge_path: Option<&str>,
    base: Option<&str>,
) -> Result<(Vec<u8>, ArtifactStats), SymbolError> {
    refresh_artifact_with_timeout(root, head, merge_path, base, None)
}

/// Timed variant used by the CLI boundary. No bytes are published until the
/// complete artifact has been constructed, so timeout cannot corrupt or
/// partially replace a caller-owned index.
pub fn refresh_artifact_with_timeout(
    root: &str,
    head: &str,
    merge_path: Option<&str>,
    base: Option<&str>,
    timeout: Option<std::time::Duration>,
) -> Result<(Vec<u8>, ArtifactStats), SymbolError> {
    let root = canonical_repo_root(root)?;
    let head_oid = resolve_commit(&root, head, "head")?;
    require_clean_head(&root, &head_oid)?;

    let previous = match (merge_path, base) {
        (None, None) => None,
        (Some(path), Some(base)) => {
            let base_oid = resolve_commit(&root, base, "base")?;
            let artifact = load_artifact_strict(path)?;
            if artifact.index_base_oid != base_oid {
                return Err(SymbolError::Scan(format!(
                    "merge base mismatch: artifact records {}, requested base resolves to {}",
                    artifact.index_base_oid, base_oid
                )));
            }
            Some((artifact, base_oid))
        }
        (Some(_), None) => {
            return Err(SymbolError::Scan(
                "--merge requires an explicit --base".to_string(),
            ));
        }
        (None, Some(_)) => {
            return Err(SymbolError::Scan(
                "--base is only valid with --merge".to_string(),
            ));
        }
    };

    let tracked = tracked_relative(&root)?;
    let signatures = signatures(&root, &tracked)?;
    let (files, stats) = if let Some((previous, base_oid)) = previous {
        let contents = load_dependency_contents(&root, &tracked)?;
        merge_artifact(MergeArtifactInput {
            root: &root,
            base_oid: &base_oid,
            head_oid: &head_oid,
            previous,
            tracked: &tracked,
            signatures: &signatures,
            contents: &contents,
            timeout,
        })?
    } else {
        let scanned = scan_relative_files(&root, &tracked, timeout)?;
        let files = assemble_indexed_files(&tracked, &signatures, scanned);
        (
            files,
            ArtifactStats {
                total_files: tracked.len(),
                reused: 0,
                rescanned: tracked.len(),
                rescanned_paths: tracked.clone(),
            },
        )
    };

    let artifact = IndexArtifact {
        schema: ARTIFACT_SCHEMA.to_string(),
        version: INDEX_VERSION,
        index_base_oid: head_oid,
        files,
    };
    let mut bytes = serde_json::to_vec(&artifact)
        .map_err(|e| SymbolError::Parse(format!("serialize index artifact: {e}")))?;
    bytes.push(b'\n');
    Ok((bytes, stats))
}

/// Atomically publish a completed pure artifact at the caller-selected path.
/// Parent creation, temporary-file writes, `sync_all`, and rename failures are
/// all propagated; unlike the legacy cache, an artifact write is contractual.
pub fn write_artifact_atomic(path: &str, bytes: &[u8]) -> Result<(), SymbolError> {
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| {
            SymbolError::Scan(format!("invalid artifact output path: {}", path.display()))
        })?
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!(".{name}.tmp.{}.{}", std::process::id(), nonce));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(SymbolError::Io)
}

/// Read a pure artifact without any filesystem mutation.
///
/// A valid artifact is usable only at its exact `index_base_oid`. The returned
/// table retains the artifact's canonical repository-relative paths. Any read,
/// parse, schema, version, or structural failure degrades to [`ArtifactRead::Absent`].
pub fn read_artifact_for_head(path: &str, head: &str) -> ArtifactRead {
    let artifact = match load_artifact_strict(path) {
        Ok(artifact) => artifact,
        Err(_) => return ArtifactRead::Absent,
    };
    if artifact.index_base_oid != head {
        return ArtifactRead::Stale {
            index_base_oid: artifact.index_base_oid,
            head_oid: head.to_string(),
        };
    }
    ArtifactRead::Fresh(artifact.into_table())
}

/// Resolve the requested revision in the repository, then read the
/// caller-owned artifact against that exact commit. This avoids comparing a
/// symbolic ref or abbreviated oid textually with the canonical full oid.
pub fn read_artifact_for_revision(root: &str, path: &str, head: &str) -> ArtifactRead {
    let root = match canonical_repo_root(root) {
        Ok(root) => root,
        Err(_) => return ArtifactRead::Absent,
    };
    let head_oid = match resolve_commit(&root, head, "head") {
        Ok(oid) => oid,
        Err(_) => return ArtifactRead::Absent,
    };
    let artifact = match load_artifact_strict(path) {
        Ok(artifact) => artifact,
        Err(_) => return ArtifactRead::Absent,
    };
    if artifact.index_base_oid != head_oid {
        return ArtifactRead::Stale {
            index_base_oid: artifact.index_base_oid,
            head_oid,
        };
    }

    // HEAD equality alone is insufficient in an active worktree: a dirty
    // tracked edit leaves HEAD unchanged. Re-hash the caller's checkout and
    // require an exact file/signature match before exposing the table.
    let tracked = match tracked_relative(&root) {
        Ok(paths) => paths,
        Err(_) => return ArtifactRead::Absent,
    };
    let signatures = match signatures(&root, &tracked) {
        Ok(signatures) => signatures,
        Err(_) => return ArtifactRead::Absent,
    };
    let exact = artifact.files.len() == signatures.len()
        && artifact.files.iter().all(|(file, entry)| {
            signatures
                .get(file)
                .map(|(hash, size)| entry.content_hash == *hash && entry.size == *size)
                .unwrap_or(false)
        });
    if !exact {
        return ArtifactRead::Stale {
            index_base_oid: artifact.index_base_oid,
            head_oid,
        };
    }
    ArtifactRead::Fresh(artifact.into_table())
}

fn canonical_repo_root(root: &str) -> Result<PathBuf, SymbolError> {
    let root = std::fs::canonicalize(root)?;
    let out = git_output(&root, &["rev-parse", "--show-toplevel"])?;
    let top = std::fs::canonicalize(out.trim())?;
    if root != top {
        return Err(SymbolError::Scan(format!(
            "pure index root must be the repository root: got {}, expected {}",
            root.display(),
            top.display()
        )));
    }
    Ok(root)
}

fn resolve_commit(root: &Path, rev: &str, label: &str) -> Result<String, SymbolError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(SymbolError::Scan(format!(
            "invalid {label} revision: {rev:?}"
        )));
    }
    git_output(
        root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .map(|s| s.trim().to_string())
    .map_err(|_| SymbolError::Scan(format!("cannot resolve {label} revision {rev:?}")))
}

fn require_clean_head(root: &Path, expected: &str) -> Result<(), SymbolError> {
    let actual = git_output(root, &["rev-parse", "HEAD"])?;
    if actual.trim() != expected {
        return Err(SymbolError::Scan(format!(
            "worktree HEAD mismatch: checked out {}, requested {}",
            actual.trim(),
            expected
        )));
    }
    for args in [
        ["diff", "--quiet", "--ignore-submodules", "--"].as_slice(),
        ["diff", "--cached", "--quiet", "--ignore-submodules", "--"].as_slice(),
    ] {
        let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
            .map_err(|error| SymbolError::Scan(error.to_string()))?;
        let status = Command::new(binary).args(args).current_dir(root).status()?;
        if !status.success() {
            return Err(SymbolError::Scan(
                "pure index requires a clean tracked worktree at --head".to_string(),
            ));
        }
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, SymbolError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| SymbolError::Scan(error.to_string()))?;
    let mut command = Command::new(binary);
    command.args(args).current_dir(root);
    let out = crate::command_output_bounded_with_timeout(
        &mut command,
        Some(std::time::Duration::from_secs(30)),
        MAX_GIT_METADATA_OUTPUT_BYTES,
        "git_metadata_output_bytes",
    )?;
    if !out.status.success() {
        return Err(SymbolError::Scan(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8(out.stdout)
        .map_err(|e| SymbolError::Parse(format!("git output is not UTF-8: {e}")))
}

fn tracked_relative(root: &Path) -> Result<Vec<String>, SymbolError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| SymbolError::Scan(error.to_string()))?;
    let mut command = Command::new(binary);
    command.args(["ls-files", "-z"]).current_dir(root);
    let out = crate::command_output_bounded_with_timeout(
        &mut command,
        Some(std::time::Duration::from_secs(30)),
        MAX_GIT_PATH_OUTPUT_BYTES,
        "git_tracked_path_bytes",
    )?;
    if !out.status.success() {
        return Err(SymbolError::Scan(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = String::from_utf8(out.stdout)
        .map_err(|e| SymbolError::Parse(format!("git path is not UTF-8: {e}")))?;
    let mut paths: Vec<String> = raw
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(normalize_relative)
        .collect::<Result<_, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn normalize_relative(path: &str) -> Result<String, SymbolError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(SymbolError::Scan(format!(
            "expected repository-relative path: {}",
            path.display()
        )));
    }
    let mut clean = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SymbolError::Scan(format!(
                    "path escapes repository root: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(clean.join("/"))
}

fn signatures(
    root: &Path,
    tracked: &[String],
) -> Result<BTreeMap<String, (u64, u64)>, SymbolError> {
    let absolute: Vec<String> = tracked
        .iter()
        .map(|path| root.join(path).to_string_lossy().into_owned())
        .collect();
    validate_scan_files(&absolute)?;
    let mut out = BTreeMap::new();
    for path in tracked {
        out.insert(path.clone(), content_signature(&root.join(path))?);
    }
    Ok(out)
}

fn load_dependency_contents(
    root: &Path,
    tracked: &[String],
) -> Result<BTreeMap<String, String>, SymbolError> {
    let mut contents = BTreeMap::new();
    let mut source_bytes = 0u64;
    for path in tracked.iter().filter(|path| {
        crate::imports::supports_dependency_path(path)
            || Path::new(path).file_name().and_then(|name| name.to_str()) == Some("go.mod")
    }) {
        let disk_path = root.join(path);
        let observed = match std::fs::metadata(&disk_path) {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        if observed > MAX_INDEX_GRAPH_FILE_BYTES {
            return Err(SymbolError::Limit {
                resource: "index_graph_file_bytes",
                observed,
                limit: MAX_INDEX_GRAPH_FILE_BYTES,
            });
        }
        let bytes = read_file_bounded(
            &disk_path.to_string_lossy(),
            MAX_INDEX_GRAPH_FILE_BYTES,
            "index_graph_file_bytes",
        )?;
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        source_bytes = source_bytes.saturating_add(content.len() as u64);
        if source_bytes > MAX_INDEX_GRAPH_SOURCE_BYTES {
            return Err(SymbolError::Limit {
                resource: "index_graph_source_bytes",
                observed: source_bytes,
                limit: MAX_INDEX_GRAPH_SOURCE_BYTES,
            });
        }
        contents.insert(path.clone(), content);
    }
    Ok(contents)
}

fn load_artifact_strict(path: &str) -> Result<IndexArtifact, SymbolError> {
    let bytes = read_file_bounded(path, MAX_INDEX_ARTIFACT_BYTES, "index_artifact_bytes")?;
    let artifact: IndexArtifact = serde_json::from_slice(&bytes)
        .map_err(|e| SymbolError::Parse(format!("invalid merge artifact {path}: {e}")))?;
    if artifact.schema != ARTIFACT_SCHEMA {
        return Err(SymbolError::Parse(format!(
            "unsupported merge artifact schema {:?}; expected {ARTIFACT_SCHEMA}",
            artifact.schema
        )));
    }
    if artifact.version != INDEX_VERSION {
        return Err(SymbolError::Parse(format!(
            "unsupported merge artifact version {}; expected {INDEX_VERSION}",
            artifact.version
        )));
    }
    if artifact.index_base_oid.is_empty() {
        return Err(SymbolError::Parse(
            "merge artifact has an empty index_base_oid".to_string(),
        ));
    }
    if artifact.files.len() > crate::MAX_SCAN_FILES {
        return Err(SymbolError::Limit {
            resource: "index_artifact_files",
            observed: artifact.files.len() as u64,
            limit: crate::MAX_SCAN_FILES as u64,
        });
    }
    let mut symbols = 0usize;
    for (path, entry) in &artifact.files {
        symbols = symbols.saturating_add(entry.symbols.len());
        if symbols > crate::MAX_CTAGS_SYMBOLS {
            return Err(SymbolError::Limit {
                resource: "index_artifact_symbols",
                observed: symbols as u64,
                limit: crate::MAX_CTAGS_SYMBOLS as u64,
            });
        }
        if normalize_relative(path)? != *path {
            return Err(SymbolError::Parse(format!(
                "merge artifact path is not canonical: {path:?}"
            )));
        }
        if entry.symbols.iter().any(|symbol| symbol.path != *path) {
            return Err(SymbolError::Parse(format!(
                "merge artifact contains a symbol outside its file entry: {path:?}"
            )));
        }
    }
    Ok(artifact)
}

fn read_file_bounded(
    path: &str,
    limit: u64,
    resource: &'static str,
) -> Result<Vec<u8>, SymbolError> {
    let observed = std::fs::metadata(path)?.len();
    if observed > limit {
        return Err(SymbolError::Limit {
            resource,
            observed,
            limit,
        });
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(observed as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(SymbolError::Limit {
            resource,
            observed: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

struct MergeArtifactInput<'a> {
    root: &'a Path,
    base_oid: &'a str,
    head_oid: &'a str,
    previous: IndexArtifact,
    tracked: &'a [String],
    signatures: &'a BTreeMap<String, (u64, u64)>,
    contents: &'a BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
}

fn merge_artifact(
    input: MergeArtifactInput<'_>,
) -> Result<(BTreeMap<String, IndexedFile>, ArtifactStats), SymbolError> {
    let MergeArtifactInput {
        root,
        base_oid,
        head_oid,
        previous,
        tracked,
        signatures,
        contents,
        timeout,
    } = input;
    let mut changed = changed_paths(root, base_oid, head_oid)?;
    let current_set: BTreeSet<&str> = tracked.iter().map(String::as_str).collect();
    let deleted = previous
        .files
        .keys()
        .any(|path| !current_set.contains(path.as_str()));

    // A mismatched signature proves this checkout cannot reuse the prior entry,
    // even if git's path delta omitted it. New files are covered here too.
    for path in tracked {
        let Some(&(hash, size)) = signatures.get(path) else {
            continue;
        };
        match previous.files.get(path) {
            Some(old) if old.content_hash == hash && old.size == size => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }

    // A removed unit cannot be represented in the current import graph, so its
    // old reverse edges cannot be proved here. Conservatively rescan the full
    // current universe rather than under-invalidate.
    let affected = if deleted {
        tracked.iter().cloned().collect()
    } else {
        reverse_compilation_unit_closure(&changed, tracked, contents)?
    };
    let rescan: Vec<String> = tracked
        .iter()
        .filter(|path| affected.contains(path.as_str()))
        .cloned()
        .collect();
    let scanned = scan_relative_files(root, &rescan, timeout)?;

    let mut files = BTreeMap::new();
    for path in tracked {
        let (hash, size) = signatures[path];
        if affected.contains(path.as_str()) {
            files.insert(
                path.clone(),
                IndexedFile {
                    content_hash: hash,
                    size,
                    symbols: scanned.get(path).cloned().unwrap_or_default(),
                },
            );
        } else {
            let old = previous.files.get(path).ok_or_else(|| {
                SymbolError::Parse(format!("merge artifact is missing unchanged path {path:?}"))
            })?;
            if old.content_hash != hash || old.size != size {
                return Err(SymbolError::Parse(format!(
                    "merge artifact signature mismatch for reused path {path:?}"
                )));
            }
            let mut old = old.clone();
            old.symbols.sort_by(symbol_cmp);
            files.insert(path.clone(), old);
        }
    }
    Ok((
        files,
        ArtifactStats {
            total_files: tracked.len(),
            reused: tracked.len().saturating_sub(rescan.len()),
            rescanned: rescan.len(),
            rescanned_paths: rescan,
        },
    ))
}

fn changed_paths(root: &Path, base: &str, head: &str) -> Result<BTreeSet<String>, SymbolError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| SymbolError::Scan(error.to_string()))?;
    let mut command = Command::new(binary);
    command
        .args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            base,
            head,
            "--",
        ])
        .current_dir(root);
    let raw = crate::command_output_bounded_with_timeout(
        &mut command,
        Some(std::time::Duration::from_secs(30)),
        MAX_GIT_PATH_OUTPUT_BYTES,
        "git_changed_path_bytes",
    )?;
    if !raw.status.success() {
        return Err(SymbolError::Scan(format!(
            "git diff {base} {head} failed: {}",
            String::from_utf8_lossy(&raw.stderr).trim()
        )));
    }
    let paths = String::from_utf8(raw.stdout)
        .map_err(|e| SymbolError::Parse(format!("git diff path is not UTF-8: {e}")))?;
    paths
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(normalize_relative)
        .collect()
}

fn reverse_compilation_unit_closure(
    changed: &BTreeSet<String>,
    tracked: &[String],
    contents: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>, SymbolError> {
    let imports = ImportGraph::try_build(contents, MAX_INDEX_IMPORT_EDGES)?;
    let mut affected: BTreeSet<String> = changed
        .iter()
        .filter(|path| tracked.binary_search(path).is_ok())
        .cloned()
        .collect();

    loop {
        let before = affected.len();
        let go_dirs: BTreeSet<String> = affected
            .iter()
            .filter(|path| path.ends_with(".go"))
            .map(|path| {
                path.rsplit_once('/')
                    .map(|(dir, _)| dir)
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        for path in tracked {
            if path.ends_with(".go") {
                let dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
                if go_dirs.contains(dir) {
                    affected.insert(path.clone());
                }
            }
        }
        let seeds: Vec<String> = affected.iter().cloned().collect();
        for candidate in tracked {
            if seeds.iter().any(|seed| imports.depends_on(candidate, seed)) {
                affected.insert(candidate.clone());
            }
        }
        if affected.len() == before {
            break;
        }
    }
    Ok(affected)
}

fn scan_relative_files(
    root: &Path,
    relative: &[String],
    timeout: Option<std::time::Duration>,
) -> Result<BTreeMap<String, Vec<Symbol>>, SymbolError> {
    let absolute: Vec<String> = relative
        .iter()
        .map(|path| root.join(path).to_string_lossy().into_owned())
        .collect();
    let mut by_path: BTreeMap<String, Vec<Symbol>> = relative
        .iter()
        .map(|path| (path.clone(), Vec::new()))
        .collect();
    for mut symbol in scan_files_with_timeout(&absolute, timeout)?.symbols {
        let path = Path::new(&symbol.path);
        let relative_path = if path.is_absolute() {
            path.strip_prefix(root).map_err(|_| {
                SymbolError::Parse(format!(
                    "ctags returned path outside repository: {}",
                    path.display()
                ))
            })?
        } else {
            path
        };
        let relative_path = normalize_relative(&relative_path.to_string_lossy())?;
        let symbols = by_path.get_mut(&relative_path).ok_or_else(|| {
            SymbolError::Parse(format!("ctags returned unrequested path {relative_path:?}"))
        })?;
        symbol.path = relative_path;
        symbols.push(symbol);
    }
    for symbols in by_path.values_mut() {
        symbols.sort_by(symbol_cmp);
    }
    Ok(by_path)
}

fn assemble_indexed_files(
    tracked: &[String],
    signatures: &BTreeMap<String, (u64, u64)>,
    mut scanned: BTreeMap<String, Vec<Symbol>>,
) -> BTreeMap<String, IndexedFile> {
    tracked
        .iter()
        .map(|path| {
            let (hash, size) = signatures[path];
            (
                path.clone(),
                IndexedFile {
                    content_hash: hash,
                    size,
                    symbols: scanned.remove(path).unwrap_or_default(),
                },
            )
        })
        .collect()
}

fn symbol_cmp(a: &Symbol, b: &Symbol) -> std::cmp::Ordering {
    (
        &a.path,
        a.line,
        &a.name,
        symbol_kind_rank(&a.kind),
        &a.scope,
        &a.signature,
    )
        .cmp(&(
            &b.path,
            b.line,
            &b.name,
            symbol_kind_rank(&b.kind),
            &b.scope,
            &b.signature,
        ))
}

fn symbol_kind_rank(kind: &SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Struct => 3,
        SymbolKind::Enum => 4,
        SymbolKind::Variable => 5,
        SymbolKind::Field => 6,
        SymbolKind::Constant => 7,
        SymbolKind::Module => 8,
        SymbolKind::Service => 9,
        SymbolKind::TypeAlias => 10,
        SymbolKind::Section => 11,
        SymbolKind::Key => 12,
        SymbolKind::Other => 13,
    }
}

/// Report whether the on-disk index at `cache_path` may be trusted for
/// `head_oid` **without** re-hashing (the fast read-only path). Fresh iff the
/// manifest version matches and its `index_base_oid` equals `head_oid` exactly
/// — the identical watermark rule T2's re-verify set uses. Anything else
/// (cold, drifted, empty watermark) is `Stale`: the caller must [`refresh`].
pub fn load_status(cache_path: &str, head_oid: &str) -> IndexFreshness {
    let idx = load_index(cache_path);
    if !idx.index_base_oid.is_empty() && idx.index_base_oid == head_oid {
        IndexFreshness::Fresh {
            index_base_oid: idx.index_base_oid,
            files: idx.files.len(),
        }
    } else {
        IndexFreshness::Stale {
            index_base_oid: idx.index_base_oid,
            head_oid: head_oid.to_string(),
        }
    }
}

/// Refresh the content-addressed index over `root`, re-scanning **only** the
/// files whose content changed since the last refresh, and stamp it with
/// `head_oid`. Returns the fresh whole-repo [`SymbolTable`] (byte-identical to a
/// cold scan) plus the [`IndexStats`] delta.
///
/// The tracked-file set comes from `git ls-files`; outside git (or an empty
/// tree) the current file set is whatever [`SymbolTable::scan`] discovers, and
/// the SAME content-addressed `plan_delta` runs against it (watermark =
/// `head_oid`, which is empty when there is no HEAD). Content addressing
/// guarantees correctness in both paths: a cache hit requires an identical
/// `(hash, size)`, so no stale tag can survive a real edit and a no-edit refresh
/// re-scans nothing — with or without git.
pub fn refresh(
    root: &str,
    cache_path: &str,
    head_oid: &str,
) -> Result<(SymbolTable, IndexStats), SymbolError> {
    let prev = load_index(cache_path);

    // Current tracked set. Outside git → seed from a full scan (no incremental
    // reuse is possible without a file list we can content-hash).
    let tracked = match crate::git_tracked(root)? {
        Some(t) => t,
        None => {
            // Outside git we have no tracked-file list, so the current file set
            // is whatever a whole-repo scan surfaces (ctags discovers the files
            // that carry symbols). We still content-address it: hash each
            // discovered file, run the SAME `plan_delta` against the prior
            // manifest, carry hits forward verbatim and rebuild only the changed
            // files. This makes "a hit is a proof" hold even without git — a
            // no-edit refresh re-scans nothing and a dropped file drops exactly
            // its entry (§U2).
            let table = SymbolTable::scan(root)?;
            let mut by_path: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();
            for s in table.symbols {
                by_path.entry(s.path.clone()).or_default().push(s);
            }
            let mut current: Vec<(String, u64, u64)> = Vec::new();
            let mut sig_of: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for path in by_path.keys() {
                if let Ok((h, sz)) = content_signature(Path::new(path)) {
                    current.push((path.clone(), h, sz));
                    sig_of.insert(path.clone(), (h, sz));
                }
            }
            current.sort();
            current.dedup();

            let delta = plan_delta(&prev.files, &current);

            let mut files: BTreeMap<String, IndexedFile> = BTreeMap::new();
            for path in &delta.hits {
                if let Some(e) = prev.files.get(path) {
                    files.insert(path.clone(), e.clone());
                }
            }
            for path in &delta.rescan {
                let (content_hash, size) = sig_of.get(path).copied().unwrap_or((0, 0));
                let symbols = by_path.remove(path).unwrap_or_default();
                files.insert(
                    path.clone(),
                    IndexedFile {
                        content_hash,
                        size,
                        symbols,
                    },
                );
            }

            let idx = PersistentIndex {
                version: INDEX_VERSION,
                index_base_oid: head_oid.to_string(),
                files,
            };
            let stats = IndexStats {
                total_files: current.len(),
                cache_hits: delta.hits.len(),
                rebuilt: delta.rescan.len(),
                dropped: delta.dropped,
                index_base_oid: head_oid.to_string(),
            };
            store_index(cache_path, &idx);
            return Ok((idx.into_table(), stats));
        }
    };

    // Hash the current tracked set (skipping unreadable files — they cannot be
    // indexed and are simply omitted, exactly as a cold scan would skip them).
    let mut current: Vec<(String, u64, u64)> = Vec::new();
    let mut sig_of: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for path in &tracked {
        if let Ok((h, sz)) = content_signature(Path::new(path)) {
            current.push((path.clone(), h, sz));
            sig_of.insert(path.clone(), (h, sz));
        }
    }
    current.sort();
    current.dedup();

    let delta = plan_delta(&prev.files, &current);

    // Assemble the refreshed manifest: carry hits forward verbatim, re-scan the
    // rest in one ctags pass.
    let mut files: BTreeMap<String, IndexedFile> = BTreeMap::new();
    for path in &delta.hits {
        if let Some(e) = prev.files.get(path) {
            files.insert(path.clone(), e.clone());
        }
    }
    if !delta.rescan.is_empty() {
        let fresh = scan_files(&delta.rescan)?;
        let mut by_path: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();
        for s in fresh.symbols {
            by_path.entry(s.path.clone()).or_default().push(s);
        }
        for path in &delta.rescan {
            let (content_hash, size) = sig_of.get(path).copied().unwrap_or((0, 0));
            let symbols = by_path.remove(path).unwrap_or_default();
            files.insert(
                path.clone(),
                IndexedFile {
                    content_hash,
                    size,
                    symbols,
                },
            );
        }
    }

    let idx = PersistentIndex {
        version: INDEX_VERSION,
        index_base_oid: head_oid.to_string(),
        files,
    };
    let stats = IndexStats {
        total_files: current.len(),
        cache_hits: delta.hits.len(),
        rebuilt: delta.rescan.len(),
        dropped: delta.dropped,
        index_base_oid: head_oid.to_string(),
    };
    store_index(cache_path, &idx);
    Ok((idx.into_table(), stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idxfile(hash: u64, size: u64) -> IndexedFile {
        IndexedFile {
            content_hash: hash,
            size,
            symbols: Vec::new(),
        }
    }

    #[test]
    fn content_hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hellp"));
        // empty input is the bare FNV offset basis, deterministically.
        assert_eq!(content_hash(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn delta_all_hits_when_signatures_match() {
        let mut prev = BTreeMap::new();
        prev.insert("a.rs".to_string(), idxfile(10, 100));
        prev.insert("b.rs".to_string(), idxfile(20, 200));
        let current = vec![("a.rs".to_string(), 10, 100), ("b.rs".to_string(), 20, 200)];
        let d = plan_delta(&prev, &current);
        assert_eq!(d.hits, vec!["a.rs", "b.rs"]);
        assert!(d.rescan.is_empty());
        assert_eq!(d.dropped, 0);
    }

    #[test]
    fn delta_rescans_changed_and_new_drops_vanished() {
        let mut prev = BTreeMap::new();
        prev.insert("a.rs".to_string(), idxfile(10, 100)); // unchanged → hit
        prev.insert("b.rs".to_string(), idxfile(20, 200)); // content changed → rescan
        prev.insert("gone.rs".to_string(), idxfile(30, 300)); // vanished → dropped
        let current = vec![
            ("a.rs".to_string(), 10, 100),  // same sig
            ("b.rs".to_string(), 99, 200),  // new hash
            ("new.rs".to_string(), 40, 40), // brand new
        ];
        let d = plan_delta(&prev, &current);
        assert_eq!(d.hits, vec!["a.rs"]);
        assert_eq!(d.rescan, vec!["b.rs", "new.rs"]);
        assert_eq!(d.dropped, 1, "gone.rs must be counted dropped");
    }

    #[test]
    fn delta_size_change_forces_rescan_even_if_hash_matches() {
        // Defense in depth: identical hash but different size must not hit.
        let mut prev = BTreeMap::new();
        prev.insert("a.rs".to_string(), idxfile(10, 100));
        let current = vec![("a.rs".to_string(), 10, 101)];
        let d = plan_delta(&prev, &current);
        assert!(d.hits.is_empty());
        assert_eq!(d.rescan, vec!["a.rs"]);
    }

    #[test]
    fn load_status_cold_is_stale() {
        // A cache path that does not exist → cold → Stale, never a false Fresh.
        let f = IndexFreshness::Stale {
            index_base_oid: String::new(),
            head_oid: "deadbeef".to_string(),
        };
        let got = load_status(
            &format!(
                "{}/definitely-absent-{}.cache",
                std::env::temp_dir().display(),
                std::process::id()
            ),
            "deadbeef",
        );
        assert_eq!(got, f);
        assert!(!got.is_fresh());
    }

    #[test]
    fn load_status_exact_match_is_fresh_drift_is_stale() {
        let dir = std::env::temp_dir().join(format!("shuttle-index-status-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cache = dir.join("idx.cache");
        let cache_s = cache.to_string_lossy().to_string();
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), idxfile(1, 1));
        let idx = PersistentIndex {
            version: INDEX_VERSION,
            index_base_oid: "abc123".to_string(),
            files,
        };
        store_index(&cache_s, &idx);

        // Exact watermark match → Fresh.
        match load_status(&cache_s, "abc123") {
            IndexFreshness::Fresh {
                index_base_oid,
                files,
            } => {
                assert_eq!(index_base_oid, "abc123");
                assert_eq!(files, 1);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
        // Any drift → Stale (never guess off a stale index, §7).
        assert!(matches!(
            load_status(&cache_s, "def456"),
            IndexFreshness::Stale { .. }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_mismatch_is_discarded_as_cold() {
        let dir = std::env::temp_dir().join(format!("shuttle-index-ver-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cache = dir.join("idx.cache");
        let cache_s = cache.to_string_lossy().to_string();
        // Hand-write a manifest with a bogus version.
        let bogus = "{\"version\":9999,\"index_base_oid\":\"abc123\",\"files\":{}}".to_string();
        std::fs::write(&cache, bogus).unwrap();
        // A version mismatch is discarded → treated cold → Stale for any head.
        assert!(matches!(
            load_status(&cache_s, "abc123"),
            IndexFreshness::Stale { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pure_artifact_reader_distinguishes_fresh_stale_and_absent() {
        let dir = std::env::temp_dir().join(format!(
            "shuttle-pure-index-read-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("artifact.json");
        let path_s = path.to_string_lossy();
        let mut files = BTreeMap::new();
        files.insert(
            "src/a.rs".to_string(),
            IndexedFile {
                content_hash: 1,
                size: 1,
                symbols: vec![Symbol {
                    name: "alpha".to_string(),
                    kind: SymbolKind::Function,
                    scope: None,
                    path: "src/a.rs".to_string(),
                    line: 1,
                    signature: Some("fn alpha() {}".to_string()),
                }],
            },
        );
        let artifact = IndexArtifact {
            schema: ARTIFACT_SCHEMA.to_string(),
            version: INDEX_VERSION,
            index_base_oid: "abc123".to_string(),
            files,
        };
        std::fs::write(&path, serde_json::to_vec(&artifact).unwrap()).unwrap();

        match read_artifact_for_head(&path_s, "abc123") {
            ArtifactRead::Fresh(table) => {
                assert_eq!(table.symbols.len(), 1);
                assert_eq!(table.symbols[0].path, "src/a.rs");
            }
            _ => panic!("exact OID must be fresh"),
        }
        assert!(matches!(
            read_artifact_for_head(&path_s, "def456"),
            ArtifactRead::Stale {
                index_base_oid,
                head_oid
            } if index_base_oid == "abc123" && head_oid == "def456"
        ));

        let mut incompatible = artifact.clone();
        incompatible.version += 1;
        std::fs::write(&path, serde_json::to_vec(&incompatible).unwrap()).unwrap();
        assert!(matches!(
            read_artifact_for_head(&path_s, "abc123"),
            ArtifactRead::Absent
        ));

        std::fs::write(&path, b"not json").unwrap();
        assert!(matches!(
            read_artifact_for_head(&path_s, "abc123"),
            ArtifactRead::Absent
        ));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            read_artifact_for_head(&path_s, "abc123"),
            ArtifactRead::Absent
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_artifact_is_rejected_before_reading() {
        let dir = std::env::temp_dir().join(format!(
            "shuttle-pure-index-limit-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oversized.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_INDEX_ARTIFACT_BYTES + 1).unwrap();
        let err = load_artifact_strict(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(
            err,
            SymbolError::Limit {
                resource: "index_artifact_bytes",
                observed,
                limit
            } if observed == MAX_INDEX_ARTIFACT_BYTES + 1 && limit == MAX_INDEX_ARTIFACT_BYTES
        ));
        assert!(matches!(
            read_artifact_for_head(path.to_str().unwrap(), "head"),
            ArtifactRead::Absent
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
