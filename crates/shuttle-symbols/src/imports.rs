//! File→file *import direction*: the daemonless, native realization of an
//! import graph. It exists to give [`crate::graph::SymbolGraph`] a
//! **direction** signal: the symbol-name reference edges are inherently
//! bidirectional (a name appears in both files), so their raw reverse closure
//! is cyclic. Intersecting those edges with the *acyclic* import direction
//! prunes the back-edges and yields a mechanical, zero-model DAG (see
//! ).
//!
//! # Guarantees / non-guarantees (§7 — never guess)
//! * Resolution is **closed over the given file universe**: an import that does
//!   not resolve to a file *in the universe* is dropped, never invented.
//! * Only statically visible imports are seen. Dynamic import / reflection /
//!   macro-generated references are structurally invisible and are simply
//!   absent — we never fabricate a direction for them.
//! * When a language's resolution is ambiguous we prefer to emit **no** edge
//!   over a guessed one; the consumer then degrades that hop to a weak
//!   name-match edge rather than trusting a fabricated direction.

use std::collections::{BTreeMap, BTreeSet};

use crate::SymbolError;

/// A file→file import relation: `direct[fv] = { fu : fv imports fu }`, at file
/// granularity, closed over a fixed file universe.
#[derive(Debug, Clone, Default)]
pub struct ImportGraph {
    direct: BTreeMap<String, BTreeSet<String>>,
    /// Go package (directory) → member `.go` source files present in the
    /// universe. A Go package's compile unit is the *directory*: sibling files
    /// in the same directory are the same package and reference each other with
    /// **no** import statement. Persisting this lets the SymbolGraph certify an
    /// intra-package cross-file reference as trusted-directed at file
    /// granularity instead of degrading it to a weak name-match — the
    /// "Go 包→文件粒度" alignment .
    go_pkg_files: BTreeMap<String, BTreeSet<String>>,
}

impl ImportGraph {
    /// Build the import relation from a `path → contents` map. The keys of
    /// `contents` define the *universe*: an import resolves only if it lands on
    /// one of those paths. A `go.mod` present in the map (any path whose file
    /// name is `go.mod`) supplies the Go module prefix; without it, Go imports
    /// under an unknown module simply do not resolve (degrade, never guess).
    pub fn build(contents: &BTreeMap<String, String>) -> Self {
        Self::try_build(contents, usize::MAX).unwrap_or_default()
    }

    /// Build with a hard cap on stored file-to-file edges. This is the
    /// production entry for callers that must fail closed under pathological
    /// import fan-out instead of allocating the full Cartesian relation.
    pub fn try_build(
        contents: &BTreeMap<String, String>,
        max_edges: usize,
    ) -> Result<Self, SymbolError> {
        let universe: BTreeSet<&str> = contents.keys().map(String::as_str).collect();
        let go_module = detect_go_module(contents);
        // Go: package (directory) → member source files, for import fan-out
        // *and* intra-package direction (see the `go_pkg_files` field / T4). A
        // `BTreeSet` keeps membership deduplicated and deterministic.
        let mut go_pkg_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for p in universe.iter() {
            if p.ends_with(".go") {
                let dir = dir_of(p).to_string();
                go_pkg_files
                    .entry(dir)
                    .or_default()
                    .insert((*p).to_string());
            }
        }

        let mut direct: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut edge_count = 0usize;
        for (path, content) in contents {
            let mut deps: BTreeSet<String> = BTreeSet::new();
            match lang_of(path) {
                Some(Lang::TsJs) => resolve_ts(path, content, &universe, &mut deps),
                Some(Lang::Python) => resolve_py(path, content, &universe, &mut deps),
                Some(Lang::Go) => {
                    if let Some((module, root)) = &go_module {
                        resolve_go(path, content, module, root, &go_pkg_files, &mut deps);
                    }
                }
                Some(Lang::Rust) => resolve_rust(path, content, &universe, &mut deps),
                None => {}
            }
            deps.remove(path); // never a self-edge
            if !deps.is_empty() {
                edge_count = edge_count.saturating_add(deps.len());
                if edge_count > max_edges {
                    return Err(SymbolError::Limit {
                        resource: "graph_import_edges",
                        observed: edge_count as u64,
                        limit: max_edges as u64,
                    });
                }
                direct.insert(path.clone(), deps);
            }
        }
        Ok(ImportGraph {
            direct,
            go_pkg_files,
        })
    }

