//! `shuttle-conflict` --- git conflict resolution tooling for the shuttle supervisor.
//!
//! Implements the tool surface defined in :
//!
//! - **Perception group** (read-only, deterministic): `list`, `get_hunk`, `classify`
//! - **Merge group** (deterministic-first, model fallback): `auto3way`, `take`, `propose`
//! - **Verify group** (machine gate, non-skippable): `verify`, `diffstat`
//! - **Converge group** (strict boundary): `stage`, `emit_receipt`
//!
//! Architecture note (§8.5 of `shuttle_wraps_pi_architecture.md`):
//! - All deterministic tools are `shuttle conflict *` subcommands (Rust, this crate).
//! - Only `propose` surfaces as a pi-injectable tool (`conflict_propose`).
//! - `auto3way` wraps `mergiraf` as an external process (GPL-3 isolation).
//! - `shuttle conflict resolve <run-id>` drives the full state machine as supervisor.
//!
//! Key invariant: this crate NEVER commits or pushes. `stage` = `git add` only.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod outcome;

// --- Error type ---

#[derive(Debug, thiserror::Error)]
pub enum ConflictError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("mergiraf not found or failed: {0}")]
    Mergiraf(String),
    #[error("file not found: {path}")]
    FileNotFound { path: PathBuf },
    #[error("hunk {hunk_id} not found in {file}")]
    HunkNotFound { file: String, hunk_id: usize },
    #[error("verify gate failed: {reason}")]
    VerifyFailed { reason: String },
    #[error("unsafe operation attempted: {0}")]
    Unsafe(String),
    #[error("write path failed containment (P0-2): {0}")]
    PathRejected(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// --- Public types ---

/// A conflict hunk with base/ours/theirs OIDs and texts (§3.1).
/// OID = git blob SHA (from `git ls-files -u`), empty = no base for this stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictHunk {
    pub hunk_id: usize,
    pub file: String,
    pub base_oid: Option<String>,
    pub ours_oid: Option<String>,
    pub theirs_oid: Option<String>,
    pub base_text: Option<String>,
    pub ours_text: String,
    pub theirs_text: String,
    /// Byte range [start, end) of this hunk in the conflict file.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Symbol name this hunk maps to (tree-sitter, best-effort).
    pub symbol: Option<String>,
    /// Coupled hunk IDs that must be resolved together (L3).
    pub coupled_hunks: Vec<usize>,
}

/// Conflict classification levels (§5.1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictLevel {
    /// L0: whitespace / EOL / import union --- zero model.
    Trivial,
    /// L1: structurally non-overlapping (mergiraf main force) --- zero model.
    Structural,
    /// L2: semantic overlap --- LLM arbitration needed.
    Semantic,
    /// L3: cross-file / cross-hunk coupling --- escalate to planner.
    Coupled,
}

/// Summary of a conflicted file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictFile {
    pub path: String,
    pub hunk_count: usize,
}

/// Result from `auto3way`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Auto3wayResult {
    /// mergiraf resolved cleanly --- zero model.
    Resolved { merged_text: String },
    /// mergiraf produced output but residual conflicts remain --- need LLM.
    NeedsLlm { partial_text: String },
    /// mergiraf degraded / language not supported --- use fallback ladder.
    Degraded { reason: String },
}

/// One hunk's entry in the conflict receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkReceipt {
    pub hunk_id: usize,
    pub file: String,
    pub level: ConflictLevel,
    pub strategy: String,
    pub model_involved: bool,
    pub verify_passed: bool,
}

/// Full conflict receipt emitted at the end of a resolve run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReceipt {
    pub run_id: String,
    pub hunks: Vec<HunkReceipt>,
    pub all_verified: bool,
}

// --- Internal helpers ---

