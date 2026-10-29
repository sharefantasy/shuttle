//! shuttle-symbols — daemonless symbol table + repo-map (replaces LSP).
//!
//! # Why not LSP
//! An LSP server cold-starts slowly (indexes the whole project, often seconds
//! to tens of seconds). That fights the "small fast model + low tail latency"
//! goal: a single rename should not first wait for a language server to boot.
//! Instead we build a *static* symbol table with zero resident processes:
//!
//!   1. **universal-ctags** (`--output-format=json -R`): millisecond whole-repo
//!      scan, emits `{name, kind, scope, path, line}` tags across 40+ languages.
//!      Fast and broad — finds every candidate.
//!   2. **tree-sitter** (M3+): parses a single file's syntax tree to *disambiguate
//!      scope* (same-named local vs top-level symbol, closure capture, method vs
//!      free function), trimming ctags' text-level false matches. Accurate — cuts
//!      the mis-hits.
//!
//! Both are one-shot in-process calls, no daemon, cold-start = 0.
//!
//! # repo-map (aider-style)
//! `repo_map` mirrors aider's repository map: extract def/ref tags, build a symbol
//! reference graph, rank symbols by PageRank, then emit a token-budgeted map for the
//! upstream planner. Unlike aider we (a) produce it *for the planner*, not for
//! ourselves, (b) refresh incrementally (only re-tag files touched by the EditPlan,
//! caching the rest), and (c) share the exact same tag data as the rename table —
//! one ctags scan, two uses. See docs/architecture.md §7.5.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// Memory-safety invariants for one-shot symbol discovery. These are deliberately
// hard limits rather than tuning hints: callers receive a typed `Limit` error
// and must narrow the scope or use a caller-owned index.
const MAX_SCAN_FILES: usize = 10_000;
// This counts the argv payload for explicit file paths. 512 KiB comfortably
// fits this repository's ~142 KiB tracked-file list while still failing well
// before an accidentally recursive multi-worktree scan can become unbounded.
const MAX_SCAN_PATH_BYTES: u64 = 512 * 1024;
const MAX_SCAN_INPUT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CTAGS_STDOUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CTAGS_LINE_BYTES: usize = 1024 * 1024;
const MAX_CTAGS_SYMBOLS: usize = 250_000;
const MAX_CTAGS_STDERR_BYTES: u64 = 64 * 1024;
const MAX_TOOL_PROBE_STDOUT_BYTES: u64 = 1024 * 1024;
const MAX_SYMBOL_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_CTAGS_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REPO_MAP_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPO_MAP_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REPO_MAP_REFERENCES: usize = 1_000_000;
// Highly repeated names (fields such as `id`, `status`, or metric keys) are not
// a useful cross-file binding. Expanding them across every definition creates
// a Cartesian graph; keep only names narrow enough to carry signal.
const MAX_REPO_MAP_DEFINITIONS_PER_SYMBOL: usize = 8;
const MAX_REPO_MAP_EDGE_UPDATES: u64 = 2_000_000;

pub mod cochange;
pub mod contract;
pub mod contract_anchor;
pub mod graph;
pub mod imports;
pub mod index;

#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("universal-ctags not found on PATH")]
    MissingBackend,
    #[error("ctags scan failed: {0}")]
    Scan(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("symbol scan limit exceeded: {resource} observed {observed}, limit {limit}")]
    Limit {
        resource: &'static str,
        observed: u64,
        limit: u64,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
fn ctags_backend_from_candidates(
    explicit: Option<PathBuf>,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf, SymbolError> {
    ctags_backend_from_candidates_with_timeout(explicit, candidates, None)
}

#[cfg(test)]
fn ctags_backend_from_candidates_with_timeout(
    explicit: Option<PathBuf>,
    candidates: impl IntoIterator<Item = PathBuf>,
    timeout: Option<Duration>,
) -> Result<PathBuf, SymbolError> {
    if let Some(path) = explicit {
        if is_universal_ctags_with_timeout(&path, timeout)? {
            return Ok(path);
        }
        return Err(SymbolError::Scan(format!(
            "SHUTTLE_CTAGS_BIN is not a usable Universal Ctags binary: {}",
            path.display()
        )));
    }

    let mut seen = HashSet::new();
    for candidate in candidates {
        let key = candidate.as_os_str().to_os_string();
        if !seen.insert(key) {
            continue;
        }
        if is_universal_ctags_with_timeout(&candidate, timeout)? {
            return Ok(candidate);
        }
    }
    Err(SymbolError::MissingBackend)
}

fn ctags_backend_with_timeout(timeout: Option<Duration>) -> Result<PathBuf, SymbolError> {
    let candidate = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Ctags)
        .map_err(|_| SymbolError::MissingBackend)?;
    if is_universal_ctags_with_timeout(&candidate, timeout)? {
        Ok(candidate)
    } else {
        Err(SymbolError::MissingBackend)
    }
}

#[cfg(test)]
fn ctags_backend() -> Result<PathBuf, SymbolError> {
    ctags_backend_with_timeout(None)
}

fn is_universal_ctags_with_timeout(
    path: &Path,
    timeout: Option<Duration>,
) -> Result<bool, SymbolError> {
    // Legacy runtime-tool resolution intentionally returns a bare command name
    // so the operating system can resolve it through PATH.  `Path::is_file()`
    // only checks the current directory for such names and therefore rejected a
    // perfectly usable PATH-installed Universal Ctags before it was executed.
    // Let `Command` handle both bare names and explicit paths; a missing binary
    // is already normalized to `MissingBackend` below.
    let mut command = Command::new(path);
    command.arg("--version");
    match command_output_with_timeout(&mut command, timeout) {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(stdout.contains("Universal Ctags"))
        }
        Ok(_) | Err(SymbolError::MissingBackend) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The kind of a symbol, normalized across languages (subset of ctags kinds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Variable,
    /// A data member of a struct/class/message (C++ `member`, Thrift struct
    /// field). Distinct from a free `Variable` and from a callable `Method`.
    Field,
    Constant,
    Module,
    /// An RPC/IDL service container (e.g. Thrift `service`).
    Service,
    /// A type alias (e.g. Thrift `typedef`, C `typedef`).
    TypeAlias,
    /// A config section/table (INI `[section]`, TOML `[table]`).
    Section,
    /// A config key inside a section/table (INI/TOML key).
    Key,
    Other,
}

/// A single symbol tag: a definition site located in the repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Enclosing scope (class/module), if ctags reports one.
    pub scope: Option<String>,
    pub path: String,
    pub line: usize,
    /// The raw definition/signature line, used as the repo-map snippet.
    pub signature: Option<String>,
}

impl Symbol {
    /// Whether this tag is a *code* identifier that rename/anchor consumers may
    /// act on. ctags also tags data files: a JSON `"name":` key
    /// (`SymbolKind::Key`) or an INI/TOML section is a *data field*, not a
    /// symbol — a same-named key in an eval fixture is a false match that would
    /// otherwise both enter rename plans and anchor `references` as a High
    /// Definition. Unknown kinds (`Other`) stay eligible: on code files they
    /// are language-specific definition kinds we have not enumerated, and the
    /// conservative direction for *code* tags is keep (§7).
    pub fn is_code_symbol(&self) -> bool {
        !matches!(self.kind, SymbolKind::Key | SymbolKind::Section)
    }
}

/// The whole-repo symbol table. Built once from a ctags scan and reused by both
/// rename (§7) and repo-map (§7.5).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SymbolTable {
    pub symbols: Vec<Symbol>,
}

impl SymbolTable {
    /// Scan `root` with universal-ctags and parse its JSON tag stream.
    ///
    /// Runs `ctags --output-format=json --fields=+n+K+s -R <root>`; each stdout
    /// line is one JSON tag object.
    pub fn scan(root: &str) -> Result<Self, SymbolError> {
        // Prefer the repository's tracked set. Besides making the input
        // immutable/auditable, this excludes untracked eval worktrees and
        // runtime artifacts that previously made `ctags -R` recursively scan
        // copies of the repository. Outside git, collect an explicitly bounded
        // file set while applying the same durable exclusions.
        let files = match git_tracked(root)? {
            Some(files) => files,
            None => collect_scan_files(root)?,
        };
        scan_files(&files)
    }

    /// All definition sites for a given symbol name.
    pub fn defs(&self, name: &str) -> Vec<&Symbol> {
        self.symbols.iter().filter(|s| s.name == name).collect()
    }
}

/// Map a ctags `kind` word (universal-ctags long form) to our normalized
/// [`SymbolKind`]. Unknown / language-specific kinds fold into `Other`.
fn map_kind(kind: &str) -> SymbolKind {
    match kind {
        "function" | "func" | "subroutine" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        // Thrift/C++ struct data members and record fields.
        "member" | "field" => SymbolKind::Field,
        "class" => SymbolKind::Class,
        "struct" => SymbolKind::Struct,
        "enum" | "enumerator" => SymbolKind::Enum,
        "variable" | "var" | "local" => SymbolKind::Variable,
        "constant" | "const" | "macro" => SymbolKind::Constant,
        "module" | "namespace" | "package" | "interface" => SymbolKind::Module,
        // Thrift service container.
        "service" => SymbolKind::Service,
        // Thrift/C typedef.
        "typedef" => SymbolKind::TypeAlias,
        // INI / TOML structure.
        "section" | "table" => SymbolKind::Section,
        "key" | "qkey" => SymbolKind::Key,
        // JSON tags by *value type* (universal-ctags' JSON parser emits one
        // kind per value: string/number/boolean/null/array/object). They are
        // data fields, not code symbols — a same-named `"x":` key in an eval
        // fixture must not become a rename site or a references anchor.
        "string" | "number" | "boolean" | "null" | "array" | "object" => SymbolKind::Key,
        _ => SymbolKind::Other,
    }
}

/// Turn a ctags `pattern` field (`/^   ...$/`) into a clean signature line by
/// stripping the `/^` … `$/` anchors and unescaping the backslash-escaped `/`.
fn signature_from_pattern(pattern: &str) -> Option<String> {
    let inner = pattern
        .strip_prefix("/^")
        .and_then(|s| s.strip_suffix("$/"))
        .or_else(|| pattern.strip_prefix('/').and_then(|s| s.strip_suffix('/')))?;
    Some(inner.replace("\\/", "/").trim_end().to_string())
}

