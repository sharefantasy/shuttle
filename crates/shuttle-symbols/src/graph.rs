//! Symbol-level reference graph + reverse-dependency closure (blast-radius).
//!
//! This is the *native* realization of the "SCIP value" (see
//!  and ): a
//! symbol→symbol dependency graph and its reverse closure, built **without an
//! external indexer and without a daemon**. It composes the primitives
//! `shuttle-symbols` already owns — the [`SymbolTable`] definition index and the
//! identifier scan behind [`crate::extract_references`] — and lifts them from
//! file granularity to symbol granularity.
//!
//! # Edge semantics
//! An edge `owner → callee` means: the definition `owner` references the name
//! `callee` (which is itself defined somewhere in the repo). We attribute a
//! reference occurrence to the *enclosing definition* by covering it with the
//! interval `[def_line, next_def_line)` of the same file (ctags gives the
//! definition start line). Module-level references (before the first def in a
//! file) are **not** attributed to any symbol and are dropped — attributing
//! them would be a guess (§7 — never guess).
//!
//! # Confidence
//! Text-derived edges are never `High`:
//! * **Medium** — the callee name resolves to exactly one definition.
//! * **Low**    — the callee name resolves to more than one definition
//!   (ambiguous); the edge is kept but the callee is also recorded in
//!   [`SymbolGraph::ambiguous`] so the decision is deferred, not silently bound.
//!
//! Along a blast-radius path the confidence is the **minimum** over the path
//! (one Low hop taints the whole chain), and the multi-source seeds are `High`.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::Read;

use crate::imports::ImportGraph;
use crate::{extract_references, SymbolError, SymbolTable};
const MAX_GRAPH_FILES: usize = 10_000;
const MAX_GRAPH_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GRAPH_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GRAPH_REFERENCE_OCCURRENCES: u64 = 2_000_000;
const MAX_GRAPH_IMPORT_EDGES: usize = 250_000;
const MAX_GRAPH_UNIQUE_EDGES: u64 = 250_000;

/// Trust level of a graph edge / blast-radius hit. Local to `shuttle-symbols` so the
/// crate stays leaf (shuttle-nav maps this onto its own `Confidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphConfidence {
    /// A blast-radius seed (a definition the caller explicitly asked about).
    High,
    /// Callee resolves to exactly one definition.
    Medium,
    /// Callee resolves to more than one definition (ambiguous).
    Low,
}

impl GraphConfidence {
    /// Confidence of a path is the *weakest* hop along it. With the `Ord`
    /// derive `High < Medium < Low`, so "weakest" is the *max* variant.
    fn weakest(self, other: GraphConfidence) -> GraphConfidence {
        self.max(other)
    }
}

/// How an edge's *direction* was established. T3 (see
/// ) makes the graph directional:
/// a bare symbol-name reference is inherently symmetric (the name appears in
/// both files), so its raw reverse closure is cyclic. We disambiguate direction
/// with the file→file import relation ([`crate::imports::ImportGraph`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Direction is *trusted*: either the reference is intra-file (structurally
    /// directed by the enclosing definition) or, cross-file, the owner's file
    /// imports the callee's file — the reference runs *with* the dependency.
    ImportDirected,
    /// A cross-file name reference with **no** import corroboration. Direction
    /// is unknown, so the edge is a weak, effectively undirected hint kept at
    /// [`GraphConfidence::Low`] (§7 — never guess a direction we can't see).
    NameMatch,
    /// Direction is *proven by an IDL contract* (G6, ): an IDL (proto/thrift/
    /// openapi/graphql) declares a service/method/message, and the codegen
    /// footprint on **both** sides anchors the same contract node. This is a
    /// cross-language dependency that runs *with* the schema, so it is as
    /// trustworthy as [`EdgeKind::ImportDirected`] and participates in the
    /// directed closure. It is only ever minted when both sides anchor the
    /// contract; a one-sided match degrades to [`EdgeKind::NameMatch`] (§7 —
    /// we never connect two languages on a bare name coincidence, which is the
    /// A4 failure this axis roots out).
    ContractDirected,
    /// A **co-change** review-surface hint (G7 · U1, ): two nodes whose files
    /// have *historically been edited together* (derived from
    /// `git log --numstat` by [`crate::cochange`], weighted with a
    /// commit-size decay so format/rename megacommits do not fabricate
    /// correlations). This is a naturally weak, **probabilistic** signal: it
    /// corroborates an implicit coupling the static graph cannot see (two
    /// files with no import relation but a semantic bond), yet it proves
    /// **no direction and no dependency**. It is therefore pinned — exactly
    /// like [`EdgeKind::NameMatch`] — to [`GraphConfidence::Low`], is never
    /// trusted-directed, and is **excluded from every blast-radius walk**
    /// (both the undirected recall closure and the directed zero-model DAG)
    /// as well as from split add/conflict/drop adjudication (the
    /// `is_trusted_directed()` gate). It exists solely so the
    /// review surface can say "history shows B is often edited with A — a
    /// human may want to look", never to grow the mechanical re-verify set
    /// (§7 — a correlation is never promoted to a proven edge).
    CoChange,
}

impl EdgeKind {
    /// Whether this edge's direction is *trusted* — either import-directed
    /// (structural / same-package / import relation) or contract-directed
    /// (IDL-proven cross-language). The directed blast-radius closure follows
    /// exactly the trusted edges, so both kinds are equal first-class citizens
    /// in add/conflict/drop adjudication (§2.1: the closure just recognizes one
    /// more class of provable directed edge).
    pub fn is_trusted_directed(self) -> bool {
        matches!(self, EdgeKind::ImportDirected | EdgeKind::ContractDirected)
    }
}

/// One receiving end of the blast radius: a symbol reached by walking the
/// reverse-dependency graph from the seeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactHit {
    /// The impacted symbol name.
    pub symbol: String,
    /// A definition site of that symbol (first, deterministic by path/line).
    pub path: String,
    /// 1-based definition line.
    pub line: usize,
    /// BFS distance from the nearest seed (seed itself = 0).
    pub distance: usize,
    /// Weakest confidence along the path that reached this symbol.
    pub confidence: GraphConfidence,
}

/// Resource bounds for a trusted-directed reverse-dependency walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosureLimits {
    pub max_depth: usize,
    pub max_fanout: usize,
}

/// Result of a bounded trusted-directed reverse-dependency walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedImpact {
    pub hits: Vec<ImpactHit>,
    /// True whenever a trusted edge existed beyond either configured bound.
    pub truncated: bool,
    /// Stable machine reasons for truncation.
    pub truncation_reasons: Vec<String>,
}

/// A symbol→symbol reference graph plus its reverse index.
#[derive(Debug, Clone, Default)]
pub struct SymbolGraph {
    /// Forward edges: `owner → {callees}`.
    forward: BTreeMap<String, BTreeSet<String>>,
    /// Reverse edges: `callee → {owners}` (who would be impacted if callee changes).
    reverse: BTreeMap<String, BTreeSet<String>>,
    /// Per-edge confidence, keyed by `(owner, callee)`.
    edge_conf: BTreeMap<(String, String), GraphConfidence>,
    /// Per-edge direction provenance, keyed by `(owner, callee)`.
    edge_kind: BTreeMap<(String, String), EdgeKind>,
    /// Names that resolved to more than one definition (kept as Low edges).
    pub ambiguous: BTreeSet<String>,
    /// name → first (path, line) definition site, deterministic.
    def_site: BTreeMap<String, (String, usize)>,
}

/// Extract identifier references *with their 1-based line numbers*, keeping only
/// names present in `defined`. A line-aware sibling of [`crate::extract_references`].
fn references_with_lines(
    content: &str,
    defined: &HashSet<&str>,
    remaining: u64,
) -> Result<Vec<(String, usize)>, SymbolError> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let names = extract_references(line, defined);
        for n in names {
            out.push((n, i + 1));
            if out.len() as u64 > remaining {
                return Err(SymbolError::Limit {
                    resource: "graph_reference_occurrences",
                    observed: MAX_GRAPH_REFERENCE_OCCURRENCES.saturating_add(1),
                    limit: MAX_GRAPH_REFERENCE_OCCURRENCES,
                });
            }
        }
    }
    Ok(out)
}

