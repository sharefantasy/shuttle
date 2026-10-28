//! `shuttle context-pack` — ② merged context-pack subcommand.
//!
//! Sinks the context-pack orchestration that used to live in `shuttle-tools.ts`
//! (resolve depends_on → intersect touch_hint → repomap --focus) down into a
//! single Rust process. One `SymbolTable::scan` is built and reused for the
//! whole pack, so the plugin spawns `shuttle` ONCE instead of a git-diff spawn plus
//! a repomap spawn (kills W-B). Pure symbol table + git; zero model tokens.
//!
//! Output contract (stdout JSON) mirrors the old plugin payload so downstream
//! consumers are unaffected:
//!   { summary, task, source, predecessors, shared_handoff_files,
//!     upstream_only_files, focus, contract_symbols }

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use shuttle_protocol::anchor::Sha256Digest;
use shuttle_protocol::{
    ObservedSymbolEvidence, PlannedSymbolEvidence, SymbolCoverage, SymbolFootprint,
    SymbolFootprintBasis, SymbolIdentity, SymbolIndexFreshness, SymbolRepositoryBase,
    TaskSymbolFootprint, UnresolvedSymbol, UnresolvedSymbolReason,
};
use shuttle_refactor::type_closure::{
    budget_pack, resolve_type_closure, ClosureOptions, ContextSlice,
};

const MAX_SYMBOL_INDEX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Normalize a path the same way the TS `normPath` did: backslashes → `/`,
/// strip a leading `./`, then trim surrounding whitespace.
fn norm_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.strip_prefix("./").unwrap_or(&s);
    s.trim().to_string()
}

/// Minimal YAML list reader for task-*.yaml, byte-for-byte behaviour-compatible
/// with the TS `parseYamlList`. Handles the inline `key: [a, b]` form and the
/// block form:
///   key:
///     - a
///     - b
/// Good enough for the flat `depends_on` / `touch_hint` fields the splitter
/// emits; deliberately NOT a general YAML parser.
fn parse_yaml_list(text: &str, key: &str) -> Vec<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let strip_quotes = |t: &str| -> String {
        let t = t.trim();
        let b = t.as_bytes();
        if b.len() >= 2
            && (b[0] == b'"' || b[0] == b'\'')
            && (b[b.len() - 1] == b'"' || b[b.len() - 1] == b'\'')
        {
            t[1..t.len() - 1].to_string()
        } else {
            // TS regex only strips a single leading and single trailing quote.
            let mut s = t.to_string();
            if s.starts_with('"') || s.starts_with('\'') {
                s.remove(0);
            }
            if s.ends_with('"') || s.ends_with('\'') {
                s.pop();
            }
            s
        }
    };

    for i in 0..lines.len() {
        let line = lines[i];
        // Match `^<key>\s*:\s*(.*)$`.
        let trimmed_start = line.trim_start();
        // The TS regex is anchored at start-of-line with no leading-space
        // allowance, so require the key at column 0 (after no indentation).
        if !line.starts_with(key) {
            continue;
        }
        let after_key = &line[key.len()..];
        let after_key_ws = after_key.trim_start();
        if !after_key_ws.starts_with(':') {
            continue;
        }
        let rest = after_key_ws[1..].trim();
        let _ = trimmed_start;

        // Inline form: key: [a, b, c]
        if let Some(inner) = rest.strip_prefix('[') {
            let inner = inner.strip_suffix(']').unwrap_or(inner);
            return inner
                .split(',')
                .map(&strip_quotes)
                .filter(|t| !t.is_empty())
                .collect();
        }

        // Block form: subsequent `  - item` lines.
        let mut out: Vec<String> = Vec::new();
        #[allow(clippy::needless_range_loop)]
        for j in (i + 1)..lines.len() {
            let lj = lines[j];
            let ljt = lj.trim_start();
            if let Some(item) = ljt.strip_prefix('-') {
                // TS requires `-` followed by whitespace then content.
                if item.starts_with(char::is_whitespace) {
                    out.push(strip_quotes(item.trim()));
                    continue;
                }
            }
            // Not a list item: decide whether to break or skip a blank line.
            let starts_nonspace = lj
                .chars()
                .next()
                .map(|c| !c.is_whitespace())
                .unwrap_or(false);
            let looks_like_key = {
                // `^\s*[A-Za-z_]+\s*:`
                let t = lj.trim_start();
                let key_len = t
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
                    .count();
                key_len > 0 && t[key_len..].trim_start().starts_with(':')
            };
            if starts_nonspace || looks_like_key {
                break;
            }
            if lj.trim().is_empty() {
                continue;
            }
            break;
        }
        return out;
    }
    Vec::new()
}

#[derive(Default)]
struct TaskMetadata {
    depends_on: Vec<String>,
    touch_hint: Vec<String>,
    /// Verified later against the fresh index. The compact YAML encoding is
    /// `name@repo/relative/path`; a bare name is never accepted because it
    /// cannot disambiguate duplicate definitions.
    write_symbols: Vec<String>,
}

/// Read the small, flat portion of `<tasks_dir>/<tid>.yaml` used here.
fn read_task_yaml(tasks_dir: &Option<String>, tid: &str) -> Option<TaskMetadata> {
    let dir = tasks_dir.as_ref()?;
    let path = Path::new(dir).join(format!("{tid}.yaml"));
    let text = std::fs::read_to_string(path).ok()?;
    Some(TaskMetadata {
        depends_on: parse_yaml_list(&text, "depends_on"),
        touch_hint: parse_yaml_list(&text, "touch_hint"),
        write_symbols: parse_yaml_list(&text, "write_symbols"),
    })
}