/// Parse universal-ctags `--output-format=json` output: one JSON object per line.
///
/// Each non-empty line is a tag object like:
///   {"_type":"tag","name":"process","path":"a.py","line":10,
///    "kind":"function","scope":"Service","pattern":"/^def process(...)$/"}
/// Non-tag lines (e.g. `_type == "ptag"` headers) are skipped.
#[cfg(test)]
fn parse_ctags_json(stdout: &str) -> Result<SymbolTable, SymbolError> {
    parse_ctags_reader(
        BufReader::new(stdout.as_bytes()),
        MAX_CTAGS_STDOUT_BYTES,
        MAX_CTAGS_LINE_BYTES,
        MAX_CTAGS_SYMBOLS,
    )
}

fn parse_ctags_reader<R: BufRead>(
    mut reader: R,
    max_output_bytes: u64,
    max_line_bytes: usize,
    max_symbols: usize,
) -> Result<SymbolTable, SymbolError> {
    let mut symbols = Vec::new();
    let mut line = Vec::new();
    let mut output_bytes = 0u64;
    while read_limited_line(&mut reader, &mut line, max_line_bytes, "ctags_line_bytes")? {
        output_bytes = output_bytes.saturating_add(line.len() as u64);
        if output_bytes > max_output_bytes {
            return Err(SymbolError::Limit {
                resource: "ctags_stdout_bytes",
                observed: output_bytes,
                limit: max_output_bytes,
            });
        }
        let line = std::str::from_utf8(&line)
            .map_err(|error| SymbolError::Parse(format!("ctags output is not UTF-8: {error}")))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        use serde_json::Value;
        let v: Value = serde_json::from_str(line).map_err(|e| SymbolError::Parse(e.to_string()))?;
        if v.get("_type").and_then(Value::as_str) != Some("tag") {
            continue; // skip pseudo-tags / headers
        }
        let name = match v.get("name").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let path = v
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let line_no = v.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let kind = v
            .get("kind")
            .and_then(Value::as_str)
            .map(map_kind)
            .unwrap_or(SymbolKind::Other);
        let scope = v.get("scope").and_then(Value::as_str).map(str::to_string);
        let signature = v
            .get("pattern")
            .and_then(Value::as_str)
            .and_then(signature_from_pattern);
        symbols.push(Symbol {
            name,
            kind,
            scope,
            path,
            line: line_no,
            signature,
        });
        if symbols.len() > max_symbols {
            return Err(SymbolError::Limit {
                resource: "ctags_symbols",
                observed: symbols.len() as u64,
                limit: max_symbols as u64,
            });
        }
    }
    Ok(SymbolTable { symbols })
}

/// Read one delimited record without ever growing `line` past `max_bytes`.
/// Unlike `BufRead::read_line`/`read_until`, this checks each internal buffer
/// before appending, so a malicious or corrupt producer cannot allocate an
/// arbitrarily large single record before the limit is noticed.
fn read_limited_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
    resource: &'static str,
) -> Result<bool, SymbolError> {
    line.clear();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(!line.is_empty());
        }
        let take = buf
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buf.len(), |index| index + 1);
        let observed = line.len().saturating_add(take);
        if observed > max_bytes {
            return Err(SymbolError::Limit {
                resource,
                observed: observed as u64,
                limit: max_bytes as u64,
            });
        }
        line.extend_from_slice(&buf[..take]);
        let complete = buf[take - 1] == b'\n';
        reader.consume(take);
        if complete {
            return Ok(true);
        }
    }
}

/// Rename `from` -> `to` across the repo using the symbol table (§7 tier 1).
///
/// Strategy: look up every definition site named `from` in the table and emit a
/// [`RenameEdit`] per site. These are lowered into `EditOp::SearchReplace` ops by
/// the CLI/editcore layer — zero model tokens.
///
/// Scope: ctags gives us *definition* sites (fast & broad). Reference sites and
/// scope disambiguation (same-named local vs the target symbol, method vs free
/// function) are the tree-sitter layer's job (§7 tier 2) — see
/// [`disambiguate_refs`], wired in a later milestone. Emitting definition edits
/// first keeps the common "rename a top-level def" path zero-model today.
pub fn rename(table: &SymbolTable, from: &str, to: &str) -> Result<Vec<RenameEdit>, SymbolError> {
    if from.is_empty() || from == to {
        return Ok(Vec::new());
    }
    let edits = table
        .defs(from)
        .into_iter()
        // Data-file tags (JSON/TOML/INI keys, config sections) are not code
        // identifiers: a same-named `Key` in an eval JSON is a false match, not
        // a rename site. They must go *before* `disambiguate_refs`, whose
        // unknown-language policy keeps candidates it cannot judge — for data
        // files that keep-direction is the dangerous one.
        .filter(|sym| sym.is_code_symbol())
        .map(|sym| RenameEdit {
            path: sym.path.clone(),
            line: sym.line,
            old: from.to_string(),
            new: to.to_string(),
        })
        .collect();
    // §7 tier 2: drop ctags text-level false matches (struct fields, comments,
    // strings) that aren't genuine identifier bindings, via tree-sitter.
    Ok(disambiguate_refs(edits))
}

/// Filter candidate rename sites down to those that truly refer to the target
/// symbol, using tree-sitter scope analysis (§7 tier 2).
///
/// ctags is a text-level scanner: it will tag a struct *field*, a doc comment,
/// or a string that merely spells the symbol. tree-sitter (via `ast-grep`)
/// parses each candidate file's real syntax tree, so we keep a site only when
/// the name occurs there as a genuine `identifier` / `type_identifier` node —
/// not a `field_identifier`, comment, or string literal.
///
/// Conservative by construction: if the language is unknown, the file is
/// unreadable, or `ast-grep` is unavailable, the candidate is kept (we have no
/// evidence to drop it). This keeps the common "rename a top-level def" path
/// working even without the backend, and only *removes* provable false matches.
pub fn disambiguate_refs(candidates: Vec<RenameEdit>) -> Vec<RenameEdit> {
    // Analyze each (file, name) pair at most once; renames touch few files.
    let mut genuine: BTreeMap<(String, String), Option<BTreeSet<usize>>> = BTreeMap::new();
    for e in &candidates {
        genuine
            .entry((e.path.clone(), e.old.clone()))
            .or_insert_with(|| genuine_identifier_lines(&e.path, &e.old));
    }
    retain_genuine(candidates, &genuine)
}

/// Pure core of [`disambiguate_refs`]: given, per `(path, name)`, the set of
/// lines where the name is a genuine identifier (or `None` when analysis was
/// unavailable), keep only edits landing on a genuine line.
fn retain_genuine(
    candidates: Vec<RenameEdit>,
    genuine: &BTreeMap<(String, String), Option<BTreeSet<usize>>>,
) -> Vec<RenameEdit> {
    candidates
        .into_iter()
        .filter(|e| match genuine.get(&(e.path.clone(), e.old.clone())) {
            // Not analyzed or analysis unavailable -> keep (no evidence to drop).
            None | Some(None) => true,
            // Analyzed -> keep only if the name is a genuine identifier here.
            Some(Some(lines)) => lines.contains(&e.line),
        })
        .collect()
}

