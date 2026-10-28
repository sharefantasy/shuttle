//! change-signature (docs §4「改方法签名（加/删/重排参数）」): transform a
//! function's parameter list and synchronise every call site.
//!
//! `locals` fixes the formal parameters (names, order, declaration text) inside
//! the definition; call sites are found structurally — same-file calls directly
//! from this file's tree, cross-file calls via `shuttle_nav::callers` (§4「调用点用
//! ast-grep/`shuttle callers`」). It is a 二等/second-tier refactor: whenever a call
//! site cannot be linearised safely by structure alone (keyword/spread/variadic
//! arity mismatch, multi-line call, an added parameter whose value we had to
//! guess, or any cross-file edit) the plan is downgraded to Medium with an
//! honest warning rather than emitting a guess (§7「绝不瞎猜」).
//!
//! The transform is expressed as an ordered list of [`NewParam`]s: each entry
//! either *keeps* an original parameter (by its old index — this covers reorder
//! and, by omission, delete) or *adds* a fresh one. An added parameter comes in
//! two flavours: (a) *injected* — a value is written at every call site (Medium,
//! the value is a guess); or (b) *defaulted* — a default is written into the
//! signature itself (Python `b=0`, JS/TS `b = 0`) and existing calls simply omit
//! it, so no value is guessed at any call (stays High). Defaulted adds require a
//! language with call-site-omittable defaults (Python/JS/TS) and must be
//! trailing. Deleting a parameter still referenced in the body is rejected.

use std::collections::BTreeSet;

use shuttle_nav::{Confidence, QueryOptions};
use tree_sitter::Node;

use crate::extract_method::line_start_offsets;
use crate::inline_method::{
    arg_nodes, bare_callee_name, call_kind, collect_method_calls, find_callee_def, is_method_def,
    named_kids, receiver_param_count,
};
use crate::locals::{Lang, LocalsIndex, Occurrence};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// One entry in the target parameter list.
#[derive(Debug, Clone)]
pub struct NewParam {
    /// `Some(i)`: keep original parameter `i` (reuse its declaration text and,
    /// at every call site, its original argument). `None`: a freshly added
    /// parameter — [`decl`](Self::decl) + [`default_arg`](Self::default_arg)
    /// are required.
    pub from: Option<usize>,
    /// Declaration text rendered into the signature for an added parameter
    /// (e.g. `count` or `count: i32`).
    pub decl: Option<String>,
    /// Argument value injected at each call site for an added parameter.
    pub default_arg: Option<String>,
    /// A *defaulted* add: the default lives in the signature (via [`decl`], e.g.
    /// `b=0`) and existing call sites omit the argument entirely rather than
    /// receiving an injected value. Requires a language with call-site-omittable
    /// defaults and must be a trailing parameter.
    pub omit_at_call: bool,
}

impl NewParam {
    /// Keep original parameter `i`, in this position.
    pub fn keep(i: usize) -> Self {
        Self {
            from: Some(i),
            decl: None,
            default_arg: None,
            omit_at_call: false,
        }
    }
    /// Add a new parameter declared as `decl`, injecting `default_arg` at every
    /// call site (Medium — the injected value is a guess).
    pub fn add(decl: impl Into<String>, default_arg: impl Into<String>) -> Self {
        Self {
            from: None,
            decl: Some(decl.into()),
            default_arg: Some(default_arg.into()),
            omit_at_call: false,
        }
    }
    /// Add a *defaulted* trailing parameter: `decl` carries the default in the
    /// signature (e.g. `b=0`), and existing call sites omit the argument. Only
    /// valid for languages with call-site-omittable defaults (Python/JS/TS).
    pub fn add_defaulted(decl: impl Into<String>) -> Self {
        Self {
            from: None,
            decl: Some(decl.into()),
            default_arg: None,
            omit_at_call: true,
        }
    }
}

