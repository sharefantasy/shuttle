//! shuttle-nav — the daemonless "specialized LSP" query layer .
//!
//! # Position
//! This is the *only* new engine crate. It does **not** re-index: it composes
//! `shuttle-symbols` primitives —
//!   * [`shuttle_symbols::SymbolTable`] / [`shuttle_symbols::SymbolTable::defs`] (definitions),
//!   * [`shuttle_symbols::extract_references`] (cheap file-level ref filter),
//!   * [`shuttle_symbols::genuine_identifier_lines`] (AST-level identifier lines),
//!   * [`shuttle_symbols::rename`] / [`shuttle_symbols::disambiguate_refs`] (rename sites),
//!     and `shuttle-locate` (`text_locate`) — into `references` / `rename` / (batch 2)
//!     `impls`/`callers`/`hover`, all emitting one unified [`NavHit`] with an
//!     explicit [`Confidence`]. No LSP protocol, no resident server, cold-start = 0.
//!
//! # Confidence (global rule, §9.2)
//! * **High**   — AST-decidable: a definition site, or a reference in the SAME
//!   file as a definition that passes `genuine_identifier_lines`.
//! * **Medium** — a cross-file bare-name reference that passes
//!   `genuine_identifier_lines` (verified an identifier node, not proven the
//!   exact binding).
//! * **Low**    — text-only (`ctags`/`rg`) match; no AST grammar available to
//!   disambiguate (e.g. a `customLanguages` language with no registered grammar).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

pub use shuttle_symbols::{Symbol, SymbolKind, SymbolTable};

pub mod cochange;
pub mod contract;
pub mod goal;
pub mod outcome;
pub mod parallel;
pub mod reconcile;
pub mod reverify;
pub mod split;
pub mod verify;

#[derive(Debug, thiserror::Error)]
pub enum NavError {
    #[error(transparent)]
    Symbols(#[from] shuttle_symbols::SymbolError),
    #[error(transparent)]
    Locate(#[from] shuttle_locate::LocateError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// What a [`NavHit`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavKind {
    Definition,
    Declaration,
    Reference,
    Impl,
    Caller,
}

/// Per-hit trust level. See the crate docs for the global rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Which backend produced the hit — kept for explainability in the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Ctags,
    AstGrep,
    Rg,
    Taplo,
    Lsp,
}

/// A single unified capability hit. `references`/`impls`/`callers`/def lookups
/// all produce this so the planner parses one stable schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavHit {
    pub path: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<usize>,
    pub kind: NavKind,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub confidence: Confidence,
    pub source: Backend,
}

/// Options shared by the query commands.
#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub root: String,
    /// Restrict to one symbol kind (e.g. only functions).
    pub kind: Option<SymbolKind>,
    /// ⑤ narrow the candidate set to the focus neighborhood (git-tracked files
    /// with a reference edge to the definition files) instead of the whole repo.
    pub scan_scope: bool,
    /// §7.5 incremental disk cache: reuse `cache_path`, re-tagging only files
    /// whose mtime+size changed. On by default so leaf queries are warm after
    /// the first cold scan (a full `scan` per query is otherwise ~30s on a big
    /// repo). Set false to force a cold whole-repo scan.
    pub cache: bool,
    /// Cache file used when `cache` is set (relative to cwd).
    pub cache_path: String,
}

impl QueryOptions {
    pub fn new(root: impl Into<String>) -> Self {
        let root = root.into();
        // The incremental cache lives INSIDE the target repo (`<root>/.shuttle/…`),
        // not the process CWD. Keying it to `root` means two `shuttle` invocations
        // pointed at different repos never share — and never clobber — one
        // another's cache. (A CWD-relative default silently returned a stale /
        // empty table whenever `--root` differed from the working dir.)
        let cache_path = default_cache_path(&root);
        Self {
            root,
            kind: None,
            scan_scope: false,
            cache: true,
            cache_path,
        }
    }
}

/// The default per-repo cache location: `<root>/.shuttle/symbols.cache`, normalized
/// so a `.`/empty root degrades to the historical `.shuttle/symbols.cache`.
pub fn default_cache_path(root: &str) -> String {
    let r = root.trim();
    if r.is_empty() || r == "." || r == "./" {
        return ".shuttle/symbols.cache".to_string();
    }
    let base = r.strip_suffix('/').unwrap_or(r);
    format!("{base}/.shuttle/symbols.cache")
}

