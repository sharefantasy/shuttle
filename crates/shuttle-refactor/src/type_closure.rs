//! P0.3 — cross-file type-closure resolver (the locals ↔ nav bridge).
//!
//! `locals.rs` gives us a *single-file* type closure ([`crate::locals::LocalsIndex::type_closure`]):
//! the named types a function references, split into in-file `resolved` and
//! `unresolved`. This module lifts that to the whole repo: it anchors the root
//! function to a unique symbol, then BFS-expands every unresolved type name by
//! resolving it against the [`shuttle_symbols::SymbolTable`] (cross-file defs) and
//! recursively opening each newly-found type's file to pull *its* type refs.
//!
//! # Never guess (§7)
//! A type name that resolves to zero symbols, or whose file cannot be read /
//! parsed, is recorded in [`TypeClosureGraph::unresolved`] with a reason — we do
//! **not** pick a "closest" candidate. A name that resolves to *multiple*
//! symbols is added as a node but tagged [`shuttle_nav::Confidence::Low`] and also
//! listed in `ambiguous`, so the caller (or an upstream model) can decide rather
//! than us silently binding to the first hit.
//!
//! # Confidence (mirrors shuttle-nav's global rule, §9.2)
//! * **High**   — the type is defined in the *same file* as its referrer, or it
//!   resolves to exactly one cross-file symbol whose kind is a type/container.
//! * **Medium** — exactly one cross-file symbol, but a non-type kind (best-effort).
//! * **Low**    — multiple candidate symbols (ambiguous); node kept, decision deferred.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use shuttle_nav::Confidence;
use shuttle_symbols::{Symbol, SymbolKind, SymbolTable};

use crate::locals::{Lang, LocalsIndex};
use crate::RefactorError;

/// One node of the resolved type closure.
#[derive(Debug, Clone)]
pub struct ClosureNode {
    /// The type name.
    pub symbol: String,
    /// File the definition was found in.
    pub path: String,
    /// 1-based definition line.
    pub line: usize,
    /// BFS distance from the root function (root's own in-file types = 1).
    pub distance: usize,
    /// How many times this type is referenced across the whole closure walk.
    pub ref_count: usize,
    /// Trust level for this resolution.
    pub confidence: Confidence,
    /// Estimated token cost of including this node's definition in a context
    /// pack. A cheap proxy (definition snippet chars / 4 + a small header); used
    /// only for budget ranking, not billed anywhere.
    pub est_tokens: usize,
}

/// A type name we could not bind to a definition, plus why.
#[derive(Debug, Clone)]
pub struct UnresolvedRef {
    pub symbol: String,
    /// `"no-def"` (zero symbols) or `"unreadable"` (file/parse failure).
    pub reason: &'static str,
    /// BFS distance at which this name was first encountered.
    pub distance: usize,
}

/// The whole cross-file type closure of a root function.
#[derive(Debug, Clone)]
pub struct TypeClosureGraph {
    /// Anchored root function name.
    pub root: String,
    /// Root file path.
    pub root_path: String,
    /// Resolved type nodes, deduped by (symbol,path).
    pub nodes: Vec<ClosureNode>,
    /// Type names that resolved to >1 symbol (kept in `nodes` at Low confidence).
    pub ambiguous: Vec<String>,
    /// Type names with no resolvable definition (never guessed, §7).
    pub unresolved: Vec<UnresolvedRef>,
}

/// Options controlling the BFS walk.
#[derive(Debug, Clone)]
pub struct ClosureOptions {
    /// Max BFS depth (guards against pathological graphs). Default 3.
    pub max_depth: usize,
    /// Max nodes to resolve (guards runaway fan-out). Default 200.
    pub max_nodes: usize,
}

impl Default for ClosureOptions {
    fn default() -> Self {
        ClosureOptions {
            max_depth: 3,
            max_nodes: 200,
        }
    }
}

fn is_type_kind(k: &SymbolKind) -> bool {
    matches!(k, SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum)
}

/// Read a file and build its `LocalsIndex`, or `None` if unreadable/unsupported.
fn index_of(path: &str) -> Option<LocalsIndex> {
    let lang = Lang::from_path(path)?;
    let src = std::fs::read_to_string(path).ok()?;
    LocalsIndex::build(lang, &src).ok()
}