/// Change the signature of the same-file function `func` to `new_params`, and
/// rewrite its call sites (same file directly, cross-file via `shuttle callers` when
/// `repo_root` is non-empty).
pub fn change_signature(
    path: &str,
    source: &str,
    func: &str,
    new_params: &[NewParam],
    repo_root: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();
    let line_starts = line_start_offsets(source);

    // 1. Resolve the callee definition (same file, unique) and its parameters.
    let callee = find_callee_def(root, lang, func, source)?;
    let params_node = callee
        .child_by_field_name("parameters")
        .ok_or_else(|| RefactorError::Rejected(format!("`{func}` has no parameter list")))?;

    // Methods are supported: any *receiver* parameter (Rust `self`, a Python
    // method's leading `self`/`cls`) is held fixed and never reordered/dropped,
    // and the reorder/add/delete transform indices are taken relative to the
    // *value* parameters that follow it. Call sites take the `recv.name(...)`
    // form. Because we match method calls by method name only (the receiver's
    // static type is not resolved), method plans are capped at Medium (§7).
    let is_method = is_method_def(callee, lang);
    let mut all_params = param_infos(&idx, params_node, source);
    let rc = receiver_param_count(callee, lang, source).min(all_params.len());
    let receiver_texts: Vec<String> = all_params[..rc].iter().map(|p| p.text.clone()).collect();
    let params: Vec<ParamInfo> = all_params.split_off(rc);

    // 2. Validate the transform against the original parameters.
    if new_params.is_empty() {
        return Err(RefactorError::Rejected(
            "target parameter list is empty".into(),
        ));
    }
    let mut kept: Vec<usize> = Vec::new();
    for np in new_params {
        match np.from {
            Some(i) => {
                if i >= params.len() {
                    return Err(RefactorError::Rejected(format!(
                        "kept parameter index {i} is out of range ({} params)",
                        params.len()
                    )));
                }
                if kept.contains(&i) {
                    return Err(RefactorError::Rejected(format!(
                        "parameter `{}` is kept more than once",
                        params[i].name
                    )));
                }
                kept.push(i);
            }
            None => {
                if np.decl.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(RefactorError::Rejected(
                        "an added parameter needs a declaration".into(),
                    ));
                }
                if np.omit_at_call {
                    // A *defaulted* add carries its default in the signature and
                    // is omitted at call sites, so no `default_arg` is needed.
                    if lang == Lang::Go || lang == Lang::Rust {
                        return Err(RefactorError::Rejected(format!(
                            "{lang:?} has no call-site-omittable defaults; a defaulted add \
                             cannot be applied without rewriting every call site"
                        )));
                    }
                } else if np.default_arg.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(RefactorError::Rejected(
                        "an added parameter needs a default argument for call sites".into(),
                    ));
                }
            }
        }
    }

    // A defaulted add must be trailing: once existing calls stop passing an
    // argument, every parameter after it would silently receive the wrong
    // positional value. Reject anything after the first defaulted add.
    if let Some(first_def) = new_params.iter().position(|p| p.omit_at_call) {
        if new_params[first_def + 1..].iter().any(|p| !p.omit_at_call) {
            return Err(RefactorError::Rejected(
                "a defaulted add must be trailing (no non-defaulted parameter may follow it)"
                    .into(),
            ));
        }
    }

    // 3. Deleting a parameter still referenced in the body is unsafe — reject.
    let (bs, be) = (params_node.end_byte(), callee.end_byte());
    for (i, p) in params.iter().enumerate() {
        if kept.contains(&i) {
            continue;
        }
        let used = idx
            .references
            .iter()
            .any(|r| r.name == p.name && r.start_byte >= bs && r.end_byte <= be);
        if used {
            return Err(RefactorError::Rejected(format!(
                "cannot delete `{}`: it is still used in the body of `{func}`",
                p.name
            )));
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    // Only an *injected* add (a value written at every call site) is a guess and
    // caps confidence at Medium. A *defaulted* add lives in the signature and is
    // omitted at existing calls, so it introduces no guess and stays High.
    let injected = new_params
        .iter()
        .any(|p| p.from.is_none() && !p.omit_at_call);
    if injected {
        warnings.push(
            "added parameter(s) inject a guessed default at call sites — review each site".into(),
        );
    }

    // 4. Rewrite the definition's parameter list (receiver held fixed).
    let param_texts: Vec<String> = params.iter().map(|p| p.text.clone()).collect();
    let new_value_sig = rebuild_list(new_params, &param_texts, /*for_call=*/ false)
        .map_err(RefactorError::Rejected)?;
    let new_sig = if receiver_texts.is_empty() {
        new_value_sig
    } else if new_value_sig.is_empty() {
        receiver_texts.join(", ")
    } else {
        format!("{}, {}", receiver_texts.join(", "), new_value_sig)
    };
    let mut ops = Vec::new();
    let mut sites = Vec::new();
    {
        let row = point_row(params_node);
        let (before, after) =
            splice_flexible(source, &line_starts, params_node, &format!("({new_sig})"));
        sites.push(format!(
            "{}:{} {} → {}",
            path,
            row + 1,
            before.trim(),
            after.trim()
        ));
        ops.push(search_replace(path, before, after, row + 1));
    }

    // 5. Same-file call sites (structural, precise). For a method we match
    // `recv.name(...)`; for a free function, bare `name(...)`.
    let mut same_file_calls = if is_method {
        collect_method_calls(root, lang, func, source)
    } else {
        collect_calls(root, lang, func, source)
    };
    same_file_calls.sort_by_key(|c| c.start_byte());
    for call in &same_file_calls {
        rewrite_call(
            path,
            source,
            &line_starts,
            func,
            *call,
            new_params,
            &kept,
            &params,
            &mut ops,
            &mut sites,
            &mut warnings,
            /*cross_file=*/ false,
        );
    }

    // 6. Cross-file call sites, discovered via `shuttle callers` (§4). Best-effort:
    // a missing symbol backend simply limits the plan to the current file.
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
                        func,
                        *call,
                        new_params,
                        &kept,
                        &params,
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
            "`{func}` is a method — call sites are matched by method name only \
             (receiver type not resolved); review each `.{func}(...)` site"
        ));
    }
    let confidence = if warnings.is_empty() {
        Confidence::High
    } else {
        Confidence::Medium
    };
    let intent = format!("change-signature: {func}");
    let plan = plan_from_ops(RefactorKind::ChangeSignature, path, ops, &intent);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::ChangeSignature,
            plan,
            signature: Some(format!("{func}({new_sig})")),
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

