//! move-function (a §4-family cross-file relocation): move a uniquely-defined
//! *top-level* function from one file to another. The definition is deleted at
//! the origin and appended at the destination (created if absent); call sites
//! are left untouched but flagged, because we do **not** synthesise imports —
//! guessing a module path or import syntax would violate §7「绝不瞎猜」.
//!
//! It is inherently a cross-file, second-tier refactor, so the plan is always
//! Medium with honest warnings: the moved body may depend on imports/definitions
//! that live in the origin file, and every caller (same file or cross-file via
//! `shuttle callers`) may need its import updated to the new location. The engine
//! moves the code precisely; wiring the imports is left to the reviewer.
//!
//! An opt-in `add_import` flag (default OFF) relaxes that stance *only* where a
//! correct import can be derived without guessing: JS/TS get a precise relative
//! specifier computed from the two paths; Python gets a best-effort
//! `from <dest-stem> import <func>` (flagged); Go same-directory moves need no
//! import (same package); Go cross-package and Rust cannot be inferred from a
//! bare file path, so we still refuse and degrade with an honest warning (§7).
//!
//! A further opt-in `rewrite_callers` flag (also default OFF) rewrites the
//! *import specifier* at cross-file call sites — but only for JS/TS callers
//! where `func` is the **sole named import** from the origin module, so the
//! rewrite is unambiguous. Co-imports (other symbols share the line), namespace
//! imports, and Python/Go/Rust callers are left untouched with a per-file
//! warning: guessing there would violate §7.
//!
//! A final opt-in `clean_dest_import` flag (also default OFF) handles the one
//! importer `rewrite_callers` deliberately skips: the **destination** itself.
//! If `dest` used to `import { func } from '<origin>'`, that line is stale once
//! `func` is defined locally in `dest` (a self-import / duplicate binding), so
//! we delete it — again only for the unambiguous JS/TS sole-named-import from
//! the origin module. Co-imports, namespace imports, and non-JS/TS forms are
//! left with a warning (§7).

use std::path::Path;

use shuttle_nav::{Confidence, QueryOptions};
use shuttle_protocol::{EditOp, InsertPosition};