/// Run `ast-grep` over `path`, returning the 1-indexed lines on which `name`
/// appears as a genuine `identifier` / `type_identifier` node.
///
/// Returns `None` (-> keep the candidate) when we can't judge: unknown
/// language, missing/unreadable file, or `ast-grep` absent or failing. Returns
/// `Some(set)` — possibly empty — on a successful parse, so provable
/// field/comment/string false matches are dropped.
pub fn genuine_identifier_lines(path: &str, name: &str) -> Option<BTreeSet<usize>> {
    let lang = lang_for_path(path)?;
    if !Path::new(path).is_file() {
        return None;
    }
    // Match the name only where tree-sitter sees a real identifier node,
    // excluding field_identifier, comments, and string literals. The kind set
    // is per-language: grammars name their identifier nodes differently (e.g.
    // Python has no `type_identifier`), and ast-grep rejects a rule that names
    // a kind the grammar lacks — the same lesson `identifiers_in_fragment`
    // encodes via `identifier_kinds`. A hardcoded
    // `any: [identifier, type_identifier]` here silently dropped *every*
    // Python file to the text tier (`None`) before that table was reused.
    let kinds = identifier_kinds(lang);
    if kinds.is_empty() {
        return None;
    }
    let any_block: String = kinds.iter().map(|k| format!("    - kind: {k}\n")).collect();
    let rule = format!(
        "id: shuttle-disambiguate\nlanguage: {lang}\nrule:\n  regex: \"^{name}$\"\n  any:\n{any_block}"
    );
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::AstGrep).ok()?;
    let out = Command::new(binary)
        .args(["scan", "--inline-rules", &rule, "--json", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ast_grep_lines(&String::from_utf8_lossy(&out.stdout)).ok()
}

/// Extract the set of identifier / type-identifier *names* that appear in a
/// source fragment, using ast-grep's tree-sitter parse over stdin.
///
/// Unlike [`genuine_identifier_lines`] (which needs a real file and a specific
/// name), this works on an in-memory fragment — e.g. one side of a git conflict
/// hunk, which is usually a *partial* body with no enclosing declaration.
/// tree-sitter's error-recovery still yields identifier nodes for such
/// fragments, so the extracted name set is meaningful even when the fragment
/// does not parse as a complete compilation unit.
///
/// `lang` is an ast-grep language id (see [`lang_for_path`]). Returns `None`
/// when ast-grep is unavailable or errors — callers MUST treat `None` as
/// "could not determine" and fall back conservatively (§7, never guess), never
/// as "empty set".
pub fn identifiers_in_fragment(source: &str, lang: &str) -> Option<BTreeSet<String>> {
    use std::io::Write;
    use std::process::Stdio;

    // Each tree-sitter grammar names its identifier nodes differently — e.g.
    // Python has no `type_identifier`, bash has no `identifier` (it uses
    // `variable_name` / `word`). A single hardcoded `any:[identifier,
    // type_identifier]` rule is therefore rejected outright by grammars that
    // lack one of those kinds (ast-grep exits non-zero on an unknown kind). We
    // build the rule from a per-language table of *verified* node kinds
    // instead. An empty table (e.g. yaml/json, or an unregistered custom
    // grammar) means "no identifier concept we can trust" → return None so the
    // caller falls back conservatively (§7, never guess).
    let kinds = identifier_kinds(lang);
    if kinds.is_empty() {
        return None;
    }
    let any_block: String = kinds.iter().map(|k| format!("    - kind: {k}\n")).collect();
    let rule = format!("id: shuttle-fragment-idents\nlanguage: {lang}\nrule:\n  any:\n{any_block}");
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::AstGrep).ok()?;
    let mut child = Command::new(binary)
        .args(["scan", "--inline-rules", &rule, "--json", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Write the fragment to the child's stdin, then drop the handle to signal EOF.
    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ast_grep_names(&String::from_utf8_lossy(&out.stdout)).ok()
}

/// The tree-sitter node kinds that carry an identifier *name* for a given
/// ast-grep language id. Every entry here has been verified to be a valid kind
/// matcher for that grammar (ast-grep rejects unknown kinds); adding a language
/// requires probing its grammar, never guessing (§7).
///
/// We deliberately keep this to the primary name kinds — `identifier`,
/// `type_identifier`, and each grammar's base identifier node — and exclude
/// `field_identifier` / `property_identifier`, matching the node set
/// [`genuine_identifier_lines`] trusts. An empty slice means the language has
/// no identifier concept we rely on (yaml/json) or its grammar is not
/// registered, and callers must treat that as "cannot decide".
fn identifier_kinds(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "go" | "c" | "cpp" | "java" | "typescript" | "tsx" => {
            &["identifier", "type_identifier"]
        }
        "python" | "javascript" => &["identifier"],
        "ruby" => &["identifier", "constant"],
        "bash" => &["variable_name", "word"],
        // yaml/json have no identifier concept; thrift/toml/ini are custom
        // grammars whose kinds we have not verified (and which may not be
        // registered) — return empty so the caller falls back (§7).
        _ => &[],
    }
}

/// Whether `path`'s language has an identifier concept we trust (see
/// [`identifier_kinds`]). Files without one — JSON/YAML data files, docs,
/// unregistered custom grammars — are not *rename surfaces*: the name may
/// occur there as data or prose, but no identifier can be renamed, so rename
/// reconciliation must neither plan nor count them.
pub fn is_identifier_surface(path: &str) -> bool {
    match lang_for_path(path) {
        Some(lang) => !identifier_kinds(lang).is_empty(),
        None => false,
    }
}

/// Parse `ast-grep --json` output into the set of matched identifier *texts*.
/// Empty stdout (no matches) is a valid empty set, not an error.
fn parse_ast_grep_names(stdout: &str) -> Result<BTreeSet<String>, SymbolError> {
    use serde_json::Value;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(BTreeSet::new());
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|e| SymbolError::Parse(e.to_string()))?;
    let arr = v
        .as_array()
        .ok_or_else(|| SymbolError::Parse("expected a JSON array".into()))?;
    let mut names = BTreeSet::new();
    for m in arr {
        if let Some(t) = m.get("text").and_then(Value::as_str) {
            names.insert(t.to_string());
        }
    }
    Ok(names)
}

/// Map a file extension to an `ast-grep` language id. `None` for extensions we
/// don't confidently parse (-> candidate kept unfiltered).
///
/// Two classes of languages are mapped:
///   * **built-in ast-grep grammars** (rust/python/js/ts/go/java/c/cpp/ruby,
///     plus bash/yaml/json): usable out of the box, no registration.
///   * **`customLanguages` candidates** (thrift/toml/ini): only meaningful once
///     a tree-sitter grammar is registered in `sgconfig.yml` (§6.7). They are
///     mapped here so the nav layer can *dispatch* on them; when no grammar is
///     registered, ast-grep simply errors and the caller falls back
///     (conservative — see [`genuine_identifier_lines`]).
///
/// `pub(crate)` so `shuttle-nav` (via re-export) can pick the right backend per file
/// without re-deriving the extension table.
pub fn lang_for_path(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;
    Some(match ext {
        // --- built-in ast-grep grammars ---
        "rs" => "rust",
        "py" => "python",
        "js" | "cjs" | "mjs" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        // --- customLanguages candidates (need sgconfig.yml registration) ---
        "thrift" => "thrift",
        "toml" => "toml",
        "ini" | "cfg" => "ini",
        _ => return None,
    })
}

/// Parse `ast-grep --json` output into the set of 1-indexed match start lines.
/// ast-grep reports 0-indexed lines, so we add 1 to match ctags/editor rows.
fn parse_ast_grep_lines(stdout: &str) -> Result<BTreeSet<usize>, SymbolError> {
    use serde_json::Value;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(BTreeSet::new());
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|e| SymbolError::Parse(e.to_string()))?;
    let arr = v
        .as_array()
        .ok_or_else(|| SymbolError::Parse("expected a JSON array".into()))?;
    let mut lines = BTreeSet::new();
    for m in arr {
        if let Some(l) = m
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_u64)
        {
            lines.insert(l as usize + 1);
        }
    }
    Ok(lines)
}

/// A concrete rename edit site, to be lowered into an `EditOp::SearchReplace`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameEdit {
    pub path: String,
    pub line: usize,
    pub old: String,
    pub new: String,
}

/// A token-budgeted repository map for the upstream planner (§7.5).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RepoMap {
    /// PageRank-ranked entries, highest importance first, trimmed to the budget.
    pub entries: Vec<RepoMapEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMapEntry {
    pub path: String,
    pub symbol: String,
    pub signature: Option<String>,
    /// PageRank score (for debugging / telemetry).
    pub rank: f32,
}

/// Build a repo-map from the symbol table, ranked by PageRank over the file
/// reference graph and trimmed to `token_budget`.
///
/// IO wrapper: reads each source file that owns a tag, extracts the identifiers
/// it references, keeps only those that resolve to a known definition, and hands
/// the ranking off to the pure [`repo_map_from`]. Unreadable files simply
/// contribute no outgoing references (they still exist as nodes via their defs).
pub fn repo_map(
    table: &SymbolTable,
    token_budget: usize,
    focus: &[String],
) -> Result<RepoMap, SymbolError> {
    repo_map_root(table, token_budget, focus, ".")
}

/// Root-aware variant used when a portable symbol table contains repository-
/// relative paths but the caller's working directory is elsewhere.
pub fn repo_map_root(
    table: &SymbolTable,
    token_budget: usize,
    focus: &[String],
    root: &str,
) -> Result<RepoMap, SymbolError> {
    let defined: HashSet<&str> = table.symbols.iter().map(|s| s.name.as_str()).collect();
    let mut definition_counts: BTreeMap<&str, u64> = BTreeMap::new();
    for symbol in &table.symbols {
        *definition_counts.entry(symbol.name.as_str()).or_default() += 1;
    }

    // Unique file paths, first-seen order.
    let mut seen = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    for s in &table.symbols {
        if seen.insert(s.path.as_str()) {
            files.push(s.path.clone());
        }
    }

    let mut references: Vec<(String, Vec<String>)> = Vec::new();
    let mut source_bytes = 0u64;
    let mut reference_count = 0usize;
    let mut edge_updates = 0u64;
    for f in files {
        let disk_path = if Path::new(&f).is_absolute() {
            PathBuf::from(&f)
        } else {
            Path::new(root).join(&f)
        };
        let file_bytes = std::fs::metadata(&disk_path).map(|m| m.len()).unwrap_or(0);
        if file_bytes > MAX_REPO_MAP_FILE_BYTES {
            return Err(SymbolError::Limit {
                resource: "repo_map_file_bytes",
                observed: file_bytes,
                limit: MAX_REPO_MAP_FILE_BYTES,
            });
        }
        source_bytes = source_bytes.saturating_add(file_bytes);
        if source_bytes > MAX_REPO_MAP_SOURCE_BYTES {
            return Err(SymbolError::Limit {
                resource: "repo_map_source_bytes",
                observed: source_bytes,
                limit: MAX_REPO_MAP_SOURCE_BYTES,
            });
        }
        match std::fs::read_to_string(disk_path) {
            Ok(content) => {
                let remaining = MAX_REPO_MAP_REFERENCES.saturating_sub(reference_count);
                let found = extract_references_bounded(&content, &defined, remaining)?;
                reference_count = reference_count.saturating_add(found.len());
                for name in &found {
                    let definitions = definition_counts.get(name.as_str()).copied().unwrap_or(0);
                    // A name with dozens or hundreds of definition sites is
                    // effectively an unbound field/variable token, not useful
                    // symbol evidence. Expanding every occurrence across all
                    // candidates creates a Cartesian blow-up and degrades the
                    // rank signal, so exclude it from edge construction.
                    if definitions as usize > MAX_REPO_MAP_DEFINITIONS_PER_SYMBOL {
                        continue;
                    }
                    edge_updates = edge_updates.saturating_add(definitions);
                    if edge_updates > MAX_REPO_MAP_EDGE_UPDATES {
                        return Err(SymbolError::Limit {
                            resource: "repo_map_edge_updates",
                            observed: edge_updates,
                            limit: MAX_REPO_MAP_EDGE_UPDATES,
                        });
                    }
                }
                references.push((f, found));
            }
            Err(_) => references.push((f, Vec::new())),
        }
    }

    Ok(repo_map_from(table, &references, token_budget, focus))
}

/// Extract identifier references from `content`, keeping only names present in
/// `defined`. Occurrences are preserved (reference frequency drives ranking).
pub fn extract_references(content: &str, defined: &HashSet<&str>) -> Vec<String> {
    extract_references_bounded(content, defined, usize::MAX).unwrap_or_default()
}