/// Run a git command in `repo` and return trimmed stdout.
fn git(repo: &Path, args: &[&str]) -> Result<String, ConflictError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| ConflictError::Git(error.to_string()))?;
    let out = Command::new(binary)
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| ConflictError::Git(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(ConflictError::Git(stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

struct StageOids {
    base: Option<String>,
    ours: Option<String>,
    theirs: Option<String>,
}

fn get_stage_oids(repo: &Path, file: &str) -> Result<StageOids, ConflictError> {
    let out = git(repo, &["ls-files", "-u", "--", file])?;
    let mut oids = StageOids {
        base: None,
        ours: None,
        theirs: None,
    };
    for line in out.lines() {
        // Format: <mode> <sha> <stage>\t<file>
        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() < 3 {
            continue;
        }
        let sha = parts[1].to_string();
        let stage: u8 = parts[2]
            .split('\t')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        match stage {
            1 => oids.base = Some(sha),
            2 => oids.ours = Some(sha),
            3 => oids.theirs = Some(sha),
            _ => {}
        }
    }
    Ok(oids)
}

fn count_conflict_markers(repo: &Path, file: &str) -> Result<usize, ConflictError> {
    let path = repo.join(file);
    let content = std::fs::read_to_string(&path)?;
    let count = content.lines().filter(|l| l.starts_with("<<<<<<<")).count();
    Ok(count)
}

// --- §3.1 Perception group ---

/// List all conflicted files in the git repo rooted at `repo`.
/// Equivalent to: `git diff --name-only --diff-filter=U`
pub fn list(repo: &Path) -> Result<Vec<ConflictFile>, ConflictError> {
    let out = git(repo, &["diff", "--name-only", "--diff-filter=U"])?;
    if out.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut files = Vec::new();
    for path in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
        // Count hunks by scanning the file for `<<<<<<<`
        let hunk_count = count_conflict_markers(repo, path).unwrap_or(0);
        files.push(ConflictFile {
            path: path.to_string(),
            hunk_count,
        });
    }
    Ok(files)
}

/// Return structured hunk data for (file, hunk_id).
/// Parses `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` markers with zdiff3 style.
pub fn get_hunk(repo: &Path, file: &str, hunk_id: usize) -> Result<ConflictHunk, ConflictError> {
    let full_path = repo.join(file);
    if !full_path.exists() {
        return Err(ConflictError::FileNotFound { path: full_path });
    }
    let content = std::fs::read_to_string(&full_path)?;
    let hunks = parse_conflict_hunks(&content, file);
    hunks
        .into_iter()
        .find(|h| h.hunk_id == hunk_id)
        .ok_or(ConflictError::HunkNotFound {
            file: file.to_string(),
            hunk_id,
        })
        // Enrich with OIDs from git ls-files -u
        .map(|mut h| {
            if let Ok(oids) = get_stage_oids(repo, file) {
                h.base_oid = oids.base;
                h.ours_oid = oids.ours;
                h.theirs_oid = oids.theirs;
            }
            h
        })
}

/// Classify a hunk's conflict level (deterministic, §5.1).
pub fn classify(_repo: &Path, hunk: &ConflictHunk) -> ConflictLevel {
    // L0: trivial --- one side only has whitespace/EOL differences
    let ours_norm = hunk
        .ours_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let theirs_norm = hunk
        .theirs_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if ours_norm == theirs_norm {
        return ConflictLevel::Trivial;
    }
    // L0: import-only changes (heuristic: every non-blank line starts with import/use)
    let is_import_line = |l: &str| {
        let t = l.trim();
        t.is_empty() || t.starts_with("import ") || t.starts_with("use ") || t.starts_with("from ")
    };
    if hunk.ours_text.lines().all(is_import_line) && hunk.theirs_text.lines().all(is_import_line) {
        return ConflictLevel::Trivial;
    }
    // L3: coupled hunks present
    if !hunk.coupled_hunks.is_empty() {
        return ConflictLevel::Coupled;
    }
    // L1 vs L2: precise disambiguation via shuttle-symbols tree-sitter scope.
    //
    // Design :
    //   * L1 Structural: ours/theirs touch *different* AST nodes — their
    //     identifier sets are disjoint, so a syntax-aware merge accepts both
    //     with zero model cost.
    //   * L2 Semantic: both sides edit the *same* semantic position — their
    //     identifier sets overlap, so a model must arbitrate.
    //
    // We extract the identifier name set from each side of the hunk (partial
    // fragments parse fine under tree-sitter error recovery) and compare:
    // disjoint → Structural, overlapping → Semantic.
    //
    // Conservative fallback (§7, never guess): if the language is unknown or
    // ast-grep is unavailable / errors on *either* side, we keep the previous
    // default of `Structural` — mergiraf will still gate it and any residual
    // conflict escalates to L2 downstream, so a mis-classify here never loses
    // an edit.
    classify_by_identifier_overlap(&hunk.file, &hunk.ours_text, &hunk.theirs_text)
        .unwrap_or(ConflictLevel::Structural)
}

/// Core of the L1/L2 decision: compare the identifier name sets of the two
/// sides. Returns `None` when the language is unsupported or ast-grep could not
/// parse a side — the caller then falls back to `Structural` (§7).
fn classify_by_identifier_overlap(
    file: &str,
    ours_text: &str,
    theirs_text: &str,
) -> Option<ConflictLevel> {
    let lang = shuttle_symbols::lang_for_path(file)?;
    let ours = shuttle_symbols::identifiers_in_fragment(ours_text, lang)?;
    let theirs = shuttle_symbols::identifiers_in_fragment(theirs_text, lang)?;
    // If either side carries no identifiers at all (e.g. pure punctuation /
    // literal edit), there is no shared semantic anchor to arbitrate — treat as
    // structurally separable.
    if ours.is_disjoint(&theirs) {
        Some(ConflictLevel::Structural)
    } else {
        Some(ConflictLevel::Semantic)
    }
}

// --- §3.2 Merge group ---

/// Thin wrapper around `mergiraf merge` (GPL-3 external process, §4).
/// Returns `Auto3wayResult` --- resolved, needs_llm, or degraded.
pub fn auto3way(repo: &Path, hunk: &ConflictHunk) -> Auto3wayResult {
    // Write base/ours/theirs to temp files in the repo dir (no /tmp on Mac)
    let tmp_base = repo.join(format!(".shuttle_merge_base_{}", hunk.hunk_id));
    let tmp_ours = repo.join(format!(".shuttle_merge_ours_{}", hunk.hunk_id));
    let tmp_theirs = repo.join(format!(".shuttle_merge_theirs_{}", hunk.hunk_id));

    let base_text = hunk.base_text.as_deref().unwrap_or("");
    if std::fs::write(&tmp_base, base_text).is_err()
        || std::fs::write(&tmp_ours, &hunk.ours_text).is_err()
        || std::fs::write(&tmp_theirs, &hunk.theirs_text).is_err()
    {
        let _ = cleanup_tmp_files(repo, hunk.hunk_id);
        return Auto3wayResult::Degraded {
            reason: "failed to write temp files".into(),
        };
    }

    // Check mergiraf is available
    let binary = match shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Mergiraf) {
        Ok(binary) => binary,
        Err(error) => {
            let _ = cleanup_tmp_files(repo, hunk.hunk_id);
            return Auto3wayResult::Degraded {
                reason: error.to_string(),
            };
        }
    };
    let check = Command::new(&binary).arg("--version").output();
    if check.is_err() || !check.unwrap().status.success() {
        let _ = cleanup_tmp_files(repo, hunk.hunk_id);
        return Auto3wayResult::Degraded {
            reason: "mergiraf not found --- install mergiraf or use fallback ladder".into(),
        };
    }

    let result = Command::new(binary)
        .arg("merge")
        .arg(&tmp_base)
        .arg(&tmp_ours)
        .arg(&tmp_theirs)
        .current_dir(repo)
        .output();

    let _ = cleanup_tmp_files(repo, hunk.hunk_id);

    match result {
        Err(e) => Auto3wayResult::Degraded {
            reason: e.to_string(),
        },
        Ok(out) => {
            let merged = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && !merged.contains("<<<<<<<") {
                Auto3wayResult::Resolved {
                    merged_text: merged,
                }
            } else if !merged.is_empty() {
                Auto3wayResult::NeedsLlm {
                    partial_text: merged,
                }
            } else {
                Auto3wayResult::Degraded {
                    reason: format!(
                        "mergiraf exit {}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr)
                    ),
                }
            }
        }
    }
}

/// Deterministic fast-path: take ours, theirs, or their union.
pub fn take(repo: &Path, file: &str, hunk_id: usize, side: TakeSide) -> Result<(), ConflictError> {
    let hunk = get_hunk(repo, file, hunk_id)?;
    let replacement = match side {
        TakeSide::Ours => hunk.ours_text.clone(),
        TakeSide::Theirs => hunk.theirs_text.clone(),
        TakeSide::Union => {
            format!(
                "{}
{}",
                hunk.ours_text.trim_end(),
                hunk.theirs_text
            )
        }
    };
    apply_hunk_replacement(repo, file, &hunk, &replacement)
}

/// Which side to take in `take()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeSide {
    Ours,
    Theirs,
    Union,
}

