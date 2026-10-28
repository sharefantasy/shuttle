//! Native tree-sitter `locals` resolution (docs §2.1, §6.1).
//!
//! This is the first place in shuttle to use native tree-sitter: the ast-grep binary
//! only exposes `kind` matching and cannot run `locals.scm`
//! scope/definition/reference resolution. We parse a single file, run the
//! language's `locals` query, and expose the scope tree + name bindings that the
//! refactor decision layer needs.

use std::collections::BTreeMap;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor, Tree};

use crate::RefactorError;

/// A supported target language for the refactor engine (batch 1: py/js).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    JavaScript,
    Go,
    Rust,
    TypeScript,
}

impl Lang {
    /// Map a file path to a supported language, or `None` if unsupported.
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "py" => Some(Lang::Python),
            "js" | "cjs" | "mjs" | "jsx" => Some(Lang::JavaScript),
            "go" => Some(Lang::Go),
            "rs" => Some(Lang::Rust),
            "ts" | "tsx" | "mts" | "cts" => Some(Lang::TypeScript),
            _ => None,
        }
    }

    pub(crate) fn language(self) -> tree_sitter::Language {
        match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }

    /// The `locals.scm` source for this language. JavaScript ships one upstream;
    /// Python does not, so we carry a hand-written structural subset (§6.1).
    fn locals_query(self) -> &'static str {
        match self {
            Lang::Python => include_str!("queries/python_locals.scm"),
            Lang::JavaScript => tree_sitter_javascript::LOCALS_QUERY,
            // go/rust ship no locals.scm; TS ships only a params-only subset, so
            // we carry hand-written structural subsets for all three (§6.1).
            Lang::Go => include_str!("queries/go_locals.scm"),
            Lang::Rust => include_str!("queries/rust_locals.scm"),
            Lang::TypeScript => include_str!("queries/typescript_locals.scm"),
        }
    }

    /// Node kinds that introduce a new *function* scope. Used to find the
    /// innermost enclosing function of a selection (extract-method et al.).
    pub fn function_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Python => &["function_definition", "lambda"],
            Lang::JavaScript => &[
                "function_declaration",
                "function_expression",
                "arrow_function",
                "method_definition",
            ],
            Lang::Go => &["function_declaration", "method_declaration", "func_literal"],
            Lang::Rust => &["function_item", "closure_expression"],
            Lang::TypeScript => &[
                "function_declaration",
                "function_expression",
                "arrow_function",
                "method_definition",
            ],
        }
    }

    /// Top-level *type/container* declaration kinds — classes, structs,
    /// enums, interfaces, traits, type aliases. Used by refactors that
    /// relocate a whole named type (e.g. extract-to-new-file) or read its
    /// field/member list (e.g. generate-boilerplate, implement-members).
    pub fn container_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Python => &["class_definition"],
            Lang::JavaScript => &["class_declaration"],
            Lang::Go => &["type_declaration"],
            Lang::Rust => &[
                "struct_item",
                "enum_item",
                "trait_item",
                "impl_item",
                "type_item",
                "union_item",
            ],
            Lang::TypeScript => &[
                "class_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
            ],
        }
    }
}

/// A definition or reference occurrence discovered by the locals query.
#[derive(Debug, Clone)]
pub struct Occurrence {
    pub name: String,
    /// Byte offset of the identifier in the source.
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line, 0-based column (tree-sitter Point is 0-based row/col).
    pub line: usize,
    pub col: usize,
    /// The `@local.scope` node id that most tightly encloses this occurrence.
    pub scope_id: usize,
}

/// The parsed file plus its resolved locals. Owns the `Tree` so callers can walk
/// the syntax tree for range/scope queries.
pub struct LocalsIndex {
    pub lang: Lang,
    pub source: String,
    pub tree: Tree,
    /// All `@local.definition` occurrences.
    pub definitions: Vec<Occurrence>,
    /// All `@local.reference` occurrences.
    pub references: Vec<Occurrence>,
    /// All `@local.type-ref` occurrences (P0.1): identifiers appearing in a
    /// *type position* — parameter/return/variable annotations, constructor
    /// calls, generic arguments. These feed the type-closure builder (P0.2):
    /// each names a candidate type whose definition we resolve, first in this
    /// file (`container_kinds`), then cross-file via shuttle-nav (P0.3).
    pub type_refs: Vec<Occurrence>,
    /// Byte range of every `@local.scope` node, keyed by node id.
    scopes: BTreeMap<usize, (usize, usize)>,
}