    /// Files that `file` imports *directly*.
    pub fn imports(&self, file: &str) -> BTreeSet<&str> {
        self.direct
            .get(file)
            .map(|s| s.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Does `fv` depend on `fu` transitively (fv imports … imports fu)?
    /// Cycle-safe. `fv == fu` is false (a file does not depend on itself).
    pub fn depends_on(&self, fv: &str, fu: &str) -> bool {
        if fv == fu {
            return false;
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = self
            .direct
            .get(fv)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        while let Some(cur) = stack.pop() {
            if cur == fu {
                return true;
            }
            if !seen.insert(cur) {
                continue;
            }
            if let Some(next) = self.direct.get(cur) {
                stack.extend(next.iter().map(String::as_str));
            }
        }
        false
    }

    /// Transitive closure `fv -> { every fu reachable by imports }`.
    pub fn transitive(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for start in self.direct.keys() {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut stack: Vec<String> = self
                .direct
                .get(start)
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            while let Some(cur) = stack.pop() {
                if !seen.insert(cur.clone()) {
                    continue;
                }
                if let Some(next) = self.direct.get(&cur) {
                    stack.extend(next.iter().cloned());
                }
            }
            seen.remove(start);
            out.insert(start.clone(), seen);
        }
        out
    }

    /// Number of directed import edges (for telemetry / tests).
    pub fn edge_count(&self) -> usize {
        self.direct.values().map(BTreeSet::len).sum()
    }

    /// Is the relation empty (no import direction known at all)? Consumers use
    /// this to decide whether to degrade to pure name-match edges.
    pub fn is_empty(&self) -> bool {
        self.direct.is_empty()
    }

    /// Do two distinct files belong to the **same Go package**?
    ///
    /// A Go package is exactly a directory of `.go` files: siblings in the same
    /// directory compile as one unit and see each other's top-level identifiers
    /// with **no** import statement. That makes an intra-package cross-file
    /// reference a *provable, correctly-directed* dependency at file
    /// granularity — the compiler already guarantees the two files are one
    /// package — not a name coincidence. This is the language rule (same-dir
    /// `.go` = same package), not a guess, so a consumer may trust it as an
    /// import-directed edge (§7 — we only claim what the grammar guarantees).
    ///
    /// Returns `false` for a file compared with itself, for non-`.go` files,
    /// and for files in different directories. Membership is closed over the
    /// build-time universe: a `.go` file that was never scanned is unknown and
    /// yields `false` rather than a guessed edge.
    pub fn same_go_package(&self, a: &str, b: &str) -> bool {
        if a == b || !a.ends_with(".go") || !b.ends_with(".go") {
            return false;
        }
        let da = dir_of(a);
        if da != dir_of(b) {
            return false;
        }
        match self.go_pkg_files.get(da) {
            Some(files) => files.contains(a) && files.contains(b),
            None => false,
        }
    }
}

// --------------------------------------------------------------------------
// language dispatch
// --------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Lang {
    TsJs,
    Python,
    Go,
    Rust,
}

fn lang_of(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::TsJs),
        "py" | "pyi" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        "rs" => Some(Lang::Rust),
        _ => None,
    }
}

/// Whether the native dependency graph can establish direction for `path`.
///
/// Ctags may extract definitions from many more languages, but definition
/// availability alone is not enough to authorize a hard dependency edge.
/// Callers use this predicate to degrade unsupported task inputs before they
/// mistake a name match for directed evidence.
pub fn supports_dependency_path(path: &str) -> bool {
    lang_of(path).is_some()
}