/// Push `v` onto `acc` iff not already present (insertion-order dedup, matching
/// the JS `Set` iteration order the TS relied on).
fn push_unique(acc: &mut Vec<String>, v: String) {
    if !acc.contains(&v) {
        acc.push(v);
    }
}

/// Serde view of one packed closure node (`ClosureNode` isn't `Serialize`).
#[derive(Serialize)]
struct NodeView {
    symbol: String,
    path: String,
    line: usize,
    distance: usize,
    ref_count: usize,
    confidence: &'static str,
    est_tokens: usize,
}

/// Serde view of a budget-dropped node.
#[derive(Serialize)]
struct DroppedView {
    symbol: String,
    path: String,
    score: f64,
    est_tokens: usize,
    reason: &'static str,
}

/// Serde view of an unresolved type name (never guessed, §7).
#[derive(Serialize)]
struct UnresolvedView {
    symbol: String,
    reason: &'static str,
    distance: usize,
}

/// The `--type-closure` extra payload: the budget-packed minimal-sufficient
/// context slice plus the dropped / unresolved / ambiguous tails. Emitted ONLY
/// when the type-closure mode is requested; otherwise absent (backward compat).
#[derive(Serialize)]
struct TypeClosureView {
    root: String,
    root_path: String,
    included: Vec<NodeView>,
    dropped: Vec<DroppedView>,
    unresolved: Vec<UnresolvedView>,
    ambiguous: Vec<String>,
    used_tokens: usize,
    budget_tokens: usize,
}

fn conf_str(c: shuttle_nav::Confidence) -> &'static str {
    match c {
        shuttle_nav::Confidence::High => "high",
        shuttle_nav::Confidence::Medium => "medium",
        shuttle_nav::Confidence::Low => "low",
    }
}

/// Lower a packed `ContextSlice` into its serde view.
fn view_of(slice: &ContextSlice, root_path: &str) -> TypeClosureView {
    TypeClosureView {
        root: slice.root.clone(),
        root_path: root_path.to_string(),
        included: slice
            .included
            .iter()
            .map(|n| NodeView {
                symbol: n.symbol.clone(),
                path: n.path.clone(),
                line: n.line,
                distance: n.distance,
                ref_count: n.ref_count,
                confidence: conf_str(n.confidence),
                est_tokens: n.est_tokens,
            })
            .collect(),
        dropped: slice
            .dropped
            .iter()
            .map(|d| DroppedView {
                symbol: d.symbol.clone(),
                path: d.path.clone(),
                score: d.score,
                est_tokens: d.est_tokens,
                reason: d.reason,
            })
            .collect(),
        unresolved: slice
            .unresolved
            .iter()
            .map(|u| UnresolvedView {
                symbol: u.symbol.clone(),
                reason: u.reason,
                distance: u.distance,
            })
            .collect(),
        ambiguous: slice.ambiguous.clone(),
        used_tokens: slice.used_tokens,
        budget_tokens: slice.budget_tokens,
    }
}

/// One reverse-dependency hit surfaced in the pack: a symbol that transitively
/// references something defined in the focus files, tagged with the weakest
/// confidence along the path. This is the daemonless "who else must I look at"
/// signal — the same blast radius `shuttle impact` exposes, folded into the pack so
/// a single `shuttle context-pack` spawn also answers the impact question (R3/R4).
#[derive(Serialize)]
struct GraphImpactHitView {
    symbol: String,
    path: String,
    line: usize,
    distance: usize,
    confidence: String,
}

/// One directed reference edge `from -> to` among the graph-impact node set,
/// tagged with the weakest-hop confidence. This is the SCIP DAG-edge value
/// (R3): both endpoints are guaranteed to appear in `seeds` ∪ `impacted`, so
/// the slice is self-consistent — a consumer can draw the local dependency
/// subgraph without any dangling node.
#[derive(Serialize)]
struct SymbolEdgeView {
    from: String,
    to: String,
    confidence: String,
}

/// The graph-derived enrichment attached in `--graph-impact` mode: the edges
/// out of the focus symbols plus the reverse-dependency closure (blast radius)
/// and a derived `touch_hint` — the deduped set of files that closure lands in,
/// beyond what metadata already named. Present ONLY when enabled; absent keeps
/// the legacy contract byte-for-byte (§7 backward compat).
#[derive(Serialize)]
struct GraphImpactView {
    /// Seed symbols = every symbol defined in a focus file.
    seeds: Vec<String>,
    /// Reverse-dependency closure hits (excludes the seeds themselves).
    impacted: Vec<GraphImpactHitView>,
    /// Files the closure lands in that were NOT already in `focus` — the
    /// graph-derived addition to `touch_hint`.
    derived_touch_hint: Vec<String>,
    /// Reference edges among the seed ∪ impacted symbol set (R3 DAG value).
    /// Every endpoint is present in `seeds` or `impacted`.
    symbol_edges: Vec<SymbolEdgeView>,
}

fn graph_confidence_str(c: shuttle_symbols::graph::GraphConfidence) -> &'static str {
    match c {
        shuttle_symbols::graph::GraphConfidence::High => "high",
        shuttle_symbols::graph::GraphConfidence::Medium => "medium",
        shuttle_symbols::graph::GraphConfidence::Low => "low",
    }
}