impl LocalsIndex {
    /// Parse `source` for `lang` and resolve its locals query.
    pub fn build(lang: Lang, source: &str) -> Result<LocalsIndex, RefactorError> {
        let mut parser = Parser::new();
        parser
            .set_language(&lang.language())
            .map_err(|e| RefactorError::Parse(e.to_string()))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| RefactorError::Parse("tree-sitter returned no tree".into()))?;

        let query = Query::new(&lang.language(), lang.locals_query())
            .map_err(|e| RefactorError::Parse(format!("bad locals query: {e:?}")))?;

        // Capture index -> role. We only care about scope/definition/reference;
        // capture names look like `local.scope`, `local.definition`,
        // `local.definition.function`, `local.reference`, etc.
        let names = query.capture_names();

        let mut scopes: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
        let mut definitions: Vec<Occurrence> = Vec::new();
        let mut references: Vec<Occurrence> = Vec::new();
        let mut type_refs: Vec<Occurrence> = Vec::new();

        let src = source.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&query, tree.root_node(), src);
        while let Some(m) = it.next() {
            for cap in m.captures {
                let cname = names[cap.index as usize];
                let node = cap.node;
                if cname == "local.scope" {
                    scopes.insert(node.id(), (node.start_byte(), node.end_byte()));
                } else if cname.starts_with("local.definition") {
                    definitions.push(occurrence(source, node, node.start_byte()));
                } else if cname.starts_with("local.reference") {
                    references.push(occurrence(source, node, node.start_byte()));
                } else if cname.starts_with("local.type-ref") {
                    type_refs.push(occurrence(source, node, node.start_byte()));
                }
            }
        }

        // Resolve each occurrence's tightest enclosing scope (smallest range that
        // contains it). The module/root always contains everything.
        let mut idx = LocalsIndex {
            lang,
            source: source.to_string(),
            tree,
            definitions,
            references,
            type_refs,
            scopes,
        };
        idx.assign_scopes();
        Ok(idx)
    }

    fn assign_scopes(&mut self) {
        // Precompute a stable scope list (id,start,end) so we can pick the
        // smallest enclosing range per occurrence.
        let scope_ranges: Vec<(usize, usize, usize)> = self
            .scopes
            .iter()
            .map(|(id, (s, e))| (*id, *s, *e))
            .collect();
        let pick = |b: usize| -> usize {
            let mut best: Option<(usize, usize)> = None; // (span, id)
            for (id, s, e) in &scope_ranges {
                if *s <= b && b < *e {
                    let span = e - s;
                    match best {
                        Some((bspan, _)) if span >= bspan => {}
                        _ => best = Some((span, *id)),
                    }
                }
            }
            best.map(|(_, id)| id).unwrap_or(0)
        };
        for d in &mut self.definitions {
            d.scope_id = pick(d.start_byte);
        }
        for r in &mut self.references {
            r.scope_id = pick(r.start_byte);
        }
        for t in &mut self.type_refs {
            t.scope_id = pick(t.start_byte);
        }
    }

    /// Byte range of a scope node id, if known.
    pub fn scope_range(&self, id: usize) -> Option<(usize, usize)> {
        self.scopes.get(&id).copied()
    }

    /// Resolve a reference to its governing *definition* occurrence using the
    /// locals rule: the same-named definition in the nearest enclosing scope
    /// that has one. Returns `None` for free/global names (no local def in any
    /// enclosing scope), which the caller treats as visible in inner functions
    /// too (so not a parameter). Function-local, zero cross-file noise.
    pub fn binding_def(&self, r: &Occurrence) -> Option<&Occurrence> {
        // Candidate defs of the same name whose scope encloses the reference.
        let mut best: Option<(&Occurrence, usize)> = None; // (def, scope span)
        for d in self.definitions.iter().filter(|d| d.name == r.name) {
            let Some((ds, de)) = self.scope_range(d.scope_id) else {
                continue;
            };
            if ds <= r.start_byte && r.start_byte < de {
                let span = de - ds;
                match best {
                    Some((_, bspan)) if span >= bspan => {}
                    _ => best = Some((d, span)),
                }
            }
        }
        best.map(|(d, _)| d)
    }

    /// All references to `name` whose binding resolves (by the locals rule:
    /// nearest enclosing scope with a same-named definition) to a definition
    /// *inside* `scope_byte_range`. This is the function-local name-binding used
    /// by rename-local and inline-variable.
    ///
    /// We approximate the locals rule structurally: a reference binds to the
    /// definition of the same name in the nearest enclosing scope that has one.
    /// For function-local refactors this is sufficient and, crucially, carries
    /// zero cross-file candidate noise.
    pub fn references_bound_to(
        &self,
        name: &str,
        def_scope_range: (usize, usize),
    ) -> Vec<&Occurrence> {
        self.references
            .iter()
            .filter(|r| r.name == name)
            .filter(|r| {
                // The reference is governed by def_scope if def_scope encloses it
                // AND no *inner* scope between them re-defines `name`.
                let (ds, de) = def_scope_range;
                if !(ds <= r.start_byte && r.start_byte < de) {
                    return false;
                }
                !self.shadowed_between(name, r.start_byte, def_scope_range)
            })
            .collect()
    }

    /// True if some scope strictly inside `outer` (but still enclosing `at`)
    /// re-defines `name`, i.e. the reference is shadowed and does NOT bind to the
    /// outer definition.
    fn shadowed_between(&self, name: &str, at: usize, outer: (usize, usize)) -> bool {
        let (os, oe) = outer;
        self.definitions.iter().any(|d| {
            if d.name != name {
                return false;
            }
            let Some((ss, se)) = self.scope_range(d.scope_id) else {
                return false;
            };
            // An inner scope: contained in outer, contains `at`, and is smaller.
            os <= ss && se <= oe && (ss, se) != (os, oe) && ss <= at && at < se
        })
    }
}