// --------------------------------------------------------------------------
// call-site rewriting
// --------------------------------------------------------------------------

/// Rewrite one call `func(args)` per the target parameter transform, appending
/// an op + preview line, or a warning if the site is skipped.
#[allow(clippy::too_many_arguments)]
fn rewrite_call(
    path: &str,
    source: &str,
    line_starts: &[usize],
    func: &str,
    call: Node,
    new_params: &[NewParam],
    kept: &[usize],
    def_params: &[ParamInfo],
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

    // Arity must match the original parameter count; otherwise the call uses
    // keyword / spread / variadic / default arguments we must not reorder blind.
    if args.len() != def_params.len() {
        warnings.push(format!(
            "skipped call at {where_}: {} argument(s) but {} parameter(s) — not reordered",
            args.len(),
            def_params.len()
        ));
        return;
    }

    // Dropping a call argument with side effects changes behaviour — warn.
    for (i, a) in args.iter().enumerate() {
        if !kept.contains(&i) && !is_trivial(*a) {
            warnings.push(format!(
                "call at {where_}: dropped argument `{}` may have side effects",
                a.utf8_text(source.as_bytes()).unwrap_or("").trim()
            ));
        }
    }

    let arg_texts: Vec<String> = args
        .iter()
        .map(|n| {
            n.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();
    let new_args = match rebuild_list(new_params, &arg_texts, /*for_call=*/ true) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("skipped call at {where_}: {e}"));
            return;
        }
    };

    let _ = func;
    let (before, after) = splice_flexible(source, line_starts, args_node, &format!("({new_args})"));
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

/// Build a comma-joined parameter/argument list from the target transform.
/// `for_call` selects the added-entry payload (call default vs signature decl).
fn rebuild_list(
    new_params: &[NewParam],
    originals: &[String],
    for_call: bool,
) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    for np in new_params {
        match np.from {
            Some(i) => {
                let t = originals
                    .get(i)
                    .ok_or_else(|| format!("position {i} missing"))?;
                out.push(t.clone());
            }
            None => {
                if for_call {
                    // A defaulted add is omitted at call sites entirely; only an
                    // injected add contributes an argument value.
                    if np.omit_at_call {
                        continue;
                    }
                    out.push(
                        np.default_arg
                            .clone()
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    );
                } else {
                    out.push(np.decl.clone().unwrap_or_default().trim().to_string());
                }
            }
        }
    }
    Ok(out.join(", "))
}

// --------------------------------------------------------------------------
// parameters & call collection
// --------------------------------------------------------------------------

/// A formal parameter: its binding name (via locals) and full declaration text.
struct ParamInfo {
    name: String,
    text: String,
}

