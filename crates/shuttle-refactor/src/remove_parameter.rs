//! remove-parameter: drop a trailing parameter that is *never read* in the
//! function body, deleting the trailing argument at every call site.
//!
//! This is the pure-deletion subset of inline-parameter. Where inline-parameter
//! substitutes an agreed constant into a *used* parameter's body references,
//! remove-parameter refuses to touch anything the body reads: if the parameter
//! is read even once we reject (removing it would leave dangling references).
//! Because nothing is substituted, call sites need not agree on any value — we
//! only require the argument be droppable (matching arity, single-line call).
//! Same-file calls are rewritten structurally; cross-file ones are discovered
//! via `shuttle callers` and cap confidence at Medium.

use std::collections::BTreeSet;

use shuttle_nav::{Confidence, QueryOptions};
use tree_sitter::Node;

use crate::change_signature::{collect_calls, point_row, splice_flexible};
use crate::extract_method::line_start_offsets;
use crate::inline_method::{
    collect_method_calls, find_callee_def, is_method_def, named_kids, receiver_param_count,
};
use crate::inline_parameter::{drop_trailing_arg, param_body_refs, validate_call, CrossCalls};
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// Remove the trailing parameter `param_name` from `func`, provided it is never
/// read in the body, and drop the trailing argument at each call site.
pub fn remove_parameter(
    path: &str,
    source: &str,
    func: &str,
    param_name: &str,
    repo_root: &str,
) -> Result<RefactorPlan, RefactorError> {
    let param_name = param_name.trim();
    if param_name.is_empty() {
        return Err(RefactorError::Rejected("no parameter name given".into()));
    }

    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();
    let line_starts = line_start_offsets(source);

    // 1. Resolve the callee + its parameter list / body.
    let callee = find_callee_def(root, lang, func, source)?;
    let params_node = callee
        .child_by_field_name("parameters")
        .ok_or_else(|| RefactorError::Rejected(format!("`{func}` has no parameter list")))?;
    let body = callee
        .child_by_field_name("body")
        .ok_or_else(|| RefactorError::Rejected(format!("`{func}` has no body")))?;

    // Methods are supported: a leading receiver (Rust `self`, a Python method's
    // `self`/`cls`) stays in the signature and is excluded from call-site arity.
    // Since only a *trailing* parameter may be removed, the receiver is never
    // the target. Method call sites take the `recv.name(...)` form and, because
    // the receiver's static type is not resolved, cap the plan at Medium (§7).
    let is_method = is_method_def(callee, lang);
    let receiver_count = receiver_param_count(callee, lang, source);

    // 2. Locate the target parameter; require it be *trailing* (dropping a
    // middle positional parameter would re-index the remaining arguments).
    let existing_params: Vec<Node> = named_kids(params_node)
        .into_iter()
        .filter(|p| p.kind() != "comment")
        .collect();
    let param_count = existing_params.len();
    if param_count == 0 {
        return Err(RefactorError::Rejected(format!(
            "`{func}` takes no parameters"
        )));
    }
    let mut target_idx: Option<usize> = None;
    for (i, p) in existing_params.iter().enumerate() {
        let (ps, pe) = (p.start_byte(), p.end_byte());
        if idx
            .definitions
            .iter()
            .any(|d| d.name == param_name && d.start_byte >= ps && d.end_byte <= pe)
        {
            target_idx = Some(i);
            break;
        }
    }
    let Some(target_idx) = target_idx else {
        return Err(RefactorError::Rejected(format!(
            "`{param_name}` is not a parameter of `{func}`"
        )));
    };
    if target_idx != param_count - 1 {
        return Err(RefactorError::Rejected(format!(
            "only a trailing parameter can be removed; `{param_name}` is at position {} of {param_count}",
            target_idx + 1
        )));
    }
    // A defaulted parameter (`b=5`) may be omitted by callers — its argument is
    // not uniformly present, so dropping "the trailing argument" is unsound.
    let target_decl = existing_params[target_idx]
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    if target_decl.contains('=') {
        return Err(RefactorError::Rejected(format!(
            "`{param_name}` has a default value — call sites may omit it, so it cannot be removed positionally"
        )));
    }

    // 3. The parameter must be *unused*. If the body reads it, removing it would
    // leave dangling references — that is a rename/inline job, not a removal.
    let body_refs = param_body_refs(&idx, &existing_params[target_idx], param_name, body);
    if !body_refs.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{param_name}` is read {} time(s) in the body of `{func}` — cannot remove a used parameter (try inline-parameter)",
            body_refs.len()
        )));
    }

    // 4. Gather every call site (same file structurally, cross-file via nav).
    // We reuse inline-parameter's `validate_call` to enforce matching arity and
    // single-line calls; the collected argument texts are discarded (we don't
    // require value agreement — the parameter is unused). At call sites the
    // receiver is *not* supplied as a positional argument, so the expected
    // argument count is the value-parameter count.
    let call_arity = param_count - receiver_count.min(param_count);
    let mut discard = Vec::new();
    let mut cross_file = false;

    let mut same_file_calls = if is_method {
        collect_method_calls(root, lang, func, source)
    } else {
        collect_calls(root, lang, func, source)
    };
    same_file_calls.sort_by_key(|c| c.start_byte());
    let mut same_file_call_ops: Vec<(String, usize, Node)> = Vec::new();
    for call in &same_file_calls {
        let arg = validate_call(path, source, *call, call_arity, &mut discard)?;
        if let Some(args_node) = arg {
            same_file_call_ops.push((path.to_string(), point_row(*call), args_node));
        }
    }

    let mut cross_calls: CrossCalls = Vec::new();
    if !repo_root.is_empty() {
        let opts = QueryOptions::new(repo_root);
        if let Ok(hits) = shuttle_nav::callers(func, &opts) {
            let mut other: BTreeSet<String> = BTreeSet::new();
            for h in hits {
                if h.path != path {
                    other.insert(h.path);
                }
            }
            for f in other {
                let Ok(fsrc) = std::fs::read_to_string(&f) else {
                    continue;
                };
                let Some(flang) = Lang::from_path(&f) else {
                    continue;
                };
                let Ok(fidx) = LocalsIndex::build(flang, &fsrc) else {
                    continue;
                };
                let froot = fidx.tree.root_node();
                let fstarts = line_start_offsets(&fsrc);
                let mut calls = if is_method {
                    collect_method_calls(froot, flang, func, &fsrc)
                } else {
                    collect_calls(froot, flang, func, &fsrc)
                };
                calls.sort_by_key(|c| c.start_byte());
                let mut rows: Vec<(usize, usize, usize)> = Vec::new();
                for call in &calls {
                    cross_file = true;
                    let arg = validate_call(&f, &fsrc, *call, call_arity, &mut discard)?;
                    if let Some(args_node) = arg {
                        rows.push((
                            point_row(*call),
                            args_node.start_byte(),
                            args_node.end_byte(),
                        ));
                    }
                }
                if !rows.is_empty() {
                    cross_calls.push((f, fsrc, fstarts, rows));
                }
            }
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    warnings.push(format!(
        "`{param_name}` is never read in the body of `{func}` — removing it and its argument at each call"
    ));

    let mut ops = Vec::new();
    let mut sites = Vec::new();

    // 5. Rewrite the signature: drop the trailing parameter.
    let new_decls: Vec<String> = existing_params
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != target_idx)
        .map(|(_, p)| {
            p.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();
    let new_sig = format!("({})", new_decls.join(", "));
    {
        let row = point_row(params_node);
        let (before, after) = splice_flexible(source, &line_starts, params_node, &new_sig);
        sites.push(format!(
            "{}:{} {} → {}",
            path,
            row + 1,
            before.trim(),
            after.trim()
        ));
        ops.push(search_replace(path, before, after, row + 1));
    }

    // 6. Drop the trailing argument at every same-file call.
    for (cpath, row, args_node) in &same_file_call_ops {
        if let Some((before, after)) =
            drop_trailing_arg(source, &line_starts, *row, *args_node, call_arity)
        {
            sites.push(format!(
                "{}:{} {} → {}",
                cpath,
                row + 1,
                before.trim(),
                after.trim()
            ));
            ops.push(search_replace(cpath, before, after, row + 1));
        }
    }

    // 7. Drop the trailing argument at every cross-file call.
    for (f, fsrc, fstarts, rows) in &cross_calls {
        let Some(flang) = Lang::from_path(f) else {
            continue;
        };
        let Ok(fidx) = LocalsIndex::build(flang, fsrc) else {
            continue;
        };
        let froot = fidx.tree.root_node();
        let mut calls = if is_method {
            collect_method_calls(froot, flang, func, fsrc)
        } else {
            collect_calls(froot, flang, func, fsrc)
        };
        calls.sort_by_key(|c| c.start_byte());
        for (row, abs_s, abs_e) in rows {
            let Some(call) = calls.iter().find(|c| {
                c.child_by_field_name("arguments")
                    .map(|a| a.start_byte() == *abs_s && a.end_byte() == *abs_e)
                    .unwrap_or(false)
            }) else {
                continue;
            };
            let Some(args_node) = call.child_by_field_name("arguments") else {
                continue;
            };
            if let Some((before, after)) =
                drop_trailing_arg(fsrc, fstarts, *row, args_node, call_arity)
            {
                sites.push(format!(
                    "{}:{} (cross-file) {} → {}",
                    f,
                    row + 1,
                    before.trim(),
                    after.trim()
                ));
                ops.push(search_replace(f, before, after, row + 1));
            }
        }
    }

    if cross_file {
        warnings.push(
            "cross-file call sites were rewritten heuristically (shuttle callers) — confidence capped"
                .into(),
        );
    }
    if is_method {
        warnings.push(format!(
            "`{func}` is a method — call sites are matched by method name only \
             (receiver type not resolved); review each `.{func}(...)` site"
        ));
    }

    let confidence = if cross_file || is_method {
        Confidence::Medium
    } else {
        Confidence::High
    };
    let intent = format!("remove-parameter: {func}({param_name})");
    let plan = plan_from_ops(RefactorKind::RemoveParameter, path, ops, &intent);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::RemoveParameter,
            plan,
            signature: Some(format!("{func}{new_sig}")),
            params: Vec::new(),
            returns: Vec::new(),
            confidence,
            warnings,
            sites,
        },
        path,
        source,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str, path: &str, func: &str, param: &str) -> RefactorPlan {
        remove_parameter(path, src, func, param, "").unwrap()
    }
    fn edits(p: &RefactorPlan) -> Vec<(String, String)> {
        p.plan
            .ops
            .iter()
            .map(|o| match o {
                shuttle_protocol::EditOp::SearchReplace {
                    search, replace, ..
                } => (search.clone(), replace.clone()),
                _ => panic!("expected SearchReplace"),
            })
            .collect()
    }

    #[test]
    fn python_removes_unused_trailing_parameter() {
        let src = "def f(a, unused):\n    return a + 1\n\nf(1, 2)\nf(3, 99)\n";
        let p = plan(src, "m.py", "f", "unused");
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a, unused):" && r == "def f(a):"));
        assert!(e.iter().any(|(s, r)| s == "f(1, 2)" && r == "f(1)"));
        assert!(e.iter().any(|(s, r)| s == "f(3, 99)" && r == "f(3)"));
        // Body is never touched.
        assert!(!e.iter().any(|(s, _)| s.contains("return")));
    }

    #[test]
    fn multiline_signature_and_call_removed() {
        // A multi-line signature and a multi-line call are matched as spans and
        // the trailing unused parameter/argument is dropped, collapsing them.
        let src = "def f(\n    a,\n    unused,\n):\n    return a + 1\n\nf(\n    1,\n    2,\n)\n";
        let p = plan(src, "m.py", "f", "unused");
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "(\n    a,\n    unused,\n)" && r == "(a)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "(\n    1,\n    2,\n)" && r == "(1)"));
    }

    #[test]
    fn rejects_used_parameter() {
        let src = "def f(a, k):\n    return a + k\n\nf(1, 2)\n";
        let err = remove_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("read"), "got: {m}");
        }
    }

    #[test]
    fn rejects_non_trailing_parameter() {
        let src = "def f(a, b):\n    return b\n\nf(1, 2)\n";
        let err = remove_parameter("m.py", src, "f", "a", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("trailing"), "got: {m}");
        }
    }

    #[test]
    fn rejects_arity_mismatch() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 2)\nf(1)\n";
        let err = remove_parameter("m.py", src, "f", "unused", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_defaulted_parameter() {
        let src = "def f(a, unused=5):\n    return a\n\nf(1)\n";
        let err = remove_parameter("m.py", src, "f", "unused", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("default"), "got: {m}");
        }
    }

    #[test]
    fn removes_disagreeing_call_values_since_unused() {
        // Unlike inline-parameter, differing argument values are fine: the
        // parameter is unused, so we only delete the argument.
        let src = "def f(a, unused):\n    return a\n\nf(1, 2)\nf(3, 77)\n";
        let p = plan(src, "m.py", "f", "unused");
        let e = edits(&p);
        assert!(e.iter().any(|(s, r)| s == "f(1, 2)" && r == "f(1)"));
        assert!(e.iter().any(|(s, r)| s == "f(3, 77)" && r == "f(3)"));
    }

    #[test]
    fn rust_removes_unused_trailing_parameter() {
        let src = "fn scale(x: i32, unused: i32) -> i32 {\n    x * 2\n}\n\nfn main() {\n    let _ = scale(2, 4);\n    let _ = scale(9, 5);\n}\n";
        let p = plan(src, "m.rs", "scale", "unused");
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn scale(x: i32, unused: i32)")
                && r.contains("fn scale(x: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("scale(2, 4)") && r.contains("scale(2)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("scale(9, 5)") && r.contains("scale(9)")));
    }

    #[test]
    fn rejects_no_such_parameter() {
        let src = "def f(a):\n    return a\n\nf(1)\n";
        let err = remove_parameter("m.py", src, "f", "nope", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn python_method_drops_value_param_keeping_self() {
        // A class method whose trailing value parameter is unused: `self` is a
        // receiver (held fixed), the trailing value param is dropped from the
        // signature, and the `obj.m(...)` call site drops its trailing arg.
        // Method call sites are matched by name only, so confidence caps at Medium.
        let src = "class C:\n    def m(self, a, unused):\n        return a + 1\n\nc = C()\nc.m(1, 2)\nc.m(3, 99)\n";
        let p = plan(src, "m.py", "m", "unused");
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "def m(self, a, unused):" && r.trim() == "def m(self, a):"));
        assert!(e.iter().any(|(s, r)| s == "c.m(1, 2)" && r == "c.m(1)"));
        assert!(e.iter().any(|(s, r)| s == "c.m(3, 99)" && r == "c.m(3)"));
        // Receiver self is never removed; body untouched.
        assert!(!e.iter().any(|(s, _)| s.contains("return")));
    }

    #[test]
    fn rust_method_drops_value_param_keeping_self() {
        let src = "struct S;\nimpl S {\n    fn scale(&self, x: i32, unused: i32) -> i32 {\n        x * 2\n    }\n}\n\nfn main() {\n    let s = S;\n    let _ = s.scale(2, 4);\n}\n";
        let p = plan(src, "m.rs", "scale", "unused");
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn scale(&self, x: i32, unused: i32)")
                && r.contains("fn scale(&self, x: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.scale(2, 4)") && r.contains("s.scale(2)")));
    }
}