/// The directory component of a `/`-separated path (`""` for a bare file name).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Normalize a `.`/`..`-containing `/`-separated path into canonical form.
fn norm_join(base_dir: &str, rel: &str) -> String {
    // Preserve a leading `/`: an absolute base (`/Users/x`) must stay absolute
    // so candidates match an absolute-path universe. Splitting on `/` drops the
    // empty leading segment, so we remember it and re-attach it.
    let absolute = base_dir.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    let start = if base_dir.is_empty() { "" } else { base_dir };
    for seg in start.split('/').chain(rel.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

// --------------------------------------------------------------------------
// TS / JS  (ESM `from "./x"`, `export … from`, `require("./x")`, `import("./x")`)
// --------------------------------------------------------------------------

fn resolve_ts(path: &str, content: &str, universe: &BTreeSet<&str>, out: &mut BTreeSet<String>) {
    let base = dir_of(path);
    for spec in ts_specifiers(content) {
        // Only relative specifiers are first-party file edges.
        if !(spec.starts_with("./") || spec.starts_with("../")) {
            continue;
        }
        let joined = norm_join(base, &spec);
        for cand in ts_candidates(&joined) {
            if universe.contains(cand.as_str()) {
                out.insert(cand);
                break;
            }
        }
    }
}

/// Candidate resolved paths for a TS/JS relative specifier, in priority order.
fn ts_candidates(joined: &str) -> Vec<String> {
    let mut c = Vec::new();
    // Explicit `.js` in an ESM import usually maps to a `.ts` source.
    if let Some(stem) = joined.strip_suffix(".js") {
        c.push(format!("{stem}.ts"));
        c.push(format!("{stem}.tsx"));
    }
    if joined.ends_with(".ts") || joined.ends_with(".tsx") {
        c.push(joined.to_string());
    } else {
        for ext in ["ts", "tsx", "js", "jsx"] {
            c.push(format!("{joined}.{ext}"));
        }
        for ext in ["ts", "tsx", "js", "jsx"] {
            c.push(format!("{joined}/index.{ext}"));
        }
    }
    c
}

/// Extract module specifiers from `import`/`export … from` / `require(…)` /
/// dynamic `import(…)`. Line-based and tolerant: it looks for the keyword then
/// the first quoted string on that logical line.
fn ts_specifiers(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim_start();
        let has_from = (t.starts_with("import") || t.starts_with("export")) && t.contains("from");
        let has_bare_import = t.starts_with("import") && t.contains('"')
            || t.starts_with("import") && t.contains('\'');
        if has_from || t.contains("require(") || t.contains("import(") || has_bare_import {
            if let Some(s) = first_quoted(line) {
                out.push(s);
            }
        }
    }
    out
}

// --------------------------------------------------------------------------
// Python  (`from .mod import …`, `from pkg.mod import …`, `import pkg.mod`)
// --------------------------------------------------------------------------

fn resolve_py(path: &str, content: &str, universe: &BTreeSet<&str>, out: &mut BTreeSet<String>) {
    let base = dir_of(path);
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("from ") {
            // `from X import …` — X is the module.
            let module = rest.split(" import").next().unwrap_or("").trim();
            for cand in py_candidates(base, module, universe) {
                out.insert(cand);
            }
        } else if let Some(rest) = t.strip_prefix("import ") {
            // `import a.b.c[, …]` — each comma-separated absolute module.
            for part in rest.split(',') {
                let module = part.split(" as ").next().unwrap_or("").trim();
                for cand in py_candidates(base, module, universe) {
                    out.insert(cand);
                }
            }
        }
    }
}