/// A type/container definition discovered in *this* file (P0.2): a struct,
/// class, enum, trait, interface, or type alias. The type-closure builder
/// resolves each `type_refs` name against these before falling back to the
/// cross-file resolver (P0.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDefSite {
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line of the definition's name identifier.
    pub line: usize,
}

/// The single-file type closure of one root function (P0.2): every named type
/// its signature/body references, split into those defined in *this* file
/// (`resolved`) and those that are not (`unresolved`). Unresolved names are
/// handed to the cross-file resolver (P0.3); we never guess a definition for
/// them here (§7).
#[derive(Debug, Clone)]
pub struct ClosureSlice {
    pub root_name: String,
    /// Type definitions found in this file, deduped by name.
    pub resolved: Vec<TypeDefSite>,
    /// Type names referenced but not defined in this file, deduped, sorted.
    pub unresolved: Vec<String>,
}

impl LocalsIndex {
    /// Enumerate every top-level type/container definition in this file
    /// (`container_kinds`): struct/class/enum/trait/interface/type-alias. Used
    /// to resolve a `type_refs` name to its in-file definition (P0.2).
    ///
    /// Name extraction is grammar-tolerant: we first try the node's own `name`
    /// field, then (for wrapper nodes like Go's `type_declaration`) the `name`
    /// field of a named child. Anonymous containers (e.g. Rust `impl` blocks)
    /// contribute no name and are skipped.
    pub fn container_defs(&self) -> Vec<TypeDefSite> {
        let kinds = self.lang.container_kinds();
        let src = self.source.as_bytes();
        let mut out: Vec<TypeDefSite> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let root = self.tree.root_node();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if kinds.contains(&node.kind()) {
                if let Some(name_node) = container_name_node(node) {
                    let name = name_node.utf8_text(src).unwrap_or("").to_string();
                    if !name.is_empty() && seen.insert(name.clone()) {
                        out.push(TypeDefSite {
                            name,
                            start_byte: node.start_byte(),
                            end_byte: node.end_byte(),
                            line: name_node.start_position().row + 1,
                        });
                    }
                }
            }
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                stack.push(ch);
            }
        }
        out.sort_by_key(|d| d.start_byte);
        out
    }

    /// Byte range of the innermost `function_kinds` node enclosing `byte`, i.e.
    /// the function that a selection/anchor sits inside. Used to scope the
    /// type-closure to one function body (P0.2).
    pub fn enclosing_function_range(&self, byte: usize) -> Option<(usize, usize)> {
        let kinds = self.lang.function_kinds();
        let root = self.tree.root_node();
        let mut best: Option<(usize, usize)> = None;
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if kinds.contains(&node.kind()) && node.start_byte() <= byte && byte < node.end_byte() {
                let span = node.end_byte() - node.start_byte();
                match best {
                    Some((bs, be)) if (be - bs) <= span => {}
                    _ => best = Some((node.start_byte(), node.end_byte())),
                }
            }
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                stack.push(ch);
            }
        }
        best
    }

    /// Build the single-file type closure of the function whose byte range is
    /// `fn_range` (P0.2). Collects every `type_refs` occurrence inside the
    /// range, resolves each name against this file's `container_defs`, and
    /// partitions into resolved (in-file defs) vs unresolved (cross-file /
    /// external — handed to P0.3, never guessed here).
    ///
    /// `root_name` is a caller-supplied label for the root function (e.g. its
    /// symbol name) used only for reporting.
    pub fn type_closure(&self, root_name: &str, fn_range: (usize, usize)) -> ClosureSlice {
        let (lo, hi) = fn_range;
        let defs = self.container_defs();
        let def_by_name: BTreeMap<&str, &TypeDefSite> =
            defs.iter().map(|d| (d.name.as_str(), d)).collect();

        let mut resolved: Vec<TypeDefSite> = Vec::new();
        let mut resolved_names: std::collections::BTreeSet<String> = Default::default();
        let mut unresolved: std::collections::BTreeSet<String> = Default::default();

        for t in self.type_refs.iter() {
            if !(lo <= t.start_byte && t.start_byte < hi) {
                continue;
            }
            match def_by_name.get(t.name.as_str()) {
                Some(def) => {
                    if resolved_names.insert(t.name.clone()) {
                        resolved.push((*def).clone());
                    }
                }
                None => {
                    unresolved.insert(t.name.clone());
                }
            }
        }
        // A name resolved in-file is never also reported unresolved.
        for n in resolved_names.iter() {
            unresolved.remove(n);
        }
        resolved.sort_by_key(|d| d.start_byte);
        ClosureSlice {
            root_name: root_name.to_string(),
            resolved,
            unresolved: unresolved.into_iter().collect(),
        }
    }
}

