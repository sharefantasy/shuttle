//! new-module-wiring (§9 E6): create a brand-new module file and *wire it into*
//! its package aggregator — the `mod`/`export`/`import` line that makes the new
//! module reachable. This is the deterministic half of "add a new module": the
//! wiring line is a mechanical function of the module's file stem and its path
//! relative to the aggregator (§7「绝不瞎猜」); we never invent the module's
//! *contents* beyond a one-line placeholder, and we refuse anything a bare file
//! path cannot determine (Rust nested paths, Go cross-package import paths).
//!
//! Per aggregator language:
//!   * Rust  (`lib.rs`/`mod.rs`) — `pub mod <stem>;` (+ `pub use <stem>::<sym>;`
//!     when a symbol is named). Requires the module be a **sibling** of the
//!     aggregator; a nested path needs an intermediate `mod` we won't guess.
//!   * TS/JS (`index.ts`)       — `export * from './<stem>';` or
//!     `export { <sym> } from './<spec>';` (relative specifier computed).
//!   * Python (`__init__.py`)   — `from . import <stem>` or
//!     `from .<stem> import <sym>`. Sibling-only (nested packages not guessed).
//!   * Go — same directory is already the same package (no wiring needed);
//!     cross-directory needs the module's import path (go.mod) we can't infer.
//!     Either way E6 rejects with an honest explanation.
//!
//! The new module file is emitted with `CreateFile` (a minimal placeholder) only
//! when it does not already exist; the wiring line is appended to the aggregator
//! by anchoring on its full verbatim text (unique by construction), so the edit
//! range is never guessed. Idempotent: an aggregator that already carries the
//! wiring line is rejected rather than duplicated. Confidence is **High** — the
//! wiring is a pure projection — with warnings for the placeholder body and any
//! import path the caller must verify.

use shuttle_nav::Confidence;
use shuttle_protocol::EditOp;

use crate::locals::Lang;
use crate::move_function::relative_module_specifier;
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use std::path::Path;