/// Resolve the full cross-file type closure of `root_fn` defined in `root_path`.
///
/// `table` is a pre-built symbol table (caller owns scan/cache). We anchor the
/// root function, compute its single-file closure, then BFS-expand unresolved
/// names across files. Every hop records distance/ref_count/confidence; names
/// with no def or an unreadable file go to `unresolved` (§7 — never guessed).
pub fn resolve_type_closure(
    root_fn: &str,
    root_path: &str,
    table: &SymbolTable,
    opts: &ClosureOptions,
) -> Result<TypeClosureGraph, RefactorError> {
    let root_idx = index_of(root_path)
        .ok_or_else(|| RefactorError::Parse(format!("cannot index root file: {root_path}")))?;

    // Anchor: find the enclosing function range for `root_fn`'s definition in
    // this file. We locate the def occurrence by name among locals definitions.
    let root_def = root_idx
        .definitions
        .iter()
        .find(|d| d.name == root_fn)
        .ok_or_else(|| RefactorError::Parse(format!("root fn not found in file: {root_fn}")))?;
    let fn_range = root_idx
        .enclosing_function_range(root_def.start_byte)
        .unwrap_or((root_def.start_byte, root_idx.source.len()));

    let mut nodes: BTreeMap<(String, String), ClosureNode> = BTreeMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    let mut unresolved: BTreeMap<String, UnresolvedRef> = BTreeMap::new();
    let mut visited_types: BTreeSet<String> = BTreeSet::new();

    // BFS queue of (file_path, fn_range_or_full, distance). Seed with the root.
    let mut queue: VecDeque<(String, (usize, usize), usize)> = VecDeque::new();
    queue.push_back((root_path.to_string(), fn_range, 0));

    // Track ref counts across the whole walk.
    let mut ref_count: BTreeMap<String, usize> = BTreeMap::new();

    while let Some((path, range, depth)) = queue.pop_front() {
        if depth >= opts.max_depth || nodes.len() >= opts.max_nodes {
            continue;
        }
        let Some(idx) = index_of(&path) else {
            continue;
        };
        let slice = idx.type_closure(root_fn, range);

        // In-file resolved types: High confidence, distance = depth+1.
        for def in &slice.resolved {
            *ref_count.entry(def.name.clone()).or_insert(0) += 1;
            let key = (def.name.clone(), path.clone());
            nodes.entry(key).or_insert_with(|| ClosureNode {
                symbol: def.name.clone(),
                path: path.clone(),
                line: def.line,
                distance: depth + 1,
                ref_count: 0, // filled below
                confidence: Confidence::High,
                est_tokens: 0,
            });
        }

        // Cross-file: resolve each unresolved name against the symbol table.
        for name in &slice.unresolved {
            *ref_count.entry(name.clone()).or_insert(0) += 1;
            if visited_types.contains(name) {
                continue;
            }
            let defs: Vec<&Symbol> = table.defs(name);
            match defs.len() {
                0 => {
                    unresolved.entry(name.clone()).or_insert(UnresolvedRef {
                        symbol: name.clone(),
                        reason: "no-def",
                        distance: depth + 1,
                    });
                }
                1 => {
                    let d = defs[0];
                    let conf = if is_type_kind(&d.kind) {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    };
                    visited_types.insert(name.clone());
                    let key = (name.clone(), d.path.clone());
                    nodes.entry(key).or_insert_with(|| ClosureNode {
                        symbol: name.clone(),
                        path: d.path.clone(),
                        line: d.line,
                        distance: depth + 1,
                        ref_count: 0,
                        confidence: conf,
                        est_tokens: 0,
                    });
                    // Recurse into the defining file to expand this type's own
                    // type refs. We scope to the whole file (a type def is not a
                    // function scope), enqueuing at depth+1.
                    queue.push_back((d.path.clone(), (0, usize::MAX), depth + 1));
                }
                _ => {
                    // Ambiguous: keep a node but defer the decision (§7).
                    ambiguous.insert(name.clone());
                    visited_types.insert(name.clone());
                    let d = defs[0];
                    let key = (name.clone(), d.path.clone());
                    nodes.entry(key).or_insert_with(|| ClosureNode {
                        symbol: name.clone(),
                        path: d.path.clone(),
                        line: d.line,
                        distance: depth + 1,
                        ref_count: 0,
                        confidence: Confidence::Low,
                        est_tokens: 0,
                    });
                }
            }
        }
    }

    // Fill ref_count and drop any unresolved name that later resolved somewhere.
    let resolved_names: BTreeSet<String> = nodes.values().map(|n| n.symbol.clone()).collect();
    for n in nodes.values_mut() {
        n.ref_count = *ref_count.get(&n.symbol).unwrap_or(&1);
        n.est_tokens = estimate_def_tokens(&n.path, n.line);
    }
    unresolved.retain(|name, _| !resolved_names.contains(name));

    let mut nodes_vec: Vec<ClosureNode> = nodes.into_values().collect();
    nodes_vec.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.ref_count.cmp(&a.ref_count))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });

    Ok(TypeClosureGraph {
        root: root_fn.to_string(),
        root_path: root_path.to_string(),
        nodes: nodes_vec,
        ambiguous: ambiguous.into_iter().collect(),
        unresolved: unresolved.into_values().collect(),
    })
}