fn extract_references_bounded(
    content: &str,
    defined: &HashSet<&str>,
    max_references: usize,
) -> Result<Vec<String>, SymbolError> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let word = &content[start..i];
            if defined.contains(word) {
                out.push(word.to_string());
                if out.len() > max_references {
                    return Err(SymbolError::Limit {
                        resource: "repo_map_references",
                        observed: out.len() as u64,
                        limit: max_references as u64,
                    });
                }
            }
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// Pure core of the repo-map (§7.5). Given the symbol table and, for each file,
/// the symbol names it references, build a file->file reference graph, rank the
/// files with personalized PageRank (biased toward `focus`), lift the rank onto
/// individual symbols (weighted by cross-file reference count), then emit the
/// highest-ranked entries until `token_budget` is exhausted.
pub fn repo_map_from(
    table: &SymbolTable,
    references: &[(String, Vec<String>)],
    token_budget: usize,
    focus: &[String],
) -> RepoMap {
    // name -> files that define it.
    let mut def_files: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for s in &table.symbols {
        def_files
            .entry(s.name.as_str())
            .or_default()
            .insert(s.path.as_str());
    }
    def_files.retain(|_, files| files.len() <= MAX_REPO_MAP_DEFINITIONS_PER_SYMBOL);

    // Node set: every file that defines or references a symbol.
    let mut nodes: BTreeSet<String> = BTreeSet::new();
    for s in &table.symbols {
        nodes.insert(s.path.clone());
    }
    for (f, _) in references {
        nodes.insert(f.clone());
    }

    // Edges ref_file -> def_file (weighted), plus per-symbol cross-file ref counts.
    let mut edges: BTreeMap<String, BTreeMap<String, f32>> = BTreeMap::new();
    let mut sym_refs: BTreeMap<(String, String), u32> = BTreeMap::new();
    for (ref_file, names) in references {
        for name in names {
            if let Some(dfs) = def_files.get(name.as_str()) {
                let w = 1.0 / dfs.len() as f32;
                for df in dfs {
                    if *df == ref_file.as_str() {
                        continue; // ignore self-references
                    }
                    *edges
                        .entry(ref_file.clone())
                        .or_default()
                        .entry((*df).to_string())
                        .or_insert(0.0) += w;
                    *sym_refs
                        .entry((name.clone(), (*df).to_string()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    let ranks = pagerank(&nodes, &edges, focus, 0.85, 50);

    // Lift file rank onto symbols; heavily-referenced symbols rank higher.
    let mut entries: Vec<RepoMapEntry> = table
        .symbols
        .iter()
        .map(|s| {
            let file_rank = ranks.get(&s.path).copied().unwrap_or(0.0);
            let refs = sym_refs
                .get(&(s.name.clone(), s.path.clone()))
                .copied()
                .unwrap_or(0);
            RepoMapEntry {
                path: s.path.clone(),
                symbol: s.name.clone(),
                signature: s.signature.clone(),
                rank: file_rank * (1.0 + refs as f32),
            }
        })
        .collect();

    // Highest rank first; deterministic tie-break by path then symbol.
    entries.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });

    // Trim to the token budget.
    let mut spent = 0usize;
    let mut kept = Vec::new();
    for e in entries {
        let cost = token_cost(&e);
        if spent + cost > token_budget {
            break;
        }
        spent += cost;
        kept.push(e);
    }

    RepoMap { entries: kept }
}

/// Rough token cost of a repo-map entry (~4 chars/token, plus overhead).
fn token_cost(e: &RepoMapEntry) -> usize {
    let sig = e.signature.as_deref().map(str::len).unwrap_or(0);
    sig / 4 + e.symbol.len() / 4 + 2
}

/// Personalized PageRank over a directed, weighted graph. `focus` files take the
/// entire teleport mass (personalized restart); with empty `focus` the teleport
/// vector is uniform. Dangling nodes (no out-edges) redistribute their mass over
/// the teleport vector each iteration so total rank is conserved.
fn pagerank(
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeMap<String, f32>>,
    focus: &[String],
    damping: f32,
    iters: usize,
) -> BTreeMap<String, f32> {
    let n = nodes.len();
    if n == 0 {
        return BTreeMap::new();
    }

    // Teleport / personalization vector.
    let focus_in: BTreeSet<&str> = focus
        .iter()
        .map(String::as_str)
        .filter(|f| nodes.contains(*f))
        .collect();
    let mut teleport: BTreeMap<&str, f32> = BTreeMap::new();
    if focus_in.is_empty() {
        let p = 1.0 / n as f32;
        for node in nodes {
            teleport.insert(node.as_str(), p);
        }
    } else {
        let p = 1.0 / focus_in.len() as f32;
        for node in nodes {
            let v = if focus_in.contains(node.as_str()) {
                p
            } else {
                0.0
            };
            teleport.insert(node.as_str(), v);
        }
    }

    // Total out-weight per source node.
    let mut out_sum: BTreeMap<&str, f32> = BTreeMap::new();
    for (src, dsts) in edges {
        out_sum.insert(src.as_str(), dsts.values().sum());
    }

    let mut rank: BTreeMap<String, f32> = nodes
        .iter()
        .map(|node| {
            (
                node.clone(),
                teleport.get(node.as_str()).copied().unwrap_or(0.0),
            )
        })
        .collect();

    for _ in 0..iters {
        let mut next: BTreeMap<String, f32> = nodes
            .iter()
            .map(|node| {
                let t = teleport.get(node.as_str()).copied().unwrap_or(0.0);
                (node.clone(), (1.0 - damping) * t)
            })
            .collect();

        // Dangling mass -> teleport.
        let mut dangling = 0.0f32;
        for node in nodes {
            if out_sum.get(node.as_str()).copied().unwrap_or(0.0) <= 0.0 {
                dangling += rank[node];
            }
        }
        for node in nodes {
            let t = teleport.get(node.as_str()).copied().unwrap_or(0.0);
            *next.get_mut(node).unwrap() += damping * dangling * t;
        }

        // Push rank along edges.
        for (src, dsts) in edges {
            let total = out_sum.get(src.as_str()).copied().unwrap_or(0.0);
            if total <= 0.0 {
                continue;
            }
            let share = rank[src] / total;
            for (dst, w) in dsts {
                if let Some(slot) = next.get_mut(dst) {
                    *slot += damping * share * w;
                }
            }
        }

        rank = next;
    }

    rank
}

// ---------------------------------------------------------------------------
// §7.5 incremental symbol cache
// ---------------------------------------------------------------------------

/// Cache schema version. Bump to force a cold rebuild when the on-disk layout
/// changes.
const CACHE_VERSION: u32 = 1;

/// One cached file's tags plus the stat signature used to invalidate them.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    /// Modification time in nanoseconds since the Unix epoch.
    mtime_ns: u128,
    /// File size in bytes.
    size: u64,
    symbols: Vec<Symbol>,
}

/// On-disk incremental symbol cache (architecture.md §7.5). Keyed by the
/// path ctags reports (relative to cwd); each entry carries an mtime+size
/// signature so a warm scan re-runs ctags only on files that actually changed.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SymbolCache {
    version: u32,
    files: BTreeMap<String, CachedFile>,
}

impl SymbolCache {
    /// Flatten every cached file's tags back into a whole-repo [`SymbolTable`].
    fn into_table(self) -> SymbolTable {
        let mut symbols = Vec::new();
        for entry in self.files.into_values() {
            symbols.extend(entry.symbols);
        }
        SymbolTable { symbols }
    }
}

/// Stat `path` into the cache signature `(mtime_ns, size)`. `None` if the file
/// is gone / unreadable (the caller then drops it from the cache).
fn file_sig(path: &str) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let size = md.len();
    let mtime = md.modified().ok()?;
    let ns = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((ns, size))
}

/// Normalize a focus/touch hint the same way the plugin does: trim, backslash
/// -> `/`, strip a leading `./` so it matches ctags' reported path.
fn norm_focus(p: &str) -> String {
    let p = p.trim().replace('\\', "/");
    match p.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => p,
    }
}

/// Run ctags over an explicit file list (no `-R`). Reports the same paths as a
/// recursive scan, so results slot straight into the cache. Empty list ->
/// empty table (no process spawned).
pub fn scan_files(files: &[String]) -> Result<SymbolTable, SymbolError> {
    scan_files_with_timeout(files, None)
}

/// Run a bounded one-shot ctags scan. A timeout kills the child and returns an
/// error; callers must retain or rebuild their previous artifact rather than
/// publishing a partial index.
pub fn scan_files_with_timeout(
    files: &[String],
    timeout: Option<Duration>,
) -> Result<SymbolTable, SymbolError> {
    if files.is_empty() {
        return Ok(SymbolTable::default());
    }
    validate_scan_files(files)?;
    let ctags = ctags_backend_with_timeout(timeout)?;
    let mut cmd = Command::new(&ctags);
    cmd.args(["--output-format=json", "--fields=+n+K+s", "-f", "-"]);
    for f in files {
        cmd.arg(f);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| SymbolError::MissingBackend)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        parse_ctags_reader(
            BufReader::new(stdout),
            MAX_CTAGS_STDOUT_BYTES,
            MAX_CTAGS_LINE_BYTES,
            MAX_CTAGS_SYMBOLS,
        )
    });
    let stderr_reader = std::thread::spawn(move || {
        read_bounded(stderr, MAX_CTAGS_STDERR_BYTES, "ctags_stderr_bytes")
    });
    let timeout = timeout.unwrap_or(DEFAULT_CTAGS_TIMEOUT);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(SymbolError::Scan(format!(
                "ctags scan timed out after {} ms",
                timeout.as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let table = stdout_reader
        .join()
        .map_err(|_| SymbolError::Scan("ctags stdout reader panicked".to_string()))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| SymbolError::Scan("ctags stderr reader panicked".to_string()))??;
    if !status.success() {
        return Err(SymbolError::Scan(
            String::from_utf8_lossy(&stderr).into_owned(),
        ));
    }
    Ok(table)
}

pub(crate) fn validate_scan_files(files: &[String]) -> Result<(), SymbolError> {
    if files.len() > MAX_SCAN_FILES {
        return Err(SymbolError::Limit {
            resource: "scan_files",
            observed: files.len() as u64,
            limit: MAX_SCAN_FILES as u64,
        });
    }
    let mut path_bytes = 0u64;
    let mut input_bytes = 0u64;
    for path in files {
        path_bytes = path_bytes.saturating_add(path.len() as u64 + 1);
        if path_bytes > MAX_SCAN_PATH_BYTES {
            return Err(SymbolError::Limit {
                resource: "scan_path_bytes",
                observed: path_bytes,
                limit: MAX_SCAN_PATH_BYTES,
            });
        }
        input_bytes =
            input_bytes.saturating_add(std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0));
        if input_bytes > MAX_SCAN_INPUT_BYTES {
            return Err(SymbolError::Limit {
                resource: "scan_input_bytes",
                observed: input_bytes,
                limit: MAX_SCAN_INPUT_BYTES,
            });
        }
    }
    Ok(())
}