/// Best-effort extraction of a container declaration's name node. Tries the
/// node's own `name` field first, then the `name` field of any named child
/// (covers wrapper grammars such as Go's `type_declaration` -> `type_spec`).
fn container_name_node(node: Node) -> Option<Node> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(n);
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        if let Some(n) = ch.child_by_field_name("name") {
            return Some(n);
        }
    }
    None
}

fn occurrence(source: &str, node: Node, start_byte: usize) -> Occurrence {
    let sp = node.start_position();
    Occurrence {
        name: node.utf8_text(source.as_bytes()).unwrap_or("").to_string(),
        start_byte,
        end_byte: node.end_byte(),
        line: sp.row + 1,
        col: sp.column,
        scope_id: 0,
    }
}

#[cfg(test)]
mod type_ref_tests {
    use super::*;

    fn names_of(idx: &LocalsIndex) -> Vec<String> {
        let mut v: Vec<String> = idx.type_refs.iter().map(|o| o.name.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn python_captures_type_refs() {
        let src =
            "def sync(events: EventBatch, conn: Conn) -> Report:\n    r = Report()\n    return r\n";
        let idx = LocalsIndex::build(Lang::Python, src).unwrap();
        let got = names_of(&idx);
        assert!(got.contains(&"EventBatch".to_string()), "got {got:?}");
        assert!(got.contains(&"Conn".to_string()), "got {got:?}");
        assert!(got.contains(&"Report".to_string()), "got {got:?}");
    }

    #[test]
    fn rust_captures_type_refs() {
        let src = "fn run(cfg: Config, table: SymbolTable) -> Report {\n    let x: Widget = Widget::new();\n    Report::default()\n}\n";
        let idx = LocalsIndex::build(Lang::Rust, src).unwrap();
        let got = names_of(&idx);
        assert!(got.contains(&"Config".to_string()), "got {got:?}");
        assert!(got.contains(&"SymbolTable".to_string()), "got {got:?}");
        assert!(got.contains(&"Report".to_string()), "got {got:?}");
        assert!(got.contains(&"Widget".to_string()), "got {got:?}");
    }

    #[test]
    fn typescript_captures_type_refs() {
        let src = "function run(cfg: Config, t: SymTable): Report {\n  const w: Widget = new Widget();\n  return w;\n}\n";
        let idx = LocalsIndex::build(Lang::TypeScript, src).unwrap();
        let got = names_of(&idx);
        assert!(got.contains(&"Config".to_string()), "got {got:?}");
        assert!(got.contains(&"SymTable".to_string()), "got {got:?}");
        assert!(got.contains(&"Report".to_string()), "got {got:?}");
        assert!(got.contains(&"Widget".to_string()), "got {got:?}");
    }

    #[test]
    fn go_captures_type_refs() {
        let src =
            "func run(cfg Config, t SymTable) Report {\n\tvar w Widget\n\treturn Report{}\n}\n";
        let idx = LocalsIndex::build(Lang::Go, src).unwrap();
        let got = names_of(&idx);
        assert!(got.contains(&"Config".to_string()), "got {got:?}");
        assert!(got.contains(&"SymTable".to_string()), "got {got:?}");
        assert!(got.contains(&"Widget".to_string()), "got {got:?}");
        assert!(got.contains(&"Report".to_string()), "got {got:?}");
    }
}

#[cfg(test)]
mod closure_tests {
    use super::*;

    #[test]
    fn python_single_file_closure_partitions() {
        // Report + Conn defined in-file; EventBatch is external (unresolved).
        let src = "\
class Report:\n    pass\n\nclass Conn:\n    pass\n\ndef sync(events: EventBatch, conn: Conn) -> Report:\n    r = Report()\n    return r\n";
        let idx = LocalsIndex::build(Lang::Python, src).unwrap();
        // Anchor inside sync().
        let anchor = src.find("r = Report()").unwrap();
        let range = idx.enclosing_function_range(anchor).expect("enclosing fn");
        let slice = idx.type_closure("sync", range);
        let resolved: Vec<&str> = slice.resolved.iter().map(|d| d.name.as_str()).collect();
        assert!(resolved.contains(&"Report"), "resolved={resolved:?}");
        assert!(resolved.contains(&"Conn"), "resolved={resolved:?}");
        assert!(
            slice.unresolved.contains(&"EventBatch".to_string()),
            "unresolved={:?}",
            slice.unresolved
        );
        // Resolved names never also appear unresolved.
        assert!(!slice.unresolved.contains(&"Report".to_string()));
    }

    #[test]
    fn rust_single_file_closure_finds_struct() {
        let src = "\
struct Config { n: usize }\n\nfn run(cfg: Config) -> Config {\n    let c: Config = cfg;\n    c\n}\n";
        let idx = LocalsIndex::build(Lang::Rust, src).unwrap();
        let anchor = src.find("let c: Config").unwrap();
        let range = idx.enclosing_function_range(anchor).unwrap();
        let slice = idx.type_closure("run", range);
        let resolved: Vec<&str> = slice.resolved.iter().map(|d| d.name.as_str()).collect();
        assert!(resolved.contains(&"Config"), "resolved={resolved:?}");
    }

    #[test]
    fn container_defs_enumerates_types() {
        let src = "\
struct A;\nenum B { X }\ntrait C {}\ntype D = u32;\n";
        let idx = LocalsIndex::build(Lang::Rust, src).unwrap();
        let mut got: Vec<String> = idx.container_defs().into_iter().map(|d| d.name).collect();
        got.sort();
        assert_eq!(got, vec!["A", "B", "C", "D"], "got={got:?}");
    }
}
