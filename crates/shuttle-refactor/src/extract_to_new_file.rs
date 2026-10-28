//! extract-to-new-file (§9 E4, a creation-family cousin of move-function):
//! relocate a uniquely-defined *top-level* item — a function **or** a
//! type/container (class, struct, enum, trait, interface, type alias) — out of
//! its current file and into a **brand-new** file that this refactor creates.
//!
//! It is the deterministic sibling of `move_function`: instead of appending to
//! an existing destination, it *always* emits a `CreateFile`, and it refuses if
//! the destination already exists (that case is exactly `move_function`'s job —
//! we do not want two refactors silently overlapping, and clobbering a file
//! would violate §7「绝不瞎猜」). The item's exact source block is deleted at
//! the origin and written verbatim into the new file; call sites are reported
//! but not rewritten, so the plan is honestly Medium.
//!
//! The optional `add_import` / `rewrite_callers` flags reuse move-function's
//! import machinery unchanged (JS/TS derive a precise relative specifier;
//! Python is best-effort and flagged; Go same-dir is a no-op; Go cross-package
//! and Rust degrade with an honest warning). `clean_dest_import` has no meaning
//! here — the destination is new and empty — so it is intentionally absent.

use shuttle_nav::{Confidence, QueryOptions};
use shuttle_protocol::EditOp;

use crate::inline_method::find_callee_def;
use crate::locals::{Lang, LocalsIndex};
use crate::move_function::{relative_module_specifier, synthesize_import};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use std::path::Path;
use tree_sitter::Node;

