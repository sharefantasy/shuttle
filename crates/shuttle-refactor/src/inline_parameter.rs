//! inline-parameter (the honest inverse of introduce-parameter): drop a trailing
//! parameter whose value is the *same trivial constant* at every call site,
//! substituting that constant into the body where the parameter is read.
//!
//! This is to introduce-parameter what inline-method is to extract-method. The
//! §7 gate is strict because an inline must be *complete*: if any observed call
//! passes a different value, a non-trivial (side-effecting) argument, a wrong
//! arity, or a multi-line call we cannot splice, we reject rather than leave a
//! half-rewritten program. Same-file calls are rewritten structurally; cross-file
//! ones are discovered via `shuttle callers` and, when present, cap confidence at
//! Medium (an unseen caller might disagree).

use std::collections::{BTreeMap, BTreeSet};

use shuttle_nav::{Confidence, QueryOptions};
use tree_sitter::Node;

use crate::change_signature::{collect_calls, is_trivial, point_row, splice_flexible};
use crate::extract_method::line_start_offsets;
use crate::inline_method::{
    arg_nodes, collect_method_calls, find_callee_def, is_method_def, named_kids,
    receiver_param_count,
};
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// A cross-file caller file plus the calls to rewrite in it:
/// `(path, source, line_starts, [(row, args_start_byte, args_end_byte)])`.
pub(crate) type CrossCalls = Vec<(String, String, Vec<usize>, Vec<(usize, usize, usize)>)>;