/// Resolve a Python module reference (relative `.mod` or absolute `pkg.mod`) to
/// candidate files inside the universe.
fn py_candidates(base: &str, module: &str, universe: &BTreeSet<&str>) -> Vec<String> {
    if module.is_empty() {
        return Vec::new();
    }
    let mut resolved = Vec::new();
    // Leading dots = relative levels: one dot = current package.
    let leading_dots = module.chars().take_while(|c| *c == '.').count();
    if leading_dots > 0 {
        // Ascend `leading_dots - 1` directories from base.
        let mut dir = base.to_string();
        for _ in 0..leading_dots.saturating_sub(1) {
            dir = dir_of(&dir).to_string();
        }
        let tail = &module[leading_dots..];
        let rel = tail.replace('.', "/");
        push_py_paths(&dir, &rel, universe, &mut resolved);
    } else {
        // Absolute: the module is resolved against some source root on
        // `sys.path`, which we do not know. Rather than guess a single root, we
        // try the repo root and every ancestor directory of the importing file
        // as a candidate anchor, keeping only paths that land on a *real* file
        // in the universe. This covers flat repos (sibling modules), `src`
        // layouts, and nested packages without inventing anything (§7): an
        // anchor that resolves to no universe file contributes no edge.
        let rel = module.replace('.', "/");
        let mut anchor = base.to_string();
        loop {
            push_py_paths(&anchor, &rel, universe, &mut resolved);
            if anchor.is_empty() {
                break;
            }
            let parent = dir_of(&anchor).to_string();
            if parent == anchor {
                // dir_of returned itself (no more separators): finish at root.
                push_py_paths("", &rel, universe, &mut resolved);
                break;
            }
            anchor = parent;
        }
    }
    resolved.sort();
    resolved.dedup();
    resolved
}

fn push_py_paths(dir: &str, rel: &str, universe: &BTreeSet<&str>, out: &mut Vec<String>) {
    let joined = if rel.is_empty() {
        dir.to_string()
    } else {
        norm_join(dir, rel)
    };
    for cand in [format!("{joined}.py"), format!("{joined}/__init__.py")] {
        if universe.contains(cand.as_str()) {
            out.push(cand);
        }
    }
}

// --------------------------------------------------------------------------
// Go  (import block + single import; package = directory fan-out)
// --------------------------------------------------------------------------

/// Locate the module's `go.mod`: returns `(module_path, root_dir)` where
/// `root_dir` is the directory containing `go.mod` (scan-relative, matching how
/// `go_pkg_files` is keyed). A first-party import `module/<sub>` resolves to the
/// on-disk directory `root_dir/<sub>` — anchoring at `go.mod` is what makes
/// cross-package fan-out work even when the scan root is *above* the module.
fn detect_go_module(contents: &BTreeMap<String, String>) -> Option<(String, String)> {
    for (path, content) in contents {
        if path.rsplit('/').next() == Some("go.mod") {
            for line in content.lines() {
                let t = line.trim_start();
                if let Some(rest) = t.strip_prefix("module ") {
                    return Some((rest.trim().to_string(), dir_of(path).to_string()));
                }
            }
        }
    }
    None
}

fn resolve_go(
    path: &str,
    content: &str,
    module: &str,
    root_dir: &str,
    go_pkg_files: &BTreeMap<String, BTreeSet<String>>,
    out: &mut BTreeSet<String>,
) {
    for spec in go_import_specs(content) {
        // Only first-party packages under the module path.
        let sub = if spec == module {
            ""
        } else if let Some(sub) = spec.strip_prefix(&format!("{module}/")) {
            sub
        } else {
            continue;
        };
        // Map the module-relative package path onto its on-disk directory,
        // anchored at the `go.mod` location (`root_dir`). `go_pkg_files` is
        // keyed by scan-relative directories, so a bare `sub` would only match
        // when `go.mod` happened to sit at the scan root. Joining through
        // `root_dir` makes cross-package fan-out correct regardless of where
        // the scan was rooted.
        let pkgdir = norm_join(root_dir, sub);
        if let Some(files) = go_pkg_files.get(&pkgdir) {
            for fu in files {
                if fu != path {
                    out.insert(fu.clone());
                }
            }
        }
    }
}

/// Import specifiers from a Go file: both the `import ( … )` block and single
/// `import "…"` lines. Returns the quoted package paths.
fn go_import_specs(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let t = line.trim();
        if !in_block {
            if t.starts_with("import (") {
                in_block = true;
                continue;
            }
            if let Some(rest) = t.strip_prefix("import ") {
                if let Some(s) = first_quoted(rest) {
                    out.push(s);
                }
            }
        } else {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            if let Some(s) = first_quoted(t) {
                out.push(s);
            }
        }
    }
    out
}