fn edge_kind_str(kind: shuttle_symbols::graph::EdgeKind) -> &'static str {
    match kind {
        shuttle_symbols::graph::EdgeKind::ImportDirected => "import_directed",
        shuttle_symbols::graph::EdgeKind::ContractDirected => "contract_directed",
        shuttle_symbols::graph::EdgeKind::NameMatch => "name_match",
        shuttle_symbols::graph::EdgeKind::CoChange => "co_change",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct HandoffSymbolView {
    /// Producer/provider symbol (the callee of the underlying reference edge).
    from_sym: SymbolIdentity,
    /// Consumer symbol (the owner of the underlying reference edge).
    to_sym: SymbolIdentity,
    confidence: String,
    kind: String,
}

#[derive(Debug, Default)]
struct ResolvedEvidence {
    symbols: Vec<SymbolIdentity>,
    unresolved: Vec<UnresolvedSymbol>,
}

#[derive(Debug, Deserialize)]
struct SymbolIndexRefInput {
    schema_version: String,
    status: String,
    #[serde(default)]
    artifact_ref: Option<String>,
    #[serde(default)]
    symbol_index_digest: Option<String>,
    #[serde(default)]
    base_oid: Option<String>,
    #[serde(default)]
    coverage: Option<String>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug)]
struct IndexInput {
    table: Option<Arc<shuttle_symbols::SymbolTable>>,
    status: String,
    artifact_ref: String,
    digest: String,
    base_oid: String,
    coverage: String,
    truncated: bool,
}

fn path_matches(symbol_path: &str, task_path: &str) -> bool {
    let symbol_path = norm_path(symbol_path);
    let task_path = norm_path(task_path);
    symbol_path == task_path
        || symbol_path.ends_with(&format!("/{task_path}"))
        || task_path.ends_with(&format!("/{symbol_path}"))
}

fn digest_ref(bytes: &[u8]) -> String {
    format!("sha256:{}", shuttle_protocol::sha256_hex(bytes))
}

fn read_symbol_index_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let observed = std::fs::metadata(path).ok()?.len();
    if observed > MAX_SYMBOL_INDEX_INPUT_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(observed as usize);
    file.take(MAX_SYMBOL_INDEX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_SYMBOL_INDEX_INPUT_BYTES).then_some(bytes)
}

/// Bind JSON artifacts by their canonical logical value rather than incidental
/// whitespace or the producer's trailing newline. Closed consumers in other
/// repositories recompute this same sorted-key compact representation.
fn canonical_json_digest(bytes: &[u8]) -> Option<String> {
    fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut ordered = serde_json::Map::new();
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort();
                for key in keys {
                    ordered.insert(key.clone(), canonicalize(&map[key]));
                }
                serde_json::Value::Object(ordered)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(canonicalize).collect())
            }
            _ => value.clone(),
        }
    }

    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let encoded = serde_json::to_vec(&canonicalize(&value)).ok()?;
    Some(digest_ref(&encoded))
}