/// The formal parameters of `params_node`, in source order. The binding name is
/// taken from the locals definition sitting inside each parameter node; the text
/// is the parameter's exact source (preserving type annotations / defaults).
fn param_infos(idx: &LocalsIndex, params_node: Node, source: &str) -> Vec<ParamInfo> {
    let mut out = Vec::new();
    for p in named_kids(params_node) {
        if p.kind() == "comment" {
            continue;
        }
        let (ps, pe) = (p.start_byte(), p.end_byte());
        let name = idx
            .definitions
            .iter()
            .filter(|d| d.start_byte >= ps && d.end_byte <= pe)
            .min_by_key(|d| d.start_byte)
            .map(|d: &Occurrence| d.name.clone())
            .unwrap_or_else(|| {
                p.utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
        let text = p
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(ParamInfo { name, text });
    }
    out
}

/// Every call `func(...)` with a bare-identifier callee reachable from `root`.
pub(crate) fn collect_calls<'a>(
    root: Node<'a>,
    lang: Lang,
    func: &str,
    source: &str,
) -> Vec<Node<'a>> {
    let ck = call_kind(lang);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == ck && bare_callee_name(n, source).as_deref() == Some(func) {
            out.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

// --------------------------------------------------------------------------
// line-splice utilities
// --------------------------------------------------------------------------

pub(crate) fn point_row(n: Node) -> usize {
    n.start_position().row
}

pub(crate) fn node_spans_multiple_lines(n: Node) -> bool {
    n.start_position().row != n.end_position().row
}

/// A trivial argument (identifier / literal / simple access) has no side effects
/// worth preserving when dropped.
pub(crate) fn is_trivial(n: Node) -> bool {
    matches!(
        n.kind(),
        "identifier"
            | "integer"
            | "float"
            | "string"
            | "true"
            | "false"
            | "none"
            | "null"
            | "number"
            | "integer_literal"
            | "float_literal"
            | "string_literal"
            | "boolean_literal"
            | "char_literal"
            | "int_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "attribute"
            | "member_expression"
            | "field_expression"
            | "selector_expression"
            | "self"
            | "this"
    )
}

/// Replace `node`'s byte span (which must sit on line `row`, 0-based) with
/// `replacement`, returning `((), old_line, new_line)`.
pub(crate) fn splice_line(
    source: &str,
    line_starts: &[usize],
    row: usize,
    node: Node,
    replacement: &str,
) -> ((), String, String) {
    let line_start = line_starts[row];
    let line_end = source[line_start..]
        .find('\n')
        .map(|off| line_start + off)
        .unwrap_or(source.len());
    let old_line = source[line_start..line_end].to_string();
    let a = node.start_byte() - line_start;
    let b = node.end_byte() - line_start;
    let mut new_line = String::with_capacity(old_line.len());
    new_line.push_str(&old_line[..a]);
    new_line.push_str(replacement);
    new_line.push_str(&old_line[b..]);
    ((), old_line, new_line)
}

/// Splice `node`'s full span with `replacement`, transparently handling nodes
/// that span multiple lines. For a single-line node this returns the whole
/// line rewritten (the editcore line-anchored convention, matching
/// [`splice_line`]). For a multi-line node — e.g. a signature or call whose
/// parentheses wrap across lines — it returns the node's *own* source text as
/// the `search` and `replacement` as the `replace`, so the multi-line span is
/// matched and collapsed to the (single-line) `replacement` in one op. The
/// caller pairs this with `line_hint = node.start row + 1`.
pub(crate) fn splice_flexible(
    source: &str,
    line_starts: &[usize],
    node: Node,
    replacement: &str,
) -> (String, String) {
    if node_spans_multiple_lines(node) {
        let before = source[node.start_byte()..node.end_byte()].to_string();
        (before, replacement.to_string())
    } else {
        let row = node.start_position().row;
        let (_, before, after) = splice_line(source, line_starts, row, node, replacement);
        (before, after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str, path: &str, func: &str, nps: Vec<NewParam>) -> RefactorPlan {
        change_signature(path, src, func, &nps, "").unwrap()
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
    fn python_reorder_updates_def_and_calls() {
        let src = "def f(a, b):\n    return a - b\n\nx = f(1, 2)\ny = f(p, q)\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a, b):" && r == "def f(b, a):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = f(1, 2)" && r == "x = f(2, 1)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "y = f(p, q)" && r == "y = f(q, p)"));
    }

    #[test]
    fn python_delete_unused_param() {
        let src = "def f(a, b):\n    return a\n\nf(1, 2)\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a, b):" && r == "def f(a):"));
        assert!(e.iter().any(|(s, r)| s == "f(1, 2)" && r == "f(1)"));
    }

    #[test]
    fn delete_used_param_rejected() {
        let src = "def f(a, b):\n    return a + b\n\nf(1, 2)\n";
        let err = change_signature("m.py", src, "f", &[NewParam::keep(0)], "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn add_param_is_medium_with_default() {
        let src = "def f(a):\n    return a\n\nf(1)\n";
        let p = plan(
            src,
            "m.py",
            "f",
            vec![NewParam::keep(0), NewParam::add("b", "0")],
        );
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a):" && r == "def f(a, b):"));
        assert!(e.iter().any(|(s, r)| s == "f(1)" && r == "f(1, 0)"));
    }

    #[test]
    fn javascript_reorder() {
        let src = "function g(x, y) {\n  return x / y;\n}\nconst r = g(10, 2);\n";
        let p = plan(src, "m.js", "g", vec![NewParam::keep(1), NewParam::keep(0)]);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "function g(x, y) {" && r == "function g(y, x) {"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "const r = g(10, 2);" && r == "const r = g(2, 10);"));
    }

    #[test]
    fn go_reorder_typed_params() {
        let src = "package p\n\nfunc add(a int, b int) int {\n\treturn a + b\n}\n\nfunc use() {\n\t_ = add(1, 2)\n}\n";
        let p = plan(
            src,
            "m.go",
            "add",
            vec![NewParam::keep(1), NewParam::keep(0)],
        );
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("func add(a int, b int) int")
                && r.contains("func add(b int, a int) int")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("add(1, 2)") && r.contains("add(2, 1)")));
    }

    #[test]
    fn rust_reorder_typed_params() {
        let src = "fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n\nfn main() {\n    let _ = sub(5, 3);\n}\n";
        let p = plan(
            src,
            "m.rs",
            "sub",
            vec![NewParam::keep(1), NewParam::keep(0)],
        );
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn sub(a: i32, b: i32)")
                && r.contains("fn sub(b: i32, a: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("sub(5, 3)") && r.contains("sub(3, 5)")));
    }

    #[test]
    fn typescript_reorder_typed_params() {
        let src =
            "function h(a: number, b: number): number {\n  return a - b;\n}\nconst z = h(9, 4);\n";
        let p = plan(src, "m.ts", "h", vec![NewParam::keep(1), NewParam::keep(0)]);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("h(a: number, b: number)")
                && r.contains("h(b: number, a: number)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("h(9, 4)") && r.contains("h(4, 9)")));
    }

    #[test]
    fn rust_method_reorders_value_params_keeping_self() {
        // `self` is held fixed; only the value parameters are reordered, and the
        // `recv.m(..)` call site is rewritten. Method plans cap at Medium.
        let src = "struct S;\nimpl S {\n    fn m(&self, a: i32, b: i32) -> i32 { a - b }\n}\nfn main() { let s = S; s.m(5, 3); }\n";
        let p = plan(src, "m.rs", "m", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("fn m(&self, a: i32, b: i32)")
                && r.contains("fn m(&self, b: i32, a: i32)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.m(5, 3)") && r.contains("s.m(3, 5)")));
    }

    #[test]
    fn python_method_reorders_keeping_self() {
        let src = "class C:\n    def m(self, a, b):\n        return a - b\nc = C()\nc.m(1, 2)\n";
        let p = plan(src, "m.py", "m", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("def m(self, a, b):") && r.contains("def m(self, b, a):")));
        assert!(e.iter().any(|(s, r)| s == "c.m(1, 2)" && r == "c.m(2, 1)"));
    }

    #[test]
    fn go_method_reorders_value_params() {
        // Go receivers live in a separate field, so all `parameters` entries are
        // value params; `s.M(..)` selector calls are matched.
        let src = "package p\ntype S struct{}\nfunc (s S) M(a int, b int) int { return a - b }\nfunc main() { var s S; s.M(1, 2) }\n";
        let p = plan(src, "m.go", "M", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("func (s S) M(a int, b int)")
                && r.contains("func (s S) M(b int, a int)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("s.M(1, 2)") && r.contains("s.M(2, 1)")));
    }

    #[test]
    fn js_method_reorders_and_rewrites_member_call() {
        let src = "class C { m(a, b) { return a - b; } }\nconst c = new C();\nc.m(1, 2);\n";
        let p = plan(src, "m.js", "m", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("m(a, b)") && r.contains("m(b, a)")));
        assert!(e
            .iter()
            .any(|(s, r)| s.contains("c.m(1, 2)") && r.contains("c.m(2, 1)")));
    }

    #[test]
    fn arity_mismatch_call_is_skipped_and_medium() {
        let src = "def f(a, b):\n    return a\n\nf(1, 2)\nf(1, 2, 3)\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        let e = edits(&p);
        assert!(e.iter().any(|(s, r)| s == "f(1, 2)" && r == "f(2, 1)"));
        assert!(!e.iter().any(|(s, _)| s == "f(1, 2, 3)"));
        assert!(p.warnings.iter().any(|w| w.contains("not reordered")));
    }

    #[test]
    fn dropping_sideeffecting_arg_warns() {
        let src = "def f(a, b):\n    return a\n\nf(1, g())\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::Medium);
        assert!(p.warnings.iter().any(|w| w.contains("side effects")));
    }

    #[test]
    fn unknown_function_rejected() {
        let src = "def f(a):\n    return a\n";
        let err = change_signature("m.py", src, "nope", &[NewParam::keep(0)], "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn empty_target_rejected() {
        let src = "def f(a):\n    return a\n";
        let err = change_signature("m.py", src, "f", &[], "").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn python_add_defaulted_omits_at_call_stays_high() {
        // The default lives in the signature (`b=0`); existing calls keep their
        // original arity (omit `b`), so nothing is guessed and confidence is High.
        let src = "def f(a):\n    return a\n\nf(1)\n";
        let p = plan(
            src,
            "m.py",
            "f",
            vec![NewParam::keep(0), NewParam::add_defaulted("b=0")],
        );
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a):" && r == "def f(a, b=0):"));
        // The call is unchanged (arity preserved) → no op emitted for it.
        assert!(!e.iter().any(|(s, _)| s == "f(1)"));
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn js_add_defaulted_stays_high() {
        let src = "function g(x) {\n  return x;\n}\nconst r = g(10);\n";
        let p = plan(
            src,
            "m.js",
            "g",
            vec![NewParam::keep(0), NewParam::add_defaulted("y = 0")],
        );
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "function g(x) {" && r == "function g(x, y = 0) {"));
        assert!(!e.iter().any(|(s, _)| s == "const r = g(10);"));
    }

    #[test]
    fn go_add_defaulted_rejected() {
        // Go has no call-site-omittable defaults → honest rejection.
        let src = "package p\n\nfunc add(a int) int {\n\treturn a\n}\n";
        let err = change_signature(
            "m.go",
            src,
            "add",
            &[NewParam::keep(0), NewParam::add_defaulted("b int")],
            "",
        )
        .unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rust_add_defaulted_rejected() {
        let src = "fn f(a: i32) -> i32 {\n    a\n}\n";
        let err = change_signature(
            "m.rs",
            src,
            "f",
            &[NewParam::keep(0), NewParam::add_defaulted("b: i32")],
            "",
        )
        .unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn defaulted_add_must_be_trailing() {
        // A non-defaulted parameter after a defaulted add is rejected.
        let src = "def f(a):\n    return a\n\nf(1)\n";
        let err = change_signature(
            "m.py",
            src,
            "f",
            &[NewParam::add_defaulted("b=0"), NewParam::keep(0)],
            "",
        )
        .unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn multiline_signature_reorder_collapses() {
        // A signature wrapped across lines is matched as a multi-line span and
        // rewritten to a single collapsed parameter list.
        let src = "def f(\n    a,\n    b,\n):\n    return a - b\n\nx = f(1, 2)\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        // The whole multi-line parameter list is the search anchor.
        assert!(e
            .iter()
            .any(|(s, r)| s == "(\n    a,\n    b,\n)" && r == "(b, a)"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "x = f(1, 2)" && r == "x = f(2, 1)"));
    }

    #[test]
    fn multiline_call_reorder_collapses() {
        // A call whose argument list wraps across lines is matched as a
        // multi-line span and collapsed on rewrite.
        let src = "def f(a, b):\n    return a - b\n\nx = f(\n    1,\n    2,\n)\n";
        let p = plan(src, "m.py", "f", vec![NewParam::keep(1), NewParam::keep(0)]);
        assert_eq!(p.confidence, Confidence::High);
        let e = edits(&p);
        assert!(e
            .iter()
            .any(|(s, r)| s == "def f(a, b):" && r == "def f(b, a):"));
        assert!(e
            .iter()
            .any(|(s, r)| s == "(\n    1,\n    2,\n)" && r == "(2, 1)"));
    }
}