// --------------------------------------------------------------------------
// Rust  (`mod foo;` sibling/submodule; `use crate::a::b` best-effort)
// --------------------------------------------------------------------------

fn resolve_rust(path: &str, content: &str, universe: &BTreeSet<&str>, out: &mut BTreeSet<String>) {
    let base = dir_of(path);
    // crate root = the `src` dir that owns this file, if discernible.
    let src_root: Option<&str> = path.rfind("/src/").map(|i| &path[..i + 4]);
    for line in content.lines() {
        let t = line.trim_start();
        // `mod foo;` → foo.rs or foo/mod.rs in the same directory.
        if let Some(rest) = t.strip_prefix("mod ") {
            let name = rest.trim_end_matches(';').trim();
            if name.is_empty() || name.contains('{') {
                continue; // inline module body, not a file
            }
            for cand in [
                norm_join(base, &format!("{name}.rs")),
                norm_join(base, &format!("{name}/mod.rs")),
            ] {
                if universe.contains(cand.as_str()) {
                    out.insert(cand);
                    break;
                }
            }
        }
        // `use crate::a::b::…` / `pub use crate::…` → resolve to a *file*, not an
        // item. Because the trailing segments may name items inside a module
        // file (`use crate::util::u` → `util.rs`, item `u`), we try successively
        // shorter module paths and accept the *longest* that maps to a real file
        // in the universe. This is deterministic prefix resolution, not a guess:
        // if nothing resolves the hop degrades to a weak name-match edge (§7).
        let use_rest = t
            .strip_prefix("use crate::")
            .or_else(|| t.strip_prefix("pub use crate::"))
            .or_else(|| t.strip_prefix("pub(crate) use crate::"));
        if let (Some(rest), Some(src_root)) = (use_rest, src_root) {
            // Segment path up to the first terminator, dropping any `as`/glob tail.
            let segs_src = rest.split([';', '{', ' ']).next().unwrap_or("");
            let segs: Vec<&str> = segs_src.split("::").filter(|s| !s.is_empty()).collect();
            // Try longest→shortest module prefix (drop trailing item segments).
            let mut n = segs.len();
            while n >= 1 {
                let joined = segs[..n].join("/");
                let mut hit = false;
                for cand in [
                    norm_join("", &format!("{src_root}/{joined}.rs")),
                    norm_join("", &format!("{src_root}/{joined}/mod.rs")),
                ] {
                    if universe.contains(cand.as_str()) {
                        out.insert(cand);
                        hit = true;
                        break;
                    }
                }
                if hit {
                    break;
                }
                n -= 1;
            }
        }
    }
}

// --------------------------------------------------------------------------
// shared
// --------------------------------------------------------------------------