/// Cheap token estimate for a definition: read a small window around its line
/// and approximate chars/4. Returns a floor of 8 so every node has a nonzero
/// cost (prevents division/degenerate packing). Best-effort: unreadable files
/// fall back to the floor.
fn estimate_def_tokens(path: &str, line: usize) -> usize {
    const FLOOR: usize = 8;
    let Ok(src) = std::fs::read_to_string(path) else {
        return FLOOR;
    };
    let lines: Vec<&str> = src.lines().collect();
    if lines.is_empty() {
        return FLOOR;
    }
    // Window: the def line plus up to 12 following lines (a small type body).
    let start = line.saturating_sub(1);
    let end = (start + 13).min(lines.len());
    let chars: usize = lines[start..end].iter().map(|l| l.len()).sum();
    (chars / 4).max(FLOOR)
}

/// A high-score node that did NOT fit in the token budget. Fed to the failure
/// envelope's `whats_missing` (P1) and to the "precise re-fetch" path (P3).
#[derive(Debug, Clone)]
pub struct DroppedNode {
    pub symbol: String,
    pub path: String,
    pub score: f64,
    pub est_tokens: usize,
    pub reason: &'static str,
}

/// The budget-bounded minimal-sufficient context slice (P0.4).
#[derive(Debug, Clone)]
pub struct ContextSlice {
    pub root: String,
    /// Nodes that fit, in packing (descending-score) order.
    pub included: Vec<ClosureNode>,
    /// High-score nodes dropped for budget, in descending-score order.
    pub dropped: Vec<DroppedNode>,
    /// Unresolved names carried through from the graph (§7 — never guessed).
    pub unresolved: Vec<UnresolvedRef>,
    /// Ambiguous names carried through (decision deferred).
    pub ambiguous: Vec<String>,
    /// Total estimated tokens of `included`.
    pub used_tokens: usize,
    pub budget_tokens: usize,
}

/// Score a node for budget ranking: closer to the root and more-referenced is
/// higher; low-confidence nodes are penalised so ambiguous resolutions lose to
/// solid ones under pressure.
fn node_score(n: &ClosureNode) -> f64 {
    let distance_term = 1.0 / (n.distance as f64); // 1.0, 0.5, 0.33 ...
    let ref_term = (1 + n.ref_count) as f64;
    let conf_mult = match n.confidence {
        Confidence::High => 1.0,
        Confidence::Medium => 0.7,
        Confidence::Low => 0.4,
    };
    distance_term * ref_term * conf_mult
}