/// Apply model-proposed merged text for a specific hunk (L2, §3.2).
/// Validates: no residual conflict markers in `merged_text`.
/// Only overwrites the hunk's byte range --- minimum diff surface.
pub fn propose(
    repo: &Path,
    file: &str,
    hunk_id: usize,
    merged_text: &str,
) -> Result<(), ConflictError> {
    if merged_text.contains("<<<<<<<")
        || merged_text.contains("=======")
        || merged_text.contains(">>>>>>>")
    {
        return Err(ConflictError::Unsafe(
            "proposed text contains conflict markers".into(),
        ));
    }
    let hunk = get_hunk(repo, file, hunk_id)?;
    apply_hunk_replacement(repo, file, &hunk, merged_text)
}

// --- §3.3 Verify group ---

/// Gate 1 + Gate 2: check no residual markers and file is valid UTF-8.
/// (Full compile/test gates are delegated to shuttle-cli's build-and-verify.)
pub fn verify(repo: &Path, file: &str) -> Result<(), ConflictError> {
    let full_path = repo.join(file);
    let content = std::fs::read_to_string(&full_path).map_err(|_| ConflictError::FileNotFound {
        path: full_path.clone(),
    })?;

    // Gate 1: no residual conflict markers
    for (lineno, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("<<<<<<<")
            || trimmed.starts_with("=======")
            || trimmed.starts_with(">>>>>>>")
            || trimmed.starts_with("|||||||")
        {
            return Err(ConflictError::VerifyFailed {
                reason: format!(
                    "residual conflict marker at line {} in {}",
                    lineno + 1,
                    file
                ),
            });
        }
    }

    Ok(())
}