fn repo_id(root: &str) -> Option<String> {
    if let Ok(value) = std::env::var("SHUTTLE_REPO_ID") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git).ok()?;
    Command::new(binary)
        .args(["-C", root, "remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn symbol_ref(
    symbol: &shuttle_symbols::Symbol,
    repo_id: &str,
    base_oid: &str,
) -> Option<SymbolIdentity> {
    let path = norm_path(&symbol.path);
    let language = shuttle_symbols::lang_for_path(&path)?.to_string();
    let kind = serde_json::to_value(&symbol.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".to_string());
    SymbolIdentity::from_signature_bytes(
        repo_id,
        base_oid,
        language,
        path,
        symbol.name.clone(),
        kind,
        symbol.scope.clone(),
        symbol.line as u64,
        symbol.signature.as_deref().unwrap_or_default().as_bytes(),
    )
    .ok()
}

/// Resolve only explicit NAME@PATH evidence. A seed is accepted exactly when
/// the fresh table contains one matching definition; ambiguity and absence are
/// both fail-safe omissions.
fn resolve_write_evidence(
    table: &shuttle_symbols::SymbolTable,
    raw: &[String],
    repo_id: &str,
    base_oid: &str,
) -> ResolvedEvidence {
    let mut resolved = BTreeSet::new();
    let mut unresolved = BTreeSet::new();
    for seed in raw {
        let Some((name, path)) = seed.split_once('@') else {
            unresolved.insert(UnresolvedSymbol {
                declaration: seed.clone(),
                reason: UnresolvedSymbolReason::InvalidDeclaration,
            });
            continue;
        };
        let matches: Vec<&shuttle_symbols::Symbol> = table
            .symbols
            .iter()
            .filter(|symbol| symbol.name == name && path_matches(&symbol.path, path))
            .collect();
        if matches.len() == 1 {
            match symbol_ref(matches[0], repo_id, base_oid) {
                Some(identity) => {
                    resolved.insert(identity);
                }
                None => {
                    unresolved.insert(UnresolvedSymbol {
                        declaration: seed.clone(),
                        reason: UnresolvedSymbolReason::Unsupported,
                    });
                }
            }
        } else {
            unresolved.insert(UnresolvedSymbol {
                declaration: seed.clone(),
                reason: if matches.is_empty() {
                    UnresolvedSymbolReason::NotFound
                } else {
                    UnresolvedSymbolReason::Ambiguous
                },
            });
        }
    }
    ResolvedEvidence {
        symbols: resolved.into_iter().collect(),
        unresolved: unresolved.into_iter().collect(),
    }
}

fn derive_symbol_truth(
    table: &shuttle_symbols::SymbolTable,
    root: &str,
    current_writes: &[SymbolIdentity],
    predecessor_writes: &[SymbolIdentity],
    repo_id: &str,
    base_oid: &str,
) -> Result<(Vec<SymbolIdentity>, Vec<HandoffSymbolView>), shuttle_symbols::SymbolError> {
    let graph = shuttle_symbols::graph::SymbolGraph::try_from_scan_root(table, root)?;
    let unique_defs: BTreeMap<&str, &shuttle_symbols::Symbol> = table
        .symbols
        .iter()
        .filter_map(|symbol| {
            let defs = table.defs(&symbol.name);
            (defs.len() == 1).then_some((symbol.name.as_str(), defs[0]))
        })
        .collect();
    let current_names: BTreeSet<&str> = current_writes.iter().map(|s| s.name.as_str()).collect();
    let predecessor_names: BTreeSet<&str> =
        predecessor_writes.iter().map(|s| s.name.as_str()).collect();
    let mut reads = BTreeSet::new();
    let mut handoffs = BTreeSet::new();

    for (owner, callee, confidence, kind) in graph.edges_with_kind() {
        if !current_names.contains(owner) {
            continue;
        }
        let (Some(owner_def), Some(callee_def)) = (
            unique_defs.get(owner).copied(),
            unique_defs.get(callee).copied(),
        ) else {
            continue;
        };
        if owner_def.path == callee_def.path && owner == callee {
            continue;
        }
        let Some(callee_ref) = symbol_ref(callee_def, repo_id, base_oid) else {
            continue;
        };
        reads.insert(callee_ref.clone());
        if predecessor_names.contains(callee) && kind.is_trusted_directed() {
            let Some(owner_ref) = symbol_ref(owner_def, repo_id, base_oid) else {
                continue;
            };
            handoffs.insert(HandoffSymbolView {
                from_sym: callee_ref,
                to_sym: owner_ref,
                confidence: graph_confidence_str(confidence).to_string(),
                kind: edge_kind_str(kind).to_string(),
            });
        }
    }
    Ok((reads.into_iter().collect(), handoffs.into_iter().collect()))
}

fn read_index_input(root: &str, input_path: &str, requested_head: &str) -> IndexInput {
    let input_bytes = match read_symbol_index_bytes(Path::new(input_path)) {
        Some(bytes) => bytes,
        None => {
            return IndexInput {
                table: None,
                status: "absent".to_string(),
                artifact_ref: input_path.to_string(),
                digest: String::new(),
                base_oid: requested_head.to_string(),
                coverage: "none".to_string(),
                truncated: false,
            };
        }
    };
    let direct_digest =
        canonical_json_digest(&input_bytes).unwrap_or_else(|| digest_ref(&input_bytes));
    let reference = serde_json::from_slice::<SymbolIndexRefInput>(&input_bytes)
        .ok()
        .filter(|value| value.schema_version == "shuttle_symbol_index_ref/v1");
    if let Some(reference) = reference {
        let status = reference.status.to_ascii_lowercase();
        if status != "fresh" || reference.truncated {
            return IndexInput {
                table: None,
                status: if reference.truncated {
                    "truncated".to_string()
                } else {
                    status.clone()
                },
                artifact_ref: input_path.to_string(),
                digest: reference.symbol_index_digest.unwrap_or_default(),
                base_oid: reference
                    .base_oid
                    .unwrap_or_else(|| requested_head.to_string()),
                coverage: reference.coverage.unwrap_or_else(|| "none".to_string()),
                truncated: reference.truncated || status == "truncated",
            };
        }
        let Some(artifact_ref) = reference.artifact_ref else {
            return IndexInput {
                table: None,
                status: "absent".to_string(),
                artifact_ref: input_path.to_string(),
                digest: String::new(),
                base_oid: requested_head.to_string(),
                coverage: "none".to_string(),
                truncated: false,
            };
        };
        let artifact_path = {
            let path = Path::new(&artifact_ref);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                Path::new(input_path)
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            }
        };
        let artifact_bytes = match read_symbol_index_bytes(&artifact_path) {
            Some(bytes) => bytes,
            None => {
                return IndexInput {
                    table: None,
                    status: "absent".to_string(),
                    artifact_ref,
                    digest: String::new(),
                    base_oid: requested_head.to_string(),
                    coverage: "none".to_string(),
                    truncated: false,
                };
            }
        };
        let digest =
            canonical_json_digest(&artifact_bytes).unwrap_or_else(|| digest_ref(&artifact_bytes));
        drop(artifact_bytes);
        if reference
            .symbol_index_digest
            .as_deref()
            .is_some_and(|expected| expected != digest)
            || reference
                .base_oid
                .as_deref()
                .is_some_and(|base| base != requested_head)
        {
            return IndexInput {
                table: None,
                status: "stale".to_string(),
                artifact_ref,
                digest,
                base_oid: reference.base_oid.unwrap_or_default(),
                coverage: reference.coverage.unwrap_or_else(|| "none".to_string()),
                truncated: false,
            };
        }
        let artifact_path_string = artifact_path.to_string_lossy().to_string();
        let read = shuttle_symbols::index::read_artifact_for_revision(
            root,
            &artifact_path_string,
            requested_head,
        );
        return index_input_from_read(
            read,
            artifact_ref,
            digest,
            requested_head,
            reference.coverage.unwrap_or_else(|| "exact".to_string()),
        );
    }
    drop(input_bytes);
    let read = shuttle_symbols::index::read_artifact_for_revision(root, input_path, requested_head);
    index_input_from_read(
        read,
        input_path.to_string(),
        direct_digest,
        requested_head,
        "exact".to_string(),
    )
}

fn index_input_from_read(
    read: shuttle_symbols::index::ArtifactRead,
    artifact_ref: String,
    digest: String,
    requested_head: &str,
    coverage: String,
) -> IndexInput {
    match read {
        shuttle_symbols::index::ArtifactRead::Fresh(table) => IndexInput {
            table: Some(Arc::new(table)),
            status: "fresh".to_string(),
            artifact_ref,
            digest,
            base_oid: requested_head.to_string(),
            coverage,
            truncated: false,
        },
        shuttle_symbols::index::ArtifactRead::Stale { index_base_oid, .. } => IndexInput {
            table: None,
            status: "stale".to_string(),
            artifact_ref,
            digest,
            base_oid: index_base_oid,
            coverage: "none".to_string(),
            truncated: false,
        },
        shuttle_symbols::index::ArtifactRead::Absent => IndexInput {
            table: None,
            status: "absent".to_string(),
            artifact_ref,
            digest: String::new(),
            base_oid: requested_head.to_string(),
            coverage: "none".to_string(),
            truncated: false,
        },
    }
}

/// Build the graph-impact enrichment from the *already-scanned* table (no second
/// scan). Seeds are the symbols defined in `focus` files; the closure is their
/// reverse-dependency BFS. `derived_touch_hint` is the closure's file set minus
/// the files already in `focus`. Returns `None` when there are no seeds.
fn build_graph_impact(
    table: &shuttle_symbols::SymbolTable,
    focus: &[String],
    root: &str,
) -> Result<Option<GraphImpactView>, shuttle_symbols::SymbolError> {
    let focus_ends: Vec<String> = focus.iter().map(|f| norm_path(f)).collect();
    let in_focus = |path: &str| -> bool {
        let ep = norm_path(path);
        focus_ends
            .iter()
            .any(|f| ep == *f || ep.ends_with(&format!("/{f}")) || f.ends_with(&format!("/{ep}")))
    };
    let seeds: Vec<String> = table
        .symbols
        .iter()
        .filter(|s| in_focus(&s.path))
        .map(|s| s.name.clone())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    if seeds.is_empty() {
        return Ok(None);
    }
    let graph = shuttle_symbols::graph::SymbolGraph::try_from_scan_root(table, root)?;
    let radius = graph.blast_radius(&seeds, 64);
    let seed_set: std::collections::HashSet<&String> = seeds.iter().collect();

    let mut impacted: Vec<GraphImpactHitView> = Vec::new();
    let mut derived: Vec<String> = Vec::new();
    for h in radius {
        if h.distance == 0 || seed_set.contains(&h.symbol) {
            continue; // skip the seeds themselves; we want who ELSE is hit
        }
        if !h.path.is_empty() && !in_focus(&h.path) {
            push_unique(&mut derived, norm_path(&h.path));
        }
        impacted.push(GraphImpactHitView {
            symbol: h.symbol,
            path: h.path,
            line: h.line,
            distance: h.distance,
            confidence: graph_confidence_str(h.confidence).to_string(),
        });
    }
    // R3: fold the edges whose BOTH endpoints fall in the node set (seeds plus
    // every impacted symbol). This keeps the slice self-consistent — no edge
    // points at a symbol the consumer can't see.
    let mut node_set: std::collections::HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();
    for h in &impacted {
        node_set.insert(h.symbol.as_str());
    }
    let mut symbol_edges: Vec<SymbolEdgeView> = graph
        .edges()
        .into_iter()
        .filter(|(from, to, _)| node_set.contains(from) && node_set.contains(to))
        .map(|(from, to, conf)| SymbolEdgeView {
            from: from.to_string(),
            to: to.to_string(),
            confidence: graph_confidence_str(conf).to_string(),
        })
        .collect();
    symbol_edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));

    Ok(Some(GraphImpactView {
        seeds,
        impacted,
        derived_touch_hint: derived,
        symbol_edges,
    }))
}

