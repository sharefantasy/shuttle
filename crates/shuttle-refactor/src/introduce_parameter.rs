//! introduce-parameter (a §4-family sibling of change-signature): promote a
//! constant expression that occurs inside a function body into a fresh trailing
//! parameter. The body occurrences become references to the new parameter, the
//! signature grows one entry, and every call site passes the *original*
//! expression as the extra trailing argument.
//!
//! Scope binding is fixed by `locals`: the parameters and locals of the target
//! function are the names that must **not** appear in the promoted expression.
//! If the expression reads any function-internal binding, callers cannot supply
//! its value from outside — the refactor is rejected rather than guessed (§7
//! 「绝不瞎猜」). It is a second-tier refactor for the same reasons as
//! change-signature: same-file call sites are rewritten structurally, cross-file
//! ones via `shuttle callers` (best-effort → Medium), and any awkward site (arity
//! mismatch, multi-line call/signature) downgrades or is skipped honestly.

use std::collections::{BTreeMap, BTreeSet};

use shuttle_nav::{Confidence, QueryOptions};
use tree_sitter::Node;

use crate::change_signature::{
    collect_calls, node_spans_multiple_lines, point_row, splice_flexible,
};
use crate::extract_method::line_start_offsets;
use crate::inline_method::{
    arg_nodes, collect_method_calls, find_callee_def, is_method_def, named_kids,
    receiver_param_count,
};
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// Promote every occurrence of `expr` inside the same-file function `func` into
/// a new trailing parameter `param_name`, passing the original `expr` at each
/// call site (same file structurally; cross-file via `shuttle callers` when
/// `repo_root` is non-empty).
pub fn introduce_parameter(
    path: &str,
    source: &str,
    func: &str,
    expr: &str,
    param_name: &str,
    repo_root: &str,
) -> Result<RefactorPlan, RefactorError> {
    let expr = expr.trim();
    let param_name = param_name.trim();
    if expr.is_empty() {
        return Err(RefactorError::Rejected(
            "the expression to promote is empty".into(),
        ));
    }
    if param_name.is_empty() || !param_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(RefactorError::Rejected(format!(
            "`{param_name}` is not a valid parameter name"
        )));
    }

    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();
    let line_starts = line_start_offsets(source);

    // 1. Resolve the callee definition (same file, unique) + its parameter list.
    let callee = find_callee_def(root, lang, func, source)?;
    let params_node = callee
        .child_by_field_name("parameters")
        .ok_or_else(|| RefactorError::Rejected(format!("`{func}` has no parameter list")))?;
    let body = callee
        .child_by_field_name("body")
        .ok_or_else(|| RefactorError::Rejected(format!("`{func}` has no body")))?;

    // Method awareness: a receiver (`self`/`&self`, Python `self`/`cls`) is held
    // fixed at the front of the signature and never appears as a positional call
    // argument. Call sites are matched by method name only (receiver type
    // unresolved), so confidence caps at Medium. Go receivers live in a separate
    // `receiver` field, so `receiver_param_count` returns 0 for them.
    let is_method = is_method_def(callee, lang);

    // Reject if the new name collides with an existing parameter.
    let existing_params: Vec<Node> = named_kids(params_node)
        .into_iter()
        .filter(|p| p.kind() != "comment")
        .collect();
    let param_count = existing_params.len();
    let receiver_count = receiver_param_count(callee, lang, source).min(param_count);
    let call_arity = param_count - receiver_count;
    for p in &existing_params {
        let (ps, pe) = (p.start_byte(), p.end_byte());
        if idx
            .definitions
            .iter()
            .any(|d| d.name == param_name && d.start_byte >= ps && d.end_byte <= pe)
        {
            return Err(RefactorError::Rejected(format!(
                "`{param_name}` is already a parameter of `{func}`"
            )));
        }
    }

    // 2. Find the body occurrences: complete named nodes whose source text is
    // exactly `expr`. We do not descend into a matched node (avoids overlapping
    // parent/child matches for parenthesised forms).
    let (bs, be) = (body.start_byte(), body.end_byte());
    let mut occ: Vec<Node> = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        let same = n
            .utf8_text(source.as_bytes())
            .map(|t| t.trim() == expr)
            .unwrap_or(false);
        if n.id() != body.id() && n.is_named() && same {
            occ.push(n);
            continue; // don't descend into a match
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    if occ.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "expression `{expr}` does not occur in the body of `{func}`"
        )));
    }
    occ.sort_by_key(|n| n.start_byte());

    // 3. §7 legality gate: the promoted expression must not read any
    // function-internal binding (a parameter or a local). Collect the names
    // defined anywhere inside the callee, then check whether any reference that
    // sits inside a matched occurrence resolves to one of them.
    let internal: BTreeSet<&str> = idx
        .definitions
        .iter()
        .filter(|d| d.start_byte >= callee.start_byte() && d.end_byte <= callee.end_byte())
        .map(|d| d.name.as_str())
        .collect();
    for o in &occ {
        let (es, ee) = (o.start_byte(), o.end_byte());
        if let Some(r) = idx
            .references
            .iter()
            .find(|r| r.start_byte >= es && r.end_byte <= ee && internal.contains(r.name.as_str()))
        {
            return Err(RefactorError::Rejected(format!(
                "cannot promote `{expr}`: it reads the function-internal name `{}` — \
                 callers cannot supply that value",
                r.name
            )));
        }
    }

    // Every occurrence must sit on a single line (splice convention).
    for o in &occ {
        if node_spans_multiple_lines(*o) {
            return Err(RefactorError::Rejected(
                "a matched occurrence spans multiple lines (single-line only)".into(),
            ));
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    warnings.push(format!(
        "the promoted expression `{expr}` is injected verbatim at each call site — \
         it must be valid in every caller's scope"
    ));

    let mut ops = Vec::new();
    let mut sites = Vec::new();

    // 4. Rewrite the definition's parameter list: append the new parameter.
    let mut decls: Vec<String> = existing_params
        .iter()
        .map(|p| {
            p.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();
    decls.push(param_name.to_string());
    let new_sig = format!("({})", decls.join(", "));
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

    // 5. Replace body occurrences with the parameter name. Multiple occurrences
    // on one line are spliced together (right-to-left) into a single op.
    let mut by_row: BTreeMap<usize, Vec<Node>> = BTreeMap::new();
    for o in &occ {
        by_row.entry(point_row(*o)).or_default().push(*o);
    }
    for (row, nodes) in &by_row {
        let (before, after) = replace_on_line(source, &line_starts, *row, nodes, param_name);
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

    // 6. Same-file call sites (structural, precise): append `expr` as a new
    // trailing argument.
    let mut same_file_calls = if is_method {
        collect_method_calls(root, lang, func, source)
    } else {
        collect_calls(root, lang, func, source)
    };
    same_file_calls.sort_by_key(|c| c.start_byte());
    for call in &same_file_calls {
        // Skip a call that lexically overlaps the definition's own byte range —
        // recursive/self calls inside the body were already handled by step 5's
        // occurrence pass only for the promoted expr, not the call arguments; a
        // recursive call still needs the extra argument, so we DO rewrite it,
        // but must not double-count. Calls are distinct nodes, so this is fine.
        let _ = (bs, be);
        rewrite_call(
            path,
            source,
            &line_starts,
            *call,
            expr,
            call_arity,
            &mut ops,
            &mut sites,
            &mut warnings,
            /*cross_file=*/ false,
        );
    }

    // 7. Cross-file call sites via `shuttle callers` (§4). Best-effort.
    let mut cross_file = false;
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
                for call in &calls {
                    cross_file = true;
                    rewrite_call(
                        &f,
                        &fsrc,
                        &fstarts,
                        *call,
                        expr,
                        call_arity,
                        &mut ops,
                        &mut sites,
                        &mut warnings,
                        true,
                    );
                }
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
            "`{func}` is a method — call sites are matched by method name only (receiver type \
             not resolved); review each `.{func}(...)` site"
        ));
    }

    let confidence = if cross_file || is_method {
        Confidence::Medium
    } else {
        Confidence::High
    };
    let intent = format!("introduce-parameter: {func}({param_name})");
    let plan = plan_from_ops(RefactorKind::IntroduceParameter, path, ops, &intent);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::IntroduceParameter,
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

/// Append `expr` as a new trailing argument to one `func(args)` call.
#[allow(clippy::too_many_arguments)]
fn rewrite_call(
    path: &str,
    source: &str,
    line_starts: &[usize],
    call: Node,
    expr: &str,
    param_count: usize,
    ops: &mut Vec<shuttle_protocol::EditOp>,
    sites: &mut Vec<String>,
    warnings: &mut Vec<String>,
    cross_file: bool,
) {
    let Some(args_node) = call.child_by_field_name("arguments") else {
        return;
    };
    let args = arg_nodes(call);
    let row = point_row(call);
    let where_ = format!("{}:{}", path, row + 1);

    // Arity must match the *original* parameter count; anything else means the
    // call uses keyword/spread/variadic/default args we must not rewrite blind.
    if args.len() != param_count {
        warnings.push(format!(
            "skipped call at {where_}: {} argument(s) but {param_count} parameter(s) — not rewritten",
            args.len()
        ));
        return;
    }

    let mut arg_texts: Vec<String> = args
        .iter()
        .map(|n| {
            n.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();
    arg_texts.push(expr.to_string());
    let new_args = format!("({})", arg_texts.join(", "));

    let (before, after) = splice_flexible(source, line_starts, args_node, &new_args);
    if before == after {
        return;
    }
    let tag = if cross_file { " (cross-file)" } else { "" };
    sites.push(format!(
        "{}:{}{} {} → {}",
        path,
        row + 1,
        tag,
        before.trim(),
        after.trim()
    ));
    ops.push(search_replace(path, before, after, row + 1));
}

/// Replace every `nodes[*]` byte span (all on line `row`, 0-based) with
/// `repl`, right-to-left so earlier offsets stay valid. Returns
/// `(old_line, new_line)`.
fn replace_on_line(
    source: &str,
    line_starts: &[usize],
    row: usize,
    nodes: &[Node],
    repl: &str,
) -> (String, String) {
    let line_start = line_starts[row];
    let line_end = source[line_start..]
        .find('\n')
        .map(|off| line_start + off)
        .unwrap_or(source.len());
    let old_line = source[line_start..line_end].to_string();
    let mut spans: Vec<(usize, usize)> = nodes
        .iter()
        .map(|n| (n.start_byte() - line_start, n.end_byte() - line_start))
        .collect();
    spans.sort_by_key(|s| s.0);
    let mut new_line = old_line.clone();
    for (a, b) in spans.into_iter().rev() {
        if b <= new_line.len() {
            new_line.replace_range(a..b, repl);
        }
    }
    (old_line, new_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str, path: &str, func: &str, expr: &str, name: &str) -> RefactorPlan {
        introduce_parameter(path, src, func, expr, name, "").unwrap()
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
    fn python_promotes_literal_and_updates_calls() {
        let src = "def area(w):\n    return w * 10\n\nx = area(3)\ny = area(5)\n";
        let p = plan(src, "m.py", "area", "10", "factor");
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def area(w):" && r == "def area(w, factor):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "    return w * 10" && r == "    return w * factor"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = area(3)" && r == "x = area(3, 10)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "y = area(5)" && r == "y = area(5, 10)"));
    }

    #[test]
    fn rejects_expression_reading_a_local() {
        // `w * 2` reads the parameter `w`, so callers cannot supply it.
        let src = "def f(w):\n    return w * 2\n";
        let err = introduce_parameter("m.py", src, "f", "w * 2", "p", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_missing_expression() {
        let src = "def f(a):\n    return a + 1\n";
        let err = introduce_parameter("m.py", src, "f", "99", "p", "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn multiple_occurrences_on_one_line_collapse_to_one_op() {
        let src = "def f(a):\n    return 5 + a + 5\n\nz = f(1)\n";
        let p = plan(src, "m.py", "f", "5", "k");
        let e = edits(&p);
        // One op for the body line, replacing both 5s.
        assert!(e
            .iter()
            .any(|(s, r)| s == "    return 5 + a + 5" && r == "    return k + a + k"));
        assert!(e.iter().any(|(s, r)| s == "z = f(1)" && r == "z = f(1, 5)"));
    }

    #[test]
    fn arity_mismatch_call_is_skipped_with_warning() {
        // A call with the wrong argument count is not rewritten.
        let src = "def f(a):\n    return a * 100\n\nf(1)\nf(1, 2)\n";
        let p = plan(src, "m.py", "f", "100", "n");
        assert!(p.warnings.iter().any(|w| w.contains("not rewritten")));
        let e = edits(&p);
        assert!(e.iter().any(|(s, r)| s == "f(1)" && r == "f(1, 100)"));
        assert!(!e.iter().any(|(s, _)| s == "f(1, 2)"));
    }

    #[test]
    fn rust_promotes_literal() {
        let src =
            "fn scale(x: i32) -> i32 {\n    x * 4\n}\n\nfn main() {\n    let _ = scale(2);\n}\n";
        let p = plan(src, "m.rs", "scale", "4", "factor");
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn scale(x: i32)") && r.contains("scale(x: i32, factor)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "x * 4" && r.trim() == "x * factor"));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("scale(2)") && r.contains("scale(2, 4)")));
    }

    #[test]
    fn python_method_promotes_literal_keeping_self() {
        // A class method: `self` is a fixed receiver, so the new parameter is
        // appended after the value params and call sites gain a trailing arg
        // without touching the receiver. Method sites match by name → Medium.
        let src = "class C:\n    def area(self, w):\n        return w * 10\n\nc = C()\nx = c.area(3)\ny = c.area(5)\n";
        let p = plan(src, "m.py", "area", "10", "factor");
        assert_eq!(p.confidence, Confidence::Medium);
        assert!(p.warnings.iter().any(|w| w.contains("method")));
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "def area(self, w):"
                && r.trim() == "def area(self, w, factor):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "        return w * 10" && r == "        return w * factor"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = c.area(3)" && r == "x = c.area(3, 10)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "y = c.area(5)" && r == "y = c.area(5, 10)"));
    }

    #[test]
    fn rust_method_promotes_literal_keeping_self() {
        let src = "struct S;\nimpl S {\n    fn scale(&self, x: i32) -> i32 {\n        x * 4\n    }\n}\n\nfn main() {\n    let s = S;\n    let _ = s.scale(2);\n}\n";
        let p = plan(src, "m.rs", "scale", "4", "factor");
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e.iter().any(|(s, r)| s.contains("fn scale(&self, x: i32)")
            && r.contains("fn scale(&self, x: i32, factor)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.trim() == "x * 4" && r.trim() == "x * factor"));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.scale(2)") && r.contains("s.scale(2, 4)")));
    }
}