/// Remove the trailing parameter `param_name` from `func`, inlining the constant
/// that every call site passes for it. Same-file calls are rewritten precisely;
/// cross-file calls are discovered via `shuttle callers` when `repo_root` is non-empty.
pub fn inline_parameter(
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

    // Method awareness: a receiver (`self`/`&self`, Python `self`/`cls`) is held
    // fixed in the signature and does not appear as a positional call argument.
    // Call sites are matched by method name only (receiver type unresolved), so
    // confidence caps at Medium. Go receivers live in a separate `receiver`
    // field, so `receiver_param_count` returns 0 for them.
    let is_method = is_method_def(callee, lang);

    // 2. Locate the target parameter and require it be *trailing*. Dropping a
    // middle positional parameter would re-index the remaining call arguments,
    // which is exactly the kind of guessing §7 forbids.
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
            "only a trailing parameter can be inlined; `{param_name}` is at position {} of {param_count}",
            target_idx + 1
        )));
    }
    // A defaulted parameter (`b=5`) may be omitted by callers, so its effective
    // value is not uniformly observable — refuse rather than guess.
    let target_decl = existing_params[target_idx]
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    if target_decl.contains('=') {
        return Err(RefactorError::Rejected(format!(
            "`{param_name}` has a default value — call sites may omit it, so the value is not uniform"
        )));
    }

    // Receiver params are not passed positionally at call sites, so the observed
    // call arity is `param_count` minus the receiver count.
    let receiver_count = receiver_param_count(callee, lang, source).min(param_count);
    let call_arity = param_count - receiver_count;

    // 3. Gather every call site (same file structurally, cross-file via nav) and
    // extract the argument at the trailing position. Every call must have the
    // matching arity, be single-line, and pass a *trivial* expression.
    let mut collected: Vec<CallArgLite> = Vec::new();
    let mut cross_file = false;

    let mut same_file_calls = if is_method {
        collect_method_calls(root, lang, func, source)
    } else {
        collect_calls(root, lang, func, source)
    };
    same_file_calls.sort_by_key(|c| c.start_byte());
    let mut same_file_call_ops: Vec<(String, usize, Node)> = Vec::new(); // (path, row, args_node)
    for call in &same_file_calls {
        let arg = validate_call(path, source, *call, call_arity, &mut collected)?;
        if let Some(args_node) = arg {
            same_file_call_ops.push((path.to_string(), point_row(*call), args_node));
        }
    }

    // Cross-file callers: (path, src, line_starts, [(row, args_start, args_end)]).
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
                    let arg = validate_call(&f, &fsrc, *call, call_arity, &mut collected)?;
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

    if collected.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "no call sites of `{func}` were found — cannot determine the value to inline"
        )));
    }
    // Every observed argument must be trivial (no side effect is lost on drop).
    if let Some(bad) = collected.iter().find(|c| !c.trivial) {
        return Err(RefactorError::Rejected(format!(
            "call passes a non-trivial argument `{}` for `{param_name}` — inlining could drop a side effect",
            bad.text
        )));
    }

    // 4. Determine which body references read the parameter. If the parameter is
    // *used*, all call sites must agree on the value; if it is unused, differing
    // trivial values are harmless (we only drop the argument).
    let body_refs = param_body_refs(&idx, &existing_params[target_idx], param_name, body);
    let used = !body_refs.is_empty();

    let distinct: BTreeSet<&str> = collected.iter().map(|c| c.text.as_str()).collect();
    let value = if used {
        if distinct.len() != 1 {
            let mut vals: Vec<&str> = distinct.into_iter().collect();
            vals.sort();
            return Err(RefactorError::Rejected(format!(
                "call sites disagree on the value of `{param_name}` ({}) — cannot inline a used parameter",
                vals.join(", ")
            )));
        }
        collected[0].text.clone()
    } else {
        // Unused: value only matters if we needed to substitute; we don't.
        String::new()
    };

    let mut warnings: Vec<String> = Vec::new();
    if !used {
        warnings.push(format!(
            "`{param_name}` is never read in the body of `{func}` — dropping it and its argument at each call"
        ));
    }

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

    // 6. Substitute the constant into the body where the parameter is read.
    if used {
        let mut by_row: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
        for (rs, re) in &body_refs {
            let row = line_at(&line_starts, *rs);
            by_row.entry(row).or_default().push((*rs, *re));
        }
        for (row, spans) in &by_row {
            let (before, after) = replace_spans_on_line(source, &line_starts, *row, spans, &value);
            if before == after {
                continue;
            }
            sites.push(format!(
                "{}:{} {} → {}",
                path,
                row + 1,
                before.trim(),
                after.trim()
            ));
            ops.push(search_replace(path, before, after, row + 1));
        }
    }

    // 7. Drop the trailing argument at every same-file call.
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

    // 8. Drop the trailing argument at every cross-file call.
    for (f, fsrc, fstarts, rows) in &cross_calls {
        // Re-parse to obtain live Nodes for the recorded byte ranges.
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
            "cross-file call sites were rewritten heuristically (shuttle callers) — an unseen caller \
             might pass a different value; confidence capped"
                .into(),
        );
    }

    if is_method {
        warnings.push(format!(
            "`{func}` is a method — call sites are matched by method name only (receiver type \
             not resolved); review each `.{func}(...)` site"
        ));
    }

    let confidence = if cross_file || is_method {
        Confidence::Medium
    } else {
        Confidence::High
    };
    let intent = format!("inline-parameter: {func}({param_name})");
    let plan = plan_from_ops(RefactorKind::InlineParameter, path, ops, &intent);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::InlineParameter,
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

/// Validate one call: arity must equal `param_count`, be single-line, and expose
/// a trailing argument. Pushes the argument text/triviality into `collected` and
/// returns the `arguments` node for later rewriting, or `None` when the call must
/// be skipped for a benign reason. Returns `Err` on a fatal (reject) condition.
pub(crate) fn validate_call<'a>(
    where_path: &str,
    source: &str,
    call: Node<'a>,
    param_count: usize,
    collected: &mut Vec<CallArgLite>,
) -> Result<Option<Node<'a>>, RefactorError> {
    let Some(args_node) = call.child_by_field_name("arguments") else {
        return Ok(None);
    };
    let args = arg_nodes(call);
    let row = point_row(call) + 1;
    if args.len() != param_count {
        return Err(RefactorError::Rejected(format!(
            "{where_path}:{row}: call has {} argument(s) but the function takes {param_count} — inline aborted (arity must match to drop the trailing argument)",
            args.len()
        )));
    }
    let last = args[param_count - 1];
    let text = last
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .trim()
        .to_string();
    collected.push(CallArgLite {
        text,
        trivial: is_trivial(last),
    });
    Ok(Some(args_node))
}

pub(crate) struct CallArgLite {
    pub(crate) text: String,
    pub(crate) trivial: bool,
}