#[derive(Serialize)]
struct ContextPack {
    summary: String,
    task: Option<String>,
    source: String,
    predecessors: Vec<String>,
    shared_handoff_files: Vec<String>,
    upstream_only_files: Vec<String>,
    focus: Vec<String>,
    contract_symbols: Vec<shuttle_symbols::RepoMapEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    read_symbols: Vec<SymbolIdentity>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    write_symbols: Vec<SymbolIdentity>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    handoff_symbols: Vec<HandoffSymbolView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_status: Option<String>,
    /// G10.1 typed producer output. Present only when the caller supplies an
    /// index artifact/ref; legacy context-pack JSON therefore remains unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_footprint: Option<SymbolFootprint>,
    /// Present ONLY in `--type-closure` mode; absent otherwise so the old
    /// output contract is byte-for-byte preserved (§7 backward compat).
    #[serde(skip_serializing_if = "Option::is_none")]
    type_closure_slice: Option<TypeClosureView>,
    /// Present ONLY in `--graph-impact` mode (env SHUTTLE_GRAPH_IMPACT); absent keeps
    /// the legacy output contract byte-for-byte (§7). R3/R4: fold the symbol
    /// graph's edges + blast radius into the pack.
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_impact: Option<GraphImpactView>,
}

/// ② Entry point for `shuttle context-pack`. Mirrors the old TS `shuttle_context_pack`
/// orchestration in ONE process, reusing a single `SymbolTable`.
#[allow(clippy::too_many_arguments)]
pub fn run_context_pack(
    tasks_dir: Option<String>,
    task_id: Option<String>,
    depends_on: Vec<String>,
    touch_hint: Vec<String>,
    git_base: Option<String>,
    symbol_index: Option<String>,
    index_head: Option<String>,
    explicit_write_symbols: Vec<String>,
    budget: usize,
    root: String,
    scan_scope: bool,
) -> anyhow::Result<()> {
    // --- Step 1: resolve this task's depends_on + touch_hint. ---
    let self_meta = task_id
        .as_ref()
        .and_then(|tid| read_task_yaml(&tasks_dir, tid));
    let deps: Vec<String> = if !depends_on.is_empty() {
        depends_on.clone()
    } else {
        self_meta
            .as_ref()
            .map(|m| m.depends_on.clone())
            .unwrap_or_default()
    };
    let self_touch: Vec<String> = if !touch_hint.is_empty() {
        touch_hint.clone()
    } else {
        self_meta
            .as_ref()
            .map(|m| m.touch_hint.clone())
            .unwrap_or_default()
    };
    let self_write_evidence = if !explicit_write_symbols.is_empty() {
        explicit_write_symbols
    } else {
        self_meta
            .as_ref()
            .map(|m| m.write_symbols.clone())
            .unwrap_or_default()
    };

    // --- Step 2: gather predecessor touch files (metadata first). ---
    let mut pred_touch: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &deps {
        if let Some(metadata) = read_task_yaml(&tasks_dir, d) {
            if !metadata.touch_hint.is_empty() {
                pred_touch.insert(d.clone(), metadata.touch_hint);
            }
        }
    }

    // --- Step 3: pick the focus (handoff) files. ---
    let self_set: std::collections::HashSet<String> =
        self_touch.iter().map(|f| norm_path(f)).collect();
    let mut shared: Vec<String> = Vec::new(); // files both sides touch = contract seam
    let mut upstream_only: Vec<String> = Vec::new(); // files only upstream produced
    for files in pred_touch.values() {
        for f in files {
            let nf = norm_path(f);
            if self_set.contains(&nf) {
                push_unique(&mut shared, nf);
            } else {
                push_unique(&mut upstream_only, nf);
            }
        }
    }

    let mut focus: Vec<String> = Vec::new();
    for f in shared.iter().chain(upstream_only.iter()) {
        push_unique(&mut focus, f.clone());
    }
    let mut source = "metadata".to_string();

    // --- Fallback A: no metadata edges -> infer from git diff. ---
    if focus.is_empty() {
        if let Some(base) = &git_base {
            let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
            let out = Command::new(git)
                .args(["-C", &root, "diff", "--name-only", base])
                .output();
            if let Ok(o) = out {
                if o.status.success() {
                    let text = String::from_utf8_lossy(&o.stdout);
                    for l in text.split('\n') {
                        let t = l.trim();
                        if !t.is_empty() {
                            push_unique(&mut focus, norm_path(t));
                        }
                    }
                    source = "git_diff".to_string();
                }
            }
        }
    }

    // --- Fallback B: still nothing -> plain focused repomap on self touch. ---
    if focus.is_empty() {
        for f in &self_touch {
            push_unique(&mut focus, norm_path(f));
        }
        source = "repomap_only".to_string();
    }

    let symbol_paths: Vec<&str> = self_write_evidence
        .iter()
        .filter_map(|seed| seed.split_once('@').map(|(_, path)| path))
        .collect();
    let relevant_paths: Vec<&str> = self_touch
        .iter()
        .map(String::as_str)
        .chain(symbol_paths)
        .collect();
    let unsupported = relevant_paths
        .iter()
        .any(|path| !shuttle_symbols::imports::supports_dependency_path(path));
    let requested_head = index_head
        .or_else(|| shuttle_symbols::index::git_head_oid(&root))
        .unwrap_or_default();
    let resolved_repo_id = repo_id(&root);
    let mut index_input = symbol_index.as_deref().map(|path| {
        if unsupported {
            IndexInput {
                table: None,
                status: "unsupported".to_string(),
                artifact_ref: path.to_string(),
                digest: read_symbol_index_bytes(Path::new(path))
                    .map(|bytes| {
                        canonical_json_digest(&bytes).unwrap_or_else(|| digest_ref(&bytes))
                    })
                    .unwrap_or_default(),
                base_oid: requested_head.clone(),
                coverage: "none".to_string(),
                truncated: false,
            }
        } else {
            read_index_input(&root, path, &requested_head)
        }
    });
    if index_input
        .as_ref()
        .is_some_and(|input| input.table.is_some())
        && resolved_repo_id.is_none()
    {
        // A repository identifier is part of SymbolIdentity. Without one the
        // index is usable as a cache but cannot mint exact portable identity.
        if let Some(input) = &mut index_input {
            input.table = None;
            input.status = "ambiguous".to_string();
            input.coverage = "none".to_string();
        }
    }
    if index_input
        .as_ref()
        .is_some_and(|input| input.table.is_some())
    {
        source = "symbol_index".to_string();
    }

    // --- Step 4: emit a token-budgeted symbol skeleton for the focus set. ---
    // A fresh, revision-bound caller index is already the authoritative table;
    // reuse it instead of doing a second live ctags scan. This removes the eval
    // path that used to index once and then recursively scan the worktree again
    // for every task. Without a usable index, the scan remains fail-closed and
    // resource-bounded by shuttle-symbols.
    let live_table = index_input
        .as_ref()
        .and_then(|input| input.table.clone())
        .map(Ok)
        .unwrap_or_else(|| {
            if scan_scope {
                shuttle_symbols::SymbolTable::scan_scoped(&root, &focus)
            } else {
                shuttle_symbols::SymbolTable::scan(&root)
            }
            .map(Arc::new)
        })?;
    let map = shuttle_symbols::repo_map_root(live_table.as_ref(), budget, &focus, &root)?;

    let mut unresolved = Vec::new();
    let (mut write_symbols, mut read_symbols, mut handoff_symbols) =
        if let (Some(input), Some(repo_id), Some(table)) = (
            &index_input,
            resolved_repo_id.as_deref(),
            index_input.as_ref().and_then(|input| input.table.as_ref()),
        ) {
            let writes =
                resolve_write_evidence(table, &self_write_evidence, repo_id, &input.base_oid);
            let predecessor_evidence: Vec<String> = deps
                .iter()
                .filter_map(|task| read_task_yaml(&tasks_dir, task))
                .flat_map(|metadata| metadata.write_symbols)
                .collect();
            let predecessor_writes =
                resolve_write_evidence(table, &predecessor_evidence, repo_id, &input.base_oid);
            unresolved.extend(writes.unresolved);
            let (reads, handoffs) = derive_symbol_truth(
                table,
                &root,
                &writes.symbols,
                &predecessor_writes.symbols,
                repo_id,
                &input.base_oid,
            )?;
            (writes.symbols, reads, handoffs)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
    if !unresolved.is_empty() {
        if let Some(input) = &mut index_input {
            input.status = if unresolved
                .iter()
                .any(|item| item.reason == UnresolvedSymbolReason::Ambiguous)
            {
                "ambiguous".to_string()
            } else if unresolved
                .iter()
                .any(|item| item.reason == UnresolvedSymbolReason::Unsupported)
            {
                "unsupported".to_string()
            } else {
                "absent".to_string()
            };
            input.coverage = "partial".to_string();
        }
        // Mixed exact/unresolved declarations are not a complete proof. Do not
        // leak the resolvable subset as an apparently exact task footprint.
        write_symbols.clear();
        read_symbols.clear();
        handoff_symbols.clear();
    }
    let index_status = index_input.as_ref().map(|input| input.status.clone());
    let symbol_footprint = index_input.as_ref().and_then(|input| {
        // A closed footprint must bind a real repository, revision and index
        // digest. If any binding is absent or malformed, omit the typed
        // envelope and leave the explicit top-level index_status as the
        // fail-closed signal; never fabricate a digest to make a DTO parse.
        let repo_id = resolved_repo_id.clone()?;
        let symbol_index_digest = Sha256Digest::parse(input.digest.clone()).ok()?;
        let freshness = if input.table.is_some() {
            // Resolution ambiguity does not make the caller-owned index stale;
            // it makes the task declaration unresolved. Keep those axes apart.
            SymbolIndexFreshness::Fresh
        } else {
            match input.status.as_str() {
                "stale" => SymbolIndexFreshness::Stale,
                "unsupported" => SymbolIndexFreshness::Unsupported,
                "timeout" => SymbolIndexFreshness::Timeout,
                _ => SymbolIndexFreshness::Absent,
            }
        };
        let coverage = if input.coverage == "exact"
            && freshness == SymbolIndexFreshness::Fresh
            && !input.truncated
            && unresolved.is_empty()
        {
            SymbolCoverage::Exact
        } else if input.coverage == "partial" || !unresolved.is_empty() {
            SymbolCoverage::Partial
        } else {
            SymbolCoverage::None
        };
        let mut degraded_reasons = Vec::new();
        if input.status != "fresh" {
            degraded_reasons.push(input.status.clone());
        }
        if input.truncated && !degraded_reasons.iter().any(|reason| reason == "truncated") {
            degraded_reasons.push("truncated".to_string());
        }
        let planned = if !unresolved.is_empty() {
            PlannedSymbolEvidence::unresolved(unresolved.clone())
        } else if freshness == SymbolIndexFreshness::Fresh && input.status == "fresh" {
            PlannedSymbolEvidence::validated(read_symbols.clone(), write_symbols.clone())
        } else {
            let reason = match input.status.as_str() {
                "stale" => UnresolvedSymbolReason::Stale,
                "unsupported" => UnresolvedSymbolReason::Unsupported,
                "timeout" => UnresolvedSymbolReason::Timeout,
                "truncated" => UnresolvedSymbolReason::Overflow,
                "ambiguous" => UnresolvedSymbolReason::Ambiguous,
                _ => UnresolvedSymbolReason::Absent,
            };
            PlannedSymbolEvidence::planner_guess(
                self_write_evidence
                    .iter()
                    .cloned()
                    .map(|declaration| UnresolvedSymbol {
                        declaration,
                        reason,
                    })
                    .collect(),
            )
        };
        SymbolFootprint::new(
            SymbolFootprintBasis {
                repositories: vec![SymbolRepositoryBase {
                    repo_id,
                    base_oid: input.base_oid.clone(),
                }],
                symbol_index_ref: input.artifact_ref.clone(),
                symbol_index_digest,
                analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
                coverage,
                freshness,
                truncated: input.truncated,
                degraded_reasons,
            },
            vec![TaskSymbolFootprint::new(
                task_id.clone().unwrap_or_else(|| "(ad-hoc)".to_string()),
                planned,
                ObservedSymbolEvidence::not_observed(),
            )],
        )
        .ok()
    });

    // Trim the repomap to entries inside the focus files — that IS the contract.
    let focus_ends: Vec<String> = focus
        .iter()
        .map(|f| f.strip_prefix("./").unwrap_or(f).to_string())
        .collect();
    let contract: Vec<shuttle_symbols::RepoMapEntry> = map
        .entries
        .into_iter()
        .filter(|e| {
            let ep = e.path.strip_prefix("./").unwrap_or(&e.path);
            focus_ends.iter().any(|f| ep.ends_with(f.as_str()))
        })
        .collect();

    let summary = format!(
        "Context pack for {} [source={}]: {} predecessor(s), {} shared handoff file(s), \
         {} upstream-only file(s); {} contract symbol(s).",
        task_id.clone().unwrap_or_else(|| "(ad-hoc)".to_string()),
        source,
        deps.len(),
        shared.len(),
        upstream_only.len(),
        contract.len(),
    );

    // --- Step 5 (opt): --type-closure mode. Activated via env so the CLI arg
    // surface and `run_context_pack` signature stay byte-identical (the flag is
    // threaded in by the caller without touching this function's contract).
    // When set, we resolve the cross-file type closure of the target symbol,
    // budget-pack it (reusing `budget` as the token budget), and attach the
    // slice. The legacy fields above are left untouched. §7: never guessed —
    // unresolved / ambiguous names are surfaced, not bound.
    let type_closure_slice = match std::env::var("SHUTTLE_TYPE_CLOSURE").ok() {
        Some(sym) if !sym.trim().is_empty() => {
            // Anchor file: explicit env, else the first focus file, else first
            // touch hint. If none is available we cannot anchor -> skip cleanly.
            let anchor_file = std::env::var("SHUTTLE_TYPE_CLOSURE_FILE")
                .ok()
                .filter(|f| !f.trim().is_empty())
                .or_else(|| focus.first().cloned())
                .or_else(|| self_touch.first().map(|f| norm_path(f)));
            match anchor_file {
                Some(file) => {
                    match resolve_type_closure(
                        sym.trim(),
                        &file,
                        &live_table,
                        &ClosureOptions::default(),
                    ) {
                        Ok(graph) => {
                            let slice = budget_pack(&graph, budget);
                            Some(view_of(&slice, &graph.root_path))
                        }
                        // Resolution failure (e.g. root fn not found, unreadable
                        // file) is non-fatal: the legacy pack still emits.
                        Err(_) => None,
                    }
                }
                None => None,
            }
        }
        _ => None,
    };

    // --- Step 6 (opt): --graph-impact mode. Env-gated like --type-closure so
    // the CLI arg surface and this function's signature stay byte-identical.
    // Reuses the SAME `table` scanned above — no second scan. Folds the symbol
    // reference graph (edges + reverse-dependency closure of the focus symbols)
    // into the pack, plus a derived `touch_hint` of files the closure reaches
    // beyond `focus`. §7: nothing guessed — confidence is carried per hit.
    let graph_impact = if std::env::var_os("SHUTTLE_GRAPH_IMPACT").is_some() {
        build_graph_impact(&live_table, &focus, &root)?
    } else {
        None
    };

    let pack = ContextPack {
        summary,
        task: task_id,
        source,
        predecessors: deps,
        shared_handoff_files: shared,
        upstream_only_files: upstream_only,
        focus,
        contract_symbols: contract,
        read_symbols,
        write_symbols,
        handoff_symbols,
        index_status,
        symbol_footprint,
        type_closure_slice,
        graph_impact,
    };

    println!("{}", serde_json::to_string_pretty(&pack)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_path_matches_ts_normpath() {
        assert_eq!(norm_path("./src/foo.rs"), "src/foo.rs");
        assert_eq!(norm_path("src\\win\\foo.rs"), "src/win/foo.rs");
        // Note: norm_path strips `./` BEFORE trimming, so a leading-space input
        // keeps the `./` — this matches the legacy TS normPath ordering.
        assert_eq!(norm_path("  ./a/b.py  "), "./a/b.py");
    }

    fn scratch_repo(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_ctxpack_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctags_present() -> bool {
        shuttle_symbols::SymbolTable::scan(".").is_ok()
    }

    #[test]
    fn graph_impact_folds_reverse_closure_and_derives_touch_hint() {
        if !ctags_present() {
            eprintln!("universal-ctags unavailable; skipping graph_impact E2E");
            return;
        }
        let dir = scratch_repo("gi");
        // core (a.py) is called by wrapper (b.py) and test_core (test_a.py).
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

        let table = shuttle_symbols::SymbolTable::scan(dir.to_str().unwrap()).unwrap();
        // Focus on a.py -> seed = core.
        let focus = vec![format!("{}/a.py", dir.to_string_lossy())];
        let gi = build_graph_impact(&table, &focus, dir.to_str().unwrap())
            .unwrap()
            .expect("graph impact present");

        assert!(
            gi.seeds.iter().any(|s| s == "core"),
            "seed core: {:?}",
            gi.seeds
        );
        // Reverse closure must reach wrapper AND test_core, but NOT re-list core.
        let hit_syms: Vec<&str> = gi.impacted.iter().map(|h| h.symbol.as_str()).collect();
        assert!(
            hit_syms.contains(&"wrapper"),
            "wrapper impacted: {hit_syms:?}"
        );
        assert!(
            hit_syms.contains(&"test_core"),
            "test_core impacted: {hit_syms:?}"
        );
        assert!(!hit_syms.contains(&"core"), "seeds excluded from impacted");
        // Derived touch hint should name b.py and test_a.py (not the focus a.py).
        assert!(gi.derived_touch_hint.iter().any(|f| f.ends_with("b.py")));
        assert!(gi
            .derived_touch_hint
            .iter()
            .any(|f| f.ends_with("test_a.py")));
        // The focus file itself (a.py) must not be re-listed as a derived hint.
        assert!(!gi
            .derived_touch_hint
            .iter()
            .any(|f| f.ends_with("/a.py") || f == "a.py"));

        // R3: every emitted edge is self-consistent — both endpoints live in the
        // node set (seed `core` plus the impacted callers).
        let mut node_set: std::collections::HashSet<&str> =
            gi.seeds.iter().map(|s| s.as_str()).collect();
        for h in &gi.impacted {
            node_set.insert(h.symbol.as_str());
        }
        for e in &gi.symbol_edges {
            assert!(
                node_set.contains(e.from.as_str()) && node_set.contains(e.to.as_str()),
                "dangling edge {} -> {} not in node set",
                e.from,
                e.to
            );
        }
        // wrapper -> core and test_core -> core are the real reference edges.
        assert!(
            gi.symbol_edges
                .iter()
                .any(|e| e.from == "wrapper" && e.to == "core"),
            "expected wrapper -> core edge: {:?}",
            gi.symbol_edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graph_impact_is_none_when_focus_defines_nothing() {
        if !ctags_present() {
            return;
        }
        let dir = scratch_repo("gi_empty");
        std::fs::write(dir.join("a.py"), "def core():\n    return 1\n").unwrap();
        std::fs::write(dir.join("blank.py"), "# nothing defined\n").unwrap();
        let table = shuttle_symbols::SymbolTable::scan(dir.to_str().unwrap()).unwrap();
        // Focus a file that defines no symbols -> no seeds -> None.
        let focus = vec![format!("{}/blank.py", dir.to_string_lossy())];
        assert!(build_graph_impact(&table, &focus, dir.to_str().unwrap())
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