/// Greedily pack the closure into `budget_tokens`, highest score first. Nodes
/// that don't fit are recorded in `dropped` (not silently discarded) so the
/// caller can surface "context truncated — these types were left out" and
/// re-fetch precisely later. Deterministic: ties break by (distance, symbol).
pub fn budget_pack(graph: &TypeClosureGraph, budget_tokens: usize) -> ContextSlice {
    let mut ranked: Vec<(f64, &ClosureNode)> =
        graph.nodes.iter().map(|n| (node_score(n), n)).collect();
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.distance.cmp(&b.1.distance))
            .then_with(|| a.1.symbol.cmp(&b.1.symbol))
    });

    let mut included: Vec<ClosureNode> = Vec::new();
    let mut dropped: Vec<DroppedNode> = Vec::new();
    let mut used = 0usize;
    for (score, n) in ranked {
        if used + n.est_tokens <= budget_tokens {
            used += n.est_tokens;
            included.push(n.clone());
        } else {
            dropped.push(DroppedNode {
                symbol: n.symbol.clone(),
                path: n.path.clone(),
                score,
                est_tokens: n.est_tokens,
                reason: "budget",
            });
        }
    }

    ContextSlice {
        root: graph.root.clone(),
        included,
        dropped,
        unresolved: graph.unresolved.clone(),
        ambiguous: graph.ambiguous.clone(),
        used_tokens: used,
        budget_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_symbols::{Symbol, SymbolKind, SymbolTable};
    use std::io::Write;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn cross_file_closure_resolves_and_flags_unresolved() {
        // Build a tiny 2-file "repo" in a temp dir under .shuttletmp.
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.shuttletmp/tc_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // types.py defines Report (which references a Widget defined here too).
        let types_py = write(
            &base,
            "types.py",
            "class Widget:\n    pass\n\nclass Report:\n    def __init__(self):\n        self.w = Widget()\n",
        );
        // main.py defines sync() referencing Report (cross-file) and Missing (nowhere).
        let main_py = write(
            &base,
            "main.py",
            "def sync(conn: Conn, out: Report) -> Missing:\n    r = Report()\n    return r\n",
        );

        // Hand-build a symbol table (avoids requiring ctags in unit tests).
        let table = SymbolTable {
            symbols: vec![
                Symbol {
                    name: "Report".into(),
                    kind: SymbolKind::Class,
                    scope: None,
                    path: types_py.clone(),
                    line: 4,
                    signature: None,
                },
                Symbol {
                    name: "Widget".into(),
                    kind: SymbolKind::Class,
                    scope: None,
                    path: types_py.clone(),
                    line: 1,
                    signature: None,
                },
                // `Conn` intentionally absent from types.py but present elsewhere.
            ],
        };

        let g = resolve_type_closure("sync", &main_py, &table, &ClosureOptions::default()).unwrap();

        let resolved: Vec<&str> = g.nodes.iter().map(|n| n.symbol.as_str()).collect();
        // Report resolves cross-file (High: class kind).
        assert!(resolved.contains(&"Report"), "nodes={resolved:?}");
        // Missing has no def anywhere -> unresolved, never guessed.
        assert!(
            g.unresolved
                .iter()
                .any(|u| u.symbol == "Missing" && u.reason == "no-def"),
            "unresolved={:?}",
            g.unresolved
        );
        // Conn has no def -> unresolved too.
        assert!(
            g.unresolved.iter().any(|u| u.symbol == "Conn"),
            "unresolved={:?}",
            g.unresolved
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    fn node(sym: &str, dist: usize, refs: usize, est: usize, conf: Confidence) -> ClosureNode {
        ClosureNode {
            symbol: sym.into(),
            path: "x".into(),
            line: 1,
            distance: dist,
            ref_count: refs,
            confidence: conf,
            est_tokens: est,
        }
    }

    #[test]
    fn budget_pack_drops_low_score_and_records_them() {
        let g = TypeClosureGraph {
            root: "sync".into(),
            root_path: "main".into(),
            nodes: vec![
                node("A", 1, 5, 40, Confidence::High),   // high score
                node("B", 2, 1, 40, Confidence::Medium), // mid
                node("C", 3, 0, 40, Confidence::Low),    // low
            ],
            ambiguous: vec![],
            unresolved: vec![],
        };
        // Budget fits only ~2 nodes of 40 tokens.
        let slice = budget_pack(&g, 85);
        let inc: Vec<&str> = slice.included.iter().map(|n| n.symbol.as_str()).collect();
        assert!(inc.contains(&"A"), "A must be included: {inc:?}");
        assert_eq!(slice.included.len(), 2, "only 2 fit: {inc:?}");
        assert_eq!(slice.dropped.len(), 1, "1 dropped");
        assert_eq!(slice.dropped[0].symbol, "C", "lowest-score C dropped");
        assert_eq!(slice.dropped[0].reason, "budget");
        assert!(slice.used_tokens <= 85);
    }
}