fn read_bounded(
    mut reader: impl Read,
    limit: u64,
    resource: &'static str,
) -> Result<Vec<u8>, SymbolError> {
    let mut bytes = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(SymbolError::Limit {
            resource,
            observed: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

fn command_output_with_timeout(
    command: &mut Command,
    timeout: Option<Duration>,
) -> Result<Output, SymbolError> {
    command_output_bounded_with_timeout(
        command,
        timeout,
        MAX_TOOL_PROBE_STDOUT_BYTES,
        "tool_probe_stdout_bytes",
    )
}

fn command_output_bounded_with_timeout(
    command: &mut Command,
    timeout: Option<Duration>,
    stdout_limit: u64,
    stdout_resource: &'static str,
) -> Result<Output, SymbolError> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| SymbolError::MissingBackend)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader =
        std::thread::spawn(move || read_bounded(&mut stdout, stdout_limit, stdout_resource));
    let stderr_reader = std::thread::spawn(move || {
        read_bounded(
            &mut stderr,
            MAX_CTAGS_STDERR_BYTES,
            "subprocess_stderr_bytes",
        )
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(SymbolError::Scan(format!(
                "ctags scan timed out after {} ms",
                timeout.unwrap().as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| SymbolError::Scan("ctags stdout reader panicked".to_string()))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| SymbolError::Scan("ctags stderr reader panicked".to_string()))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Seed a fresh cache from a full scan: group tags by path, stat each file.
fn seed_cache(table: &SymbolTable) -> SymbolCache {
    let mut by_path: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();
    for s in &table.symbols {
        by_path.entry(s.path.clone()).or_default().push(s.clone());
    }
    let mut files = BTreeMap::new();
    for (path, symbols) in by_path {
        let (mtime_ns, size) = file_sig(&path).unwrap_or((0, 0));
        files.insert(
            path,
            CachedFile {
                mtime_ns,
                size,
                symbols,
            },
        );
    }
    SymbolCache {
        version: CACHE_VERSION,
        files,
    }
}

/// Load the cache; any read/parse failure yields an empty (cold) cache.
fn load_cache(path: &str) -> SymbolCache {
    match std::fs::File::open(path)
        .map_err(SymbolError::Io)
        .and_then(|file| read_bounded(file, MAX_SYMBOL_CACHE_BYTES, "symbol_cache_bytes"))
    {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => SymbolCache::default(),
    }
}

/// Persist the cache next to its path (creating `.shuttle/` if needed). Best-effort:
/// a write failure is swallowed — the cache is an optimization, never a
/// correctness dependency.
fn store_cache(path: &str, cache: &SymbolCache) {
    let json = match serde_json::to_string(cache) {
        Ok(j) => j,
        Err(_) => return,
    };
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

impl SymbolTable {
    /// Incremental cached scan (architecture.md §7.5) — the counterpart to the
    /// full [`scan`](Self::scan).
    ///
    /// Loads `cache_path`; for every known file whose mtime+size still match,
    /// reuse its cached tags. ctags is re-run only on files that changed plus
    /// any `focus` hints (the paths an EditPlan touched — possibly brand new).
    /// Files that vanished are dropped. On a cold / absent / version-mismatched
    /// cache it falls back to a full [`scan`](Self::scan) and seeds the cache.
    /// The refreshed cache is written back (best-effort).
    ///
    /// Invalidation is deliberately conservative (mtime+size, per §8): it would
    /// rather MISS and recompute than serve a stale tag.
    pub fn scan_cached(
        root: &str,
        cache_path: &str,
        focus: &[String],
    ) -> Result<Self, SymbolError> {
        let mut cache = load_cache(cache_path);
        if cache.version != CACHE_VERSION || cache.files.is_empty() {
            // Cold: full scan seeds the cache.
            let table = Self::scan(root)?;
            store_cache(cache_path, &seed_cache(&table));
            return Ok(table);
        }

        // Warm: validate each known file, collecting the ones to re-scan.
        let mut rescan: BTreeSet<String> = BTreeSet::new();
        let mut vanished: Vec<String> = Vec::new();
        for (path, entry) in &cache.files {
            match file_sig(path) {
                None => vanished.push(path.clone()),
                Some((ns, size)) => {
                    if ns != entry.mtime_ns || size != entry.size {
                        rescan.insert(path.clone());
                    }
                }
            }
        }
        for p in vanished {
            cache.files.remove(&p);
        }
        // Focus hints: an EditPlan-touched file may be brand new (absent from
        // the cache) or just changed — always re-scan it if it exists on disk.
        for f in focus {
            let f = norm_focus(f);
            if Path::new(&f).is_file() {
                rescan.insert(f);
            }
        }

        if !rescan.is_empty() {
            let list: Vec<String> = rescan.into_iter().collect();
            let fresh = scan_files(&list)?;
            let mut by_path: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();
            for s in fresh.symbols {
                by_path.entry(s.path.clone()).or_default().push(s);
            }
            for path in &list {
                match file_sig(path) {
                    Some((mtime_ns, size)) => {
                        let symbols = by_path.remove(path).unwrap_or_default();
                        cache.files.insert(
                            path.clone(),
                            CachedFile {
                                mtime_ns,
                                size,
                                symbols,
                            },
                        );
                    }
                    None => {
                        cache.files.remove(path);
                    }
                }
            }
        }

        store_cache(cache_path, &cache);
        Ok(cache.into_table())
    }
}

/// Collect git-tracked files under `root`, as cwd-relative paths that slot
/// straight into [`scan_files`] / [`repo_map`]. `None` when git is unavailable
/// or the tree is empty — the caller then falls back to a full scan.
fn git_tracked(root: &str) -> Result<Option<Vec<String>>, SymbolError> {
    let binary = match shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git) {
        Ok(binary) => binary,
        Err(_) => return Ok(None),
    };
    let mut command = Command::new(binary);
    command.args(["-C", root, "ls-files", "-z"]);
    let out = command_output_bounded_with_timeout(
        &mut command,
        Some(Duration::from_secs(30)),
        MAX_SCAN_PATH_BYTES,
        "git_tracked_path_bytes",
    )?;
    if !out.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let files: Vec<String> = raw
        .split('\0')
        .filter(|s| !s.is_empty())
        .filter(|rel| !scan_path_excluded(Path::new(rel)))
        .map(|rel| join_root(root, rel))
        .collect();
    if files.is_empty() {
        Ok(None)
    } else {
        validate_scan_files(&files)?;
        Ok(Some(files))
    }
}

fn collect_scan_files(root: &str) -> Result<Vec<String>, SymbolError> {
    let root = Path::new(root);
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut path_bytes = 0u64;
    let mut input_bytes = 0u64;
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if scan_path_excluded(relative) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let path = path.to_string_lossy().to_string();
                let observed_files = files.len() + 1;
                if observed_files > MAX_SCAN_FILES {
                    return Err(SymbolError::Limit {
                        resource: "scan_files",
                        observed: observed_files as u64,
                        limit: MAX_SCAN_FILES as u64,
                    });
                }
                path_bytes = path_bytes.saturating_add(path.len() as u64 + 1);
                if path_bytes > MAX_SCAN_PATH_BYTES {
                    return Err(SymbolError::Limit {
                        resource: "scan_path_bytes",
                        observed: path_bytes,
                        limit: MAX_SCAN_PATH_BYTES,
                    });
                }
                input_bytes = input_bytes.saturating_add(entry.metadata()?.len());
                if input_bytes > MAX_SCAN_INPUT_BYTES {
                    return Err(SymbolError::Limit {
                        resource: "scan_input_bytes",
                        observed: input_bytes,
                        limit: MAX_SCAN_INPUT_BYTES,
                    });
                }
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn scan_path_excluded(path: &Path) -> bool {
    const EXCLUDED_DIRS: &[&str] = &[
        ".git",
        ".shuttle",
        ".agent-local",
        "node_modules",
        "dist",
        "build",
        "out",
        "target",
        ".venv",
        "vendor",
    ];
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if components
        .iter()
        .any(|component| EXCLUDED_DIRS.contains(component))
    {
        return true;
    }
    if components.windows(2).any(|pair| pair == ["eval", "runs"]) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.ends_with(".min.js") || name.ends_with(".lock") || name.ends_with("-lock.json")
}

/// Join a `git ls-files` path (relative to `root`) back onto `root` so it is
/// openable from the process cwd — matching how [`scan`](SymbolTable::scan)
/// reports paths. A `.`/empty root is a no-op.
fn join_root(root: &str, rel: &str) -> String {
    let r = root.trim_end_matches('/');
    if r.is_empty() || r == "." {
        rel.to_string()
    } else {
        format!("{r}/{rel}")
    }
}

/// Pure scope selection (⑤): keep every `tracked` path that is a `focus`
/// file or textually references one of `anchor_names` (a symbol *defined* in
/// the focus set) — i.e. focus plus its dependents (an inbound reference
/// edge at file granularity). `read` yields a file's contents (None if
/// unreadable). Deterministic and side-effect free, so it is unit-tested.
fn select_scope(
    tracked: &[String],
    focus: &BTreeSet<String>,
    anchor_names: &HashSet<String>,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Vec<String> {
    let anchor_refs: HashSet<&str> = anchor_names.iter().map(String::as_str).collect();
    let mut scope: BTreeSet<String> = BTreeSet::new();
    for path in tracked {
        let np = norm_focus(path);
        if focus.contains(&np) {
            scope.insert(np);
            continue;
        }
        if anchor_refs.is_empty() {
            continue;
        }
        if let Some(content) = read(&np) {
            if !extract_references(&content, &anchor_refs).is_empty() {
                scope.insert(np);
            }
        }
    }
    // Focus files are always in scope, even if brand new / untracked.
    for f in focus {
        scope.insert(f.clone());
    }
    scope.into_iter().collect()
}

impl SymbolTable {
    /// ⑤ Scope-narrowed scan (perf plan §2.⑤). Instead of tagging the whole
    /// repo, tag only the *focus neighborhood*: git-tracked files that have a
    /// reference edge to the `focus` set, plus the focus files themselves.
    /// Anchors are the symbols *defined* in the focus files, so a tracked file
    /// is pulled in exactly when it references one of them (keyword noise can't
    /// leak in — anchors are real definitions).
    ///
    /// Falls back to a full [`scan`](Self::scan) when git is unavailable or
    /// `focus` is empty (nothing to narrow around). Kills waste point W-E
    /// (`scan` was always whole-repo, even for focused calls) and as a side
    /// benefit skips untracked bulk (e.g. build outputs) a bare recursive scan
    /// would otherwise crawl.
    pub fn scan_scoped(root: &str, focus: &[String]) -> Result<Self, SymbolError> {
        let tracked = match git_tracked(root)? {
            Some(t) if !focus.is_empty() => t,
            _ => return Self::scan(root),
        };
        // Focus files that actually exist on disk.
        let focus_files: Vec<String> = focus
            .iter()
            .map(|f| norm_focus(f))
            .filter(|f| Path::new(f).is_file())
            .collect();
        if focus_files.is_empty() {
            return Self::scan(root);
        }
        // Cheap ctags pass over just the focus files -> the names they define.
        let focus_tbl = scan_files(&focus_files)?;
        let anchor_names: HashSet<String> =
            focus_tbl.symbols.iter().map(|s| s.name.clone()).collect();
        let focus_set: BTreeSet<String> = focus_files.into_iter().collect();
        let scope = select_scope(&tracked, &focus_set, &anchor_names, |p| {
            std::fs::read_to_string(p).ok()
        });
        scan_files(&scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    const SAMPLE: &str = concat!(
        r#"{"_type":"ptag","name":"!_TAG_FILE_FORMAT","pattern":"2"}"#,
        "\n",
        r#"{"_type":"tag","name":"process","path":"a.py","line":10,"kind":"function","pattern":"/^def process(self, data):$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"process","path":"b.py","line":3,"kind":"method","scope":"Service","scopeKind":"class","pattern":"/^    def process(self):$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"Service","path":"b.py","line":1,"kind":"class","pattern":"/^class Service:$/"}"#,
        "\n",
    );

    #[test]
    fn parses_tags_and_skips_pseudo() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        assert_eq!(t.symbols.len(), 3, "ptag header must be skipped");
    }

    #[test]
    fn cache_seed_roundtrip_preserves_symbols() {
        let table = parse_ctags_json(SAMPLE).unwrap();
        let cache = seed_cache(&table);
        assert_eq!(cache.version, CACHE_VERSION);
        // SAMPLE spans exactly two files: a.py and b.py.
        assert_eq!(cache.files.len(), 2);
        let rebuilt = cache.into_table();
        assert_eq!(rebuilt.symbols.len(), table.symbols.len());
    }

    #[test]
    fn select_scope_keeps_focus_and_dependents() {
        let tracked = vec!["a.py".to_string(), "b.py".to_string(), "c.py".to_string()];
        let mut focus = BTreeSet::new();
        focus.insert("a.py".to_string());
        let mut anchors = HashSet::new();
        anchors.insert("process".to_string()); // defined in focus a.py
        let contents: BTreeMap<&str, &str> = [
            ("a.py", "def process(): pass"),
            ("b.py", "from a import process\nprocess()"), // references anchor -> in
            ("c.py", "x = 1  # unrelated"),               // no anchor -> out
        ]
        .into_iter()
        .collect();
        let scope = select_scope(&tracked, &focus, &anchors, |p| {
            contents.get(p).map(|s| s.to_string())
        });
        assert!(scope.contains(&"a.py".to_string()), "focus file always in");
        assert!(scope.contains(&"b.py".to_string()), "dependent pulled in");
        assert!(!scope.contains(&"c.py".to_string()), "unrelated excluded");
    }

    #[test]
    fn join_root_prefixes_non_dot_root() {
        assert_eq!(join_root(".", "x/y.rs"), "x/y.rs");
        assert_eq!(join_root("", "x/y.rs"), "x/y.rs");
        assert_eq!(
            join_root("crates", "shuttle/lib.rs"),
            "crates/shuttle/lib.rs"
        );
        assert_eq!(
            join_root("crates/", "shuttle/lib.rs"),
            "crates/shuttle/lib.rs"
        );
    }

    #[test]
    fn maps_kinds_and_scope() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        let svc = t.defs("Service");
        assert_eq!(svc.len(), 1);
        assert_eq!(svc[0].kind, SymbolKind::Class);
        let methods: Vec<_> = t
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].scope.as_deref(), Some("Service"));
    }

    #[test]
    fn signature_strips_pattern_anchors() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        let f = t.defs("process");
        let py_fn = f.iter().find(|s| s.path == "a.py").unwrap();
        assert_eq!(py_fn.signature.as_deref(), Some("def process(self, data):"));
    }

    #[test]
    fn defs_finds_all_same_named_sites() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        assert_eq!(t.defs("process").len(), 2);
        assert!(t.defs("nonexistent").is_empty());
    }

    #[test]
    fn rename_emits_edit_per_def_site() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        let edits = rename(&t, "process", "handle").unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits
            .iter()
            .all(|e| e.old == "process" && e.new == "handle"));
        let paths: Vec<_> = edits.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"a.py") && paths.contains(&"b.py"));
    }

    #[test]
    fn rename_noop_on_empty_or_identity() {
        let t = parse_ctags_json(SAMPLE).unwrap();
        assert!(rename(&t, "", "x").unwrap().is_empty());
        assert!(rename(&t, "process", "process").unwrap().is_empty());
    }

    #[test]
    fn is_code_symbol_excludes_data_file_kinds() {
        // "string"/"number" are what universal-ctags' JSON parser actually
        // emits (one kind per *value type*), verified empirically — not "key".
        let t = parse_ctags_json(concat!(
            r#"{"_type":"tag","name":"x","path":"a.rs","line":1,"kind":"function"}"#,
            "\n",
            r#"{"_type":"tag","name":"x","path":"a.json","line":2,"kind":"string"}"#,
            "\n",
            r#"{"_type":"tag","name":"x","path":"a2.json","line":2,"kind":"number"}"#,
            "\n",
            r#"{"_type":"tag","name":"x","path":"a.toml","line":5,"kind":"key"}"#,
            "\n",
            r#"{"_type":"tag","name":"x","path":"a.ini","line":3,"kind":"section"}"#,
            "\n",
            r#"{"_type":"tag","name":"x","path":"a.unknownext","line":4,"kind":"weird"}"#,
            "\n",
        ))
        .unwrap();
        let by_path = |p: &str| t.symbols.iter().find(|s| s.path == p).unwrap();
        assert!(by_path("a.rs").is_code_symbol());
        assert!(
            by_path("a.unknownext").is_code_symbol(),
            "Other keeps (code-side conservative)"
        );
        assert!(
            !by_path("a.json").is_code_symbol(),
            "JSON string value is data, not a symbol"
        );
        assert!(
            !by_path("a2.json").is_code_symbol(),
            "JSON number value is data, not a symbol"
        );
        assert!(
            !by_path("a.toml").is_code_symbol(),
            "TOML key is data, not a symbol"
        );
        assert!(
            !by_path("a.ini").is_code_symbol(),
            "INI section is data, not a symbol"
        );
    }

    #[test]
    fn is_identifier_surface_follows_identifier_kinds() {
        assert!(is_identifier_surface("a.rs"));
        assert!(is_identifier_surface("a.py"));
        assert!(
            !is_identifier_surface("a.json"),
            "JSON: no identifier concept"
        );
        assert!(
            !is_identifier_surface("a.yaml"),
            "YAML: no identifier concept"
        );
        assert!(!is_identifier_surface("README.md"), "unmapped extension");
    }

    #[test]
    fn rename_drops_data_file_tags_keeps_code_defs() {
        // D4: a JSON key and an INI section share the name with a Rust fn;
        // only the code def may become a rename site. The .rs path does not
        // exist on disk, so disambiguation stays in its conservative keep
        // branch — the filter under test is the kind filter, not AST analysis.
        let t = parse_ctags_json(concat!(
            r#"{"_type":"tag","name":"evidence_digest","path":"crates/x/src/l.rs","line":46,"kind":"function"}"#,
            "\n",
            r#"{"_type":"tag","name":"evidence_digest","path":"vendor/real.json","line":110,"kind":"key"}"#,
            "\n",
            r#"{"_type":"tag","name":"evidence_digest","path":"conf/app.ini","line":3,"kind":"section"}"#,
            "\n",
        ))
        .unwrap();
        let edits = rename(&t, "evidence_digest", "renamed_digest").unwrap();
        assert_eq!(edits.len(), 1, "only the code def survives: {edits:?}");
        assert_eq!(edits[0].path, "crates/x/src/l.rs");
        assert_eq!(edits[0].line, 46);
    }

    #[test]
    fn genuine_identifier_lines_python_hits_def_and_call_only() {
        // D5: Python has no `type_identifier` node, so the pre-table hardcoded
        // `any: [identifier, type_identifier]` rule was rejected by ast-grep and
        // the whole file fell back to the text tier. With the per-language kind
        // table the AST tier must hit exactly the def and call lines.
        let dir = scratch_dir("genuine_py");
        let p = dir.join("t.py");
        std::fs::write(
            &p,
            concat!(
                "def _resolve_dispatcher(root):\n",
                "    return root\n",
                "\n",
                "def run(root):\n",
                "    d = _resolve_dispatcher(root)\n",
                "    # mention _resolve_dispatcher in comment\n",
                "    s = \"_resolve_dispatcher in string\"\n",
                "    return d\n",
            ),
        )
        .unwrap();
        match genuine_identifier_lines(p.to_str().unwrap(), "_resolve_dispatcher") {
            Some(lines) => {
                let expected: BTreeSet<usize> = [1usize, 5].into_iter().collect();
                assert_eq!(
                    lines, expected,
                    "AST tier must hit def+call and skip comment/string: {lines:?}"
                );
            }
            None => eprintln!("ast-grep unavailable; skipping strict assertion"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_malformed_json() {
        assert!(parse_ctags_json("{not valid json}").is_err());
    }

    #[test]
    fn ctags_parser_rejects_oversized_line_before_unbounded_growth() {
        let err = parse_ctags_reader(BufReader::new(&b"123456789\n"[..]), 100, 8, 10).unwrap_err();
        assert!(matches!(
            err,
            SymbolError::Limit {
                resource: "ctags_line_bytes",
                observed: 10,
                limit: 8
            }
        ));
    }

    #[test]
    fn ctags_parser_rejects_symbol_count_over_limit() {
        let tags = concat!(
            "{\"_type\":\"tag\",\"name\":\"a\",\"path\":\"a.rs\"}\n",
            "{\"_type\":\"tag\",\"name\":\"b\",\"path\":\"b.rs\"}\n",
        );
        let err = parse_ctags_reader(BufReader::new(tags.as_bytes()), 1024, 1024, 1).unwrap_err();
        assert!(matches!(
            err,
            SymbolError::Limit {
                resource: "ctags_symbols",
                observed: 2,
                limit: 1
            }
        ));
    }

    #[test]
    fn scan_file_count_limit_fails_before_spawning_ctags() {
        let files = vec!["missing.rs".to_string(); MAX_SCAN_FILES + 1];
        let err = scan_files(&files).unwrap_err();
        assert!(matches!(
            err,
            SymbolError::Limit {
                resource: "scan_files",
                observed,
                limit
            } if observed == (MAX_SCAN_FILES + 1) as u64 && limit == MAX_SCAN_FILES as u64
        ));
    }

    fn node_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pagerank_ranks_referenced_node_higher() {
        // a -> b: b is "important" (incoming reference), so it must outrank a.
        let nodes = node_set(&["a", "b"]);
        let mut edges: BTreeMap<String, BTreeMap<String, f32>> = BTreeMap::new();
        edges.entry("a".into()).or_default().insert("b".into(), 1.0);
        let r = pagerank(&nodes, &edges, &[], 0.85, 50);
        assert!(r["b"] > r["a"], "referenced node b should outrank a: {r:?}");
    }

    #[test]
    fn pagerank_focus_boosts_focused_node() {
        let nodes = node_set(&["a", "b"]);
        let mut edges: BTreeMap<String, BTreeMap<String, f32>> = BTreeMap::new();
        edges.entry("a".into()).or_default().insert("b".into(), 1.0);
        let base = pagerank(&nodes, &edges, &[], 0.85, 50);
        let focused = pagerank(&nodes, &edges, &["a".into()], 0.85, 50);
        assert!(
            focused["a"] > base["a"],
            "focusing a should raise its rank: base={base:?} focused={focused:?}"
        );
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_symbols_test_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_ctags_script(dir: &Path, name: &str, version_line: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "printf '%s\\n' \"{version_line}\"").unwrap();
        let mut perms = file.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn ctags_backend_prefers_first_usable_universal_candidate() {
        let dir = scratch_dir("ctags_prefers_universal");
        let bsd = write_ctags_script(&dir, "bsd-ctags", "ctags from BSD");
        let universal = write_ctags_script(&dir, "universal-ctags", "Universal Ctags 6.2.1");

        let selected = ctags_backend_from_candidates(None, vec![bsd, universal.clone()]).unwrap();
        assert_eq!(selected, universal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctags_backend_respects_usable_explicit_override() {
        let dir = scratch_dir("ctags_explicit_ok");
        let explicit = write_ctags_script(&dir, "explicit-ctags", "Universal Ctags 6.2.1");
        let other = write_ctags_script(&dir, "other-ctags", "Universal Ctags 6.1.0");

        let selected = ctags_backend_from_candidates(Some(explicit.clone()), vec![other]).unwrap();
        assert_eq!(selected, explicit);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctags_backend_rejects_invalid_explicit_override() {
        let dir = scratch_dir("ctags_explicit_bad");
        let explicit = write_ctags_script(&dir, "bsd-ctags", "ctags from BSD");

        let err = ctags_backend_from_candidates(Some(explicit.clone()), Vec::<PathBuf>::new())
            .unwrap_err();
        match err {
            SymbolError::Scan(msg) => {
                assert!(msg.contains("SHUTTLE_CTAGS_BIN is not a usable Universal Ctags binary"));
                assert!(msg.contains(&explicit.display().to_string()));
            }
            other => panic!("expected SymbolError::Scan, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sym(name: &str, path: &str, sig: &str) -> Symbol {
        Symbol {
            name: name.into(),
            kind: SymbolKind::Function,
            scope: None,
            path: path.into(),
            line: 1,
            signature: Some(sig.into()),
        }
    }

    #[test]
    fn repo_map_from_ranks_referenced_symbol_first() {
        let table = SymbolTable {
            symbols: vec![
                sym("helper", "lib.rs", "fn helper()"),
                sym("main", "main.rs", "fn main()"),
            ],
        };
        // main.rs references `helper` twice; lib.rs references nothing.
        let refs = vec![
            (
                "main.rs".to_string(),
                vec!["helper".to_string(), "helper".to_string()],
            ),
            ("lib.rs".to_string(), Vec::new()),
        ];
        let map = repo_map_from(&table, &refs, 1000, &[]);
        assert_eq!(map.entries.len(), 2);
        assert_eq!(map.entries[0].symbol, "helper");
        assert!(map.entries[0].rank > map.entries[1].rank);
    }

    #[test]
    fn repo_map_from_respects_token_budget() {
        let table = SymbolTable {
            symbols: vec![
                sym("helper", "lib.rs", "fn helper()"),
                sym("main", "main.rs", "fn main()"),
            ],
        };
        let refs = vec![
            ("main.rs".to_string(), vec!["helper".to_string()]),
            ("lib.rs".to_string(), Vec::new()),
        ];
        // Budget 0 admits nothing (every entry costs >= 2 tokens).
        assert!(repo_map_from(&table, &refs, 0, &[]).entries.is_empty());
        // A tiny budget admits only the single highest-ranked entry.
        let small = repo_map_from(&table, &refs, 5, &[]);
        assert_eq!(small.entries.len(), 1);
        assert_eq!(small.entries[0].symbol, "helper");
    }

    fn edit(path: &str, line: usize, name: &str) -> RenameEdit {
        RenameEdit {
            path: path.into(),
            line,
            old: name.into(),
            new: format!("{name}_new"),
        }
    }

    #[test]
    fn retain_genuine_drops_only_provable_false_matches() {
        let candidates = vec![
            edit("s.rs", 2, "path"), // genuine identifier line
            edit("s.rs", 3, "path"), // struct field — not genuine
            edit("x.py", 9, "path"), // file not analyzed -> kept
        ];
        let mut genuine: BTreeMap<(String, String), Option<BTreeSet<usize>>> = BTreeMap::new();
        // s.rs analyzed: only line 2 is a real identifier.
        genuine.insert(
            ("s.rs".into(), "path".into()),
            Some([2usize].into_iter().collect()),
        );
        // x.py absent from the map -> treated as unanalyzed (kept).
        let kept = retain_genuine(candidates, &genuine);
        let lines: Vec<_> = kept.iter().map(|e| (e.path.as_str(), e.line)).collect();
        assert_eq!(lines, vec![("s.rs", 2), ("x.py", 9)]);
    }

    #[test]
    fn retain_genuine_keeps_all_when_analysis_unavailable() {
        let candidates = vec![edit("a.rs", 1, "foo"), edit("a.rs", 5, "foo")];
        let mut genuine: BTreeMap<(String, String), Option<BTreeSet<usize>>> = BTreeMap::new();
        genuine.insert(("a.rs".into(), "foo".into()), None); // backend unavailable
        assert_eq!(retain_genuine(candidates, &genuine).len(), 2);
    }

    #[test]
    fn parse_ast_grep_lines_normalizes_and_empties() {
        assert!(parse_ast_grep_lines("").unwrap().is_empty());
        assert!(parse_ast_grep_lines("[]").unwrap().is_empty());
        let json = r#"[{"range":{"start":{"line":1,"column":3}}},{"range":{"start":{"line":3,"column":0}}}]"#;
        let lines = parse_ast_grep_lines(json).unwrap();
        // 0-indexed 1 & 3 -> 1-indexed 2 & 4.
        assert_eq!(lines, [2usize, 4].into_iter().collect());
    }

    #[test]
    fn parse_ast_grep_lines_rejects_non_array() {
        assert!(parse_ast_grep_lines("{\"not\":\"array\"}").is_err());
    }

    #[test]
    fn lang_for_path_maps_known_and_skips_unknown() {
        assert_eq!(lang_for_path("a.rs"), Some("rust"));
        assert_eq!(lang_for_path("b.py"), Some("python"));
        assert_eq!(lang_for_path("c.tsx"), Some("tsx"));
        // built-in ast-grep grammars added for the nav layer.
        assert_eq!(lang_for_path("s.sh"), Some("bash"));
        assert_eq!(lang_for_path("s.bash"), Some("bash"));
        assert_eq!(lang_for_path("k.yaml"), Some("yaml"));
        assert_eq!(lang_for_path("k.yml"), Some("yaml"));
        assert_eq!(lang_for_path("d.json"), Some("json"));
        // customLanguages candidates: mapped for dispatch even before a grammar
        // is registered (ast-grep errors -> caller falls back conservatively).
        assert_eq!(lang_for_path("i.thrift"), Some("thrift"));
        assert_eq!(lang_for_path("c.toml"), Some("toml"));
        assert_eq!(lang_for_path("c.ini"), Some("ini"));
        assert_eq!(lang_for_path("d.unknownext"), None);
        assert_eq!(lang_for_path("noext"), None);
    }

    // --- M1 language extension: Thrift / INI ctags kind normalization ---

    /// Real universal-ctags JSON shape for a Thrift IDL file (captured from
    /// `ctags --output-format=json --fields=+n+K+s svc.thrift`).
    const THRIFT_TAGS: &str = concat!(
        r#"{"_type":"tag","name":"demo","path":"svc.thrift","line":1,"kind":"namespace","pattern":"/^namespace go demo$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"User","path":"svc.thrift","line":3,"kind":"struct","pattern":"/^struct User {$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"id","path":"svc.thrift","line":4,"kind":"member","scope":"User","scopeKind":"struct","pattern":"/^  1: i64 id$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"Color","path":"svc.thrift","line":8,"kind":"enum","pattern":"/^enum Color {$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"RED","path":"svc.thrift","line":9,"kind":"enumerator","scope":"Color","scopeKind":"enum","pattern":"/^  RED = 1$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"UserService","path":"svc.thrift","line":13,"kind":"service","pattern":"/^service UserService {$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"getUser","path":"svc.thrift","line":14,"kind":"function","scope":"UserService","scopeKind":"service","pattern":"/^  User getUser(1: i64 id)$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"MAX","path":"svc.thrift","line":17,"kind":"const","pattern":"/^const i32 MAX = 100$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"Timestamp","path":"svc.thrift","line":18,"kind":"typedef","pattern":"/^typedef i64 Timestamp$/"}"#,
        "\n",
    );

    /// Real universal-ctags JSON shape for an INI file (`Iniconf` parser).
    const INI_TAGS: &str = concat!(
        r#"{"_type":"tag","name":"section_one","path":"app.ini","line":1,"kind":"section","pattern":"/^[section_one]$/"}"#,
        "\n",
        r#"{"_type":"tag","name":"key_a","path":"app.ini","line":2,"kind":"key","scope":"section_one","scopeKind":"section","pattern":"/^key_a = 1$/"}"#,
        "\n",
    );

    #[test]
    fn map_kind_normalizes_thrift_and_ini_kinds() {
        // Thrift-specific + config kinds all get first-class variants now,
        // instead of collapsing into Other.
        assert_eq!(map_kind("service"), SymbolKind::Service);
        assert_eq!(map_kind("typedef"), SymbolKind::TypeAlias);
        assert_eq!(map_kind("member"), SymbolKind::Field);
        assert_eq!(map_kind("field"), SymbolKind::Field);
        assert_eq!(map_kind("namespace"), SymbolKind::Module);
        assert_eq!(map_kind("enumerator"), SymbolKind::Enum);
        assert_eq!(map_kind("section"), SymbolKind::Section);
        assert_eq!(map_kind("table"), SymbolKind::Section);
        assert_eq!(map_kind("key"), SymbolKind::Key);
        assert_eq!(map_kind("qkey"), SymbolKind::Key);
        // A callable method stays a Method (no longer aliased to member).
        assert_eq!(map_kind("method"), SymbolKind::Method);
        // Genuinely unknown kinds still fold into Other (conservative).
        assert_eq!(map_kind("annotationdecl"), SymbolKind::Other);
    }

    #[test]
    fn parse_thrift_tags_carries_service_typedef_and_members() {
        let t = parse_ctags_json(THRIFT_TAGS).unwrap();
        // Every non-pseudo tag is retained.
        assert_eq!(t.symbols.len(), 9);

        let svc = t.defs("UserService");
        assert_eq!(svc.len(), 1);
        assert_eq!(svc[0].kind, SymbolKind::Service);

        let alias = t.defs("Timestamp");
        assert_eq!(alias[0].kind, SymbolKind::TypeAlias);

        let member = t.defs("id");
        assert_eq!(member[0].kind, SymbolKind::Field);
        assert_eq!(member[0].scope.as_deref(), Some("User"));

        // An RPC method under a service is still a Function (ctags reports it so).
        let rpc = t.defs("getUser");
        assert_eq!(rpc[0].kind, SymbolKind::Function);
        assert_eq!(rpc[0].scope.as_deref(), Some("UserService"));

        // No thrift symbol silently degraded to Other.
        assert!(
            !t.symbols.iter().any(|s| s.kind == SymbolKind::Other),
            "thrift kinds must all normalize: {:?}",
            t.symbols
                .iter()
                .filter(|s| s.kind == SymbolKind::Other)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ini_tags_carries_section_and_key() {
        let t = parse_ctags_json(INI_TAGS).unwrap();
        assert_eq!(t.symbols.len(), 2);
        assert_eq!(t.defs("section_one")[0].kind, SymbolKind::Section);
        let key = t.defs("key_a");
        assert_eq!(key[0].kind, SymbolKind::Key);
        assert_eq!(key[0].scope.as_deref(), Some("section_one"));
    }

    #[test]
    fn thrift_kinds_survive_serde_roundtrip() {
        // The new SymbolKind variants must serialize/deserialize (they flow
        // through repo-map JSON and the disk cache).
        for (k, wire) in [
            (SymbolKind::Service, "service"),
            (SymbolKind::TypeAlias, "typealias"),
            (SymbolKind::Field, "field"),
            (SymbolKind::Section, "section"),
            (SymbolKind::Key, "key"),
        ] {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{wire}\""), "wire form for {k:?}");
            let back: SymbolKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    /// Live-ctags E2E: scan a real Thrift file on disk and confirm the symbols
    /// (service, struct, typedef, member) come through with normalized kinds.
    /// Skipped when universal-ctags is unavailable or its build has no Thrift
    /// parser (matches the nav-layer guard).
    #[test]
    fn scan_thrift_file_end_to_end() {
        if ctags_backend().is_err() {
            eprintln!("universal-ctags unavailable; skipping thrift E2E");
            return;
        }
        if !ctags_supports_thrift() {
            eprintln!("ctags build has no Thrift parser; skipping thrift E2E");
            return;
        }
        let dir = scratch_dir("scan_thrift");
        let src = "namespace go demo\n\nstruct User {\n  1: i64 id\n  2: string name\n}\n\nservice UserService {\n  User getUser(1: i64 id)\n}\n\ntypedef i64 Timestamp\n";
        std::fs::write(dir.join("svc.thrift"), src).unwrap();
        let table = SymbolTable::scan(dir.to_str().unwrap()).unwrap();

        let svc = table.defs("UserService");
        assert_eq!(svc.len(), 1, "service must be tagged: {:?}", table.symbols);
        assert_eq!(svc[0].kind, SymbolKind::Service);
        assert_eq!(table.defs("Timestamp")[0].kind, SymbolKind::TypeAlias);
        assert_eq!(table.defs("User")[0].kind, SymbolKind::Struct);
        assert_eq!(table.defs("id")[0].kind, SymbolKind::Field);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live-ctags E2E: bash functions are tagged (built-in `Sh` parser), so the
    /// `.sh`/`.bash` extension is fully backed on the ctags side too — not just
    /// the ast-grep dispatch side.
    #[test]
    fn scan_bash_file_end_to_end() {
        if ctags_backend().is_err() {
            eprintln!("universal-ctags unavailable; skipping bash E2E");
            return;
        }
        let dir = scratch_dir("scan_bash");
        let src = "#!/bin/bash\nGLOBAL=1\nfunction deploy() {\n  echo hi\n}\nrollback() {\n  echo bye\n}\n";
        std::fs::write(dir.join("deploy.sh"), src).unwrap();
        let table = SymbolTable::scan(dir.to_str().unwrap()).unwrap();

        let deploy = table.defs("deploy");
        assert_eq!(
            deploy.len(),
            1,
            "bash fn must be tagged: {:?}",
            table.symbols
        );
        assert_eq!(deploy[0].kind, SymbolKind::Function);
        assert_eq!(table.defs("rollback")[0].kind, SymbolKind::Function);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ast_grep_ok() -> bool {
        Command::new("ast-grep")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// The Thrift parser is not present in every Universal Ctags build (the
    /// 2021-era package shipped by some distros predates it). Probe the build
    /// itself rather than assuming the binary implies the parser.
    fn ctags_supports_thrift() -> bool {
        let Ok(ctags) = ctags_backend() else {
            return false;
        };
        Command::new(ctags)
            .arg("--list-kinds-full=Thrift")
            .output()
            .map(|o| {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            })
            .unwrap_or(false)
    }

    #[test]
    fn identifier_kinds_table_is_verified_and_conservative() {
        // Languages with a known identifier concept expose a non-empty table;
        // yaml/json and unknown ids expose none (caller falls back).
        assert!(identifier_kinds("rust").contains(&"type_identifier"));
        assert!(identifier_kinds("python").contains(&"identifier"));
        assert!(!identifier_kinds("python").contains(&"type_identifier"));
        assert!(identifier_kinds("bash").contains(&"variable_name"));
        assert!(!identifier_kinds("bash").contains(&"identifier"));
        assert!(identifier_kinds("yaml").is_empty());
        assert!(identifier_kinds("json").is_empty());
        assert!(identifier_kinds("no-such-lang").is_empty());
    }

    #[test]
    fn identifiers_in_fragment_none_for_empty_kind_table() {
        // yaml has no identifier kinds -> None (cannot decide), never empty set.
        assert!(identifiers_in_fragment("key: value", "yaml").is_none());
    }

    #[test]
    fn identifiers_in_fragment_rust_partial_body() {
        if !ast_grep_ok() {
            return;
        }
        // A partial body (no enclosing fn) still yields its identifier names.
        let got = identifiers_in_fragment("    let total = compute(items);\n", "rust")
            .expect("rust fragment should parse");
        assert!(got.contains("total"));
        assert!(got.contains("compute"));
        assert!(got.contains("items"));
    }

    #[test]
    fn identifiers_in_fragment_python_no_type_identifier_kind() {
        if !ast_grep_ok() {
            return;
        }
        // Python grammar lacks `type_identifier`; the table must still work
        // (regression guard: a hardcoded combined rule errored here before).
        let got = identifiers_in_fragment("result = handler(req)\n", "python")
            .expect("python fragment should parse");
        assert!(got.contains("result"));
        assert!(got.contains("handler"));
        assert!(got.contains("req"));
    }

    #[test]
    fn identifiers_in_fragment_disjoint_vs_overlapping() {
        if !ast_grep_ok() {
            return;
        }
        let a = identifiers_in_fragment("    let a = foo();\n", "rust").unwrap();
        let b = identifiers_in_fragment("    let b = bar();\n", "rust").unwrap();
        assert!(
            a.is_disjoint(&b),
            "distinct locals must be disjoint: {a:?} vs {b:?}"
        );
        let c = identifiers_in_fragment("    let total = compute() + tax;\n", "rust").unwrap();
        let d = identifiers_in_fragment("    let total = compute() - fee;\n", "rust").unwrap();
        assert!(!c.is_disjoint(&d), "shared `total`/`compute` must overlap");
    }
}