/// Extract the uniquely-defined top-level item `name` (a function or a
/// type/container) from `src_path` into the **new** file `dest_path`.
///
/// `repo_root` (when non-empty) enables an `shuttle callers` blast-radius report.
/// `add_import` prepends a best-effort origin-side import (see module docs and
/// §7); `rewrite_callers` re-points unambiguous JS/TS caller imports at the new
/// file. Fails if `dest_path` already exists (use `move_function` for that),
/// if the item is not found / not unique / not top-level, or across languages.
pub fn extract_to_new_file(
    src_path: &str,
    source: &str,
    name: &str,
    dest_path: &str,
    repo_root: &str,
    add_import: bool,
    rewrite_callers: bool,
) -> Result<RefactorPlan, RefactorError> {
    if src_path == dest_path {
        return Err(RefactorError::Rejected(
            "source and destination are the same file — nothing to extract".into(),
        ));
    }
    let lang =
        Lang::from_path(src_path).ok_or_else(|| RefactorError::UnsupportedLang(src_path.into()))?;
    let dest_lang = Lang::from_path(dest_path)
        .ok_or_else(|| RefactorError::UnsupportedLang(dest_path.into()))?;
    if lang != dest_lang {
        return Err(RefactorError::Rejected(format!(
            "cannot extract across languages ({src_path} → {dest_path})"
        )));
    }
    // The defining trait of E4: the destination must NOT already exist. An
    // existing file is move-function's territory; creating over it would
    // clobber, and guessing how to merge would violate §7.
    if Path::new(dest_path).exists() {
        return Err(RefactorError::Rejected(format!(
            "`{dest_path}` already exists — use `move-function` to append into an existing file; \
             extract-to-new-file only creates fresh files"
        )));
    }

    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    // 1. Resolve the unique top-level definition — function first, then a
    // type/container. Both must be direct children of the file root; a nested
    // item carries scope we cannot safely relocate (§7).
    let item = resolve_top_level_item(root, lang, name, source)?;
    let parent_is_root = item.parent().map(|p| p.id() == root.id()).unwrap_or(false);
    if !parent_is_root {
        return Err(RefactorError::Rejected(format!(
            "`{name}` is not a top-level item — nested/method extraction is out of scope"
        )));
    }

    // 2. The exact definition block (top-level → column 0, verbatim).
    let block = item
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();

    let mut warnings: Vec<String> = vec![
        format!(
            "the extracted item may rely on imports/definitions in `{src_path}` that are absent \
             in the new `{dest_path}` — add them manually"
        ),
        format!(
            "callers of `{name}` are not re-imported automatically — update their import of the \
             new location by hand"
        ),
    ];
    let mut ops: Vec<EditOp> = Vec::new();
    let mut sites: Vec<String> = Vec::new();

    // 3. Delete the definition at the origin (block text is unique).
    ops.push(search_replace(
        src_path,
        block.clone(),
        String::new(),
        item.start_position().row + 1,
    ));
    sites.push(format!(
        "{}:{} (remove `{name}`)",
        src_path,
        item.start_position().row + 1
    ));

    // 4. Create the new destination file holding exactly the extracted block.
    let content = if block.ends_with('\n') {
        block.clone()
    } else {
        format!("{block}\n")
    };
    ops.push(EditOp::CreateFile {
        path: dest_path.to_string(),
        content,
    });
    sites.push(format!("{dest_path} (create + add `{name}`)"));

    // 5. Cross-file caller blast radius (best-effort, count-only).
    if !repo_root.is_empty() {
        let opts = QueryOptions::new(repo_root);
        if let Ok(hits) = shuttle_nav::callers(name, &opts) {
            let others: std::collections::BTreeSet<String> = hits
                .into_iter()
                .filter(|h| h.path != src_path && h.path != dest_path)
                .map(|h| h.path)
                .collect();
            if !others.is_empty() {
                warnings.push(format!(
                    "{} other file(s) reference `{name}` and will need their import updated: {}",
                    others.len(),
                    others.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    // 6. Optional origin-side re-import (reuses move-function's synthesis).
    if add_import {
        match synthesize_import(lang, src_path, dest_path, name) {
            Ok(Some((import_line, note))) => match source.lines().next() {
                Some(first) if !first.trim().is_empty() => {
                    ops.insert(
                        0,
                        EditOp::Insert {
                            path: src_path.to_string(),
                            anchor: first.to_string(),
                            position: shuttle_protocol::InsertPosition::Before,
                            content: import_line,
                        },
                    );
                    sites.push(format!("{src_path}:1 (re-import `{name}`)"));
                    warnings.push(note);
                }
                _ => warnings.push(format!(
                    "--add-import requested but `{src_path}` has no non-empty first line to anchor \
                     the import on"
                )),
            },
            Ok(None) => warnings.push(format!(
                "--add-import: no import needed — `{name}` stays in the same package as `{src_path}`"
            )),
            Err(reason) => warnings.push(format!("--add-import skipped: {reason} (§7 degrade)")),
        }
    }

    // 7. Optional cross-file caller import rewrite (JS/TS sole-named-import
    // only; everything else degrades with an honest warning — reuses move-
    // function's specifier derivation).
    if rewrite_callers {
        if repo_root.is_empty() {
            warnings.push("--rewrite-callers needs --root to locate call sites; skipped".into());
        } else if !matches!(lang, Lang::JavaScript | Lang::TypeScript) {
            warnings.push(format!(
                "--rewrite-callers only rewrites JS/TS imports; {lang:?} callers left untouched \
                 (§7 degrade)"
            ));
        } else if let Ok(new_spec) = relative_module_specifier(src_path, dest_path) {
            let opts = QueryOptions::new(repo_root);
            let src_stem = Path::new(src_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let caller_files: std::collections::BTreeSet<String> =
                shuttle_nav::callers(name, &opts)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|h| h.path != src_path && h.path != dest_path)
                    .map(|h| h.path)
                    .collect();
            for cf in caller_files {
                match std::fs::read_to_string(&cf) {
                    Ok(content) => {
                        match crate::move_function::rewrite_caller_import_pub(
                            &content, name, src_stem, &new_spec,
                        ) {
                            Some((old_line, new_line, row)) => {
                                ops.push(search_replace(&cf, old_line, new_line, row + 1));
                                sites.push(format!(
                                    "{cf}:{} (re-point import of `{name}`)",
                                    row + 1
                                ));
                            }
                            None => warnings.push(format!(
                                "caller `{cf}` imports `{name}` in a form we cannot safely rewrite \
                                 (co-import / namespace / not found) — fix it by hand (§7)"
                            )),
                        }
                    }
                    Err(_) => {
                        warnings.push(format!("could not read caller `{cf}` to rewrite import"))
                    }
                }
            }
        } else {
            warnings.push(
                "--rewrite-callers: could not derive a relative specifier for the destination; \
                 callers left untouched (§7 degrade)"
                    .into(),
            );
        }
    }

    // Always cross-file → Medium (§7): the item is relocated exactly, but
    // imports/callers remain the reviewer's responsibility.
    let intent = format!("extract-to-new-file: {name} ({src_path} → new {dest_path})");
    let plan = plan_from_ops(RefactorKind::ExtractToNewFile, src_path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::ExtractToNewFile,
        plan,
        signature: Some(format!("{name} @ new {dest_path}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::Medium,
        warnings,
        sites,
    })
}

/// Resolve the unique top-level function or type/container named `name`.
/// Functions are matched first (reusing `find_callee_def`); if none is found we
/// fall back to a type/container search. A name defined more than once is
/// rejected as ambiguous (§7).
fn resolve_top_level_item<'a>(
    root: Node<'a>,
    lang: Lang,
    name: &str,
    source: &str,
) -> Result<Node<'a>, RefactorError> {
    // Try the function resolver first; a "not defined" error just means we
    // should look for a type instead, so we swallow it and fall through.
    if let Ok(f) = find_callee_def(root, lang, name, source) {
        return Ok(f);
    }
    let kinds = lang.container_kinds();
    let mut hits: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) && container_name(n, source).as_deref() == Some(name) {
            hits.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    match hits.len() {
        0 => Err(RefactorError::Rejected(format!(
            "`{name}` is not defined as a top-level function or type in this file"
        ))),
        1 => Ok(hits[0]),
        _ => Err(RefactorError::Rejected(format!(
            "`{name}` is defined {} times in this file — extraction is ambiguous",
            hits.len()
        ))),
    }
}

/// Best-effort name of a container node. Most grammars expose a `name` field;
/// Go's `type_declaration` wraps a `type_spec` whose name we descend to.
fn container_name(node: Node, source: &str) -> Option<String> {
    if let Some(id) = node.child_by_field_name("name") {
        return id.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
    }
    // Go: type_declaration → type_spec { name, type }.
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "type_spec" {
            if let Some(id) = ch.child_by_field_name("name") {
                return id.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::EditOp;

    fn tmp_dest(name: &str) -> String {
        // A path guaranteed not to exist so the "dest must be new" gate passes.
        let base = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".into());
        format!("{base}/__ea_e4_nonexistent__/{name}")
    }

    #[test]
    fn extracts_top_level_function_to_new_file() {
        let src = "src/lib.rs";
        let source = "fn keep() {}\n\nfn moved(x: i32) -> i32 {\n    x + 1\n}\n";
        let dest = tmp_dest("moved.rs");
        let plan = extract_to_new_file(src, source, "moved", &dest, "", false, false).unwrap();
        assert_eq!(plan.op, RefactorKind::ExtractToNewFile);
        assert!(matches!(plan.confidence, Confidence::Medium));
        // Exactly one CreateFile carrying the moved block verbatim.
        let creates: Vec<_> = plan
            .plan
            .ops
            .iter()
            .filter_map(|o| match o {
                EditOp::CreateFile { path, content } => Some((path.clone(), content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].0, dest);
        assert!(creates[0].1.contains("fn moved(x: i32) -> i32"));
        assert!(creates[0].1.ends_with('\n'));
        // And a same-file deletion of the origin block.
        assert!(plan.plan.ops.iter().any(|o| matches!(
            o,
            EditOp::SearchReplace { path, replace, .. } if path == src && replace.is_empty()
        )));
    }

    #[test]
    fn extracts_top_level_type_to_new_file() {
        let src = "src/lib.rs";
        let source = "struct Kept;\n\nstruct Moved {\n    a: i32,\n}\n";
        let dest = tmp_dest("moved_type.rs");
        let plan = extract_to_new_file(src, source, "Moved", &dest, "", false, false).unwrap();
        let created = plan.plan.ops.iter().find_map(|o| match o {
            EditOp::CreateFile { content, .. } => Some(content.clone()),
            _ => None,
        });
        assert!(created.unwrap().contains("struct Moved {"));
    }

    #[test]
    fn rejects_when_destination_exists() {
        // The source file itself certainly exists on disk during the test run;
        // pointing dest at an existing file must be refused.
        let src = "src/lib.rs";
        let source = "fn moved() {}\n";
        // Point dest at a `.rs` file that certainly exists relative to the
        // crate's working directory during `cargo test` (this very module).
        let dest = "src/extract_to_new_file.rs".to_string();
        let err = extract_to_new_file(src, source, "moved", &dest, "", false, false).unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("already exists")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_same_path() {
        let err =
            extract_to_new_file("a.rs", "fn f() {}\n", "f", "a.rs", "", false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_cross_language() {
        let err = extract_to_new_file(
            "a.rs",
            "fn f() {}\n",
            "f",
            tmp_dest("x.py").as_str(),
            "",
            false,
            false,
        )
        .unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("across languages")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_name() {
        let dest = tmp_dest("nope.rs");
        let err = extract_to_new_file("a.rs", "fn f() {}\n", "ghost", &dest, "", false, false)
            .unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("not defined")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ambiguous_name() {
        // Two functions with the same name (invalid Rust, but the resolver must
        // still refuse rather than guess).
        let dest = tmp_dest("dup.rs");
        let source = "fn dup() {}\nfn dup() {}\n";
        let err = extract_to_new_file("a.rs", source, "dup", &dest, "", false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }
}