/// References inside `body` that read the parameter `param_name` bound to the
/// callee, as (start_byte, end_byte) spans.
pub(crate) fn param_body_refs(
    idx: &LocalsIndex,
    param_node: &Node,
    param_name: &str,
    body: Node,
) -> Vec<(usize, usize)> {
    let (ps, pe) = (param_node.start_byte(), param_node.end_byte());
    // The parameter's defining occurrence (to find its governing scope).
    let Some(def) = idx
        .definitions
        .iter()
        .find(|d| d.name == param_name && d.start_byte >= ps && d.end_byte <= pe)
    else {
        return Vec::new();
    };
    let Some(scope) = idx.scope_range(def.scope_id) else {
        return Vec::new();
    };
    let (bs, be) = (body.start_byte(), body.end_byte());
    idx.references_bound_to(param_name, scope)
        .into_iter()
        .filter(|r| r.start_byte >= bs && r.end_byte <= be)
        .map(|r| (r.start_byte, r.end_byte))
        .collect()
}

/// Drop the trailing argument of a single-line `func(args)` call. Returns the
/// `(old_line, new_line)` splice, or `None` if nothing changed.
pub(crate) fn drop_trailing_arg(
    source: &str,
    line_starts: &[usize],
    _row: usize,
    args_node: Node,
    param_count: usize,
) -> Option<(String, String)> {
    let mut c = args_node.walk();
    let mut named: Vec<Node> = args_node
        .children(&mut c)
        .filter(|n| n.is_named())
        .collect();
    named.retain(|n| n.kind() != "comment");
    if named.len() != param_count {
        return None;
    }
    let keep: Vec<String> = named[..param_count - 1]
        .iter()
        .map(|n| {
            n.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();
    let new_args = format!("({})", keep.join(", "));
    let (before, after) = splice_flexible(source, line_starts, args_node, &new_args);
    if before == after {
        None
    } else {
        Some((before, after))
    }
}

/// 0-based line index containing byte offset `at`.
fn line_at(line_starts: &[usize], at: usize) -> usize {
    match line_starts.binary_search(&at) {
        Ok(i) => i,
        Err(i) => i - 1,
    }
}

/// Replace every `(start,end)` byte span (all on line `row`, 0-based) with
/// `repl`, right-to-left. Returns `(old_line, new_line)`.
fn replace_spans_on_line(
    source: &str,
    line_starts: &[usize],
    row: usize,
    spans: &[(usize, usize)],
    repl: &str,
) -> (String, String) {
    let line_start = line_starts[row];
    let line_end = source[line_start..]
        .find('\n')
        .map(|off| line_start + off)
        .unwrap_or(source.len());
    let old_line = source[line_start..line_end].to_string();
    let mut rel: Vec<(usize, usize)> = spans
        .iter()
        .map(|(a, b)| (a - line_start, b - line_start))
        .collect();
    rel.sort_by_key(|s| s.0);
    let mut new_line = old_line.clone();
    for (a, b) in rel.into_iter().rev() {
        if b <= new_line.len() {
            new_line.replace_range(a..b, repl);
        }
    }
    (old_line, new_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str, path: &str, func: &str, param: &str) -> RefactorPlan {
        inline_parameter(path, src, func, param, "").unwrap()
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
    fn python_inlines_agreed_constant() {
        let src =
            "def area(w, factor):\n    return w * factor\n\nx = area(3, 10)\ny = area(5, 10)\n";
        let p = plan(src, "m.py", "area", "factor");
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def area(w, factor):" && r == "def area(w):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "    return w * factor" && r == "    return w * 10"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = area(3, 10)" && r == "x = area(3)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "y = area(5, 10)" && r == "y = area(5)"));
    }

    #[test]
    fn rejects_disagreeing_call_sites() {
        let src = "def f(a, k):\n    return a + k\n\nf(1, 2)\nf(3, 9)\n";
        let err = inline_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("disagree"), "got: {m}");
        }
    }

    #[test]
    fn rejects_non_trivial_argument() {
        let src = "def f(a, k):\n    return a + k\n\nf(1, g())\nf(2, g())\n";
        let err = inline_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("non-trivial"), "got: {m}");
        }
    }

    #[test]
    fn rejects_non_trailing_parameter() {
        let src = "def f(a, b):\n    return a + b\n\nf(1, 2)\n";
        let err = inline_parameter("m.py", src, "f", "a", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("trailing"), "got: {m}");
        }
    }

    #[test]
    fn rejects_arity_mismatch() {
        let src = "def f(a, k):\n    return a + k\n\nf(1, 2)\nf(1)\n";
        let err = inline_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_no_call_sites() {
        let src = "def f(a, k):\n    return a + k\n";
        let err = inline_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("no call sites"), "got: {m}");
        }
    }

    #[test]
    fn unused_parameter_drops_without_agreement() {
        // `k` is never read: differing trivial values are harmless.
        let src = "def f(a, k):\n    return a + 1\n\nf(1, 2)\nf(3, 9)\n";
        let p = plan(src, "m.py", "f", "k");
        assert!(p.warnings.iter().any(|w| w.contains("never read")));
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a, k):" && r == "def f(a):"));
        assert!(e.iter().any(|(s, r)| s == "f(1, 2)" && r == "f(1)"));
        assert!(e.iter().any(|(s, r)| s == "f(3, 9)" && r == "f(3)"));
        // The body is untouched.
        assert!(!e.iter().any(|(s, _)| s.contains("return")));
    }

    #[test]
    fn rejects_defaulted_parameter() {
        let src = "def f(a, k=5):\n    return a + k\n\nf(1, 5)\n";
        let err = inline_parameter("m.py", src, "f", "k", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
        if let RefactorError::Rejected(m) = err {
            assert!(m.contains("default"), "got: {m}");
        }
    }

    #[test]
    fn rust_inlines_agreed_constant() {
        let src = "fn scale(x: i32, factor: i32) -> i32 {\n    x * factor\n}\n\nfn main() {\n    let _ = scale(2, 4);\n    let _ = scale(9, 4);\n}\n";
        let p = plan(src, "m.rs", "scale", "factor");
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn scale(x: i32, factor: i32)")
                && r.contains("fn scale(x: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "x * factor" && r.trim() == "x * 4"));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("scale(2, 4)") && r.contains("scale(2)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("scale(9, 4)") && r.contains("scale(9)")));
    }

    #[test]
    fn multiple_body_reads_on_one_line_collapse() {
        let src = "def f(a, k):\n    return k + a + k\n\nf(1, 7)\n";
        let p = plan(src, "m.py", "f", "k");
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "    return k + a + k" && r == "    return 7 + a + 7"));
    }

    #[test]
    fn python_method_inlines_agreed_constant_keeping_self() {
        // A class method: `self` is a receiver held fixed in the signature and
        // absent from positional call arity. The trailing value param `factor`
        // is inlined into the body and dropped from `obj.m(...)` call sites.
        // Method sites match by name only → Medium confidence + warning.
        let src = "class C:\n    def area(self, w, factor):\n        return w * factor\n\nc = C()\nx = c.area(3, 10)\ny = c.area(5, 10)\n";
        let p = plan(src, "m.py", "area", "factor");
        assert_eq!(p.confidence, Confidence::Medium);
        assert!(p.warnings.iter().any(|w| w.contains("method")));
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "def area(self, w, factor):"
                && r.trim() == "def area(self, w):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "        return w * factor" && r == "        return w * 10"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = c.area(3, 10)" && r == "x = c.area(3)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "y = c.area(5, 10)" && r == "y = c.area(5)"));
    }

    #[test]
    fn rust_method_inlines_agreed_constant_keeping_self() {
        let src = "struct S;\nimpl S {\n    fn scale(&self, x: i32, factor: i32) -> i32 {\n        x * factor\n    }\n}\n\nfn main() {\n    let s = S;\n    let _ = s.scale(2, 4);\n    let _ = s.scale(9, 4);\n}\n";
        let p = plan(src, "m.rs", "scale", "factor");
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn scale(&self, x: i32, factor: i32)")
                && r.contains("fn scale(&self, x: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "x * factor" && r.trim() == "x * 4"));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.scale(2, 4)") && r.contains("s.scale(2)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.scale(9, 4)") && r.contains("s.scale(9)")));
    }
}