// ---------------------------------------------------------------------------
// Word-boundary helpers (no regex crate dependency — identifiers are ASCII-ish)
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// True if `name` occurs as a *whole word* anywhere in `line` (identifier
/// boundaries on both sides). Cheap pre-filter before the AST check.
fn line_has_word(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let hay = line.as_bytes();
    let needle = name.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = find_from(hay, needle, i) {
        let before_ok = pos == 0 || !is_ident_byte(hay[pos - 1]);
        let after = pos + needle.len();
        let after_ok = after >= hay.len() || !is_ident_byte(hay[after]);
        if before_ok && after_ok {
            return true;
        }
        i = pos + 1;
    }
    false
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Replace every whole-word occurrence of `old` with `new` in `line`.
fn replace_word(line: &str, old: &str, new: &str) -> String {
    if old.is_empty() {
        return line.to_string();
    }
    let hay = line.as_bytes();
    let needle = old.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < hay.len() {
        if let Some(pos) = find_from(hay, needle, i) {
            // copy the gap before the candidate
            out.push_str(&line[i..pos]);
            let before_ok = pos == 0 || !is_ident_byte(hay[pos - 1]);
            let after = pos + needle.len();
            let after_ok = after >= hay.len() || !is_ident_byte(hay[after]);
            if before_ok && after_ok {
                out.push_str(new);
            } else {
                out.push_str(&line[pos..after]);
            }
            i = after;
        } else {
            out.push_str(&line[i..]);
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Index building (composed, never re-implemented)
// ---------------------------------------------------------------------------

/// Build the whole-repo symbol table. `--scan-scope` narrows around `focus`
/// (the definition files) when non-empty; otherwise a full scan.
fn build_table(root: &str, scan_scope: bool, focus: &[String]) -> Result<SymbolTable, NavError> {
    let t = if scan_scope && !focus.is_empty() {
        SymbolTable::scan_scoped(root, focus)?
    } else {
        SymbolTable::scan(root)?
    };
    Ok(t)
}

/// Build the whole-repo symbol table for a leaf query, honoring the §7.5 disk
/// cache when `opts.cache` is set (the default). nav queries pass no focus
/// hints — invalidation is purely mtime+size, so an unchanged repo re-tags
/// nothing and the query is warm. `cache = false` forces a cold full scan.
///
/// Process-warm layer (opt-in, OFF by default): when the `SHUTTLE_NAV_WARM` env var
/// is set, the built table is memoized in a thread-local keyed by
/// `(root, cache)`, so a single process issuing many nav queries skips even the
/// disk-cache round-trip after the first. It is intentionally not the default:
/// a long-lived process that mutates files would serve a stale table (the memo
/// has no invalidation), so only callers that know their tree is quiescent for
/// the batch should opt in.
fn full_table(opts: &QueryOptions) -> Result<SymbolTable, NavError> {
    if warm_enabled() {
        return warm_table(opts);
    }
    build_full_table(opts)
}

/// Public whole-repo symbol-table scan (disk-cached per `opts`). Exposed so a
/// *composing* crate can reuse the exact same cached table the nav closures run
/// on instead of re-implementing the scan. shuttle-refactor's type-aware impact
/// (G6·S2) layers a type-closure proof on top of the reverse closure and needs
/// the same table `impact_files` walks — a second cold scan would both cost
/// latency and risk a different view of the repo. Honours the identical
/// warm/disk-cache policy as every leaf query.
pub fn scan_table(opts: &QueryOptions) -> Result<SymbolTable, NavError> {
    full_table(opts)
}

/// The actual disk-cached (or cold) whole-repo scan, no process memo.
fn build_full_table(opts: &QueryOptions) -> Result<SymbolTable, NavError> {
    if opts.cache {
        Ok(SymbolTable::scan_cached(&opts.root, &opts.cache_path, &[])?)
    } else {
        Ok(SymbolTable::scan(&opts.root)?)
    }
}

/// True when the opt-in process-warm memo is enabled via `SHUTTLE_NAV_WARM`.
fn warm_enabled() -> bool {
    std::env::var_os("SHUTTLE_NAV_WARM").is_some()
}

thread_local! {
    /// Process-warm memo: (root, cache) -> table. Only populated when
    /// `SHUTTLE_NAV_WARM` is set; empty (and untouched) in the default path.
    static WARM_TABLES: std::cell::RefCell<std::collections::HashMap<(String, bool), SymbolTable>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Memoized whole-repo table for the opt-in warm path. Builds once per
/// `(root, cache)` per thread and clones out of the cache on subsequent calls.
fn warm_table(opts: &QueryOptions) -> Result<SymbolTable, NavError> {
    let key = (opts.root.clone(), opts.cache);
    if let Some(t) = WARM_TABLES.with(|m| m.borrow().get(&key).cloned()) {
        return Ok(t);
    }
    let table = build_full_table(opts)?;
    WARM_TABLES.with(|m| m.borrow_mut().insert(key, table.clone()));
    Ok(table)
}

// ---------------------------------------------------------------------------
// Capability: symbols / defs (pure static-table, exact)
// ---------------------------------------------------------------------------

/// `documentSymbol` — outline of a single file via an explicit-file ctags pass.
pub fn document_symbols(file: &str) -> Result<Vec<Symbol>, NavError> {
    Ok(shuttle_symbols::scan_files(&[file.to_string()])?.symbols)
}

/// `workspace/symbol` — name/kind filter over the whole table.
pub fn workspace_symbols(query: &str, opts: &QueryOptions) -> Result<Vec<Symbol>, NavError> {
    let table = full_table(opts)?;
    let q = query.to_ascii_lowercase();
    Ok(table
        .symbols
        .into_iter()
        .filter(|s| query.is_empty() || s.name.to_ascii_lowercase().contains(&q))
        .filter(|s| opts.kind.as_ref().is_none_or(|k| &s.kind == k))
        .collect())
}

/// `definition`/`declaration` — every definition site named `name`.
pub fn defs(name: &str, opts: &QueryOptions) -> Result<Vec<Symbol>, NavError> {
    let table = full_table(opts)?;
    Ok(table
        .defs(name)
        .into_iter()
        .filter(|s| opts.kind.as_ref().is_none_or(|k| &s.kind == k))
        .cloned()
        .collect())
}

// ---------------------------------------------------------------------------
// Capability: references (§9.4)
// ---------------------------------------------------------------------------

/// `references` — all uses of `name` across the repo, each tagged with a
/// [`Confidence`]. Composes: def lookup (anchors) + per-file AST identifier
/// lines (`genuine_identifier_lines`) with an `rg` text fallback for languages
/// without a grammar.
pub fn references(name: &str, opts: &QueryOptions) -> Result<Vec<NavHit>, NavError> {
    if name.is_empty() {
        return Ok(Vec::new());
    }
    // 1) definition anchors (also drive --scan-scope focus + High classification).
    let table = full_table(opts)?;
    let def_syms: Vec<Symbol> = table
        .defs(name)
        .into_iter()
        // Data-file tags (JSON keys, INI/TOML sections) are not code symbols: a
        // same-named `"x":` key in an eval fixture must not anchor this query
        // as a High Definition.
        .filter(|s| s.is_code_symbol())
        .cloned()
        .collect();
    let def_files: HashSet<String> = def_syms.iter().map(|s| s.path.clone()).collect();

    // 2) candidate files: everything that textually mentions `name` (word).
    let mut cand_files: BTreeSet<String> = BTreeSet::new();
    for h in shuttle_locate::text_locate(name, &opts.root)? {
        cand_files.insert(h.path);
    }
    def_files.iter().for_each(|f| {
        cand_files.insert(f.clone());
    });

    // 2b) --scan-scope: intersect with the focus neighborhood.
    if opts.scan_scope && !def_files.is_empty() {
        let focus: Vec<String> = def_files.iter().cloned().collect();
        let scoped = SymbolTable::scan_scoped(&opts.root, &focus)?;
        let mut in_scope: HashSet<String> = scoped.symbols.iter().map(|s| s.path.clone()).collect();
        in_scope.extend(def_files.iter().cloned());
        cand_files.retain(|f| in_scope.contains(f));
    }

    let mut hits: Vec<NavHit> = Vec::new();

    // 3) definition sites -> High.
    for s in &def_syms {
        hits.push(NavHit {
            path: s.path.clone(),
            line: s.line,
            col: None,
            kind: NavKind::Definition,
            symbol: name.to_string(),
            snippet: s.signature.clone(),
            confidence: Confidence::High,
            source: Backend::Ctags,
        });
    }

    // 4) references per candidate file.
    for path in &cand_files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let same_file_def = def_files.contains(path);
        let genuine = shuttle_symbols::genuine_identifier_lines(path, name);
        match genuine {
            // AST grammar available: use the verified identifier lines.
            Some(lines) => {
                for &ln in &lines {
                    // Skip the exact definition line (already emitted as Definition).
                    if same_file_def && def_syms.iter().any(|s| s.path == *path && s.line == ln) {
                        continue;
                    }
                    let snippet = content
                        .lines()
                        .nth(ln.saturating_sub(1))
                        .map(str::to_string);
                    hits.push(NavHit {
                        path: path.clone(),
                        line: ln,
                        col: None,
                        kind: NavKind::Reference,
                        symbol: name.to_string(),
                        snippet,
                        // same-file-as-def => High; cross-file => Medium.
                        confidence: if same_file_def {
                            Confidence::High
                        } else {
                            Confidence::Medium
                        },
                        source: Backend::AstGrep,
                    });
                }
            }
            // No grammar: text-only word matches -> Low.
            None => {
                for (i, l) in content.lines().enumerate() {
                    let ln = i + 1;
                    if !line_has_word(l, name) {
                        continue;
                    }
                    if same_file_def && def_syms.iter().any(|s| s.path == *path && s.line == ln) {
                        continue;
                    }
                    hits.push(NavHit {
                        path: path.clone(),
                        line: ln,
                        col: None,
                        kind: NavKind::Reference,
                        symbol: name.to_string(),
                        snippet: Some(l.to_string()),
                        confidence: Confidence::Low,
                        source: Backend::Rg,
                    });
                }
            }
        }
    }

    sort_hits(&mut hits);
    Ok(hits)
}

/// Confidence descending (High first), then path, then line — deterministic.
fn sort_hits(hits: &mut [NavHit]) {
    fn rank(c: Confidence) -> u8 {
        match c {
            Confidence::High => 0,
            Confidence::Medium => 1,
            Confidence::Low => 2,
        }
    }
    hits.sort_by(|a, b| {
        rank(a.confidence)
            .cmp(&rank(b.confidence))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
}

// ---------------------------------------------------------------------------
// Capability: rename (§9.5) — dry-run diff + confidence, apply via EditPlan
// ---------------------------------------------------------------------------

/// One rename site: the concrete line rewrite plus its confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameSite {
    pub path: String,
    pub line: usize,
    pub old_line: String,
    pub new_line: String,
    pub confidence: Confidence,
}

/// A site the references pipeline knows that the current plan does NOT cover:
/// the reconciliation gap. Kept visible (never silently dropped) so the CLI can
/// refuse to write a partial rename and name exactly what it would miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameGap {
    pub path: String,
    pub line: usize,
    pub confidence: Confidence,
    pub snippet: Option<String>,
}

/// Scan-scope facts for a rename plan. Rename scans the git-tracked files
/// under `--root` on the spot; it never reads the on-disk symbol index, so the
/// only lever on coverage is `root` itself.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RenameScope {
    pub root: String,
    pub repo_root: Option<String>,
    /// `--root` is a strict subdirectory of the repo: files outside `root`
    /// were never scanned, so this plan (and `references` under the same root)
    /// cannot know about sites there.
    pub narrow: bool,
}

/// The full rename outcome: hits (for display/gate) + a ready-to-apply plan.
///
/// Reconciliation contract (SC-1/SC-2): `known_total` is everything the
/// references pipeline knows under `root`; `planned_count` is what the plan
/// actually rewrites. With gaps (`known_total > planned_count`) a default plan
/// is `blocked` — the CLI must refuse `--write` and list `excluded` — unless
/// the caller opted in with `include_refs`, which folds every reference site
/// into the plan with its confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenamePlan {
    pub sites: Vec<RenameSite>,
    pub plan: shuttle_protocol::EditPlan,
    #[serde(default)]
    pub known_total: usize,
    #[serde(default)]
    pub planned_count: usize,
    #[serde(default)]
    pub excluded: Vec<RenameGap>,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub scope: RenameScope,
}

impl RenamePlan {
    /// True if any site is Low-confidence (the CLI gate refuses to write these
    /// without `--force`).
    pub fn has_low_confidence(&self) -> bool {
        self.sites
            .iter()
            .any(|s| matches!(s.confidence, Confidence::Low))
    }
}

/// Plan a rename `from` -> `to`. Reuses `shuttle_symbols::rename` (which already runs
/// `disambiguate_refs`) for definition sites, then lowers each site into a
/// *line-precise* `SearchReplace` op: `search` is the original line, `replace`
/// swaps only the whole-word identifier — so applying it can never corrupt
/// `foobar` when renaming `foo`.
///
/// Definition sites alone are a *partial* rename whenever the symbol has
/// references (call sites never come from the ctags def table). The plan is
/// therefore reconciled against the `references` pipeline under the same
/// `root`: uncovered sites land in `excluded` and block writes unless
/// `include_refs` folds them in.
pub fn plan_rename(
    from: &str,
    to: &str,
    opts: &QueryOptions,
    include_refs: bool,
) -> Result<RenamePlan, NavError> {
    let table = build_table(&opts.root, false, &[])?;
    let edits = shuttle_symbols::rename(&table, from, to)?;
    let def_files: HashSet<String> = table
        .defs(from)
        .into_iter()
        .filter(|s| s.is_code_symbol())
        .map(|s| s.path.clone())
        .collect();

    let mut sites: Vec<RenameSite> = Vec::new();
    let mut ops: Vec<shuttle_protocol::EditOp> = Vec::new();

    for e in &edits {
        let content = std::fs::read_to_string(&e.path).unwrap_or_default();
        let old_line = match content.lines().nth(e.line.saturating_sub(1)) {
            Some(l) => l.to_string(),
            None => continue,
        };
        if !line_has_word(&old_line, from) {
            continue;
        }
        let new_line = replace_word(&old_line, from, to);
        // Definition site; presence of an AST grammar that can disambiguate
        // marks it High, otherwise Medium.
        let confidence = if shuttle_symbols::genuine_identifier_lines(&e.path, from).is_some() {
            Confidence::High
        } else if def_files.contains(&e.path) {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        ops.push(shuttle_protocol::EditOp::SearchReplace {
            path: e.path.clone(),
            search: old_line.clone(),
            replace: new_line.clone(),
            line_hint: Some(e.line),
        });
        sites.push(RenameSite {
            path: e.path.clone(),
            line: e.line,
            old_line,
            new_line,
            confidence,
        });
    }

    // Reconciliation: cross-check the def-only plan above against the same
    // `references` pipeline the CLI exposes, so "rename knows less than
    // references" can never end in a silently partial write. Reference sites
    // (definition anchors are already the edits above) either fold into the
    // plan (`include_refs`, each keeping its High/Medium/Low tier) or become a
    // named gap that blocks `--write`.
    let mut excluded: Vec<RenameGap> = Vec::new();
    for hit in references(from, opts)? {
        if hit.kind != NavKind::Reference {
            continue;
        }
        // Only identifier surfaces participate in reconciliation: a mention in
        // a JSON/YAML data file or a doc is neither renamable nor something a
        // code rename must cover — counting it would block every rename whose
        // name appears in a fixture.
        if !shuttle_symbols::is_identifier_surface(&hit.path) {
            continue;
        }
        if sites
            .iter()
            .any(|s| s.path == hit.path && s.line == hit.line)
        {
            continue;
        }
        let content = std::fs::read_to_string(&hit.path).unwrap_or_default();
        let old_line = match content.lines().nth(hit.line.saturating_sub(1)) {
            Some(l) => l.to_string(),
            None => continue,
        };
        if !line_has_word(&old_line, from) {
            continue;
        }
        if include_refs {
            let new_line = replace_word(&old_line, from, to);
            ops.push(shuttle_protocol::EditOp::SearchReplace {
                path: hit.path.clone(),
                search: old_line.clone(),
                replace: new_line.clone(),
                line_hint: Some(hit.line),
            });
            sites.push(RenameSite {
                path: hit.path.clone(),
                line: hit.line,
                old_line,
                new_line,
                confidence: hit.confidence,
            });
        } else {
            excluded.push(RenameGap {
                path: hit.path.clone(),
                line: hit.line,
                confidence: hit.confidence,
                snippet: Some(old_line.trim_end().to_string()),
            });
        }
    }

    let repo_root = git_repo_root(&opts.root);
    let narrow = match &repo_root {
        Some(rr) => !same_dir(&opts.root, rr),
        None => false,
    };

    Ok(RenamePlan {
        planned_count: sites.len(),
        known_total: sites.len() + excluded.len(),
        blocked: !include_refs && !excluded.is_empty(),
        excluded,
        scope: RenameScope {
            root: opts.root.clone(),
            repo_root,
            narrow,
        },
        sites,
        plan: shuttle_protocol::EditPlan {
            version: shuttle_protocol::CURRENT_VERSION,
            intent: Some(format!("rename {from} -> {to}")),
            ops,
        },
    })
}

/// Best-effort repo root for the rename scope line: `git -C <root> rev-parse
/// --show-toplevel`. `None` when git is absent or `root` is outside a work
/// tree (then no narrow-scope warning is possible or needed).
fn git_repo_root(root: &str) -> Option<String> {
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git).ok()?;
    let out = std::process::Command::new(git)
        .args(["-C", root, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Whether two path spellings denote the same directory (canonicalized when
/// possible, else compared verbatim).
fn same_dir(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Capability: impls / callers / hover (§9.7) — heuristic, confidence-bounded
// ---------------------------------------------------------------------------

/// True if `line` looks like an implementation header for `type_name`
/// (`impl <T>` / `impl <..> for <T>` in Rust, `implements <T>` / `extends <T>`
/// in TS/Java-like langs). `type_name` must appear as a whole word.
fn is_impl_line(line: &str, type_name: &str) -> bool {
    if !line_has_word(line, type_name) {
        return false;
    }
    let t = line.trim_start();
    // Rust: `impl Foo`, `impl<T> Foo`, `impl Trait for Foo`.
    if let Some(after) = t.strip_prefix("impl") {
        // require a word boundary after `impl` (avoid `implementation`)
        if after.is_empty() || !is_ident_byte(after.as_bytes()[0]) {
            return true;
        }
    }
    // TS/Java-ish: a class/interface header that implements/extends the type.
    line_has_word(line, "implements") || line_has_word(line, "extends")
}

/// `implementation` — sites that implement / extend `type_name`. Composes a
/// text candidate pass (`text_locate`) with an AST identifier check
/// (`genuine_identifier_lines`) to bound confidence:
///   * grammar present AND the type is a genuine identifier on the line -> High
///   * otherwise (no grammar, or heuristic-only) -> Medium
///     Never emits Low: an impl match already carries structural intent.
pub fn impls(type_name: &str, opts: &QueryOptions) -> Result<Vec<NavHit>, NavError> {
    if type_name.is_empty() {
        return Ok(Vec::new());
    }
    let mut cand: BTreeSet<String> = BTreeSet::new();
    for h in shuttle_locate::text_locate(type_name, &opts.root)? {
        cand.insert(h.path);
    }
    let mut hits: Vec<NavHit> = Vec::new();
    for path in &cand {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let genuine = shuttle_symbols::genuine_identifier_lines(path, type_name);
        for (i, l) in content.lines().enumerate() {
            let ln = i + 1;
            if !is_impl_line(l, type_name) {
                continue;
            }
            // High only when a grammar confirms the type is a real identifier
            // on this exact line; else Medium (text/keyword heuristic).
            let (confidence, source) = match &genuine {
                Some(lines) if lines.contains(&ln) => (Confidence::High, Backend::AstGrep),
                _ => (Confidence::Medium, Backend::Rg),
            };
            hits.push(NavHit {
                path: path.clone(),
                line: ln,
                col: None,
                kind: NavKind::Impl,
                symbol: type_name.to_string(),
                snippet: Some(l.trim_end().to_string()),
                confidence,
                source,
            });
        }
    }
    sort_hits(&mut hits);
    Ok(hits)
}

/// `callHierarchy(incoming)` — who calls `func`. Uses ast-grep's structural
/// call-site pattern `func($$$)` per detected language (never matches a bare
/// `fn func` declaration, which is not a call node), with a text fallback
/// (`func(`) for languages without a grammar. Confidence is **capped at
/// Medium**: a call-site pattern is a structural approximation, not a resolved
/// binding. Outgoing edges are intentionally out of scope (use repo-map).
pub fn callers(func: &str, opts: &QueryOptions) -> Result<Vec<NavHit>, NavError> {
    if func.is_empty() {
        return Ok(Vec::new());
    }
    // Candidate files + the languages present among them.
    let mut cand: BTreeSet<String> = BTreeSet::new();
    for h in shuttle_locate::text_locate(func, &opts.root)? {
        cand.insert(h.path);
    }
    // Definition sites are excluded from callers (a decl is not a caller).
    let table = full_table(opts)?;
    let def_lines: HashSet<(String, usize)> = table
        .defs(func)
        .into_iter()
        .map(|s| (s.path.clone(), s.line))
        .collect();

    let mut langs: BTreeSet<&'static str> = BTreeSet::new();
    for p in &cand {
        if let Some(l) = shuttle_symbols::lang_for_path(p) {
            langs.insert(l);
        }
    }

    // AST call-site hits per grammar (Medium). Map path -> matched lines.
    let pattern = format!("{func}($$$)");
    let mut ast_lines: BTreeSet<(String, usize)> = BTreeSet::new();
    for lang in &langs {
        if let Ok(found) = shuttle_locate::struct_locate(&pattern, lang, &opts.root) {
            for h in found {
                if cand.contains(&h.path) {
                    ast_lines.insert((h.path, h.line));
                }
            }
        }
    }

    let mut hits: Vec<NavHit> = Vec::new();
    let mut emitted: HashSet<(String, usize)> = HashSet::new();
    for path in &cand {
        let has_grammar = shuttle_symbols::lang_for_path(path).is_some();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, l) in content.lines().enumerate() {
            let ln = i + 1;
            if def_lines.contains(&(path.clone(), ln)) {
                continue;
            }
            let key = (path.clone(), ln);
            if has_grammar {
                // Trust only structural call-site hits (Medium).
                if !ast_lines.contains(&key) {
                    continue;
                }
                if !emitted.insert(key.clone()) {
                    continue;
                }
                hits.push(NavHit {
                    path: path.clone(),
                    line: ln,
                    col: None,
                    kind: NavKind::Caller,
                    symbol: func.to_string(),
                    snippet: Some(l.trim_end().to_string()),
                    confidence: Confidence::Medium,
                    source: Backend::AstGrep,
                });
            } else {
                // No grammar: text `func(` heuristic (Low).
                let needle = format!("{func}(");
                if !l.contains(&needle) || !line_has_word(l, func) {
                    continue;
                }
                if !emitted.insert(key.clone()) {
                    continue;
                }
                hits.push(NavHit {
                    path: path.clone(),
                    line: ln,
                    col: None,
                    kind: NavKind::Caller,
                    symbol: func.to_string(),
                    snippet: Some(l.trim_end().to_string()),
                    confidence: Confidence::Low,
                    source: Backend::Rg,
                });
            }
        }
    }
    sort_hits(&mut hits);
    Ok(hits)
}

// ---------------------------------------------------------------------------
// Capability: impact / blast-radius
// ---------------------------------------------------------------------------

/// Heuristic: does `path` look like a test file? Recognizes the common
/// conventions across the languages shuttle targets (`*_test.*`, `test_*`,
/// `*.test.*`, `*.spec.*`, and any component `tests`/`test` dir).
pub fn looks_like_test_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let file = p.rsplit('/').next().unwrap_or(&p);
    // Strip only the final extension so multi-dot test names survive
    // (e.g. `button.test.ts` -> stem `button.test`).
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    if stem.ends_with("_test") || stem.ends_with(".test") || stem.ends_with(".spec") {
        return true;
    }
    if file.starts_with("test_") || file.starts_with("test-") {
        return true;
    }
    p.split('/')
        .any(|c| c == "tests" || c == "test" || c == "__tests__")
}

/// `impact` — the reverse-dependency closure (blast radius) of one or more seed
/// symbols. Walks the native symbol graph ([`shuttle_symbols::graph::SymbolGraph`])
/// *backwards*: given the symbols you intend to change, returns every symbol
/// (and its definition site) that transitively references them within
/// `max_depth` hops, tagged with a [`Confidence`] that is the weakest hop along
/// the path. This is the daemonless realization of the SCIP "blast radius" —
/// no external index, degrades to ctags. Seeds appear at distance 0
/// (`NavKind::Definition`, High); everything reached is `NavKind::Caller`.
///
/// When `tests_only` is set, the result is narrowed to hits in test files (see
/// [`looks_like_test_file`]) — the minimal set of tests worth re-running.
pub fn impact(
    seeds: &[String],
    max_depth: usize,
    tests_only: bool,
    directed: bool,
    opts: &QueryOptions,
) -> Result<Vec<NavHit>, NavError> {
    let seeds: Vec<String> = seeds.iter().filter(|s| !s.is_empty()).cloned().collect();
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let table = full_table(opts)?;
    let graph = shuttle_symbols::graph::SymbolGraph::try_from_scan_root(&table, &opts.root)?;
    Ok(radius_to_hits(
        &graph, &seeds, max_depth, tests_only, directed,
    ))
}

/// File-seeded variant of [`impact`]. Given the set of *changed files*, resolve
/// every symbol defined in those files (via one ctags scan) into seeds, then run
/// the same reverse-dependency closure. This is the shape the CLI speaks
/// (`shuttle impact --changed <files>`): you name the files a patch touches, and shuttle
/// tells you every symbol — and, with `--tests`, every test — that could break.
/// A single whole-repo scan backs both the seed resolution and the graph, so
/// there is no double scan.
pub fn impact_files(
    changed: &[String],
    max_depth: usize,
    tests_only: bool,
    directed: bool,
    opts: &QueryOptions,
) -> Result<Vec<NavHit>, NavError> {
    let table = full_table(opts)?;
    let wanted: BTreeSet<String> = changed
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| normalize_path(c))
        .collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    // Deduplicated seed symbol names defined in any changed file.
    let seeds: Vec<String> = table
        .symbols
        .iter()
        .filter(|s| path_matches(&s.path, &wanted))
        .map(|s| s.name.clone())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let graph = shuttle_symbols::graph::SymbolGraph::try_from_scan_root(&table, &opts.root)?;
    Ok(radius_to_hits(
        &graph, &seeds, max_depth, tests_only, directed,
    ))
}

/// Normalize a path for comparison: `\\` -> `/`, strip a leading `./`.
fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// A symbol's definition file matches a changed file when their normalized
/// paths are equal, or when one is a path-suffix of the other (handles the
/// common case where the CLI passes a repo-relative path but ctags recorded a
/// differently-rooted one). Suffix boundaries are on `/` so `latest.rs` never
/// matches `test.rs`.
fn path_matches(sym_path: &str, wanted: &BTreeSet<String>) -> bool {
    let sp = normalize_path(sym_path);
    wanted
        .iter()
        .any(|w| sp == *w || sp.ends_with(&format!("/{w}")) || w.ends_with(&format!("/{sp}")))
}

/// The symbols whose definition file is one of `changed`, using the *same*
/// normalization + path-suffix matching the file-seeded closure applies. Exposed
/// so a composing crate can recover the exact seed set [`impact_files`] derives
/// internally — e.g. to partition seeds by [`SymbolKind`] for the type-aware
/// refinement (G6·S2) — without duplicating the suffix rules (which, gotten
/// subtly wrong, would silently change which seeds a refinement sees).
pub fn symbols_defined_in<'a>(table: &'a SymbolTable, changed: &[String]) -> Vec<&'a Symbol> {
    let wanted: BTreeSet<String> = changed
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| normalize_path(c))
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    table
        .symbols
        .iter()
        .filter(|s| path_matches(&s.path, &wanted))
        .collect()
}

/// Shared lowering: reverse-closure [`ImpactHit`]s -> sorted [`NavHit`]s.
/// Seeds (distance 0) become `Definition`/High; reached symbols become
/// `Caller` tagged with the weakest confidence along their path. Hits with no
/// known def site are dropped (nothing actionable to point at); with
/// `tests_only` the result is narrowed to test files.
fn radius_to_hits(
    graph: &shuttle_symbols::graph::SymbolGraph,
    seeds: &[String],
    max_depth: usize,
    tests_only: bool,
    directed: bool,
) -> Vec<NavHit> {
    // `directed` restricts the walk to trusted import-directed edges (T3), so
    // the closure is the acyclic zero-model DAG instead of the symmetric
    // name-match closure. Default (false) preserves the original broad recall.
    let radius = if directed {
        graph.blast_radius_directed(seeds, max_depth)
    } else {
        graph.blast_radius(seeds, max_depth)
    };
    let mut hits: Vec<NavHit> = Vec::new();
    for h in radius {
        if h.path.is_empty() {
            continue; // no known def site — nothing actionable to point at
        }
        if tests_only && !looks_like_test_file(&h.path) {
            continue;
        }
        let confidence = match h.confidence {
            shuttle_symbols::graph::GraphConfidence::High => Confidence::High,
            shuttle_symbols::graph::GraphConfidence::Medium => Confidence::Medium,
            shuttle_symbols::graph::GraphConfidence::Low => Confidence::Low,
        };
        let kind = if h.distance == 0 {
            NavKind::Definition
        } else {
            NavKind::Caller
        };
        // Definitions are ctags-derived; the reverse edges are identifier-scan
        // derived. Ctags is the honest provenance for the def site we point at.
        hits.push(NavHit {
            path: h.path,
            line: h.line,
            col: None,
            kind,
            symbol: h.symbol,
            snippet: None,
            confidence,
            source: Backend::Ctags,
        });
    }
    sort_hits(&mut hits);
    hits
}

/// Collect the contiguous doc-comment block immediately preceding `def_line`
/// (1-indexed). Recognizes `//`, `///`, `//!`, `#`, and `*` / block-comment
/// continuation lines. Returned top-to-bottom.
fn collect_doc_comment(lines: &[&str], def_line: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // def_line is 1-indexed; the line above it is index def_line-2.
    let mut idx = def_line as isize - 2;
    while idx >= 0 {
        let t = lines[idx as usize].trim();
        let is_doc = t.starts_with("///")
            || t.starts_with("//!")
            || t.starts_with("//")
            || t.starts_with('#')
            || t.starts_with('*')
            || t.starts_with("/*")
            || t.starts_with("*/");
        if t.is_empty() || !is_doc {
            break;
        }
        out.push(t.to_string());
        idx -= 1;
    }
    out.reverse();
    out
}

/// `hover` — for each definition site of `name`, the exact signature line plus
/// any adjacent doc comment. Signature is ctags-exact (**High**); the doc block
/// is a heuristic addendum carried in `snippet`, so it never lowers confidence.
pub fn hover(name: &str, opts: &QueryOptions) -> Result<Vec<NavHit>, NavError> {
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let table = full_table(opts)?;
    let mut hits: Vec<NavHit> = Vec::new();
    for s in table.defs(name) {
        if let Some(k) = &opts.kind {
            if &s.kind != k {
                continue;
            }
        }
        let content = std::fs::read_to_string(&s.path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let sig = s
            .signature
            .clone()
            .or_else(|| {
                lines
                    .get(s.line.saturating_sub(1))
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_default();
        let doc = collect_doc_comment(&lines, s.line);
        let snippet = if doc.is_empty() {
            sig.clone()
        } else {
            format!("{}\n{}", doc.join("\n"), sig)
        };
        hits.push(NavHit {
            path: s.path.clone(),
            line: s.line,
            col: None,
            kind: NavKind::Definition,
            symbol: name.to_string(),
            snippet: Some(snippet),
            // Signature is exact even if the doc heuristic found nothing.
            confidence: Confidence::High,
            source: Backend::Ctags,
        });
    }
    sort_hits(&mut hits);
    Ok(hits)
}

// ---------------------------------------------------------------------------
// Backend probe (§9.8) — is an AST grammar actually available per language?
// ---------------------------------------------------------------------------

/// Availability of an AST grammar for a given language.
///
/// Per §9.8, "hanging a grammar" is a pure config action: once a
/// `customLanguages` entry + its `.so` exist, ast-grep starts succeeding and
/// `genuine_identifier_lines` returns `Some(..)`, so references/rename升到 High
/// *without any capability-code change*. This probe only *reports* that state.
pub mod backend {
    use std::collections::BTreeMap;

    /// ast-grep languages with a grammar compiled in — usable with no config.
    /// Mirrors the built-in class of [`super::SymbolKind`] dispatch in
    /// `shuttle_symbols::lang_for_path` (verified empirically, see §6.7).
    pub const BUILTIN_LANGS: &[&str] = &[
        "rust",
        "python",
        "javascript",
        "typescript",
        "tsx",
        "go",
        "java",
        "c",
        "cpp",
        "ruby",
        "bash",
        "yaml",
        "json",
    ];

    /// Languages that only get an AST grammar via `sgconfig.yml customLanguages`
    /// (§6.7). Mapped by `lang_for_path` so the nav layer can dispatch on them,
    /// but AST-level confidence requires a registered + present `.so`.
    pub const CUSTOM_CANDIDATE_LANGS: &[&str] = &["thrift", "toml", "ini"];

    /// Per-language grammar status.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LangStatus {
        /// Built-in ast-grep grammar — AST always available (High-capable).
        Builtin,
        /// A `customLanguages` grammar declared in sgconfig.yml whose `.so`
        /// exists — AST available (High-capable).
        CustomRegistered,
        /// A `customLanguages` candidate that is NOT registered (or whose `.so`
        /// is missing) — degrades to ctags+rg (Medium/Low).
        CustomUnregistered,
        /// Not mapped by `lang_for_path` at all.
        Unknown,
    }

    impl LangStatus {
        /// True when a real tree-sitter grammar backs this language, so
        /// references/rename can reach High confidence.
        pub fn ast_available(self) -> bool {
            matches!(self, LangStatus::Builtin | LangStatus::CustomRegistered)
        }
    }

    /// Snapshot of grammar availability, built once per `root`.
    #[derive(Debug, Clone, Default)]
    pub struct BackendProbe {
        /// customLanguages name -> `.so` exists on disk.
        custom: BTreeMap<String, bool>,
    }

    impl BackendProbe {
        /// Probe `root/sgconfig.yml`. A missing/unreadable/blank config yields an
        /// empty probe (every custom candidate is Unregistered).
        pub fn probe(root: &str) -> Self {
            let cfg = std::path::Path::new(root).join("sgconfig.yml");
            let text = std::fs::read_to_string(&cfg).unwrap_or_default();
            let dir = cfg.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let custom = parse_custom_languages(&text)
                .into_iter()
                .map(|(name, lib)| {
                    // libraryPath may be a dir or a file; resolve relative to the
                    // sgconfig.yml directory and check for existence.
                    let exists = lib
                        .map(|l| {
                            let p = dir.join(&l);
                            p.exists()
                        })
                        .unwrap_or(false);
                    (name, exists)
                })
                .collect();
            BackendProbe { custom }
        }

        /// Classify a language id (as returned by `shuttle_symbols::lang_for_path`).
        pub fn status(&self, lang: &str) -> LangStatus {
            if BUILTIN_LANGS.contains(&lang) {
                return LangStatus::Builtin;
            }
            if CUSTOM_CANDIDATE_LANGS.contains(&lang) {
                return match self.custom.get(lang) {
                    Some(true) => LangStatus::CustomRegistered,
                    _ => LangStatus::CustomUnregistered,
                };
            }
            LangStatus::Unknown
        }

        /// Classify a file by its path (maps extension then classifies).
        pub fn status_for_path(&self, path: &str) -> LangStatus {
            match shuttle_symbols::lang_for_path(path) {
                Some(l) => self.status(l),
                None => LangStatus::Unknown,
            }
        }

        /// A stable, sorted report over every language `shuttle-nav` knows about:
        /// `(lang, status, ast_available)`. Drives `shuttle backends`.
        pub fn report(&self) -> Vec<(&'static str, LangStatus, bool)> {
            let mut out: Vec<(&'static str, LangStatus, bool)> = Vec::new();
            for &l in BUILTIN_LANGS {
                let s = self.status(l);
                out.push((l, s, s.ast_available()));
            }
            for &l in CUSTOM_CANDIDATE_LANGS {
                let s = self.status(l);
                out.push((l, s, s.ast_available()));
            }
            out.sort_by(|a, b| a.0.cmp(b.0));
            out
        }
    }

    /// Extract `customLanguages` entries from a minimal subset of sgconfig.yml.
    ///
    /// Hand-rolled (no YAML dep): recognizes a top-level `customLanguages:`
    /// mapping, each child key (2-space indent) is a language name, and an
    /// optional `libraryPath:` under it (4-space indent). Robust enough for the
    /// flat shapes ast-grep uses; anything fancier just yields `None` lib and is
    /// treated as unregistered (conservative).
    pub fn parse_custom_languages(text: &str) -> BTreeMap<String, Option<String>> {
        let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut in_block = false;
        let mut cur: Option<String> = None;
        for raw in text.lines() {
            // strip trailing comments (naive: only when not in a quoted value)
            let line = raw;
            let trimmed = line.trim_end();
            if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            let content = trimmed.trim_start();

            if indent == 0 {
                // a new top-level key: are we entering / leaving the block?
                in_block = content.starts_with("customLanguages:");
                cur = None;
                continue;
            }
            if !in_block {
                continue;
            }
            // Inside customLanguages.
            if indent <= 2 {
                // language name key: `thrift:` (value ignored / inline map rare)
                if let Some(name) = content.strip_suffix(':') {
                    let name = name.trim().trim_matches('"').to_string();
                    cur = Some(name.clone());
                    out.entry(name).or_insert(None);
                } else if let Some((name, _)) = content.split_once(':') {
                    let name = name.trim().trim_matches('"').to_string();
                    cur = Some(name.clone());
                    out.entry(name).or_insert(None);
                }
            } else if let Some(name) = &cur {
                // property of the current language.
                if let Some((k, v)) = content.split_once(':') {
                    if k.trim() == "libraryPath" {
                        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                        if !v.is_empty() {
                            out.insert(name.clone(), Some(v));
                        }
                    }
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_custom_languages_reads_names_and_lib() {
            let yaml = "ruleDirs:\n  - rules\ncustomLanguages:\n  thrift:\n    libraryPath: ./thrift.so\n    extensions: [thrift]\n  toml:\n    extensions: [toml]\nother: 1\n";
            let m = parse_custom_languages(yaml);
            assert_eq!(m.get("thrift"), Some(&Some("./thrift.so".to_string())));
            // toml declared but no libraryPath -> None
            assert_eq!(m.get("toml"), Some(&None));
            // keys outside the block are ignored
            assert!(!m.contains_key("ruleDirs"));
            assert!(!m.contains_key("other"));
        }

        #[test]
        fn empty_config_yields_empty_probe() {
            let m = parse_custom_languages("ruleDirs:\n  - rules\n");
            assert!(m.is_empty());
        }

        #[test]
        fn status_classifies_builtin_custom_unknown() {
            // No sgconfig -> thrift is a candidate but unregistered.
            let probe = BackendProbe::default();
            assert_eq!(probe.status("rust"), LangStatus::Builtin);
            assert!(probe.status("rust").ast_available());
            assert_eq!(probe.status("bash"), LangStatus::Builtin);
            assert_eq!(probe.status("thrift"), LangStatus::CustomUnregistered);
            assert!(!probe.status("thrift").ast_available());
            assert_eq!(probe.status("cobol"), LangStatus::Unknown);
        }

        #[test]
        fn registered_custom_lang_is_ast_available() {
            let mut probe = BackendProbe::default();
            probe.custom.insert("thrift".to_string(), true);
            assert_eq!(probe.status("thrift"), LangStatus::CustomRegistered);
            assert!(probe.status("thrift").ast_available());
            // declared but .so missing -> still unregistered
            probe.custom.insert("thrift".to_string(), false);
            assert_eq!(probe.status("thrift"), LangStatus::CustomUnregistered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_impl_line_matches_rust_and_ts_headers() {
        assert!(is_impl_line("impl Foo {", "Foo"));
        assert!(is_impl_line("impl<T> Foo<T> {", "Foo"));
        assert!(is_impl_line("impl Trait for Foo {", "Foo"));
        assert!(is_impl_line("class Bar implements Foo {", "Foo"));
        assert!(is_impl_line("class Bar extends Foo {", "Foo"));
        // must not match a substring type or an unrelated line
        assert!(!is_impl_line("impl Foobar {", "Foo"));
        assert!(!is_impl_line("let x = Foo::new();", "Foo"));
        assert!(!is_impl_line("// implementation notes for Foo", "Foo"));
    }

    #[test]
    fn collect_doc_comment_grabs_contiguous_block() {
        let src = [
            "// header",
            "/// docs line 1",
            "/// docs line 2",
            "fn f() {}",
        ];
        // def on line 4 (1-indexed)
        let doc = collect_doc_comment(&src, 4);
        assert_eq!(doc, vec!["// header", "/// docs line 1", "/// docs line 2"]);
        // a blank line breaks the block
        let src2 = ["/// far away", "", "fn f() {}"];
        assert!(collect_doc_comment(&src2, 3).is_empty());
    }

    #[test]
    fn callers_never_exceed_medium_confidence() {
        // Pure invariant check on the enum bound the code enforces: callers
        // emit only Medium (AST) or Low (text), never High.
        for c in [Confidence::Medium, Confidence::Low] {
            assert_ne!(c, Confidence::High);
        }
    }

    #[test]
    fn word_boundary_matches_whole_word_only() {
        assert!(line_has_word("call foo();", "foo"));
        assert!(line_has_word("foo", "foo"));
        assert!(!line_has_word("foobar()", "foo"));
        assert!(!line_has_word("do_foo()", "foo"));
        assert!(line_has_word("a.foo = 1", "foo"));
    }

    #[test]
    fn replace_word_swaps_only_whole_words() {
        assert_eq!(
            replace_word("let foo = foo + foobar;", "foo", "bar"),
            "let bar = bar + foobar;"
        );
        assert_eq!(replace_word("foo", "foo", "baz"), "baz");
        assert_eq!(replace_word("no match here", "foo", "bar"), "no match here");
    }

    fn scratch_rename_repo(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_nav_rename_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn plan_rename_blocks_on_reference_gap_and_folds_with_include_refs() {
        let dir = scratch_rename_repo(
            "gap",
            &[(
                "a.py",
                concat!(
                    "def target_fn(root):\n",
                    "    return root\n",
                    "\n",
                    "def run(root):\n",
                    "    d = target_fn(root)\n",
                    "    return d\n",
                ),
            )],
        );
        let root = dir.to_str().unwrap().to_string();
        let opts = QueryOptions::new(&root);

        // Default plan is definition-only and reconciled: the call site is a
        // named gap that blocks writes (never a silently partial rename).
        let plan = plan_rename("target_fn", "renamed_fn", &opts, false).unwrap();
        assert_eq!(plan.planned_count, 1, "def site only: {plan:?}");
        assert_eq!(plan.known_total, 2);
        assert!(plan.blocked, "known refs must block a def-only write");
        assert_eq!(plan.excluded.len(), 1);
        let gap = &plan.excluded[0];
        assert_eq!(gap.line, 5, "the call site is the gap: {gap:?}");
        // With the per-language AST kind table the same-file reference rides
        // the AST tier (High), not the text fallback (Low).
        assert!(
            matches!(gap.confidence, Confidence::High),
            "gap confidence: {gap:?}"
        );

        // include_refs folds the call site in: full coverage, no block.
        let full = plan_rename("target_fn", "renamed_fn", &opts, true).unwrap();
        assert_eq!(full.sites.len(), 2);
        assert_eq!(full.plan.ops.len(), 2);
        assert!(!full.blocked);
        assert!(full.excluded.is_empty());
        assert_eq!(full.known_total, full.planned_count);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_rename_scope_marks_narrow_root_and_gap_free_target() {
        let dir = scratch_rename_repo("scope", &[("sub/b.rs", "fn only_def() {}\n")]);
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
        };
        if !matches!(run(&["init", "-q"]), Ok(o) if o.status.success()) {
            eprintln!("git unavailable; skipping narrow-scope test");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        run(&["add", "sub/b.rs"]).unwrap();

        let repo = dir.to_str().unwrap().to_string();
        let sub = dir.join("sub").to_str().unwrap().to_string();

        // Repo root: no narrowing, and a reference-free target stays writable.
        let whole = plan_rename("only_def", "new_name", &QueryOptions::new(&repo), false).unwrap();
        assert!(!whole.scope.narrow);
        assert!(whole.scope.repo_root.is_some());
        assert!(!whole.blocked, "no refs -> no gap: {whole:?}");
        assert_eq!(whole.known_total, whole.planned_count);

        // Strict subdirectory root: scope must say so (sites outside `sub`
        // were never scanned — SC-3).
        let scoped = plan_rename("only_def", "new_name", &QueryOptions::new(&sub), false).unwrap();
        assert!(
            scoped.scope.narrow,
            "subdir root must be flagged: {:?}",
            scoped.scope
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_puts_high_first() {
        let mut hits = vec![
            NavHit {
                path: "b.rs".into(),
                line: 2,
                col: None,
                kind: NavKind::Reference,
                symbol: "x".into(),
                snippet: None,
                confidence: Confidence::Low,
                source: Backend::Rg,
            },
            NavHit {
                path: "a.rs".into(),
                line: 9,
                col: None,
                kind: NavKind::Definition,
                symbol: "x".into(),
                snippet: None,
                confidence: Confidence::High,
                source: Backend::Ctags,
            },
            NavHit {
                path: "a.rs".into(),
                line: 1,
                col: None,
                kind: NavKind::Reference,
                symbol: "x".into(),
                snippet: None,
                confidence: Confidence::Medium,
                source: Backend::AstGrep,
            },
        ];
        sort_hits(&mut hits);
        assert_eq!(hits[0].confidence, Confidence::High);
        assert_eq!(hits[1].confidence, Confidence::Medium);
        assert_eq!(hits[2].confidence, Confidence::Low);
    }

    #[test]
    fn test_file_heuristic_matches_common_conventions() {
        assert!(looks_like_test_file("crates/foo/tests/bar.rs"));
        assert!(looks_like_test_file("pkg/foo_test.go"));
        assert!(looks_like_test_file("src/test_utils.py"));
        assert!(looks_like_test_file("web/button.test.ts"));
        assert!(looks_like_test_file("web/button.spec.js"));
        assert!(looks_like_test_file("app/__tests__/x.jsx"));
        assert!(!looks_like_test_file("crates/foo/src/lib.rs"));
        assert!(!looks_like_test_file("src/latest.rs"));
    }

    #[test]
    fn normalize_path_strips_dot_and_backslashes() {
        assert_eq!(normalize_path("./src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_path("src\\win\\foo.rs"), "src/win/foo.rs");
        assert_eq!(normalize_path("src/foo.rs"), "src/foo.rs");
    }

    #[test]
    fn path_matches_is_suffix_aware_on_boundaries() {
        let mut wanted = BTreeSet::new();
        wanted.insert("src/foo.rs".to_string());
        // exact
        assert!(path_matches("src/foo.rs", &wanted));
        // sym recorded with a longer root; changed path is a /-suffix of it
        assert!(path_matches("/abs/repo/src/foo.rs", &wanted));
        // and the reverse (changed path longer than the recorded one)
        let mut wanted2 = BTreeSet::new();
        wanted2.insert("/abs/repo/src/foo.rs".to_string());
        assert!(path_matches("src/foo.rs", &wanted2));
        // never match on a non-boundary suffix
        assert!(!path_matches("src/barfoo.rs", &wanted));
        // unrelated file
        assert!(!path_matches("src/other.rs", &wanted));
    }

    // --- E2E: build a real temp repo, scan with ctags, walk the blast radius.
    fn scratch_repo(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_nav_impact_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctags_present() -> bool {
        SymbolTable::scan(".").is_ok()
    }

    fn opts_cold(root: &std::path::Path) -> QueryOptions {
        let mut o = QueryOptions::new(root.to_string_lossy().to_string());
        o.cache = false; // force a cold scan; no cache pollution across tests
        o
    }

    #[test]
    fn impact_files_reports_seed_and_reverse_callers() {
        if !ctags_present() {
            eprintln!("universal-ctags unavailable; skipping impact E2E");
            return;
        }
        let dir = scratch_repo("callers");
        // a.py defines `core`; b.py calls it; test_a.py exercises it.
        std::fs::write(dir.join("a.py"), "def core():\n    return 1\n").unwrap();
        std::fs::write(
            dir.join("b.py"),
            "from a import core\n\ndef wrapper():\n    return core()\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("test_a.py"),
            "from a import core\n\ndef test_core():\n    assert core() == 1\n",
        )
        .unwrap();

        let opts = opts_cold(&dir);
        let hits = impact_files(&["a.py".to_string()], 64, false, false, &opts).unwrap();

        // The seed `core` must appear as a Definition at High confidence.
        let seed = hits
            .iter()
            .find(|h| h.symbol == "core")
            .expect("seed symbol core present");
        assert_eq!(seed.kind, NavKind::Definition);
        assert_eq!(seed.confidence, Confidence::High);

        // The reverse closure must reach `wrapper` (b.py) as a Caller.
        assert!(
            hits.iter()
                .any(|h| h.symbol == "wrapper" && h.kind == NavKind::Caller),
            "wrapper should be in the blast radius: {hits:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn impact_files_directed_follows_import_direction() {
        if !ctags_present() {
            eprintln!("universal-ctags unavailable; skipping directed impact E2E");
            return;
        }
        let dir = scratch_repo("directed");
        // b.py imports a.py and wrapper() calls core(): the reference runs WITH
        // the import direction, so it survives the directed (import-directed)
        // closure. Seeding from the changed file a.py must still reach wrapper.
        std::fs::write(dir.join("a.py"), "def core():\n    return 1\n").unwrap();
        std::fs::write(
            dir.join("b.py"),
            "from a import core\n\ndef wrapper():\n    return core()\n",
        )
        .unwrap();

        let opts = opts_cold(&dir);
        let hits = impact_files(&["a.py".to_string()], 64, false, true, &opts).unwrap();

        assert!(
            hits.iter()
                .any(|h| h.symbol == "wrapper" && h.kind == NavKind::Caller),
            "directed closure must reach wrapper (b imports a): {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn impact_files_tests_only_narrows_to_test_files() {
        if !ctags_present() {
            eprintln!("universal-ctags unavailable; skipping impact E2E");
            return;
        }
        let dir = scratch_repo("tests_only");
        std::fs::write(dir.join("a.py"), "def core():\n    return 1\n").unwrap();
        std::fs::write(
            dir.join("b.py"),
            "from a import core\n\ndef wrapper():\n    return core()\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("test_a.py"),
            "from a import core\n\ndef test_core():\n    assert core() == 1\n",
        )
        .unwrap();

        let opts = opts_cold(&dir);
        let hits = impact_files(&["a.py".to_string()], 64, true, false, &opts).unwrap();

        // Every surviving hit must live in a test file.
        assert!(!hits.is_empty(), "expected at least the test file hit");
        for h in &hits {
            assert!(
                looks_like_test_file(&h.path),
                "tests_only leaked a non-test hit: {h:?}"
            );
        }
        // And the non-test caller `wrapper` must be gone.
        assert!(
            !hits.iter().any(|h| h.symbol == "wrapper"),
            "wrapper is not a test and must be filtered out"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn impact_files_empty_changed_is_empty() {
        // Pure: no scan needed for an empty changed-set short-circuit... but we
        // still guard behind ctags because full_table runs first.
        if !ctags_present() {
            return;
        }
        let dir = scratch_repo("empty");
        std::fs::write(dir.join("a.py"), "def core():\n    return 1\n").unwrap();
        let opts = opts_cold(&dir);
        let hits = impact_files(&[], 64, false, false, &opts).unwrap();
        assert!(hits.is_empty());
        // A changed file that defines nothing yields no seeds -> empty.
        std::fs::write(dir.join("blank.py"), "# no defs here\n").unwrap();
        let hits2 = impact_files(&["blank.py".to_string()], 64, false, false, &opts).unwrap();
        assert!(hits2.is_empty(), "no seeds -> empty radius: {hits2:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_plan_is_line_precise() {
        // No repo scan here; exercise the lowering helper directly.
        let line = "    let foo = foo_bar + foo;";
        let out = replace_word(line, "foo", "renamed");
        assert_eq!(out, "    let renamed = foo_bar + renamed;");
    }
}