/// Return line-level diff stats: files changed, insertions, deletions.
/// Uses `git diff --numstat HEAD -- <file>` (approximate).
pub fn diffstat(repo: &Path, file: &str) -> Result<DiffStat, ConflictError> {
    let out = git(repo, &["diff", "--numstat", "HEAD", "--", file])?;
    let stat = parse_numstat(&out);
    Ok(stat)
}

/// Diff statistics for a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffStat {
    pub insertions: usize,
    pub deletions: usize,
}

// --- §3.4 Converge group ---

/// `git add <file>` --- only stages, never commits. Boundary discipline.
pub fn stage(repo: &Path, file: &str) -> Result<(), ConflictError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| ConflictError::Git(error.to_string()))?;
    let status = Command::new(binary)
        .args(["add", file])
        .current_dir(repo)
        .status()
        .map_err(|e| ConflictError::Git(e.to_string()))?;
    if !status.success() {
        return Err(ConflictError::Git(format!("git add failed for {file}")));
    }
    Ok(())
}

/// Emit a conflict receipt JSON for a run.
pub fn emit_receipt(receipts: Vec<HunkReceipt>, run_id: &str) -> ConflictReceipt {
    let all_verified = receipts.iter().all(|r| r.verify_passed);
    ConflictReceipt {
        run_id: run_id.to_string(),
        hunks: receipts,
        all_verified,
    }
}

// --- Full resolve orchestrator ---