/// The first single- or double-quoted string on `line`, if any.
fn first_quoted(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let q = bytes[i];
        if q == b'"' || q == b'\'' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != q {
                j += 1;
            }
            if j < bytes.len() {
                return Some(line[start..j].to_string());
            }
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn ts_relative_import_resolves() {
        let c = cts(&[
            ("src/a.ts", "import { b } from './b';\nexport const a = 1;"),
            ("src/b.ts", "export const b = 2;"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("src/a.ts").contains("src/b.ts"));
        assert!(g.imports("src/b.ts").is_empty());
        assert!(g.depends_on("src/a.ts", "src/b.ts"));
        assert!(!g.depends_on("src/b.ts", "src/a.ts"));
    }

    #[test]
    fn ts_js_specifier_maps_to_ts_source() {
        let c = cts(&[
            ("src/a.ts", "import {b} from './sub/b.js';"),
            ("src/sub/b.ts", "export const b = 2;"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("src/a.ts").contains("src/sub/b.ts"));
    }

    #[test]
    fn ts_index_resolution() {
        let c = cts(&[
            ("src/a.ts", "import {b} from './sub';"),
            ("src/sub/index.ts", "export const b = 2;"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("src/a.ts").contains("src/sub/index.ts"));
    }

    #[test]
    fn ts_bare_package_import_ignored() {
        let c = cts(&[(
            "src/a.ts",
            "import fs from 'fs';\nimport {x} from 'lodash';",
        )]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("src/a.ts").is_empty());
    }

    #[test]
    fn python_relative_and_absolute() {
        let c = cts(&[
            ("pkg/a.py", "from .b import thing\nimport pkg.c"),
            ("pkg/b.py", "thing = 1"),
            ("pkg/c.py", "x = 1"),
        ]);
        let g = ImportGraph::build(&c);
        let imps = g.imports("pkg/a.py");
        assert!(imps.contains("pkg/b.py"), "relative from .b -> pkg/b.py");
        assert!(imps.contains("pkg/c.py"), "absolute pkg.c -> pkg/c.py");
    }

    #[test]
    fn python_parent_relative() {
        let c = cts(&[
            ("pkg/sub/a.py", "from ..top import y"),
            ("pkg/top.py", "y = 1"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("pkg/sub/a.py").contains("pkg/top.py"));
    }

    #[test]
    fn python_package_init() {
        let c = cts(&[
            ("app/main.py", "from lib import helper"),
            ("lib/__init__.py", "helper = 1"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("app/main.py").contains("lib/__init__.py"));
    }

    #[test]
    fn go_import_block_fans_out_to_package_files() {
        let c = cts(&[
            ("go.mod", "module example.com/app\n\ngo 1.21\n"),
            (
                "main.go",
                "package main\nimport (\n\t\"example.com/app/store\"\n)\nfunc main(){}",
            ),
            ("store/a.go", "package store\nfunc A(){}"),
            ("store/b.go", "package store\nfunc B(){}"),
        ]);
        let g = ImportGraph::build(&c);
        let imps = g.imports("main.go");
        assert!(imps.contains("store/a.go"), "fan-out to a.go: {imps:?}");
        assert!(imps.contains("store/b.go"), "fan-out to b.go: {imps:?}");
    }

    #[test]
    fn go_cross_package_import_resolves_when_gomod_below_scan_root() {
        // Regression (T4): `go_pkg_files` is keyed by scan-relative directories,
        // so a first-party import `module/<sub>` must be anchored at the
        // `go.mod` *directory* — not treated as scan-root-relative. When the
        // scan root sits ABOVE the module (paths carry a `repo/` prefix), the
        // old code looked up a bare `store` key and silently found nothing,
        // dropping every cross-package edge.
        let c = cts(&[
            ("repo/go.mod", "module example.com/app\n\ngo 1.21\n"),
            (
                "repo/main.go",
                "package main\nimport (\n\t\"example.com/app/store\"\n)\nfunc main(){}",
            ),
            ("repo/store/a.go", "package store\nfunc A(){}"),
            ("repo/store/b.go", "package store\nfunc B(){}"),
        ]);
        let g = ImportGraph::build(&c);
        let imps = g.imports("repo/main.go");
        assert!(
            imps.contains("repo/store/a.go"),
            "anchored fan-out to repo/store/a.go: {imps:?}"
        );
        assert!(
            imps.contains("repo/store/b.go"),
            "anchored fan-out to repo/store/b.go: {imps:?}"
        );
    }

    #[test]
    fn go_single_import() {
        let c = cts(&[
            ("go.mod", "module m\n"),
            ("main.go", "package main\nimport \"m/db\"\n"),
            ("db/db.go", "package db\n"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("main.go").contains("db/db.go"));
    }

    #[test]
    fn go_third_party_import_ignored() {
        let c = cts(&[
            ("go.mod", "module m\n"),
            ("main.go", "package main\nimport \"github.com/x/y\"\n"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("main.go").is_empty());
    }

    #[test]
    fn go_same_package_is_recognized_at_file_granularity() {
        // Two sibling files in the same directory form one Go package; the
        // relation must recognize their same-package membership even though
        // neither file `import`s the other (Go compile unit = directory).
        let c = cts(&[
            ("go.mod", "module example.com/app\n"),
            ("store/a.go", "package store\nfunc A(){}"),
            ("store/b.go", "package store\nfunc B(){ A() }"),
            ("other/c.go", "package other\nfunc C(){}"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(
            g.same_go_package("store/a.go", "store/b.go"),
            "siblings in store/ are the same package"
        );
        assert!(
            g.same_go_package("store/b.go", "store/a.go"),
            "same-package is symmetric"
        );
        // Different directory = different package.
        assert!(!g.same_go_package("store/a.go", "other/c.go"));
        // A file is not its own "other" package member.
        assert!(!g.same_go_package("store/a.go", "store/a.go"));
        // Non-`.go` files never count as a Go package.
        assert!(!g.same_go_package("store/a.go", "store/notes.txt"));
        // A `.go` file outside the scanned universe is unknown, not guessed.
        assert!(!g.same_go_package("store/a.go", "store/ghost.go"));
    }

    #[test]
    fn go_same_package_needs_no_gomod() {
        // Same-package membership is a directory fact independent of go.mod;
        // cross-package *import* resolution needs the module prefix, but two
        // siblings sharing a directory are the same package regardless.
        let c = cts(&[
            ("pkg/x.go", "package pkg\nfunc X(){}"),
            ("pkg/y.go", "package pkg\nfunc Y(){}"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.same_go_package("pkg/x.go", "pkg/y.go"));
    }

    #[test]
    fn go_root_package_siblings_are_same_package() {
        // Files in the repo root (dir = "") are still one package together.
        let c = cts(&[
            ("main.go", "package main\nfunc main(){}"),
            ("helper.go", "package main\nfunc helper(){}"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.same_go_package("main.go", "helper.go"));
    }

    #[test]
    fn go_without_gomod_degrades() {
        // No go.mod in the universe -> module unknown -> no guessed edges (§7).
        let c = cts(&[
            ("main.go", "package main\nimport \"m/db\"\n"),
            ("db/db.go", "package db\n"),
        ]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("main.go").is_empty());
    }

    #[test]
    fn rust_mod_and_use_crate() {
        let c = cts(&[
            ("crate/src/lib.rs", "mod helper;\npub use crate::util::u;"),
            ("crate/src/helper.rs", "pub fn h() {}"),
            ("crate/src/util.rs", "pub fn u() {}"),
        ]);
        let g = ImportGraph::build(&c);
        let imps = g.imports("crate/src/lib.rs");
        assert!(imps.contains("crate/src/helper.rs"), "mod helper: {imps:?}");
        assert!(
            imps.contains("crate/src/util.rs"),
            "use crate::util: {imps:?}"
        );
    }

    #[test]
    fn transitive_and_cycle_safe() {
        let c = cts(&[
            ("a.ts", "import {b} from './b';"),
            ("b.ts", "import {c} from './c';"),
            ("c.ts", "import {a} from './a';"), // cycle a->b->c->a
        ]);
        let g = ImportGraph::build(&c);
        // transitive must terminate despite the cycle.
        let t = g.transitive();
        assert!(t["a.ts"].contains("c.ts"));
        assert!(g.depends_on("a.ts", "c.ts"));
        assert!(g.depends_on("c.ts", "b.ts"));
    }

    #[test]
    fn unresolved_import_dropped_not_invented() {
        let c = cts(&[("src/a.ts", "import {b} from './missing';")]);
        let g = ImportGraph::build(&c);
        assert!(g.imports("src/a.ts").is_empty());
        assert!(g.is_empty());
    }

    #[test]
    fn norm_join_handles_dotdot() {
        assert_eq!(norm_join("pkg/sub", "../top"), "pkg/top");
        assert_eq!(norm_join("a/b", "./c"), "a/b/c");
        assert_eq!(norm_join("", "x/y"), "x/y");
        // Absolute bases stay absolute so candidates match an absolute universe.
        assert_eq!(norm_join("/Users/x/pkg", "a/b"), "/Users/x/pkg/a/b");
        assert_eq!(norm_join("/Users/x/pkg/sub", "../top"), "/Users/x/pkg/top");
    }
}