fn validate_graph_contents(file_contents: &BTreeMap<String, String>) -> Result<(), SymbolError> {
    if file_contents.len() > MAX_GRAPH_FILES {
        return Err(SymbolError::Limit {
            resource: "graph_files",
            observed: file_contents.len() as u64,
            limit: MAX_GRAPH_FILES as u64,
        });
    }
    let source_bytes = file_contents.values().fold(0u64, |total, content| {
        total.saturating_add(content.len() as u64)
    });
    if source_bytes > MAX_GRAPH_SOURCE_BYTES {
        return Err(SymbolError::Limit {
            resource: "graph_source_bytes",
            observed: source_bytes,
            limit: MAX_GRAPH_SOURCE_BYTES,
        });
    }
    Ok(())
}

fn read_graph_source(path: &std::path::Path) -> Result<Option<String>, SymbolError> {
    let observed = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(None),
    };
    if observed > MAX_GRAPH_FILE_BYTES {
        return Err(SymbolError::Limit {
            resource: "graph_file_bytes",
            observed,
            limit: MAX_GRAPH_FILE_BYTES,
        });
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::with_capacity(observed as usize);
    file.take(MAX_GRAPH_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_GRAPH_FILE_BYTES {
        return Err(SymbolError::Limit {
            resource: "graph_file_bytes",
            observed: bytes.len() as u64,
            limit: MAX_GRAPH_FILE_BYTES,
        });
    }
    match String::from_utf8(bytes) {
        Ok(content) => Ok(Some(content)),
        Err(_) => Ok(None),
    }
}

fn insert_graph_content(
    contents: &mut BTreeMap<String, String>,
    path: String,
    content: String,
    source_bytes: &mut u64,
) -> Result<(), SymbolError> {
    if contents.len() >= MAX_GRAPH_FILES {
        return Err(SymbolError::Limit {
            resource: "graph_files",
            observed: contents.len() as u64 + 1,
            limit: MAX_GRAPH_FILES as u64,
        });
    }
    let observed = source_bytes.saturating_add(content.len() as u64);
    if observed > MAX_GRAPH_SOURCE_BYTES {
        return Err(SymbolError::Limit {
            resource: "graph_source_bytes",
            observed,
            limit: MAX_GRAPH_SOURCE_BYTES,
        });
    }
    *source_bytes = observed;
    contents.insert(path, content);
    Ok(())
}

impl SymbolGraph {
    /// Build the graph from a scanned [`SymbolTable`] and a map of
    /// `path → file contents`. Pure: no I/O (the caller owns reads, so this is
    /// trivially testable). Use [`SymbolGraph::from_scan`] for the I/O wrapper.
    pub fn build(table: &SymbolTable, file_contents: &BTreeMap<String, String>) -> Self {
        Self::try_build(table, file_contents).unwrap_or_default()
    }

    /// Fallible graph builder used by production callers. Resource limits are
    /// part of the result contract; callers must narrow scope instead of
    /// constructing an arbitrarily large in-memory graph.
    pub fn try_build(
        table: &SymbolTable,
        file_contents: &BTreeMap<String, String>,
    ) -> Result<Self, SymbolError> {
        // The file→file import relation gives edges a *direction* (T3): a bare
        // name reference is symmetric, but if the owner's file imports the
        // callee's file the edge runs *with* the dependency and is trusted;
        // the mirror-image reference (a back-edge against the import direction)
        // is degraded to a weak name-match hint so the reverse closure is not
        // polluted by an import cycle that does not exist. Built from the same
        // contents — no extra I/O, no daemon, no external indexer.
        validate_graph_contents(file_contents)?;
        let imports = ImportGraph::try_build(file_contents, MAX_GRAPH_IMPORT_EDGES)?;
        Self::try_build_with_imports(table, file_contents, &imports)
    }

    /// Like [`SymbolGraph::build`] but with a caller-supplied [`ImportGraph`].
    /// Exposed so a consumer that already computed the import relation (e.g. a
    /// warm cache) can reuse it instead of recomputing.
    pub fn build_with_imports(
        table: &SymbolTable,
        file_contents: &BTreeMap<String, String>,
        imports: &ImportGraph,
    ) -> Self {
        Self::try_build_with_imports(table, file_contents, imports).unwrap_or_default()
    }

    pub fn try_build_with_imports(
        table: &SymbolTable,
        file_contents: &BTreeMap<String, String>,
        imports: &ImportGraph,
    ) -> Result<Self, SymbolError> {
        validate_graph_contents(file_contents)?;
        let defined: HashSet<&str> = table.symbols.iter().map(|s| s.name.as_str()).collect();

        // name -> number of definition sites (for confidence / ambiguity).
        let mut def_count: BTreeMap<&str, usize> = BTreeMap::new();
        // name -> set of files that define it (for direction resolution).
        let mut def_files: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        // path -> sorted (line, name) definitions, for interval attribution.
        let mut defs_by_file: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
        let mut def_site: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for s in &table.symbols {
            *def_count.entry(s.name.as_str()).or_insert(0) += 1;
            def_files
                .entry(s.name.as_str())
                .or_default()
                .insert(s.path.as_str());
            defs_by_file
                .entry(s.path.as_str())
                .or_default()
                .push((s.line, s.name.as_str()));
            // First-seen deterministic def site (symbols are scan-ordered; keep
            // the lexicographically smallest (path,line) for stability).
            let entry = def_site
                .entry(s.name.clone())
                .or_insert((s.path.clone(), s.line));
            if (s.path.as_str(), s.line) < (entry.0.as_str(), entry.1) {
                *entry = (s.path.clone(), s.line);
            }
        }
        for v in defs_by_file.values_mut() {
            v.sort_unstable();
        }

        let mut g = SymbolGraph {
            def_site,
            ..Default::default()
        };

        let mut reference_occurrences = 0u64;
        let mut unique_edges = 0u64;
        for (path, defs) in &defs_by_file {
            let Some(content) = file_contents.get(*path) else {
                continue; // unreadable file contributes only its def nodes
            };
            let remaining = MAX_GRAPH_REFERENCE_OCCURRENCES.saturating_sub(reference_occurrences);
            for (name, line) in references_with_lines(content, &defined, remaining)? {
                reference_occurrences = reference_occurrences.saturating_add(1);
                // Attribute the reference to the enclosing definition interval.
                let Some(owner) = enclosing_def(defs, line) else {
                    continue; // module-level ref: not attributable, dropped (§7)
                };
                if owner == name {
                    continue; // self-reference / the def line itself
                }
                let ndefs = def_count.get(name.as_str()).copied().unwrap_or(0);
                if ndefs == 0 {
                    continue; // not actually defined (shouldn't happen)
                }
                // Base confidence from definition multiplicity.
                let base_conf = if ndefs == 1 {
                    GraphConfidence::Medium
                } else {
                    g.ambiguous.insert(name.clone());
                    GraphConfidence::Low
                };

                // Direction resolution against the import relation.
                let callee_files = def_files.get(name.as_str());
                let intra_file = callee_files.map(|fs| fs.contains(path)).unwrap_or(false);
                // Go same-package direction (T4): a Go *package* is a directory, so a cross-file
                // reference to a sibling in the same package is a provable,
                // correctly-directed dependency that carries **no** import
                // statement (Go's compile unit is the package, not the file).
                // We only certify this when EVERY candidate callee def-file
                // shares the owner's package — an ambiguous name that also
                // resolves *outside* the package is never silently trusted
                // (§7 — only claim what the grammar guarantees). This is the
                // "包→文件粒度" alignment: the directed closure keeps intra-
                // package dependents that an import-only model would drop.
                let same_pkg = !intra_file
                    && callee_files
                        .map(|fs| fs.iter().all(|cf| imports.same_go_package(path, cf)))
                        .unwrap_or(false);
                // For an unambiguous cross-file callee we can test the single
                // owning file against the import relation. For an ambiguous one
                // we cannot know which def is meant, so we never claim direction.
                let (owner_imports_callee, callee_imports_owner) = if intra_file || ndefs != 1 {
                    (intra_file, false)
                } else {
                    let cf = callee_files
                        .and_then(|fs| fs.iter().next().copied())
                        .unwrap_or("");
                    (imports.depends_on(path, cf), imports.depends_on(cf, path))
                };

                let (kind, conf) = if intra_file || same_pkg || owner_imports_callee {
                    // Trusted direction: reference runs with the dependency.
                    // `same_pkg` keeps the edge's *base* confidence — an
                    // ambiguous same-package name stays Low; we simply no longer
                    // drop it from the directed closure, because the dependency
                    // itself is real (same compile unit) even when the exact
                    // target symbol is unproven.
                    (EdgeKind::ImportDirected, base_conf)
                } else if callee_imports_owner {
                    // Back-edge against the import direction: the callee's file
                    // imports the owner's, so this reference is the mirror image
                    // of the real dependency. Degrade to a weak, undirected hint
                    // (this is exactly the closure ∩ import-direction pruning).
                    (EdgeKind::NameMatch, GraphConfidence::Low)
                } else {
                    // No import corroboration either way: keep the edge at its
                    // base confidence but flag the direction as unproven so a
                    // directed-only consumer can exclude it (§7 — never guess a
                    // direction we cannot see).
                    (EdgeKind::NameMatch, base_conf)
                };

                let is_new = !g.edge_conf.contains_key(&(owner.to_string(), name.clone()));
                if is_new {
                    unique_edges = unique_edges.saturating_add(1);
                    if unique_edges > MAX_GRAPH_UNIQUE_EDGES {
                        return Err(SymbolError::Limit {
                            resource: "graph_unique_edges",
                            observed: unique_edges,
                            limit: MAX_GRAPH_UNIQUE_EDGES,
                        });
                    }
                }
                g.add_edge(owner.to_string(), name, conf, kind);
            }
        }
        Ok(g)
    }

    /// I/O wrapper: read every file that owns a tag, then [`SymbolGraph::build`].
    /// Unreadable files simply contribute no outgoing edges.
    pub fn from_scan(table: &SymbolTable) -> Self {
        Self::from_scan_root(table, ".")
    }

    pub fn try_from_scan(table: &SymbolTable) -> Result<Self, SymbolError> {
        Self::try_from_scan_root(table, ".")
    }

    /// Build a graph from a table whose paths may be repository-relative,
    /// resolving those paths against the supplied repository root for source
    /// reads. The table itself remains portable and unchanged.
    pub fn from_scan_root(table: &SymbolTable, root: &str) -> Self {
        Self::try_from_scan_root(table, root).unwrap_or_default()
    }

    pub fn try_from_scan_root(table: &SymbolTable, root: &str) -> Result<Self, SymbolError> {
        let root = std::path::Path::new(root);
        let mut seen = BTreeSet::new();
        let mut contents: BTreeMap<String, String> = BTreeMap::new();
        let mut source_bytes = 0u64;
        for s in &table.symbols {
            if seen.insert(s.path.clone()) {
                let disk_path = if std::path::Path::new(&s.path).is_absolute() {
                    std::path::PathBuf::from(&s.path)
                } else {
                    root.join(&s.path)
                };
                if let Some(c) = read_graph_source(&disk_path)? {
                    insert_graph_content(&mut contents, s.path.clone(), c, &mut source_bytes)?;
                }
            }
        }
        // Go's module manifest (`go.mod`) carries no ctags symbols, so it never
        // reaches `contents` via the tag walk above — yet it is what supplies
        // the module path that anchors cross-package import resolution (T4).
        // Without it every first-party Go import silently fails to resolve and the directed
        // closure drops cross-package dependents. Pull each Go source file's
        // nearest ancestor `go.mod` into `contents` so `ImportGraph::build`
        // can see the module prefix. Keyed by its on-disk (scan-relative) path,
        // matching how `go_pkg_files` is keyed.
        let go_dirs: BTreeSet<String> = contents
            .keys()
            .filter(|p| p.ends_with(".go"))
            .map(|p| match p.rfind('/') {
                Some(i) => p[..i].to_string(),
                None => String::new(),
            })
            .collect();
        for dir in go_dirs {
            let mut cur: &str = &dir;
            loop {
                let gomod = if cur.is_empty() {
                    "go.mod".to_string()
                } else {
                    format!("{cur}/go.mod")
                };
                if contents.contains_key(&gomod) {
                    break; // nearest manifest already loaded
                }
                let disk_path = if std::path::Path::new(&gomod).is_absolute() {
                    std::path::PathBuf::from(&gomod)
                } else {
                    root.join(&gomod)
                };
                if let Some(c) = read_graph_source(&disk_path)? {
                    insert_graph_content(&mut contents, gomod, c, &mut source_bytes)?;
                    break; // nearest manifest wins
                }
                match cur.rfind('/') {
                    Some(i) => cur = &cur[..i],
                    None => break,
                }
            }
        }
        SymbolGraph::try_build(table, &contents)
    }

    /// Add a **contract-directed** edge `owner → callee` proven by an IDL
    /// contract whose codegen footprint anchors both endpoints (G6 · S1). The
    /// direction runs *with* the schema (the callee implements/serves the
    /// contract the owner consumes), so it is minted at
    /// [`GraphConfidence::High`] and [`EdgeKind::ContractDirected`] — a first-
    /// class trusted directed edge that participates in the directed closure.
    /// This is the ONLY way a cross-language edge enters the graph; it must
    /// never be called on a bare name coincidence (§7 — that is the A4 病根).
    pub fn add_contract_edge(&mut self, owner: impl Into<String>, callee: impl Into<String>) {
        self.add_edge(
            owner.into(),
            callee.into(),
            GraphConfidence::High,
            EdgeKind::ContractDirected,
        );
    }

    /// Register a **co-change** review-surface hint between two nodes (G7 ·
    /// U1). The pair is minted at [`GraphConfidence::Low`] /
    /// [`EdgeKind::CoChange`], so it lands in the edge ledger (and is thus
    /// genuinely *seen and filtered* by the split adjudicator's
    /// `kind != EdgeKind::ImportDirected` gate) yet is **never walked** by
    /// either blast-radius closure — see [`SymbolGraph::blast_radius_impl`],
    /// which skips `CoChange` in both directed and undirected modes. This is
    /// the ONLY way a co-change signal enters the graph, and it can never
    /// upgrade a direction or grow the mechanical re-verify set (§7).
    pub fn add_cochange_edge(&mut self, a: impl Into<String>, b: impl Into<String>) {
        self.add_edge(a.into(), b.into(), GraphConfidence::Low, EdgeKind::CoChange);
    }

    fn add_edge(&mut self, owner: String, callee: String, conf: GraphConfidence, kind: EdgeKind) {
        self.forward
            .entry(owner.clone())
            .or_default()
            .insert(callee.clone());
        self.reverse
            .entry(callee.clone())
            .or_default()
            .insert(owner.clone());
        // Keep the *weakest* confidence if the same edge is seen twice.
        self.edge_conf
            .entry((owner.clone(), callee.clone()))
            .and_modify(|c| *c = c.weakest(conf))
            .or_insert(conf);
        // Direction provenance: once an edge is proven trusted (import- or
        // contract-directed) it stays trusted; a later weak `NameMatch` sighting
        // of the same pair never downgrades it. Between the two trusted kinds
        // `ContractDirected` wins — an IDL-proven cross-language edge is the
        // richer, more specific claim than a bare import direction.
        self.edge_kind
            .entry((owner, callee))
            .and_modify(|k| match kind {
                EdgeKind::ContractDirected => *k = EdgeKind::ContractDirected,
                EdgeKind::ImportDirected => {
                    // A trusted import direction upgrades either weak kind
                    // (bare name coincidence or a co-change hint) but never
                    // overrides the richer contract direction.
                    if matches!(*k, EdgeKind::NameMatch | EdgeKind::CoChange) {
                        *k = EdgeKind::ImportDirected;
                    }
                }
                // A bare name-match only ever upgrades the *weakest* kind
                // (co-change); it never touches a trusted edge.
                EdgeKind::NameMatch => {
                    if *k == EdgeKind::CoChange {
                        *k = EdgeKind::NameMatch;
                    }
                }
                // Co-change is the weakest kind of all — a weak historical
                // correlation can never demote an edge already known by a
                // stronger route (§7).
                EdgeKind::CoChange => {}
            })
            .or_insert(kind);
    }

    /// Direct forward callees of `symbol` (what it references).
    pub fn callees(&self, symbol: &str) -> Vec<&str> {
        self.forward
            .get(symbol)
            .map(|s| s.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Direct reverse callers of `symbol` (who references it).
    pub fn callers(&self, symbol: &str) -> Vec<&str> {
        self.reverse
            .get(symbol)
            .map(|s| s.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Reverse-dependency closure (blast radius): every symbol transitively
    /// reachable *backwards* from any seed within `max_depth` hops. Multi-source
    /// BFS; each hit records the shortest distance and the weakest confidence
    /// along the path that reached it. Seeds are returned at distance 0 /
    /// [`GraphConfidence::High`]. Deterministic order: distance asc, then
    /// confidence (High first), then symbol name.
    pub fn blast_radius(&self, seeds: &[String], max_depth: usize) -> Vec<ImpactHit> {
        self.blast_radius_impl(seeds, max_depth, false)
    }

    /// Confidence of a specific edge, if present.
    pub fn edge_confidence(&self, owner: &str, callee: &str) -> Option<GraphConfidence> {
        self.edge_conf
            .get(&(owner.to_string(), callee.to_string()))
            .copied()
    }

    /// All edges as `(owner, callee, confidence)`, deterministic order.
    pub fn edges(&self) -> Vec<(&str, &str, GraphConfidence)> {
        self.edge_conf
            .iter()
            .map(|((o, c), conf)| (o.as_str(), c.as_str(), *conf))
            .collect()
    }

    /// Direction provenance of a specific edge, if present.
    pub fn edge_kind(&self, owner: &str, callee: &str) -> Option<EdgeKind> {
        self.edge_kind
            .get(&(owner.to_string(), callee.to_string()))
            .copied()
    }

    /// All edges as `(owner, callee, confidence, kind)`, deterministic order.
    pub fn edges_with_kind(&self) -> Vec<(&str, &str, GraphConfidence, EdgeKind)> {
        self.edge_conf
            .iter()
            .map(|((o, c), conf)| {
                let kind = self
                    .edge_kind
                    .get(&(o.clone(), c.clone()))
                    .copied()
                    .unwrap_or(EdgeKind::NameMatch);
                (o.as_str(), c.as_str(), *conf, kind)
            })
            .collect()
    }

    /// Reverse-dependency closure restricted to **trusted-directed** edges
    /// (T3 import-directed + G6 contract-directed). Import edges follow the
    /// acyclic file→file import relation and contract edges follow IDL-proven
    /// cross-language dependencies, so the resulting closure is a zero-model
    /// DAG — the mechanical substitute for a model *guessing* task
    /// dependencies. Weak, direction-unproven name-match edges are excluded, so
    /// a back-edge that would create a phantom cycle can never appear. Same BFS
    /// contract as [`SymbolGraph::blast_radius`].
    pub fn blast_radius_directed(&self, seeds: &[String], max_depth: usize) -> Vec<ImpactHit> {
        self.blast_radius_directed_bounded(
            seeds,
            ClosureLimits {
                max_depth,
                max_fanout: usize::MAX,
            },
        )
        .hits
    }

    /// Bounded trusted-directed closure. Any omitted trusted edge is reported
    /// as truncation so callers can fall back instead of treating this slice as
    /// a complete proof.
    pub fn blast_radius_directed_bounded(
        &self,
        seeds: &[String],
        limits: ClosureLimits,
    ) -> BoundedImpact {
        let mut best: BTreeMap<String, (usize, GraphConfidence)> = BTreeMap::new();
        let mut queue: VecDeque<(String, usize, GraphConfidence)> = VecDeque::new();
        let mut depth_truncated = false;
        let mut fanout_truncated = false;
        for seed in seeds {
            best.insert(seed.clone(), (0, GraphConfidence::High));
            queue.push_back((seed.clone(), 0, GraphConfidence::High));
        }

        while let Some((symbol, distance, confidence)) = queue.pop_front() {
            let Some(callers) = self.reverse.get(&symbol) else {
                continue;
            };
            let trusted: Vec<&String> = callers
                .iter()
                .filter(|caller| {
                    self.edge_kind
                        .get(&((*caller).clone(), symbol.clone()))
                        .copied()
                        .map(EdgeKind::is_trusted_directed)
                        .unwrap_or(false)
                })
                .collect();

            if distance >= limits.max_depth {
                if !trusted.is_empty() {
                    depth_truncated = true;
                }
                continue;
            }
            if trusted.len() > limits.max_fanout {
                fanout_truncated = true;
            }
            for caller in trusted.into_iter().take(limits.max_fanout) {
                let edge_confidence = self
                    .edge_conf
                    .get(&(caller.clone(), symbol.clone()))
                    .copied()
                    .unwrap_or(GraphConfidence::Low);
                let path_confidence = confidence.weakest(edge_confidence);
                let next_distance = distance + 1;
                let improved = match best.get(caller) {
                    None => true,
                    Some((known_distance, known_confidence)) => {
                        next_distance < *known_distance
                            || (next_distance == *known_distance
                                && path_confidence < *known_confidence)
                    }
                };
                if improved {
                    best.insert(caller.clone(), (next_distance, path_confidence));
                    queue.push_back((caller.clone(), next_distance, path_confidence));
                }
            }
        }

        let mut hits: Vec<ImpactHit> = best
            .into_iter()
            .map(|(symbol, (distance, confidence))| {
                let (path, line) = self
                    .def_site
                    .get(&symbol)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), 0));
                ImpactHit {
                    symbol,
                    path,
                    line,
                    distance,
                    confidence,
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.confidence.cmp(&b.confidence))
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        let mut truncation_reasons = Vec::new();
        if depth_truncated {
            truncation_reasons.push("max_depth".to_string());
        }
        if fanout_truncated {
            truncation_reasons.push("max_fanout".to_string());
        }
        BoundedImpact {
            hits,
            truncated: depth_truncated || fanout_truncated,
            truncation_reasons,
        }
    }

    fn blast_radius_impl(
        &self,
        seeds: &[String],
        max_depth: usize,
        directed_only: bool,
    ) -> Vec<ImpactHit> {
        let mut best: BTreeMap<String, (usize, GraphConfidence)> = BTreeMap::new();
        let mut queue: VecDeque<(String, usize, GraphConfidence)> = VecDeque::new();
        for s in seeds {
            best.insert(s.clone(), (0, GraphConfidence::High));
            queue.push_back((s.clone(), 0, GraphConfidence::High));
        }
        while let Some((sym, dist, conf)) = queue.pop_front() {
            if dist >= max_depth {
                continue;
            }
            let Some(callers) = self.reverse.get(&sym) else {
                continue;
            };
            for caller in callers {
                let ekind = self.edge_kind.get(&(caller.clone(), sym.clone())).copied();
                // Co-change is a review-surface hint, never a walkable
                // closure edge — it is skipped in BOTH the undirected recall
                // walk and the directed DAG walk (§7 / G7 · U1). It can never
                // grow the mechanical impact / re-verify set.
                if ekind == Some(EdgeKind::CoChange) {
                    continue;
                }
                // In directed mode only trusted (import-directed) edges are
                // followed, so the closure is the acyclic zero-model DAG.
                if directed_only && !ekind.map(EdgeKind::is_trusted_directed).unwrap_or(false) {
                    continue;
                }
                let edge_conf = self
                    .edge_conf
                    .get(&(caller.clone(), sym.clone()))
                    .copied()
                    .unwrap_or(GraphConfidence::Low);
                let path_conf = conf.weakest(edge_conf);
                let ndist = dist + 1;
                let improved = match best.get(caller) {
                    None => true,
                    Some((d, c)) => ndist < *d || (ndist == *d && path_conf < *c),
                };
                if improved {
                    best.insert(caller.clone(), (ndist, path_conf));
                    queue.push_back((caller.clone(), ndist, path_conf));
                }
            }
        }

        let mut hits: Vec<ImpactHit> = best
            .into_iter()
            .map(|(symbol, (distance, confidence))| {
                let (path, line) = self
                    .def_site
                    .get(&symbol)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), 0));
                ImpactHit {
                    symbol,
                    path,
                    line,
                    distance,
                    confidence,
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.confidence.cmp(&b.confidence))
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        hits
    }
}

/// The definition that encloses `line` in a file, given that file's sorted
/// `(def_line, name)` list. Returns the name of the def whose interval
/// `[def_line, next_def_line)` covers `line`; `None` if `line` precedes the
/// first definition (module level — not attributable).
fn enclosing_def<'a>(defs: &[(usize, &'a str)], line: usize) -> Option<&'a str> {
    let mut owner = None;
    for (dl, name) in defs {
        if *dl <= line {
            owner = Some(*name);
        } else {
            break;
        }
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Symbol, SymbolKind};

    fn sym(name: &str, path: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            scope: None,
            path: path.to_string(),
            line,
            signature: Some(format!("fn {name}()")),
        }
    }

    /// A→B→C linear chain: changing C blasts back to B then A.
    #[test]
    fn linear_reverse_closure() {
        let table = SymbolTable {
            symbols: vec![
                sym("a", "a.rs", 1),
                sym("b", "b.rs", 1),
                sym("c", "c.rs", 1),
            ],
        };
        let mut fc = BTreeMap::new();
        fc.insert("a.rs".to_string(), "fn a() { b() }".to_string());
        fc.insert("b.rs".to_string(), "fn b() { c() }".to_string());
        fc.insert("c.rs".to_string(), "fn c() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);

        let hits = g.blast_radius(&["c".to_string()], 8);
        let names: Vec<&str> = hits.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(names, vec!["c", "b", "a"]);
        // distances: c=0, b=1, a=2
        assert_eq!(hits[0].distance, 0);
        assert_eq!(hits[1].distance, 1);
        assert_eq!(hits[2].distance, 2);
        // seed is High, downstream single-def edges are Medium.
        assert_eq!(hits[0].confidence, GraphConfidence::High);
        assert_eq!(hits[1].confidence, GraphConfidence::Medium);
    }

    /// max_depth caps the walk.
    #[test]
    fn depth_cap() {
        let table = SymbolTable {
            symbols: vec![
                sym("a", "a.rs", 1),
                sym("b", "b.rs", 1),
                sym("c", "c.rs", 1),
            ],
        };
        let mut fc = BTreeMap::new();
        fc.insert("a.rs".to_string(), "fn a() { b() }".to_string());
        fc.insert("b.rs".to_string(), "fn b() { c() }".to_string());
        fc.insert("c.rs".to_string(), "fn c() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);

        let hits = g.blast_radius(&["c".to_string()], 1);
        let names: Vec<&str> = hits.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(names, vec!["c", "b"]); // a is depth 2, excluded
    }

    /// Diamond A→B, A→C, B→D, C→D: D blasts to B, C, A once each (deduped).
    #[test]
    fn diamond_dedup() {
        let table = SymbolTable {
            symbols: vec![
                sym("a", "a.rs", 1),
                sym("b", "b.rs", 1),
                sym("c", "c.rs", 1),
                sym("d", "d.rs", 1),
            ],
        };
        let mut fc = BTreeMap::new();
        fc.insert("a.rs".to_string(), "fn a() { b(); c() }".to_string());
        fc.insert("b.rs".to_string(), "fn b() { d() }".to_string());
        fc.insert("c.rs".to_string(), "fn c() { d() }".to_string());
        fc.insert("d.rs".to_string(), "fn d() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);

        let hits = g.blast_radius(&["d".to_string()], 8);
        let names: Vec<&str> = hits.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(names, vec!["d", "b", "c", "a"]);
        // each appears exactly once
        assert_eq!(hits.len(), 4);
    }

    /// A cycle A→B→A must not loop forever.
    #[test]
    fn cycle_terminates() {
        let table = SymbolTable {
            symbols: vec![sym("a", "a.rs", 1), sym("b", "b.rs", 1)],
        };
        let mut fc = BTreeMap::new();
        fc.insert("a.rs".to_string(), "fn a() { b() }".to_string());
        fc.insert("b.rs".to_string(), "fn b() { a() }".to_string());
        let g = SymbolGraph::build(&table, &fc);

        let hits = g.blast_radius(&["a".to_string()], 8);
        assert_eq!(hits.len(), 2); // a, b — no infinite loop
    }

    /// An ambiguous callee (two defs) yields a Low edge and is recorded.
    #[test]
    fn ambiguous_callee_is_low() {
        let table = SymbolTable {
            symbols: vec![
                sym("caller", "caller.rs", 1),
                sym("dup", "one.rs", 1),
                sym("dup", "two.rs", 1),
            ],
        };
        let mut fc = BTreeMap::new();
        fc.insert("caller.rs".to_string(), "fn caller() { dup() }".to_string());
        fc.insert("one.rs".to_string(), "fn dup() {}".to_string());
        fc.insert("two.rs".to_string(), "fn dup() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);

        assert!(g.ambiguous.contains("dup"));
        assert_eq!(
            g.edge_confidence("caller", "dup"),
            Some(GraphConfidence::Low)
        );
        // Low taints the path.
        let hits = g.blast_radius(&["dup".to_string()], 8);
        let caller = hits.iter().find(|h| h.symbol == "caller").unwrap();
        assert_eq!(caller.confidence, GraphConfidence::Low);
    }

    /// T3: with an import relation, a cross-file name reference that runs *with*
    /// the import direction is trusted (ImportDirected); the mirror back-edge is
    /// degraded to a weak NameMatch so the directed closure is acyclic.
    #[test]
    fn directional_edge_prunes_back_edge() {
        // a.rs imports b.rs. a() calls b(); b() also textually mentions a
        // (e.g. a comment or a symmetric helper name), which without direction
        // would create a phantom a<->b cycle.
        let table = SymbolTable {
            symbols: vec![sym("a", "src/a.rs", 2), sym("b", "src/b.rs", 2)],
        };
        let mut fc = BTreeMap::new();
        fc.insert("src/a.rs".to_string(), "mod b;\nfn a() { b() }".to_string());
        // b's body references `a` — the back-edge against import direction.
        fc.insert("src/b.rs".to_string(), "\nfn b() { a() }".to_string());
        let g = SymbolGraph::build(&table, &fc);

        // Forward edge a->b is import-directed (a.rs imports b.rs).
        assert_eq!(g.edge_kind("a", "b"), Some(EdgeKind::ImportDirected));
        // Back edge b->a is a weak name-match (b.rs does NOT import a.rs).
        assert_eq!(g.edge_kind("b", "a"), Some(EdgeKind::NameMatch));

        // Undirected closure sees the cycle (both reachable from either seed).
        let undirected = g.blast_radius(&["b".to_string()], 8);
        let un: BTreeSet<&str> = undirected.iter().map(|h| h.symbol.as_str()).collect();
        assert!(un.contains("a") && un.contains("b"));

        // Directed closure from b reaches a (a depends on b, correct).
        let d_from_b = g.blast_radius_directed(&["b".to_string()], 8);
        let db: BTreeSet<&str> = d_from_b.iter().map(|h| h.symbol.as_str()).collect();
        assert!(db.contains("a"), "a depends on b: {db:?}");

        // Directed closure from a reaches ONLY a: nothing import-depends on a,
        // so the phantom back-edge is pruned (acyclic DAG).
        let d_from_a = g.blast_radius_directed(&["a".to_string()], 8);
        let da: Vec<&str> = d_from_a.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(da, vec!["a"], "back-edge b->a must be pruned: {da:?}");
    }

    /// T3: an intra-file reference is structurally directed even without imports.
    #[test]
    fn intra_file_edge_is_import_directed() {
        let table = SymbolTable {
            symbols: vec![sym("top", "m.rs", 1), sym("helper", "m.rs", 3)],
        };
        let mut fc = BTreeMap::new();
        fc.insert(
            "m.rs".to_string(),
            "fn top() { helper() }\n\nfn helper() {}".to_string(),
        );
        let g = SymbolGraph::build(&table, &fc);
        assert_eq!(g.edge_kind("top", "helper"), Some(EdgeKind::ImportDirected));
        // It participates in the directed closure.
        let d = g.blast_radius_directed(&["helper".to_string()], 8);
        let names: BTreeSet<&str> = d.iter().map(|h| h.symbol.as_str()).collect();
        assert!(names.contains("top"));
    }

    /// T3: a cross-file reference with NO import corroboration keeps its base
    /// confidence but is flagged NameMatch (direction unproven) and is excluded
    /// from the directed closure (§7 — never guess a direction we can't see).
    #[test]
    fn uncorroborated_cross_file_edge_excluded_from_directed() {
        let table = SymbolTable {
            symbols: vec![sym("x", "x.rs", 1), sym("y", "y.rs", 1)],
        };
        let mut fc = BTreeMap::new();
        // x() calls y(), but neither file imports the other (no mod/use).
        fc.insert("x.rs".to_string(), "fn x() { y() }".to_string());
        fc.insert("y.rs".to_string(), "fn y() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);

        assert_eq!(g.edge_kind("x", "y"), Some(EdgeKind::NameMatch));
        // base confidence preserved (single def -> Medium), not degraded to Low.
        assert_eq!(g.edge_confidence("x", "y"), Some(GraphConfidence::Medium));

        // Undirected closure still sees it.
        let u = g.blast_radius(&["y".to_string()], 8);
        assert!(u.iter().any(|h| h.symbol == "x"));
        // Directed closure excludes the unproven edge.
        let d = g.blast_radius_directed(&["y".to_string()], 8);
        let names: Vec<&str> = d.iter().map(|h| h.symbol.as_str()).collect();
        assert_eq!(names, vec!["y"], "unproven edge excluded: {names:?}");
    }

    /// Module-level references (before the first def) are not attributed.
    #[test]
    fn module_level_ref_dropped() {
        let table = SymbolTable {
            symbols: vec![sym("target", "t.rs", 5), sym("user", "u.rs", 3)],
        };
        let mut fc = BTreeMap::new();
        // u.rs references `target` on line 1 — before `user`'s def on line 3.
        fc.insert("u.rs".to_string(), "target;\n\nfn user() {}\n".to_string());
        fc.insert("t.rs".to_string(), "\n\n\n\nfn target() {}".to_string());
        let g = SymbolGraph::build(&table, &fc);
        // No symbol edge for the module-level reference.
        assert!(g.callers("target").is_empty());
    }

    /// T4 : a Go same-package
    /// cross-file reference carries NO import statement, yet it is a provable,
    /// correctly-directed dependency (the package is the compile unit). It must
    /// be certified ImportDirected so the *directed* closure keeps the
    /// intra-package dependent — the "包→文件粒度" alignment with packet_plan.
    #[test]
    fn go_same_package_edge_is_import_directed() {
        // store/a.go defines A; store/b.go (same package) calls A(). No import.
        let table = SymbolTable {
            symbols: vec![sym("A", "store/a.go", 3), sym("B", "store/b.go", 3)],
        };
        let mut fc = BTreeMap::new();
        fc.insert("go.mod".to_string(), "module example.com/app\n".to_string());
        fc.insert(
            "store/a.go".to_string(),
            "package store\n\nfunc A() int { return 1 }\n".to_string(),
        );
        fc.insert(
            "store/b.go".to_string(),
            "package store\n\nfunc B() int { return A() + 1 }\n".to_string(),
        );
        let g = SymbolGraph::build(&table, &fc);

        // The B->A edge (b.go references A) is trusted-directed, not a weak
        // name-match, because the two files are the same Go package.
        assert_eq!(
            g.edge_kind("B", "A"),
            Some(EdgeKind::ImportDirected),
            "same-package edge must be ImportDirected"
        );
        // Single def -> Medium base confidence, preserved (not degraded to Low).
        assert_eq!(g.edge_confidence("B", "A"), Some(GraphConfidence::Medium));

        // The DIRECTED closure from A now reaches B — previously dropped.
        let d = g.blast_radius_directed(&["A".to_string()], 8);
        let names: BTreeSet<&str> = d.iter().map(|h| h.symbol.as_str()).collect();
        assert!(
            names.contains("B"),
            "directed closure keeps same-package dependent: {names:?}"
        );
        // The impacted B is reported at its own FILE (store/b.go), file-grained.
        let b_hit = d.iter().find(|h| h.symbol == "B").unwrap();
        assert_eq!(b_hit.path, "store/b.go");
    }

    /// T4: same-package trust does NOT leak across packages. An ambiguous name
    /// that resolves in BOTH the owner's package and a foreign package must not
    /// be silently certified — we only trust when EVERY candidate def-file is
    /// in the same package (§7 — never guess beyond what the grammar proves).
    #[test]
    fn go_same_package_does_not_leak_across_packages() {
        // `Helper` is defined in store/ (same pkg as caller) AND in other/.
        // The reference is ambiguous, so direction stays unproven / NameMatch.
        let table = SymbolTable {
            symbols: vec![
                sym("Caller", "store/a.go", 3),
                sym("Helper", "store/h.go", 3),
                sym("Helper", "other/h.go", 3),
            ],
        };
        let mut fc = BTreeMap::new();
        fc.insert("go.mod".to_string(), "module m\n".to_string());
        fc.insert(
            "store/a.go".to_string(),
            "package store\n\nfunc Caller() { Helper() }\n".to_string(),
        );
        fc.insert(
            "store/h.go".to_string(),
            "package store\n\nfunc Helper() {}\n".to_string(),
        );
        fc.insert(
            "other/h.go".to_string(),
            "package other\n\nfunc Helper() {}\n".to_string(),
        );
        let g = SymbolGraph::build(&table, &fc);

        // Ambiguous callee (2 defs) -> Low, flagged, and direction NOT trusted
        // because one candidate def-file (other/h.go) is a foreign package.
        assert_eq!(g.edge_kind("Caller", "Helper"), Some(EdgeKind::NameMatch));
        assert!(g.ambiguous.contains("Helper"));
        // Directed closure from Helper must NOT include Caller (unproven).
        let d = g.blast_radius_directed(&["Helper".to_string()], 8);
        let names: BTreeSet<&str> = d.iter().map(|h| h.symbol.as_str()).collect();
        assert!(
            !names.contains("Caller"),
            "ambiguous cross-package name stays unproven: {names:?}"
        );
    }

    /// T4 cross-package half: a `svc` file that imports `example.com/app/store`
    /// and calls `Alpha` must yield a trusted import-directed edge to the
    /// member file `store/a.go` (file granularity), so the directed closure
    /// from `Alpha` reaches the cross-package caller `Use`. This exercises the
    /// import fan-out path (distinct from same-package direction) and the
    /// `go.mod`-anchored package resolution.
    #[test]
    fn go_cross_package_import_edge_is_import_directed() {
        let table = SymbolTable {
            symbols: vec![sym("Alpha", "store/a.go", 3), sym("Use", "svc/svc.go", 5)],
        };
        let mut fc = BTreeMap::new();
        fc.insert("go.mod".to_string(), "module example.com/app\n".to_string());
        fc.insert(
            "store/a.go".to_string(),
            "package store\n\nfunc Alpha() int { return 1 }\n".to_string(),
        );
        fc.insert(
            "svc/svc.go".to_string(),
            "package svc\n\nimport \"example.com/app/store\"\n\nfunc Use() int { return store.Alpha() }\n"
                .to_string(),
        );
        let g = SymbolGraph::build(&table, &fc);

        // Use -> Alpha runs WITH the import direction (svc imports store), so
        // it is trusted-directed.
        assert_eq!(
            g.edge_kind("Use", "Alpha"),
            Some(EdgeKind::ImportDirected),
            "cross-package import edge must be ImportDirected"
        );
        // Directed closure from Alpha reaches Use at its own file, svc/svc.go.
        let d = g.blast_radius_directed(&["Alpha".to_string()], 8);
        let use_hit = d.iter().find(|h| h.symbol == "Use");
        assert!(
            use_hit.is_some(),
            "directed closure reaches cross-package Use"
        );
        assert_eq!(use_hit.unwrap().path, "svc/svc.go");
    }

    /// `from_scan` must pull each Go source file's nearest ancestor `go.mod`
    /// into the content map even though `go.mod` carries no ctags symbols — it
    /// is what anchors cross-package import resolution. Without it, the CLI
    /// path (`impact_files` -> `from_scan`) dropped every cross-package edge.
    #[test]
    fn from_scan_loads_ancestor_go_mod() {
        use std::io::Write;
        let base = std::env::temp_dir().join(format!(
            "shuttle_from_scan_gomod_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = base.join("store");
        let svc = base.join("svc");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&svc).unwrap();
        let write = |p: &std::path::Path, body: &str| {
            let mut f = std::fs::File::create(p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        };
        write(&base.join("go.mod"), "module example.com/app\n");
        let a = store.join("a.go");
        let s = svc.join("svc.go");
        write(&a, "package store\n\nfunc Alpha() int { return 1 }\n");
        write(
            &s,
            "package svc\n\nimport \"example.com/app/store\"\n\nfunc Use() int { return store.Alpha() }\n",
        );

        let table = SymbolTable {
            symbols: vec![
                sym("Alpha", a.to_str().unwrap(), 3),
                sym("Use", s.to_str().unwrap(), 5),
            ],
        };
        let g = SymbolGraph::from_scan(&table);
        // The cross-package edge exists only if go.mod was auto-loaded.
        assert_eq!(
            g.edge_kind("Use", "Alpha"),
            Some(EdgeKind::ImportDirected),
            "from_scan must load ancestor go.mod to resolve cross-package import"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// G6-1: a contract-directed edge is a first-class trusted directed edge —
    /// it survives the directed-only closure exactly like an import edge, and
    /// its provenance is `ContractDirected` at `High` confidence.
    #[test]
    fn contract_edge_is_trusted_directed() {
        // Two symbols in different languages that share NO import relation and
        // NO name coincidence: only the injected contract edge links them.
        let table = SymbolTable {
            symbols: vec![
                sym("TsGetUserClient", "web/user_client.ts", 1),
                sym("GoGetUserHandler", "svc/user.go", 1),
            ],
        };
        let fc = BTreeMap::new(); // no bodies → no text edges at all
        let mut g = SymbolGraph::build(&table, &fc);
        assert!(g.edges().is_empty(), "no text edges without corroboration");

        // The TS client depends on the Go handler *through the IDL contract*.
        g.add_contract_edge("TsGetUserClient", "GoGetUserHandler");

        assert_eq!(
            g.edge_kind("TsGetUserClient", "GoGetUserHandler"),
            Some(EdgeKind::ContractDirected)
        );
        assert_eq!(
            g.edge_confidence("TsGetUserClient", "GoGetUserHandler"),
            Some(GraphConfidence::High)
        );
        assert!(EdgeKind::ContractDirected.is_trusted_directed());

        // Directed closure from the handler reaches the client — the contract
        // edge is followed just like an import edge would be.
        let hits = g.blast_radius_directed(&["GoGetUserHandler".to_string()], 8);
        assert!(
            hits.iter().any(|h| h.symbol == "TsGetUserClient"),
            "contract edge must participate in the directed closure"
        );
    }

    fn bounded_symbols(impact: &BoundedImpact) -> Vec<&str> {
        impact.hits.iter().map(|hit| hit.symbol.as_str()).collect()
    }

    #[test]
    fn bounded_directed_closure_is_complete_without_truncation() {
        let table = SymbolTable {
            symbols: vec![
                sym("Seed", "seed.rs", 1),
                sym("ImportCaller", "import.rs", 1),
                sym("ContractCaller", "contract.rs", 1),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_edge(
            "ImportCaller".to_string(),
            "Seed".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        graph.add_contract_edge("ContractCaller", "ImportCaller");

        let bounded = graph.blast_radius_directed_bounded(
            &["Seed".to_string()],
            ClosureLimits {
                max_depth: 2,
                max_fanout: 1,
            },
        );

        assert!(!bounded.truncated);
        assert!(bounded.truncation_reasons.is_empty());
        assert_eq!(
            bounded_symbols(&bounded),
            vec!["Seed", "ImportCaller", "ContractCaller"]
        );
        assert_eq!(
            bounded.hits,
            graph.blast_radius_directed(&["Seed".to_string()], 2),
            "the legacy API must retain its unbounded-fanout behavior",
        );
    }

    #[test]
    fn bounded_directed_closure_truncates_high_fanout_in_stable_order() {
        let table = SymbolTable {
            symbols: vec![
                sym("Seed", "seed.rs", 1),
                sym("A", "a.rs", 1),
                sym("B", "b.rs", 1),
                sym("C", "c.rs", 1),
            ],
        };
        let mut first_graph = SymbolGraph::build(&table, &BTreeMap::new());
        let mut second_graph = SymbolGraph::build(&table, &BTreeMap::new());
        for (graph, order) in [
            (&mut first_graph, ["C", "A", "B"]),
            (&mut second_graph, ["B", "C", "A"]),
        ] {
            for caller in order {
                graph.add_edge(
                    caller.to_string(),
                    "Seed".to_string(),
                    GraphConfidence::Medium,
                    EdgeKind::ImportDirected,
                );
            }
        }
        let limits = ClosureLimits {
            max_depth: 8,
            max_fanout: 2,
        };
        let first = first_graph.blast_radius_directed_bounded(&["Seed".to_string()], limits);
        let second = second_graph.blast_radius_directed_bounded(&["Seed".to_string()], limits);
        assert_eq!(first, second);
        assert!(first.truncated);
        assert_eq!(first.truncation_reasons, vec!["max_fanout"]);
        assert_eq!(bounded_symbols(&first), vec!["Seed", "A", "B"]);
    }

    #[test]
    fn bounded_directed_closure_reports_depth_only_for_a_trusted_neighbor() {
        let table = SymbolTable {
            symbols: vec![
                sym("Seed", "seed.rs", 1),
                sym("Direct", "direct.rs", 1),
                sym("Deep", "deep.rs", 1),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_edge(
            "Direct".to_string(),
            "Seed".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        graph.add_edge(
            "Deep".to_string(),
            "Direct".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        let impact = graph.blast_radius_directed_bounded(
            &["Seed".to_string()],
            ClosureLimits {
                max_depth: 1,
                max_fanout: 8,
            },
        );
        assert!(impact.truncated);
        assert_eq!(impact.truncation_reasons, vec!["max_depth"]);
        assert_eq!(bounded_symbols(&impact), vec!["Seed", "Direct"]);
    }

    #[test]
    fn bounded_directed_closure_is_deterministic_for_seed_order() {
        let table = SymbolTable {
            symbols: vec![
                sym("SeedA", "seed_a.rs", 1),
                sym("SeedB", "seed_b.rs", 1),
                sym("CallerA", "caller_a.rs", 1),
                sym("CallerB", "caller_b.rs", 1),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        for (caller, callee) in [("CallerB", "SeedB"), ("CallerA", "SeedA")] {
            graph.add_edge(
                caller.to_string(),
                callee.to_string(),
                GraphConfidence::Medium,
                EdgeKind::ImportDirected,
            );
        }
        let limits = ClosureLimits {
            max_depth: 1,
            max_fanout: 1,
        };

        let first = graph
            .blast_radius_directed_bounded(&["SeedB".to_string(), "SeedA".to_string()], limits);
        let second = graph
            .blast_radius_directed_bounded(&["SeedA".to_string(), "SeedB".to_string()], limits);

        assert_eq!(first, second);
        assert_eq!(
            bounded_symbols(&first),
            vec!["SeedA", "SeedB", "CallerA", "CallerB"]
        );
    }

    #[test]
    fn bounded_directed_closure_rejects_name_match_and_cochange() {
        let table = SymbolTable {
            symbols: vec![
                sym("Seed", "seed.rs", 1),
                sym("NameOnly", "name.rs", 1),
                sym("CoChanged", "cochange.rs", 1),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_edge(
            "NameOnly".to_string(),
            "Seed".to_string(),
            GraphConfidence::Low,
            EdgeKind::NameMatch,
        );
        graph.add_cochange_edge("CoChanged", "Seed");

        let impact = graph.blast_radius_directed_bounded(
            &["Seed".to_string()],
            ClosureLimits {
                max_depth: 8,
                max_fanout: 8,
            },
        );

        assert!(!impact.truncated);
        assert!(impact.truncation_reasons.is_empty());
        assert_eq!(bounded_symbols(&impact), vec!["Seed"]);
    }

    #[test]
    fn bounded_directed_closure_accepts_contract_directed() {
        let table = SymbolTable {
            symbols: vec![
                sym("Handler", "handler.go", 1),
                sym("Client", "client.ts", 1),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_contract_edge("Client", "Handler");

        let impact = graph.blast_radius_directed_bounded(
            &["Handler".to_string()],
            ClosureLimits {
                max_depth: 1,
                max_fanout: 1,
            },
        );

        assert!(!impact.truncated);
        assert_eq!(bounded_symbols(&impact), vec!["Handler", "Client"]);
        assert_eq!(impact.hits[1].confidence, GraphConfidence::High);
    }

    /// G6-1: `ContractDirected` is the richer claim and wins over a later
    /// `ImportDirected` sighting of the same pair, and neither trusted kind is
    /// ever downgraded by a subsequent weak `NameMatch`.
    #[test]
    fn contract_kind_wins_and_never_downgrades() {
        let table = SymbolTable {
            symbols: vec![sym("A", "a.rs", 1), sym("B", "b.rs", 1)],
        };
        let fc = BTreeMap::new();
        let mut g = SymbolGraph::build(&table, &fc);

        // Contract edge first, then an import-directed sighting of the same
        // pair: the contract provenance is preserved (more specific).
        g.add_contract_edge("A", "B");
        g.add_edge(
            "A".to_string(),
            "B".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::ContractDirected));

        // A later weak NameMatch sighting must not downgrade the trusted edge.
        g.add_edge(
            "A".to_string(),
            "B".to_string(),
            GraphConfidence::Low,
            EdgeKind::NameMatch,
        );
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::ContractDirected));
    }

    /// G6-1: an import-directed edge is *upgraded* to contract-directed when a
    /// later contract edge proves the same pair (the IDL is the stronger truth).
    #[test]
    fn import_edge_upgrades_to_contract() {
        let table = SymbolTable {
            symbols: vec![sym("A", "a.rs", 1), sym("B", "b.rs", 1)],
        };
        let fc = BTreeMap::new();
        let mut g = SymbolGraph::build(&table, &fc);
        g.add_edge(
            "A".to_string(),
            "B".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::ImportDirected));
        g.add_contract_edge("A", "B");
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::ContractDirected));
    }

    // --- G7 · U1: co-change is a review-surface hint, never a walkable edge -

    /// A `CoChange` edge lands in the ledger (so the split gate can *see and
    /// filter* it) but is walked by **neither** blast-radius closure — it can
    /// never grow the mechanical impact / re-verify set.
    #[test]
    fn cochange_edge_is_excluded_from_both_closures() {
        let table = SymbolTable {
            symbols: vec![sym("A", "a.rs", 1), sym("B", "b.rs", 1)],
        };
        let fc = BTreeMap::new();
        let mut g = SymbolGraph::build(&table, &fc);
        // History says A and B change together; register the hint.
        g.add_cochange_edge("A", "B");
        // It IS in the ledger, at Low / CoChange.
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::CoChange));
        assert_eq!(g.edge_confidence("A", "B"), Some(GraphConfidence::Low));
        assert!(!EdgeKind::CoChange.is_trusted_directed());
        // But changing B pulls in NO one via either closure — the co-change
        // hint is not a walkable edge.
        let undirected = g.blast_radius(&["B".to_string()], 8);
        assert_eq!(
            undirected
                .iter()
                .map(|h| h.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["B"],
            "undirected closure must not follow a co-change edge",
        );
        let directed = g.blast_radius_directed(&["B".to_string()], 8);
        assert_eq!(
            directed
                .iter()
                .map(|h| h.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["B"],
            "directed closure must not follow a co-change edge",
        );
    }

    /// The split adjudicator's `is_trusted_directed()` gate (see
    /// `shuttle-nav::split`) genuinely observes the co-change edge in
    /// `edges_with_kind()` and drops it — it contributes nothing to
    /// add/conflict/drop. We prove the ledger surfaces the kind so that gate
    /// can act on it (the gate itself is asserted end-to-end in shuttle-nav).
    #[test]
    fn cochange_edge_is_visible_to_but_dropped_by_the_import_gate() {
        let table = SymbolTable {
            symbols: vec![sym("A", "a.rs", 1), sym("B", "b.rs", 1)],
        };
        let fc = BTreeMap::new();
        let mut g = SymbolGraph::build(&table, &fc);
        g.add_cochange_edge("A", "B");
        let seen: Vec<_> = g
            .edges_with_kind()
            .into_iter()
            .filter(|(o, c, _, _)| *o == "A" && *c == "B")
            .collect();
        assert_eq!(seen.len(), 1, "the co-change edge is in the ledger");
        assert_eq!(seen[0].3, EdgeKind::CoChange);
        // The exact predicate the split gate applies: only trusted directed
        // edges survive; co-change is filtered out.
        let survives_gate = seen[0].3.is_trusted_directed();
        assert!(!survives_gate, "co-change must not survive the import gate");
    }

    /// Co-change is the weakest kind: a later trusted (import) sighting of the
    /// same pair upgrades it, and a co-change sighting never downgrades an
    /// already-trusted edge.
    #[test]
    fn cochange_never_downgrades_and_is_upgraded_by_import() {
        let table = SymbolTable {
            symbols: vec![sym("A", "a.rs", 1), sym("B", "b.rs", 1)],
        };
        let fc = BTreeMap::new();
        let mut g = SymbolGraph::build(&table, &fc);
        // co-change first, then a proven import direction upgrades it.
        g.add_cochange_edge("A", "B");
        assert_eq!(g.edge_kind("A", "B"), Some(EdgeKind::CoChange));
        g.add_edge(
            "A".to_string(),
            "B".to_string(),
            GraphConfidence::Medium,
            EdgeKind::ImportDirected,
        );
        assert_eq!(
            g.edge_kind("A", "B"),
            Some(EdgeKind::ImportDirected),
            "a proven import direction upgrades a co-change hint",
        );
        // The reverse: a co-change sighting can never demote a trusted edge.
        g.add_cochange_edge("A", "B");
        assert_eq!(
            g.edge_kind("A", "B"),
            Some(EdgeKind::ImportDirected),
            "a co-change hint must never downgrade a trusted edge",
        );
    }

    #[test]
    fn graph_source_budget_fails_closed() {
        let mut contents = BTreeMap::new();
        let mut source_bytes = MAX_GRAPH_SOURCE_BYTES;
        let err = insert_graph_content(
            &mut contents,
            "too-much.rs".to_string(),
            "x".to_string(),
            &mut source_bytes,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SymbolError::Limit {
                resource: "graph_source_bytes",
                observed,
                limit
            } if observed == MAX_GRAPH_SOURCE_BYTES + 1 && limit == MAX_GRAPH_SOURCE_BYTES
        ));
    }
}
