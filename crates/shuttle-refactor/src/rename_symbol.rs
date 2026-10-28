//! rename-symbol (cross-file): rename a top-level/global symbol and every
//! reference to it across the repository.
//!
//! Where `rename-local` (§4) is scope-precise and lives entirely inside one
//! function body, rename-symbol operates on module-/repo-level names that
//! `locals` cannot resolve. It leans on `shuttle-nav`'s reference discovery
//! (definition anchors + AST-verified identifier lines per file) and rewrites
//! each occurrence with a whole-word substitution.
//!
//! Honesty (§7): a cross-file textual rename cannot prove that two files' `foo`
//! are the *same* binding (that needs a full resolver / LSP). We therefore cap
//! confidence at **Medium** whenever more than the defining file is touched,
//! surface the touched-file blast radius as warnings, and refuse when the new
//! name already exists as a symbol in the table (likely collision). Same-file
//! only renames stay High.

use std::collections::BTreeMap;

use shuttle_nav::{Confidence, NavKind, QueryOptions};

use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Replace every whole-word occurrence of `name` in `line` with `new_name`.
/// Returns `None` if the line is unchanged (no genuine word boundary hit).
fn replace_word_on_line(line: &str, name: &str, new_name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let hay = line.as_bytes();
    let needle = name.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    let mut last = 0usize;
    let mut changed = false;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_byte(hay[i - 1]);
            let after = i + needle.len();
            let after_ok = after >= hay.len() || !is_ident_byte(hay[after]);
            if before_ok && after_ok {
                out.push_str(&line[last..i]);
                out.push_str(new_name);
                last = after;
                i = after;
                changed = true;
                continue;
            }
        }
        i += 1;
    }
    if !changed {
        return None;
    }
    out.push_str(&line[last..]);
    Some(out)
}