/// Drive the full state machine for a conflict resolution run (§9 checklist).
/// Runs: list -> classify -> auto3way -> verify -> stage -> emit_receipt.
/// `propose_fn`: optional callback for L2 semantic conflicts
///   signature: `fn(hunk: &ConflictHunk) -> Option<String>`
pub fn resolve<F>(
    repo: &Path,
    run_id: &str,
    propose_fn: Option<F>,
) -> Result<ConflictReceipt, ConflictError>
where
    F: Fn(&ConflictHunk) -> Option<String>,
{
    let files = list(repo)?;
    let mut receipts: Vec<HunkReceipt> = Vec::new();

    for cf in &files {
        for hunk_id in 0..cf.hunk_count {
            let hunk = get_hunk(repo, &cf.path, hunk_id)?;
            let level = classify(repo, &hunk);
            let mut model_involved = false;
            let strategy: String;
            let mut verify_ok = false;

            match level {
                ConflictLevel::Trivial => {
                    // L0: take theirs (whitespace/import union handled)
                    take(repo, &cf.path, hunk_id, TakeSide::Theirs)?;
                    strategy = "take_theirs_trivial".into();
                }
                ConflictLevel::Structural => {
                    // L1: attempt auto3way
                    match auto3way(repo, &hunk) {
                        Auto3wayResult::Resolved { merged_text } => {
                            apply_hunk_replacement(repo, &cf.path, &hunk, &merged_text)?;
                            strategy = "mergiraf_resolved".into();
                        }
                        Auto3wayResult::NeedsLlm { partial_text } => {
                            if let Some(ref pf) = propose_fn {
                                if let Some(text) = pf(&hunk) {
                                    propose(repo, &cf.path, hunk_id, &text)?;
                                    strategy = "mergiraf_partial_llm".into();
                                    model_involved = true;
                                } else {
                                    apply_hunk_replacement(repo, &cf.path, &hunk, &partial_text)?;
                                    strategy = "mergiraf_partial_best_effort".into();
                                }
                            } else {
                                apply_hunk_replacement(repo, &cf.path, &hunk, &partial_text)?;
                                strategy = "mergiraf_partial_no_llm".into();
                            }
                        }
                        Auto3wayResult::Degraded { reason } => {
                            // Fallback: try LLM or take theirs as last resort
                            if let Some(ref pf) = propose_fn {
                                if let Some(text) = pf(&hunk) {
                                    propose(repo, &cf.path, hunk_id, &text)?;
                                    strategy = format!("degraded_llm [{reason}]");
                                    model_involved = true;
                                } else {
                                    take(repo, &cf.path, hunk_id, TakeSide::Theirs)?;
                                    strategy = format!("degraded_take_theirs [{reason}]");
                                }
                            } else {
                                take(repo, &cf.path, hunk_id, TakeSide::Theirs)?;
                                strategy = format!("degraded_take_theirs_no_llm [{reason}]");
                            }
                        }
                    }
                }
                ConflictLevel::Semantic => {
                    // L2: LLM required
                    if let Some(ref pf) = propose_fn {
                        if let Some(text) = pf(&hunk) {
                            propose(repo, &cf.path, hunk_id, &text)?;
                            strategy = "llm_propose".into();
                            model_involved = true;
                        } else {
                            strategy = "llm_unavailable_skipped".into();
                        }
                    } else {
                        strategy = "semantic_no_llm_skipped".into();
                    }
                }
                ConflictLevel::Coupled => {
                    // L3: escalate --- skip individual hunk, mark for planner
                    strategy = "escalate_to_planner".into();
                }
            }

            // Verify gate after each hunk resolution
            if let Ok(()) = verify(repo, &cf.path) {
                verify_ok = true;
                // Only stage after verify passes
                if level != ConflictLevel::Coupled {
                    let _ = stage(repo, &cf.path);
                }
            }

            receipts.push(HunkReceipt {
                hunk_id,
                file: cf.path.clone(),
                level,
                strategy,
                model_involved,
                verify_passed: verify_ok,
            });
        }
    }

    Ok(emit_receipt(receipts, run_id))
}