/// Create `module_path` (if absent) and wire it into `aggregator_path`
/// (`aggregator_source` is its current text). `symbol`, when given, is
/// re-exported by name; otherwise the whole module is re-exported / declared.
pub fn new_module_wiring(
    module_path: &str,
    aggregator_path: &str,
    aggregator_source: &str,
    symbol: Option<&str>,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(aggregator_path)
        .ok_or_else(|| RefactorError::UnsupportedLang(aggregator_path.into()))?;
    let mod_lang = Lang::from_path(module_path)
        .ok_or_else(|| RefactorError::UnsupportedLang(module_path.into()))?;
    if lang != mod_lang {
        return Err(RefactorError::Rejected(format!(
            "module `{module_path}` and aggregator `{aggregator_path}` are different languages"
        )));
    }
    if module_path == aggregator_path {
        return Err(RefactorError::Rejected(
            "module and aggregator are the same file — nothing to wire".into(),
        ));
    }
    if aggregator_source.trim().is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{aggregator_path}` is empty — nothing to anchor the wiring line on; add the module \
             declaration by hand"
        )));
    }

    let stem = Path::new(module_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            RefactorError::Rejected(format!("cannot derive a module name from `{module_path}`"))
        })?
        .to_string();

    let same_dir = Path::new(module_path).parent() == Path::new(aggregator_path).parent();

    // Build the language-idiomatic wiring line (or reject).
    let wiring = match lang {
        Lang::Rust => {
            if !same_dir {
                return Err(RefactorError::Rejected(format!(
                    "`{module_path}` is not a sibling of `{aggregator_path}` — Rust needs an \
                     intermediate `mod` path that cannot be inferred from a file path (§7)"
                )));
            }
            match symbol {
                Some(sym) => format!("pub mod {stem};\npub use {stem}::{sym};"),
                None => format!("pub mod {stem};"),
            }
        }
        Lang::Python => {
            if !same_dir {
                return Err(RefactorError::Rejected(format!(
                    "`{module_path}` is not a sibling of `{aggregator_path}` — nested-package \
                     wiring is not inferable from a file path (§7)"
                )));
            }
            match symbol {
                Some(sym) => format!("from .{stem} import {sym}"),
                None => format!("from . import {stem}"),
            }
        }
        Lang::TypeScript | Lang::JavaScript => {
            let spec = relative_module_specifier(aggregator_path, module_path)
                .map_err(RefactorError::Rejected)?;
            match symbol {
                Some(sym) => format!("export {{ {sym} }} from '{spec}';"),
                None => format!("export * from '{spec}';"),
            }
        }
        Lang::Go => {
            return Err(RefactorError::Rejected(if same_dir {
                "Go files in the same directory already share a package — a new sibling module \
                     needs no wiring line"
                    .to_string()
            } else {
                format!(
                    "Go cross-directory wiring needs `{module_path}`'s import path (module \
                         root/go.mod), which cannot be inferred from a file path"
                )
            }));
        }
    };

    // Idempotence: refuse if any wiring line is already present.
    if aggregator_source
        .lines()
        .any(|l| l.trim() == wiring.lines().next().unwrap_or(&wiring).trim())
    {
        return Err(RefactorError::Rejected(format!(
            "`{aggregator_path}` already wires `{stem}` — nothing to add"
        )));
    }

    let mut ops: Vec<EditOp> = Vec::new();
    let mut sites: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Create the module file with a minimal placeholder — only when it does not
    // already exist (we never clobber; §7).
    if !Path::new(module_path).exists() {
        let content = placeholder_module(lang, &stem);
        ops.push(EditOp::CreateFile {
            path: module_path.to_string(),
            content,
        });
        sites.push(format!("{module_path} (create new module)"));
        warnings.push(format!(
            "`{module_path}` was created with a placeholder body — add the module's real contents"
        ));
    } else {
        warnings.push(format!(
            "`{module_path}` already exists — only the wiring line was added; verify it exports \
             what the aggregator expects"
        ));
    }

    // Append the wiring line to the aggregator, anchoring on its full text.
    let block = aggregator_source.to_string();
    let replacement = format!("{}\n{wiring}\n", block.trim_end());
    ops.push(search_replace(aggregator_path, block, replacement, 1));
    sites.push(format!("{aggregator_path} (wire `{stem}`)"));

    if let Some(sym) = symbol {
        if matches!(lang, Lang::TypeScript | Lang::JavaScript | Lang::Python) {
            warnings.push(format!(
                "the wiring re-exports `{sym}` — ensure the module actually defines it"
            ));
        }
    }

    let intent = format!("new-module-wiring: {stem} → {aggregator_path}");
    let plan = plan_from_ops(RefactorKind::NewModuleWiring, aggregator_path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::NewModuleWiring,
        plan,
        signature: Some(format!("wire {stem} into {aggregator_path}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::High,
        warnings,
        sites,
    })
}

/// A minimal, valid placeholder body for a freshly-created module.
fn placeholder_module(lang: Lang, stem: &str) -> String {
    match lang {
        Lang::Rust => format!("//! `{stem}` module (generated stub — add real contents).\n"),
        Lang::Python => {
            format!("\"\"\"`{stem}` module (generated stub — add real contents).\"\"\"\n")
        }
        Lang::TypeScript | Lang::JavaScript => {
            format!("// `{stem}` module (generated stub — add real contents).\nexport {{}};\n")
        }
        Lang::Go => format!("// `{stem}` module (generated stub).\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacement(plan: &RefactorPlan, path: &str) -> String {
        plan.plan
            .ops
            .iter()
            .find_map(|o| match o {
                EditOp::SearchReplace {
                    path: p, replace, ..
                } if p == path => Some(replace.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn created(plan: &RefactorPlan) -> Option<(String, String)> {
        plan.plan.ops.iter().find_map(|o| match o {
            EditOp::CreateFile { path, content } => Some((path.clone(), content.clone())),
            _ => None,
        })
    }

    // A module path guaranteed not to exist so the CreateFile branch fires.
    fn tmp_mod(name: &str) -> String {
        let base = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".into());
        format!("{base}/__ea_e6_nonexistent__/{name}")
    }

    #[test]
    fn rust_wires_pub_mod() {
        // A sibling module under a guaranteed-nonexistent dir, so its
        // aggregator sits beside it and the CreateFile branch fires.
        let dir = tmp_mod("pkg");
        let agg = format!("{dir}/lib.rs");
        let module = format!("{dir}/foo.rs");
        let src = "pub mod a;\npub mod b;\n";
        let plan = new_module_wiring(&module, &agg, src, None).unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        let r = replacement(&plan, &agg);
        assert!(r.contains("pub mod b;\npub mod foo;\n"));
        // A stub module file is created.
        let (p, c) = created(&plan).unwrap();
        assert_eq!(p, module);
        assert!(c.contains("module"));
    }

    #[test]
    fn rust_wires_pub_use_when_symbol_given() {
        let agg = "src/lib.rs";
        let src = "pub mod a;\n";
        let plan = new_module_wiring("src/thing.rs", agg, src, Some("Thing")).unwrap();
        let r = replacement(&plan, agg);
        assert!(r.contains("pub mod thing;\npub use thing::Thing;\n"));
    }

    #[test]
    fn rust_rejects_nested_module() {
        let err =
            new_module_wiring("src/sub/foo.rs", "src/lib.rs", "pub mod a;\n", None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("sibling")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn ts_export_star_and_named() {
        let star = new_module_wiring(
            "src/foo.ts",
            "src/index.ts",
            "export * from './bar';\n",
            None,
        )
        .unwrap();
        assert!(replacement(&star, "src/index.ts").contains("export * from './foo';"));
        let named = new_module_wiring(
            "src/foo.ts",
            "src/index.ts",
            "export * from './bar';\n",
            Some("F"),
        )
        .unwrap();
        assert!(replacement(&named, "src/index.ts").contains("export { F } from './foo';"));
    }

    #[test]
    fn python_from_import() {
        let plan = new_module_wiring("pkg/foo.py", "pkg/__init__.py", "from . import bar\n", None)
            .unwrap();
        assert!(replacement(&plan, "pkg/__init__.py").contains("from . import foo"));
        let named = new_module_wiring(
            "pkg/foo.py",
            "pkg/__init__.py",
            "from . import bar\n",
            Some("Foo"),
        )
        .unwrap();
        assert!(replacement(&named, "pkg/__init__.py").contains("from .foo import Foo"));
    }

    #[test]
    fn go_same_dir_needs_no_wiring() {
        let err = new_module_wiring("pkg/foo.go", "pkg/agg.go", "package pkg\n", None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("share a package")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn go_cross_dir_cannot_infer() {
        let err =
            new_module_wiring("other/foo.go", "pkg/agg.go", "package pkg\n", None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("import path")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_already_wired() {
        let src = "pub mod a;\npub mod foo;\n";
        let err = new_module_wiring("src/foo.rs", "src/lib.rs", src, None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("already wires")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_aggregator() {
        let err = new_module_wiring("src/foo.rs", "src/lib.rs", "\n", None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("empty")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_cross_language() {
        let err = new_module_wiring("src/foo.py", "src/lib.rs", "pub mod a;\n", None).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("different languages")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