/// Rename the symbol `name` to `new_name` across the repository rooted at
/// `repo_root`, using `shuttle-nav` reference discovery. `origin_path` is the file
/// the user invoked the rename from (used to distinguish same-file vs
/// cross-file for confidence). When `repo_root` is empty, only `origin_path`
/// is scanned.
pub fn rename_symbol(
    origin_path: &str,
    name: &str,
    new_name: &str,
    repo_root: &str,
) -> Result<RefactorPlan, RefactorError> {
    let name = name.trim();
    let new_name = new_name.trim();
    if name.is_empty() || new_name.is_empty() {
        return Err(RefactorError::Rejected(
            "both old and new names are required".into(),
        ));
    }
    if name == new_name {
        return Err(RefactorError::Rejected(
            "old and new names are identical".into(),
        ));
    }
    if !new_name
        .bytes()
        .enumerate()
        .all(|(i, b)| is_ident_byte(b) && !(i == 0 && b.is_ascii_digit()))
    {
        return Err(RefactorError::Rejected(format!(
            "`{new_name}` is not a valid identifier"
        )));
    }

    // Discover occurrences. With a repo root we use full cross-file reference
    // discovery; without one we degrade to the origin file alone.
    let hits = if repo_root.is_empty() {
        // Same-file only: read the origin and use nav's per-file AST verifier
        // indirectly by scanning genuine identifier lines is unavailable here
        // without a table, so treat the whole file textually (word-boundary).
        let src = std::fs::read_to_string(origin_path)
            .map_err(|e| RefactorError::Io(format!("{origin_path}: {e}")))?;
        let mut v = Vec::new();
        for (i, line) in src.lines().enumerate() {
            if replace_word_on_line(line, name, new_name).is_some() {
                v.push((origin_path.to_string(), i + 1));
            }
        }
        v
    } else {
        let opts = QueryOptions::new(repo_root);
        let refs = shuttle_nav::references(name, &opts)
            .map_err(|e| RefactorError::Rejected(format!("reference discovery failed: {e}")))?;
        if refs.is_empty() {
            return Err(RefactorError::Rejected(format!(
                "no definitions or references of `{name}` were found under {repo_root}"
            )));
        }
        // Collision guard: reject if `new_name` already resolves to a definition.
        if let Ok(existing) = shuttle_nav::defs(new_name, &opts) {
            if !existing.is_empty() {
                return Err(RefactorError::Rejected(format!(
                    "`{new_name}` already exists as a symbol ({} definition(s)) — rename would collide",
                    existing.len()
                )));
            }
        }
        refs.into_iter()
            .filter(|h| {
                matches!(
                    h.kind,
                    NavKind::Definition | NavKind::Declaration | NavKind::Reference
                )
            })
            .map(|h| (h.path, h.line))
            .collect()
    };

    if hits.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "no occurrences of `{name}` to rename"
        )));
    }

    // Group (path -> set of lines), dedup, and build one SearchReplace per line.
    let mut by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (p, ln) in hits {
        by_file.entry(p).or_default().push(ln);
    }
    for lines in by_file.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    let mut ops = Vec::new();
    let mut sites = Vec::new();
    let mut cache: BTreeMap<String, String> = BTreeMap::new();
    for (path, lines) in &by_file {
        let src = cache
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(path).unwrap_or_default());
        let file_lines: Vec<&str> = src.lines().collect();
        for &ln in lines {
            let Some(orig) = file_lines.get(ln - 1) else {
                continue;
            };
            let Some(newl) = replace_word_on_line(orig, name, new_name) else {
                continue;
            };
            sites.push(format!("{path}:{ln}"));
            ops.push(search_replace(path, orig.to_string(), newl, ln));
        }
    }

    if ops.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "no lines containing `{name}` as a whole word were found"
        )));
    }

    let touched_files: std::collections::BTreeSet<&String> = by_file.keys().collect();
    let cross_file = touched_files.len() > 1
        || (touched_files.len() == 1 && *touched_files.iter().next().unwrap() != origin_path);

    let mut warnings = Vec::new();
    let confidence = if cross_file {
        warnings.push(format!(
            "cross-file rename touched {} file(s) via textual reference discovery — an \
             unrelated same-named binding may exist; confidence capped at Medium",
            touched_files.len()
        ));
        Confidence::Medium
    } else {
        Confidence::High
    };

    let intent = format!("rename-symbol {name} -> {new_name} ({} site(s))", ops.len());
    // verify_reparse only re-parses same-file ops for `origin_path`; the plan is
    // multi-file, so it protects at least the origin's edits.
    let origin_src = cache
        .get(origin_path)
        .cloned()
        .or_else(|| std::fs::read_to_string(origin_path).ok())
        .unwrap_or_default();
    let plan = plan_from_ops(RefactorKind::RenameSymbol, origin_path, ops, &intent);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::RenameSymbol,
            plan,
            signature: None,
            params: Vec::new(),
            returns: Vec::new(),
            confidence,
            warnings,
            sites,
        },
        origin_path,
        &origin_src,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundary_respected() {
        assert_eq!(
            replace_word_on_line("foo = foobar + foo", "foo", "baz").unwrap(),
            "baz = foobar + baz"
        );
        assert!(replace_word_on_line("foobar", "foo", "baz").is_none());
        assert!(replace_word_on_line("no match here", "foo", "baz").is_none());
    }

    #[test]
    fn multiple_hits_on_one_line() {
        assert_eq!(
            replace_word_on_line("x(foo, foo)", "foo", "q").unwrap(),
            "x(q, q)"
        );
    }

    #[test]
    fn same_file_rename_is_high() {
        // No repo root: falls back to a textual same-file rename of origin.
        let dir = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".into());
        let path = format!("{dir}/rename_symbol_same_file_test.py");
        std::fs::write(
            &path,
            "def widget():\n    return 1\n\nprint(widget())\nprint(widget)\n",
        )
        .unwrap();
        let p = rename_symbol(&path, "widget", "gadget", "").unwrap();
        assert_eq!(p.confidence, Confidence::High);
        // def line + two use lines.
        assert_eq!(p.plan.ops.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_identical_names() {
        let err = rename_symbol("m.py", "foo", "foo", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_invalid_new_name() {
        let err = rename_symbol("m.py", "foo", "9bad", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }
}