use crate::inline_method::find_callee_def;
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// Move the uniquely-defined top-level function `func` from `src_path` to
/// `dest_path`. `repo_root` (when non-empty) enables an `shuttle callers` scan so the
/// plan can report which other files reference `func` and will need their
/// imports updated. When `add_import` is set the plan also prepends an
/// origin-side import so same-file references keep resolving (best-effort,
/// per-language; see the module docs and §7). When `rewrite_callers` is set,
/// cross-file JS/TS callers whose sole named import is `func` get their import
/// specifier rewritten to the destination (other cases degrade with a warning).
/// When `clean_dest_import` is set, a now-stale `import { func } from '<origin>'`
/// in the destination file (a self-import once `func` lives there) is deleted
/// (JS/TS sole-named-import only; other shapes degrade with a warning).
#[allow(clippy::too_many_arguments)]
pub fn move_function(
    src_path: &str,
    source: &str,
    func: &str,
    dest_path: &str,
    repo_root: &str,
    add_import: bool,
    rewrite_callers: bool,
    clean_dest_import: bool,
) -> Result<RefactorPlan, RefactorError> {
    if src_path == dest_path {
        return Err(RefactorError::Rejected(
            "source and destination are the same file — nothing to move".into(),
        ));
    }
    let lang =
        Lang::from_path(src_path).ok_or_else(|| RefactorError::UnsupportedLang(src_path.into()))?;
    let dest_lang = Lang::from_path(dest_path)
        .ok_or_else(|| RefactorError::UnsupportedLang(dest_path.into()))?;
    if lang != dest_lang {
        return Err(RefactorError::Rejected(format!(
            "cannot move across languages ({src_path} → {dest_path})"
        )));
    }

    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    // 1. Resolve the unique same-file definition and require it to be top-level:
    // a function nested in a class/other function carries scope we cannot safely
    // relocate, so reject rather than guess (§7).
    let func_node = find_callee_def(root, lang, func, source)?;
    let parent_is_root = func_node
        .parent()
        .map(|p| p.id() == root.id())
        .unwrap_or(false);
    if !parent_is_root {
        return Err(RefactorError::Rejected(format!(
            "`{func}` is not a top-level function — nested/method moves are out of scope"
        )));
    }

    // 2. The exact definition block (top-level → column 0, no re-indentation).
    let block = func_node
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();

    let mut warnings: Vec<String> = vec![
        format!(
            "the moved body may rely on imports/definitions in `{src_path}` that are absent in \
             `{dest_path}` — add them manually"
        ),
        format!(
            "callers of `{func}` are not re-imported automatically — update their import of the \
             new location by hand"
        ),
    ];

    let mut ops: Vec<EditOp> = Vec::new();
    let mut sites: Vec<String> = Vec::new();

    // 3. Delete the definition at the origin (block text is unique — the callee
    // is uniquely defined). Replace with empty; a residual blank line is benign.
    ops.push(search_replace(
        src_path,
        block.clone(),
        String::new(),
        func_node.start_position().row + 1,
    ));
    sites.push(format!(
        "{}:{} (remove `{func}`)",
        src_path,
        func_node.start_position().row + 1
    ));

    // 4. Append at the destination. A missing file is created outright (fast
    // path, bypasses the diff engine); an existing file gets the block inserted
    // after its last non-empty line.
    match std::fs::read_to_string(dest_path) {
        Err(_) => {
            let content = if block.ends_with('\n') {
                block.clone()
            } else {
                format!("{block}\n")
            };
            ops.push(EditOp::CreateFile {
                path: dest_path.to_string(),
                content,
            });
            sites.push(format!("{dest_path} (create + add `{func}`)"));
            warnings.push(format!("`{dest_path}` did not exist and will be created"));
        }
        Ok(dest_src) => {
            let anchor = dest_src
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .ok_or_else(|| {
                    RefactorError::Rejected(format!(
                        "`{dest_path}` is empty — nothing to anchor on"
                    ))
                })?;
            ops.push(EditOp::Insert {
                path: dest_path.to_string(),
                anchor,
                position: InsertPosition::After,
                content: format!("\n{}", block.trim_end()),
            });
            sites.push(format!("{dest_path} (append `{func}`)"));
        }
    }

    // 5. Report cross-file callers (best-effort). We do not rewrite them — only
    // count them so the reviewer knows the blast radius.
    if !repo_root.is_empty() {
        let opts = QueryOptions::new(repo_root);
        if let Ok(hits) = shuttle_nav::callers(func, &opts) {
            let mut others: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for h in hits {
                if h.path != src_path && h.path != dest_path {
                    others.insert(h.path);
                }
            }
            if !others.is_empty() {
                warnings.push(format!(
                    "{} other file(s) call `{func}` and will need their import updated: {}",
                    others.len(),
                    others.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    // 6. Optional origin-side import synthesis (opt-in). Relaxes the default
    // §7 stance ONLY where a correct import is derivable: on success we prepend
    // an import at the top of the origin file so its remaining references to
    // `func` (and same-module callers) keep resolving; on an unguessable layout
    // we do NOT synthesise and record why (honest degrade).
    if add_import {
        match synthesize_import(lang, src_path, dest_path, func) {
            Ok(Some((import_line, note))) => match source.lines().next() {
                Some(first) if !first.trim().is_empty() => {
                    ops.insert(
                        0,
                        EditOp::Insert {
                            path: src_path.to_string(),
                            anchor: first.to_string(),
                            position: InsertPosition::Before,
                            content: import_line,
                        },
                    );
                    sites.push(format!("{src_path}:1 (re-import `{func}`)"));
                    warnings.push(note);
                }
                _ => warnings.push(format!(
                    "--add-import requested but `{src_path}` has no non-empty first line to \
                     anchor the import on"
                )),
            },
            Ok(None) => warnings.push(format!(
                "--add-import: no import needed — `{func}` stays in the same package as `{src_path}`"
            )),
            Err(reason) => {
                warnings.push(format!("--add-import skipped: {reason} (§7 degrade)"))
            }
        }
    }

    // 7. Optional cross-file caller import rewrite (opt-in, needs repo_root).
    // We rewrite ONLY the unambiguous case: a JS/TS caller whose sole named
    // import is `func`, imported from the origin module. There the whole import
    // line can be safely re-pointed at the destination. Everything else
    // (co-imports, namespace/default imports, non-JS/TS) is left untouched with
    // a per-file warning — rewriting them would require guessing (§7).
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
                shuttle_nav::callers(func, &opts)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|h| h.path != src_path && h.path != dest_path)
                    .map(|h| h.path)
                    .collect();
            for cf in caller_files {
                match std::fs::read_to_string(&cf) {
                    Ok(content) => match rewrite_caller_import(&content, func, src_stem, &new_spec)
                    {
                        Some((old_line, new_line, row)) => {
                            ops.push(search_replace(&cf, old_line, new_line, row + 1));
                            sites.push(format!("{cf}:{} (re-point import of `{func}`)", row + 1));
                        }
                        None => warnings.push(format!(
                            "caller `{cf}` imports `{func}` in a form we cannot safely rewrite \
                             (co-import / namespace / not found) — fix it by hand (§7)"
                        )),
                    },
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

    // 8. Optional destination self-import cleanup (opt-in). Once `func` is
    // defined in `dest`, a pre-existing `import { func } from '<origin>'` there
    // is a stale self-import (a duplicate binding). Delete it — again only for
    // the unambiguous JS/TS sole-named-import from the origin module.
    if clean_dest_import {
        if !matches!(lang, Lang::JavaScript | Lang::TypeScript) {
            warnings.push(format!(
                "--clean-dest-import only cleans JS/TS imports; {lang:?} destination left \
                 untouched (§7 degrade)"
            ));
        } else {
            let src_stem = Path::new(src_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            // A missing destination (CreateFile branch) has nothing to clean.
            if let Ok(dest_src) = std::fs::read_to_string(dest_path) {
                match find_dest_self_import(&dest_src, func, src_stem) {
                    Some((line, row)) => {
                        ops.push(search_replace(dest_path, line, String::new(), row + 1));
                        sites.push(format!(
                            "{dest_path}:{} (drop stale self-import of `{func}`)",
                            row + 1
                        ));
                    }
                    None => warnings.push(format!(
                        "--clean-dest-import: no unambiguous stale `import {{ {func} }}` from the \
                         origin found in `{dest_path}` (co-import / namespace / absent) — check by \
                         hand (§7)"
                    )),
                }
            }
        }
    }

    // move-function is always cross-file → Medium (§7): the code is relocated
    // exactly, but imports/callers are the reviewer's responsibility.
    let intent = format!("move-function: {func} ({src_path} → {dest_path})");
    let plan = plan_from_ops(RefactorKind::MoveFunction, src_path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::MoveFunction,
        plan,
        signature: Some(format!("{func} @ {dest_path}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::Medium,
        warnings,
        sites,
    })
}

/// Best-effort origin-side import so same-file references to a moved function
/// keep resolving. `Ok(Some((line, note)))` = derived an import to prepend;
/// `Ok(None)` = none needed (Go same package); `Err(reason)` = the layout makes
/// the import path unguessable, so we degrade honestly rather than fabricate (§7).
pub(crate) fn synthesize_import(
    lang: Lang,
    src_path: &str,
    dest_path: &str,
    func: &str,
) -> Result<Option<(String, String)>, String> {
    match lang {
        Lang::JavaScript | Lang::TypeScript => {
            let spec = relative_module_specifier(src_path, dest_path)?;
            Ok(Some((
                format!("import {{ {func} }} from '{spec}';"),
                format!("added precise re-import `from '{spec}'` (JS/TS relative path)"),
            )))
        }
        Lang::Python => {
            let stem = Path::new(dest_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("cannot derive a module name from `{dest_path}`"))?;
            Ok(Some((
                format!("from {stem} import {func}"),
                format!(
                    "added best-effort `from {stem} import {func}` — verify it matches your \
                     package layout (Python import paths are not derivable from a bare file path)"
                ),
            )))
        }
        Lang::Go => {
            // Same directory → same package → the symbol is still in scope, no
            // import needed. Different directory → different package, whose
            // import path depends on the module root/go.mod → cannot infer.
            if Path::new(src_path).parent() == Path::new(dest_path).parent() {
                Ok(None)
            } else {
                Err(
                    "Go cross-package move needs the destination's import path (module \
                     root/go.mod), which cannot be inferred from a file path"
                        .into(),
                )
            }
        }
        Lang::Rust => Err(
            "Rust module paths (`use crate::…`) cannot be inferred from a file \
                           path — wire up the `use`/`mod` by hand"
                .into(),
        ),
    }
}

/// Compute a JS/TS relative import specifier pointing from `src_path`'s
/// directory to `dest_path` (extension stripped, `./`-prefixed, `/` separators).
pub(crate) fn relative_module_specifier(src_path: &str, dest_path: &str) -> Result<String, String> {
    use std::path::Component;

    fn normal_components(p: &Path) -> Vec<String> {
        p.components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str().map(|s| s.to_string()),
                _ => None,
            })
            .collect()
    }

    let src_dir = Path::new(src_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let dest_no_ext = Path::new(dest_path).with_extension("");
    let src_comps = normal_components(src_dir);
    let dest_comps = normal_components(&dest_no_ext);
    if dest_comps.is_empty() {
        return Err(format!("cannot derive a module path from `{dest_path}`"));
    }

    let max = src_comps.len().min(dest_comps.len());
    let mut common = 0;
    while common < max && src_comps[common] == dest_comps[common] {
        common += 1;
    }

    let mut parts: Vec<String> = vec!["..".to_string(); src_comps.len() - common];
    parts.extend_from_slice(&dest_comps[common..]);
    let mut spec = parts.join("/");
    if !spec.starts_with('.') {
        spec = format!("./{spec}");
    }
    Ok(spec)
}

/// Rewrite a JS/TS caller's import line so `func` is imported from `new_spec`
/// instead of the origin module. We only touch the **unambiguous** shape — a
/// single-name named import `import { func } from '<origin>'` (optionally with
/// `type`, a trailing semicolon, or double quotes) whose specifier's file stem
/// matches the origin (`src_stem`). Co-imports, namespace/default imports, or a
/// specifier that does not resolve to the origin return `None` (caller keeps
/// the honest "fix by hand" warning). Returns `(old_line, new_line, row)`.
/// `pub(crate)` shim so sibling refactors (e.g. `extract_to_new_file`) can reuse
/// the exact same JS/TS caller-import rewrite logic.
pub(crate) fn rewrite_caller_import_pub(
    content: &str,
    func: &str,
    src_stem: &str,
    new_spec: &str,
) -> Option<(String, String, usize)> {
    rewrite_caller_import(content, func, src_stem, new_spec)
}

fn rewrite_caller_import(
    content: &str,
    func: &str,
    src_stem: &str,
    new_spec: &str,
) -> Option<(String, String, usize)> {
    for (row, line) in content.lines().enumerate() {
        let t = line.trim();
        if !t.starts_with("import") || !t.contains(" from ") {
            continue;
        }
        // Named-import brace group: `{ ... }`.
        let (lb, rb) = (t.find('{')?, t.find('}')?);
        if lb > rb {
            continue;
        }
        let names: Vec<&str> = t[lb + 1..rb]
            .split(',')
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .collect();
        // Sole named import must be exactly `func` (allow `type func`).
        if names.len() != 1 {
            continue;
        }
        let sole = names[0].strip_prefix("type ").unwrap_or(names[0]).trim();
        if sole != func {
            continue;
        }
        // Extract the specifier between the quotes after `from`.
        let after = &t[rb..];
        let q0 = after.find(['\'', '"'])?;
        let quote = after.as_bytes()[q0] as char;
        let rest = &after[q0 + 1..];
        let q1 = rest.find(quote)?;
        let spec = &rest[..q1];
        // Only rewrite when the current specifier points at the origin module.
        let spec_stem = Path::new(spec)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if spec_stem != src_stem || src_stem.is_empty() {
            continue;
        }
        let new_line = line.replacen(spec, new_spec, 1);
        if new_line == line {
            return None;
        }
        return Some((line.to_string(), new_line, row));
    }
    None
}

/// Find a destination-side stale self-import of `func`: an `import { func } from
/// '<origin>'` line (JS/TS) whose specifier's file stem matches the origin
/// (`src_stem`). Returns `(line, row)` to delete. Only the unambiguous
/// sole-named-import shape qualifies; co-imports, namespace/default imports, or
/// an import from a different module return `None` (§7). Mirrors the acceptance
/// shape of [`rewrite_caller_import`].
fn find_dest_self_import(dest_src: &str, func: &str, src_stem: &str) -> Option<(String, usize)> {
    if src_stem.is_empty() {
        return None;
    }
    for (row, line) in dest_src.lines().enumerate() {
        let t = line.trim();
        if !t.starts_with("import") || !t.contains(" from ") {
            continue;
        }
        let (lb, rb) = (t.find('{')?, t.find('}')?);
        if lb > rb {
            continue;
        }
        let names: Vec<&str> = t[lb + 1..rb]
            .split(',')
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .collect();
        if names.len() != 1 {
            continue;
        }
        let sole = names[0].strip_prefix("type ").unwrap_or(names[0]).trim();
        if sole != func {
            continue;
        }
        let after = &t[rb..];
        let q0 = after.find(['\'', '"'])?;
        let quote = after.as_bytes()[q0] as char;
        let rest = &after[q0 + 1..];
        let q1 = rest.find(quote)?;
        let spec = &rest[..q1];
        let spec_stem = Path::new(spec)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if spec_stem != src_stem {
            continue;
        }
        return Some((line.to_string(), row));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops_of(p: &RefactorPlan) -> &[EditOp] {
        &p.plan.ops
    }

    #[test]
    fn moves_top_level_fn_to_new_file() {
        let src =
            "import os\n\ndef helper(a):\n    return a + 1\n\ndef main():\n    return helper(2)\n";
        // dest path guaranteed absent → CreateFile branch.
        let dest = "/nonexistent_shuttle_move_dest_helpers.py";
        let p = move_function("m.py", src, "helper", dest, "", false, false, false).unwrap();
        assert_eq!(p.confidence, Confidence::Medium);
        let ops = ops_of(&p);
        // op0: remove at origin.
        match &ops[0] {
            EditOp::SearchReplace {
                search, replace, ..
            } => {
                assert!(search.contains("def helper(a):"));
                assert!(replace.is_empty());
            }
            _ => panic!("op0 should be SearchReplace"),
        }
        // op1: create dest with the block.
        match &ops[1] {
            EditOp::CreateFile { path, content } => {
                assert_eq!(path, dest);
                assert!(content.contains("def helper(a):"));
                assert!(content.contains("return a + 1"));
            }
            _ => panic!("op1 should be CreateFile"),
        }
        assert!(p.warnings.iter().any(|w| w.contains("did not exist")));
    }

    #[test]
    fn rejects_same_file() {
        let src = "def f():\n    return 1\n";
        let err = move_function("m.py", src, "f", "m.py", "", false, false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_cross_language() {
        let src = "def f():\n    return 1\n";
        let err = move_function("m.py", src, "f", "other.rs", "", false, false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_nested_function() {
        // `inner` is nested inside `outer` → not top-level.
        let src = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        let err = move_function("m.py", src, "inner", "d.py", "", false, false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_missing_function() {
        let src = "def f():\n    return 1\n";
        let err = move_function("m.py", src, "ghost", "d.py", "", false, false, false).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn appends_to_existing_dest_file() {
        // Write a real dest file so the Insert branch is exercised.
        let dir = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".to_string());
        let dest = format!("{dir}/shuttle_move_existing_dest_{}.rs", std::process::id());
        std::fs::write(&dest, "// helpers\npub fn existing() -> i32 { 0 }\n").unwrap();
        let src = "fn moveme() -> i32 {\n    42\n}\nfn main() {\n    let _ = moveme();\n}\n";
        let p = move_function("m.rs", src, "moveme", &dest, "", false, false, false).unwrap();
        let ops = ops_of(&p);
        match ops.iter().find(|o| matches!(o, EditOp::Insert { .. })) {
            Some(EditOp::Insert {
                path,
                content,
                position,
                ..
            }) => {
                assert_eq!(path, &dest);
                assert!(matches!(position, InsertPosition::After));
                assert!(content.contains("fn moveme() -> i32 {"));
            }
            _ => panic!("expected an Insert op into the existing dest"),
        }
        let _ = std::fs::remove_file(&dest);
    }
    #[test]
    fn add_import_js_synthesises_precise_relative_specifier() {
        // src: src/app.js → dest: src/util/helpers.js  ⇒  ./util/helpers
        let src = "import x from './x.js';\nfunction helper(a) { return a + 1; }\nfunction main() { return helper(2); }\n";
        let dest = "/nonexistent_shuttle_move_dest_helpers.js";
        // Same dir case is simplest to assert deterministically:
        let p = move_function("app.js", src, "helper", dest, "", true, false, false).unwrap();
        let import = p.plan.ops.iter().find_map(|o| match o {
            EditOp::Insert {
                path,
                content,
                position,
                ..
            } if path == "app.js" && matches!(position, InsertPosition::Before) => {
                Some(content.clone())
            }
            _ => None,
        });
        let line = import.expect("expected an origin-side import insert");
        assert!(line.contains("import { helper } from '"), "got: {line}");
        assert!(
            line.contains("nonexistent_shuttle_move_dest_helpers"),
            "got: {line}"
        );
    }

    #[test]
    fn add_import_js_relative_path_walks_up_directories() {
        // Unit-test the specifier math directly for a nested layout.
        let spec = relative_module_specifier("src/a/app.js", "src/b/util/helpers.js").unwrap();
        assert_eq!(spec, "../b/util/helpers");
        let same = relative_module_specifier("src/app.js", "src/helpers.js").unwrap();
        assert_eq!(same, "./helpers");
    }

    #[test]
    fn add_import_python_is_best_effort_with_warning() {
        let src =
            "import os\ndef helper(a):\n    return a + 1\ndef main():\n    return helper(2)\n";
        // dest stem `helpers` → best-effort `from helpers import helper`.
        let dest = "/nonexistent/helpers.py";
        let p = move_function("m.py", src, "helper", dest, "", true, false, false).unwrap();
        let has_import = p.plan.ops.iter().any(|o| {
            matches!(
                o, EditOp::Insert { path, content, .. }
                if path == "m.py" && content.contains("from helpers import helper")
            )
        });
        assert!(has_import, "expected best-effort python import");
        assert!(p
            .warnings
            .iter()
            .any(|w| w.contains("best-effort") && w.contains("Python")));
    }

    #[test]
    fn add_import_go_same_dir_is_noop() {
        let src =
            "package p\nfunc Helper() int { return 1 }\nfunc Main() int { return Helper() }\n";
        // Both bare filenames → same (empty) parent → same package → no import.
        let p = move_function(
            "main.go",
            src,
            "Helper",
            "helpers.go",
            "",
            true,
            false,
            false,
        );
        // helpers.go does not exist relative to cwd → CreateFile branch; both
        // paths share the (empty) parent → Go no-op.
        let p = p.unwrap();
        assert!(p.warnings.iter().any(|w| w.contains("no import needed")));
        // No origin-side import op was added.
        assert!(!p.plan.ops.iter().any(|o| matches!(
            o, EditOp::Insert { path, .. } if path == "main.go"
        )));
    }

    #[test]
    fn add_import_rust_degrades_honestly() {
        let src = "fn moveme() -> i32 { 42 }\nfn main() { let _ = moveme(); }\n";
        let dest = "/nonexistent_shuttle_move_dest_helpers.rs";
        let p = move_function("m.rs", src, "moveme", dest, "", true, false, false).unwrap();
        assert!(p
            .warnings
            .iter()
            .any(|w| w.contains("skipped") && w.contains("Rust")));
        assert!(!p.plan.ops.iter().any(|o| matches!(
            o, EditOp::Insert { path, .. } if path == "m.rs"
        )));
    }

    #[test]
    fn add_import_go_cross_package_degrades() {
        let spec_err = synthesize_import(Lang::Go, "a/main.go", "b/helpers.go", "Helper");
        assert!(spec_err.is_err());
    }

    #[test]
    fn rewrite_caller_import_repoints_sole_named_import() {
        // `helper` is the ONLY named import from the origin module → rewrite.
        let content = "import { helper } from './util';\nconst x = helper(1);\n";
        let out = rewrite_caller_import(content, "helper", "util", "./util/helpers");
        let (old_line, new_line, row) = out.expect("should rewrite");
        assert_eq!(row, 0);
        assert_eq!(old_line, "import { helper } from './util';");
        assert_eq!(new_line, "import { helper } from './util/helpers';");
    }

    #[test]
    fn rewrite_caller_import_skips_co_imports() {
        // `helper` shares the import line with `other` → ambiguous → None.
        let content = "import { helper, other } from './util';\n";
        assert!(rewrite_caller_import(content, "helper", "util", "./util/helpers").is_none());
    }

    #[test]
    fn rewrite_caller_import_skips_namespace_import() {
        // No named-import braces → cannot safely rewrite.
        let content = "import * as U from './util';\nU.helper(1);\n";
        assert!(rewrite_caller_import(content, "helper", "util", "./util/helpers").is_none());
    }

    #[test]
    fn rewrite_caller_import_skips_wrong_module() {
        // Sole import of `helper`, but from a different module than the origin.
        let content = "import { helper } from './other';\n";
        assert!(rewrite_caller_import(content, "helper", "util", "./util/helpers").is_none());
    }

    #[test]
    fn rewrite_callers_off_leaves_caller_ops_out() {
        // Without --rewrite-callers, no caller SearchReplace ops are emitted
        // and no origin import is added.
        let src = "function helper(a) { return a + 1; }\nfunction main() { return helper(2); }\n";
        let dest = "/nonexistent_shuttle_move_dest_helpers.js";
        let p = move_function("app.js", src, "helper", dest, "", false, false, false).unwrap();
        // Only the origin-delete SearchReplace + dest CreateFile — nothing else.
        assert!(p.plan.ops.iter().all(|o| matches!(
            o,
            EditOp::SearchReplace { path, .. } if path == "app.js"
        ) || matches!(o, EditOp::CreateFile { .. })));
    }

    #[test]
    fn find_dest_self_import_matches_sole_named_import_from_origin() {
        let dest = "import { helper } from './util';\nexport function other() {}\n";
        let (line, row) = find_dest_self_import(dest, "helper", "util").expect("should match");
        assert_eq!(row, 0);
        assert_eq!(line, "import { helper } from './util';");
    }

    #[test]
    fn find_dest_self_import_skips_co_import_and_wrong_module() {
        // Co-import (shares the line) → ambiguous → None.
        let co = "import { helper, other } from './util';\n";
        assert!(find_dest_self_import(co, "helper", "util").is_none());
        // Sole import but from a different module → None.
        let wrong = "import { helper } from './other';\n";
        assert!(find_dest_self_import(wrong, "helper", "util").is_none());
        // Namespace import → no braces → None.
        let ns = "import * as U from './util';\n";
        assert!(find_dest_self_import(ns, "helper", "util").is_none());
    }

    #[test]
    fn clean_dest_import_drops_stale_self_import() {
        // Real dest file that already imports `helper` from the origin module.
        let dir = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".to_string());
        let dest = format!("{dir}/shuttle_move_cleandest_{}.js", std::process::id());
        std::fs::write(
            &dest,
            "import { helper } from './app';\nexport function other() { return helper(1); }\n",
        )
        .unwrap();
        let src = "function helper(a) { return a + 1; }\nfunction main() { return helper(2); }\n";
        let p = move_function("app.js", src, "helper", &dest, "", false, false, true).unwrap();
        // A SearchReplace deleting the stale self-import line was emitted for dest.
        let dropped = p.plan.ops.iter().any(|o| {
            matches!(
                o,
                EditOp::SearchReplace { path, search, replace, .. }
                    if path == &dest && search.contains("import { helper }") && replace.is_empty()
            )
        });
        assert!(dropped, "expected the stale self-import to be dropped");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn clean_dest_import_non_js_degrades_with_warning() {
        // Python destination → the flag only cleans JS/TS → honest warning, no op.
        let dir = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".to_string());
        let dest = format!("{dir}/shuttle_move_cleandest_{}.py", std::process::id());
        std::fs::write(
            &dest,
            "from app import helper\ndef other():\n    return helper(1)\n",
        )
        .unwrap();
        let src = "def helper(a):\n    return a + 1\ndef main():\n    return helper(2)\n";
        let p = move_function("app.py", src, "helper", &dest, "", false, false, true).unwrap();
        assert!(p
            .warnings
            .iter()
            .any(|w| w.contains("clean-dest-import") && w.contains("JS/TS")));
        let _ = std::fs::remove_file(&dest);
    }
}