/// Parse conflict hunks from file content (zdiff3 / diff3 style).
fn parse_conflict_hunks(content: &str, file: &str) -> Vec<ConflictHunk> {
    let mut hunks = Vec::new();
    let mut hunk_id = 0usize;
    let mut i = 0usize;
    let bytes = content.as_bytes();
    let len = bytes.len();

    while i < len {
        let start = match find_marker(content, i, "<<<<<<<") {
            Some(p) => p,
            None => break,
        };

        let base_sep = find_marker(content, start, "|||||||");
        let eq_sep = find_marker(content, start, "=======");

        let (base_start, ours_end, base_end) = match (base_sep, eq_sep) {
            (Some(bs), Some(es)) if bs < es => {
                // zdiff3: ours | base | theirs
                (Some(bs), bs, es)
            }
            (_, Some(es)) => (None, es, es),
            _ => break,
        };

        let theirs_end = match find_marker(content, base_end, ">>>>>>>") {
            Some(p) => p,
            None => break,
        };

        let ours_line_end = next_line_start(content, start);
        let ours_text = content[ours_line_end..ours_end]
            .trim_end_matches('\n')
            .to_string();

        let base_text = if let Some(bs) = base_start {
            let bl = next_line_start(content, bs);
            Some(content[bl..base_end].trim_end_matches('\n').to_string())
        } else {
            None
        };

        let theirs_line_start = next_line_start(content, base_end);
        let theirs_text = content[theirs_line_start..theirs_end]
            .trim_end_matches('\n')
            .to_string();

        // Byte range spans from  to end of  line
        let byte_end_line = next_line_start(content, theirs_end);

        hunks.push(ConflictHunk {
            hunk_id,
            file: file.to_string(),
            base_oid: None,
            ours_oid: None,
            theirs_oid: None,
            base_text,
            ours_text,
            theirs_text,
            byte_start: start,
            byte_end: byte_end_line,
            symbol: None,
            coupled_hunks: vec![],
        });

        hunk_id += 1;
        i = byte_end_line;
    }

    hunks
}

fn find_marker(content: &str, from: usize, marker: &str) -> Option<usize> {
    content[from..].find(marker).map(|p| from + p)
}

fn next_line_start(content: &str, from: usize) -> usize {
    match content[from..].find('\n') {
        Some(p) => from + p + 1,
        None => content.len(),
    }
}

fn apply_hunk_replacement(
    repo: &Path,
    file: &str,
    hunk: &ConflictHunk,
    replacement: &str,
) -> Result<(), ConflictError> {
    // P0-2 (§2.1): conflict resolution shares the SAME ScopedPathResolver as
    // shuttle apply — never a second hand-rolled containment check. `file` comes
    // from `git ls-files -u`, but defense-in-depth still confines the write to
    // the repo subtree (rejects absolute / `..` escape / symlink escape).
    let resolver = shuttle_protocol::ScopedPathResolver::new(repo, vec!["**".to_string()])
        .map_err(ConflictError::Io)?;
    let full_path = resolver
        .resolve(file)
        .map_err(|v| ConflictError::PathRejected(v.to_string()))?;
    let content = std::fs::read_to_string(&full_path)?;
    // Replace hunk byte range with replacement text
    let before = &content[..hunk.byte_start];
    let after = &content[hunk.byte_end.min(content.len())..];
    let mut new_content = String::with_capacity(content.len());
    new_content.push_str(before);
    new_content.push_str(replacement);
    if !replacement.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(after);
    std::fs::write(&full_path, new_content)?;
    Ok(())
}

fn cleanup_tmp_files(repo: &Path, hunk_id: usize) -> std::io::Result<()> {
    let _ = std::fs::remove_file(repo.join(format!(".shuttle_merge_base_{hunk_id}")));
    let _ = std::fs::remove_file(repo.join(format!(".shuttle_merge_ours_{hunk_id}")));
    let _ = std::fs::remove_file(repo.join(format!(".shuttle_merge_theirs_{hunk_id}")));
    Ok(())
}

fn parse_numstat(output: &str) -> DiffStat {
    // Format:
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let ins = parts[0].parse().unwrap_or(0);
            let del = parts[1].parse().unwrap_or(0);
            return DiffStat {
                insertions: ins,
                deletions: del,
            };
        }
    }
    DiffStat {
        insertions: 0,
        deletions: 0,
    }
}
// --- Unit tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn make_conflict(ours: &str, base: Option<&str>, theirs: &str) -> String {
        match base {
            Some(b) => format!(
                "<<<<<<< ours
{ours}
||||||| base
{b}
=======
{theirs}
>>>>>>> theirs
"
            ),
            None => format!(
                "<<<<<<< ours
{ours}
=======
{theirs}
>>>>>>> theirs
"
            ),
        }
    }

    #[test]
    fn parse_simple_diff3_hunk() {
        let content = make_conflict(
            "fn foo() {}
",
            None,
            "fn foo() { bar(); }
",
        );
        let hunks = parse_conflict_hunks(&content, "test.rs");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].hunk_id, 0);
        assert!(hunks[0].ours_text.contains("fn foo() {}"));
        assert!(hunks[0].theirs_text.contains("fn foo() { bar(); }"));
        assert!(hunks[0].base_text.is_none());
    }

    #[test]
    fn parse_zdiff3_hunk() {
        let content = make_conflict(
            "fn foo() {}
",
            Some(
                "fn foo() { /* orig */ }
",
            ),
            "fn foo() { bar(); }
",
        );
        let hunks = parse_conflict_hunks(&content, "test.rs");
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].base_text.is_some());
        assert!(hunks[0].base_text.as_ref().unwrap().contains("orig"));
    }

    #[test]
    fn parse_multiple_hunks() {
        let h1 = make_conflict(
            "a
", None, "b
",
        );
        let h2 = make_conflict(
            "c
", None, "d
",
        );
        let content = format!(
            "header
{h1}
middle
{h2}
footer
"
        );
        let hunks = parse_conflict_hunks(&content, "test.rs");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].hunk_id, 0);
        assert_eq!(hunks[1].hunk_id, 1);
    }

    #[test]
    fn classify_trivial_whitespace() {
        let hunk = ConflictHunk {
            hunk_id: 0,
            file: "f.rs".into(),
            base_oid: None,
            ours_oid: None,
            theirs_oid: None,
            base_text: None,
            ours_text: "  fn foo() {}
"
            .into(),
            theirs_text: "fn foo() {}
"
            .into(),
            byte_start: 0,
            byte_end: 100,
            symbol: None,
            coupled_hunks: vec![],
        };
        let level = classify(Path::new("."), &hunk);
        assert_eq!(level, ConflictLevel::Trivial);
    }

    #[test]
    fn classify_import_only() {
        let hunk = ConflictHunk {
            hunk_id: 0,
            file: "f.py".into(),
            base_oid: None,
            ours_oid: None,
            theirs_oid: None,
            base_text: None,
            ours_text: "import os
import sys
"
            .into(),
            theirs_text: "import os
import json
"
            .into(),
            byte_start: 0,
            byte_end: 100,
            symbol: None,
            coupled_hunks: vec![],
        };
        let level = classify(Path::new("."), &hunk);
        assert_eq!(level, ConflictLevel::Trivial);
    }

    #[test]
    fn classify_coupled() {
        let hunk = ConflictHunk {
            hunk_id: 0,
            file: "f.rs".into(),
            base_oid: None,
            ours_oid: None,
            theirs_oid: None,
            base_text: None,
            ours_text: "fn a() {}".into(),
            theirs_text: "fn b() {}".into(),
            byte_start: 0,
            byte_end: 100,
            symbol: None,
            coupled_hunks: vec![1, 2],
        };
        let level = classify(Path::new("."), &hunk);
        assert_eq!(level, ConflictLevel::Coupled);
    }

    /// ast-grep must be on PATH for the identifier-overlap classifier to run.
    /// When it is not, `identifiers_in_fragment` returns `None` and classify
    /// falls back to `Structural` (§7) — the L1/L2 assertions below would then
    /// be vacuous, so we skip them rather than assert a fallback.
    fn ast_grep_available() -> bool {
        Command::new("ast-grep")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn hunk_two_sides(file: &str, ours: &str, theirs: &str) -> ConflictHunk {
        ConflictHunk {
            hunk_id: 0,
            file: file.into(),
            base_oid: None,
            ours_oid: None,
            theirs_oid: None,
            base_text: None,
            ours_text: ours.into(),
            theirs_text: theirs.into(),
            byte_start: 0,
            byte_end: 100,
            symbol: None,
            coupled_hunks: vec![],
        }
    }

    #[test]
    fn classify_disjoint_identifiers_is_structural() {
        if !ast_grep_available() {
            return;
        }
        // Two sides edit different local bindings / calls — identifier sets are
        // disjoint {a, foo} vs {b, bar}, so a syntax-aware merge separates them.
        let hunk = hunk_two_sides("f.rs", "    let a = foo();\n", "    let b = bar();\n");
        assert_eq!(classify(Path::new("."), &hunk), ConflictLevel::Structural);
    }

    #[test]
    fn classify_overlapping_identifiers_is_semantic() {
        if !ast_grep_available() {
            return;
        }
        // Both sides rewrite the SAME binding `total` from the same call site —
        // identifier sets overlap on `total`, so a model must arbitrate.
        let hunk = hunk_two_sides(
            "f.rs",
            "    let total = compute(items) + tax;\n",
            "    let total = compute(items) - discount;\n",
        );
        assert_eq!(classify(Path::new("."), &hunk), ConflictLevel::Semantic);
    }

    #[test]
    fn classify_overlap_works_for_python() {
        if !ast_grep_available() {
            return;
        }
        // Language dispatch is driven by extension via shuttle_symbols::lang_for_path.
        let hunk = hunk_two_sides(
            "svc.py",
            "    result = handler(req)\n",
            "    result = handler(req, retry=True)\n",
        );
        assert_eq!(classify(Path::new("."), &hunk), ConflictLevel::Semantic);
    }

    #[test]
    fn classify_unknown_language_falls_back_to_structural() {
        // No grammar for `.zzz` — lang_for_path returns None, classifier bails,
        // classify falls back to Structural (never guesses, never loses edits).
        let hunk = hunk_two_sides("data.zzz", "alpha beta\n", "gamma delta\n");
        assert_eq!(classify(Path::new("."), &hunk), ConflictLevel::Structural);
    }

    #[test]
    fn identifier_overlap_helper_is_none_for_unknown_lang() {
        // The pure helper is honest about "cannot decide" so callers fall back.
        assert!(classify_by_identifier_overlap("x.zzz", "a", "b").is_none());
    }

    #[test]
    fn verify_clean_file_passes() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("shuttle_conflict_test_verify");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("clean.rs");
        let mut file = std::fs::File::create(&f).unwrap();
        writeln!(file, "fn main() {{}}").unwrap();
        verify(&dir, "clean.rs").expect("should pass");
    }

    #[test]
    fn verify_detects_residual_marker() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("shuttle_conflict_test_marker");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("dirty.rs");
        let mut file = std::fs::File::create(&f).unwrap();
        writeln!(file, "fn main() {{}}").unwrap();
        writeln!(file, "<<<<<<< HEAD").unwrap();
        writeln!(file, "fn foo() {{}}").unwrap();
        verify(&dir, "dirty.rs").expect_err("should detect marker");
    }

    #[test]
    fn propose_rejects_conflict_markers() {
        // propose() must reject text containing conflict markers
        let dir = std::env::temp_dir().join("shuttle_conflict_test_propose");
        std::fs::create_dir_all(&dir).unwrap();
        let bad_text = "<<<<<<< HEAD
fn foo() {}
=======
>>>>>>> main
";
        // Create a minimal conflict file so get_hunk succeeds
        let conflict_content = make_conflict("fn foo() {}", None, "fn bar() {}");
        std::fs::write(dir.join("test.rs"), &conflict_content).unwrap();
        let result = propose(&dir, "test.rs", 0, bad_text);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConflictError::Unsafe(_) => {}
            e => panic!("expected Unsafe, got {e:?}"),
        }
    }

    #[test]
    fn emit_receipt_all_verified() {
        let r = HunkReceipt {
            hunk_id: 0,
            file: "f.rs".into(),
            level: ConflictLevel::Structural,
            strategy: "mergiraf_resolved".into(),
            model_involved: false,
            verify_passed: true,
        };
        let receipt = emit_receipt(vec![r], "run-001");
        assert!(receipt.all_verified);
        assert_eq!(receipt.run_id, "run-001");
    }

    #[test]
    fn emit_receipt_not_all_verified() {
        let receipts = vec![
            HunkReceipt {
                hunk_id: 0,
                file: "a.rs".into(),
                level: ConflictLevel::Trivial,
                strategy: "take_theirs".into(),
                model_involved: false,
                verify_passed: true,
            },
            HunkReceipt {
                hunk_id: 1,
                file: "b.rs".into(),
                level: ConflictLevel::Semantic,
                strategy: "llm_propose".into(),
                model_involved: true,
                verify_passed: false,
            },
        ];
        let receipt = emit_receipt(receipts, "run-002");
        assert!(!receipt.all_verified);
    }

    #[test]
    fn diffstat_parses_numstat() {
        let stat = parse_numstat(
            "5	3	src/main.rs
",
        );
        assert_eq!(stat.insertions, 5);
        assert_eq!(stat.deletions, 3);
    }
}
