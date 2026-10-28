//! inline-method (docs §4): the inverse of extract-method. Given a cursor on a
//! call `f(args)` whose callee `f` is defined in the *same file*, bind the
//! call-site arguments to `f`'s parameters and expand its body at the call site
//! (§4「找调用点 → 参数实参绑定形参 → 展开方法体」).
//!
//! Two shapes are handled, everything else is rejected with an honest reason
//! (§7「绝不瞎猜」, this is a 二等/second-tier refactor):
//!
//!  * **value context** — the call is used as a value (`y = f(a)`, `return f(a)`,
//!    `g(f(a))`, …). Supported only when `f`'s body reduces to a single result
//!    expression; the call is replaced in place by the parenthesised, argument-
//!    substituted expression.
//!  * **statement context** — the call stands alone (`f(a);`). Supported only
//!    when `f` is void (no value-returning `return`); the statement is replaced
//!    by the re-indented, argument-substituted body.
//!
//! Downgrades to Medium (§4「有…副作用则降级」) when a non-trivial argument maps
//! to a parameter used ≠1 times (double-evaluation or a dropped side effect) or
//! when a callee local would collide with a caller local. Rejects multiple/early
//! returns, recursion, argument-count mismatch, method/scoped callees, and
//! multi-line value calls — none can be linearised safely by structure alone.

use std::collections::BTreeSet;

use shuttle_nav::Confidence;
use tree_sitter::Node;

use crate::extract_method::{enclosing_function, leading_ws, line_start_offsets, reindent_block};
use crate::locals::{Lang, LocalsIndex, Occurrence};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// Inline the callee of the call at `line` (1-based) / `col` (0-based byte).
pub fn inline_method(
    path: &str,
    source: &str,
    line: usize,
    col: usize,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();
    let lines: Vec<&str> = source.split('\n').collect();
    let line_starts = line_start_offsets(source);
    if line == 0 || line > lines.len() {
        return Err(RefactorError::Rejected(format!("line {line} out of range")));
    }
    let cursor = line_starts[line - 1] + col;

    // 1. The call node under the cursor with a bare-identifier callee.
    let call = find_call_at(root, lang, cursor)
        .ok_or_else(|| RefactorError::Rejected(format!("no function call at {line}:{col}")))?;
    let callee_name = bare_callee_name(call, source).ok_or_else(|| {
        RefactorError::Rejected(
            "callee is not a plain function (method/associated calls are not inlined)".into(),
        )
    })?;

    // 2. The callee definition — same file only (single-file, daemonless).
    let callee = find_callee_def(root, lang, &callee_name, source)?;
    if callee.start_byte() <= call.start_byte() && call.end_byte() <= callee.end_byte() {
        return Err(RefactorError::Rejected(
            "the call is inside its own callee — inlining would recurse".into(),
        ));
    }
    if body_calls(callee, lang, &callee_name, source) {
        return Err(RefactorError::Rejected(format!(
            "`{callee_name}` is recursive — inline is unsafe"
        )));
    }

    let params_node = callee
        .child_by_field_name("parameters")
        .ok_or_else(|| RefactorError::Rejected("callee has no parameter list".into()))?;
    let body = callee
        .child_by_field_name("body")
        .ok_or_else(|| RefactorError::Rejected("callee has no body".into()))?;

    // 3. Bind arguments to parameters positionally.
    let param_names = param_names_in_order(&idx, params_node);
    let arg_nodes = arg_nodes(call);
    if param_names.len() != arg_nodes.len() {
        return Err(RefactorError::Rejected(format!(
            "`{callee_name}` takes {} parameter(s) but the call passes {} — \
             default/variadic parameters are not inlined",
            param_names.len(),
            arg_nodes.len()
        )));
    }
    let args: Vec<String> = arg_nodes
        .iter()
        .map(|n| {
            n.utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .collect();

    // 4. Legality on the body's return shape.
    let returns = return_nodes(body, lang);
    let stmts = body_statements(body, lang);
    if stmts.is_empty() {
        return Err(RefactorError::Rejected("callee body is empty".into()));
    }
    if returns.len() > 1 {
        return Err(RefactorError::Rejected(format!(
            "`{callee_name}` has {} return statements — inline would change control flow",
            returns.len()
        )));
    }
    // The single return (if any) must be the last statement — otherwise it is an
    // early exit we cannot linearise.
    if let Some(ret) = returns.first() {
        let last = stmts.last().unwrap();
        if !within(*ret, *last) && ret.start_byte() != last.start_byte() {
            return Err(RefactorError::Rejected(format!(
                "`{callee_name}` returns before its last statement — inline is unsafe"
            )));
        }
    }

    // 5. Value vs statement context, decided by the call's syntactic parent.
    let parent = call.parent();
    let is_statement = parent
        .map(|p| p.kind() == "expression_statement")
        .unwrap_or(false);

    let mut warnings: Vec<String> = Vec::new();

    let (search, replace, hint) = if is_statement {
        build_statement_inline(
            &idx,
            source,
            &lines,
            &line_starts,
            call,
            parent.unwrap(),
            body,
            lang,
            &returns,
            &stmts,
            &param_names,
            &args,
            &arg_nodes,
            &callee_name,
            &mut warnings,
        )?
    } else {
        build_value_inline(
            &idx,
            source,
            &lines,
            &line_starts,
            call,
            body,
            lang,
            &returns,
            &stmts,
            &param_names,
            &args,
            &arg_nodes,
            &callee_name,
            &mut warnings,
        )?
    };

    let confidence = if warnings.is_empty() {
        Confidence::High
    } else {
        Confidence::Medium
    };

    let op = search_replace(path, search, replace, hint);
    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::InlineMethod,
            plan: plan_from_ops(
                RefactorKind::InlineMethod,
                path,
                vec![op],
                &format!("inline-method {callee_name}"),
            ),
            signature: None,
            params: Vec::new(),
            returns: Vec::new(),
            confidence,
            warnings,
            sites: vec![format!("{path}:{line} (inline {callee_name})")],
        },
        path,
        source,
    ))
}

// --------------------------------------------------------------------------
// context builders
// --------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_value_inline(
    idx: &LocalsIndex,
    source: &str,
    lines: &[&str],
    line_starts: &[usize],
    call: Node,
    body: Node,
    lang: Lang,
    returns: &[Node],
    stmts: &[Node],
    param_names: &[String],
    args: &[String],
    arg_nodes: &[Node],
    callee_name: &str,
    warnings: &mut Vec<String>,
) -> Result<(String, String, usize), RefactorError> {
    if call.start_position().row != call.end_position().row {
        return Err(RefactorError::Rejected(
            "call spans multiple lines — value inline is not attempted".into(),
        ));
    }
    // The body must reduce to a single result expression.
    let result = single_result_expr(body, lang, returns, stmts).ok_or_else(|| {
        RefactorError::Rejected(format!(
            "`{callee_name}`'s body is not a single return expression — cannot inline as a value"
        ))
    })?;
    let (rs, re) = (result.start_byte(), result.end_byte());
    let subs = param_subs(
        idx,
        source,
        param_names,
        args,
        arg_nodes,
        (rs, re),
        warnings,
    );
    let expr = splice(source, rs, re, &subs);
    let new_call = format!("({expr})");

    // Splice the new expression over the call's span, within its single line.
    let row = call.start_position().row;
    let orig = lines[row];
    let ls = line_starts[row];
    let cs = call.start_byte() - ls;
    let ce = call.end_byte() - ls;
    let mut new_line = String::new();
    new_line.push_str(&orig[..cs]);
    new_line.push_str(&new_call);
    new_line.push_str(&orig[ce..]);
    Ok((orig.to_string(), new_line, row + 1))
}

#[allow(clippy::too_many_arguments)]
fn build_statement_inline(
    idx: &LocalsIndex,
    source: &str,
    lines: &[&str],
    line_starts: &[usize],
    call: Node,
    stmt: Node,
    body: Node,
    lang: Lang,
    returns: &[Node],
    stmts: &[Node],
    param_names: &[String],
    args: &[String],
    arg_nodes: &[Node],
    callee_name: &str,
    warnings: &mut Vec<String>,
) -> Result<(String, String, usize), RefactorError> {
    let _ = (call, body);
    // Statement context discards any value, so a value-returning callee cannot be
    // inlined here without silently dropping its result.
    if let Some(ret) = returns.first() {
        if return_has_value(*ret, lang) {
            return Err(RefactorError::Rejected(format!(
                "`{callee_name}` returns a value but the call ignores it — not inlined"
            )));
        }
    }
    // Statements to keep = all body statements minus a trailing bare `return`.
    let mut kept: Vec<Node> = stmts.to_vec();
    if let Some(ret) = returns.first() {
        if kept.last().map(|l| l.start_byte())
            == Some(ret_stmt_of(*ret, *stmts.last().unwrap()).start_byte())
        {
            kept.pop();
        }
    }
    if kept.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{callee_name}` has no statements to inline"
        )));
    }
    // Anchor the block at the *line start* of the first kept statement so every
    // inlined line carries its original indentation uniformly — `reindent_block`
    // then strips the common prefix and re-indents to the call site cleanly.
    let first_row = kept.first().unwrap().start_position().row;
    let bstart = line_starts[first_row];
    let bend = kept.last().unwrap().end_byte();

    // Warn if a callee local would collide with a caller local (textual inline).
    collision_warn(
        idx,
        source,
        call,
        kept.as_slice(),
        param_names,
        callee_name,
        warnings,
    );

    let subs = param_subs(
        idx,
        source,
        param_names,
        args,
        arg_nodes,
        (bstart, bend),
        warnings,
    );
    let inlined = splice(source, bstart, bend, &subs);

    // Re-indent the inlined body to the call statement's indentation.
    let s_row = stmt.start_position().row;
    let e_row = stmt.end_position().row;
    let indent = leading_ws(lines[s_row]);
    let body_lines: Vec<&str> = inlined.split('\n').collect();
    let replace = reindent_block(&body_lines, &indent);

    let search = lines[s_row..=e_row].join("\n");
    Ok((search, replace, s_row + 1))
}

// --------------------------------------------------------------------------
// argument substitution
// --------------------------------------------------------------------------

/// Substitution spans (absolute byte offsets) for every parameter reference that
/// falls inside `region`, replacing it with the bound argument text. Emits
/// double-evaluation / dropped-side-effect warnings for non-trivial arguments.
fn param_subs(
    idx: &LocalsIndex,
    _source: &str,
    param_names: &[String],
    args: &[String],
    arg_nodes: &[Node],
    region: (usize, usize),
    warnings: &mut Vec<String>,
) -> Vec<(usize, usize, String)> {
    let (rs, re) = region;
    let mut subs: Vec<(usize, usize, String)> = Vec::new();
    for (i, name) in param_names.iter().enumerate() {
        // The parameter's binding definition (inside the callee parameter list)
        // and its governing scope give the exact set of body references.
        let Some(def) = idx.definitions.iter().find(|d| d.name == *name) else {
            continue;
        };
        let Some(scope) = idx.scope_range(def.scope_id) else {
            continue;
        };
        let refs: Vec<&Occurrence> = idx
            .references_bound_to(name, scope)
            .into_iter()
            .filter(|r| r.start_byte >= rs && r.end_byte <= re)
            .collect();
        let count = refs.len();
        let trivial = arg_nodes.get(i).map(|n| is_trivial_arg(*n)).unwrap_or(true);
        if !trivial && count > 1 {
            warnings.push(format!(
                "argument `{}` is substituted for parameter `{name}` used {count} times — \
                 it would be evaluated repeatedly (possible side-effect change)",
                args[i]
            ));
        }
        if !trivial && count == 0 {
            warnings.push(format!(
                "argument `{}` for unused parameter `{name}` is dropped — its side effects \
                 would no longer run",
                args[i]
            ));
        }
        for r in refs {
            subs.push((r.start_byte, r.end_byte, args[i].clone()));
        }
    }
    subs
}

/// Splice `subs` (absolute offsets, must lie within `[start,end)`) into the
/// substring `source[start..end]`, right-to-left so earlier offsets stay valid.
fn splice(source: &str, start: usize, end: usize, subs: &[(usize, usize, String)]) -> String {
    let mut bytes = source.as_bytes()[start..end].to_vec();
    let mut ordered: Vec<&(usize, usize, String)> = subs.iter().collect();
    ordered.sort_by_key(|(s, _, _)| *s);
    for (s, e, rep) in ordered.into_iter().rev() {
        if *s >= start && *e <= end {
            let (rs, re) = (s - start, e - start);
            bytes.splice(rs..re, rep.bytes());
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| String::new())
}

// --------------------------------------------------------------------------
// structural helpers
// --------------------------------------------------------------------------

pub(crate) fn call_kind(lang: Lang) -> &'static str {
    match lang {
        Lang::Python => "call",
        _ => "call_expression",
    }
}

/// Deepest call node containing `byte` whose callee is a bare identifier.
fn find_call_at(root: Node, lang: Lang, byte: usize) -> Option<Node> {
    let ck = call_kind(lang);
    let mut best: Option<Node> = None;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.start_byte() <= byte && byte < n.end_byte() {
            if n.kind() == ck {
                match best {
                    Some(b)
                        if (b.end_byte() - b.start_byte()) <= (n.end_byte() - n.start_byte()) => {}
                    _ => best = Some(n),
                }
            }
            let mut c = n.walk();
            for ch in n.children(&mut c) {
                stack.push(ch);
            }
        }
    }
    best
}

/// The callee identifier of `call`, only when it is a plain `name(...)` form.
pub(crate) fn bare_callee_name(call: Node, source: &str) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "identifier" {
        return None;
    }
    let name = func.utf8_text(source.as_bytes()).ok()?;
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// The *method* name of a `recv.name(...)` call — the trailing identifier of a
/// member/attribute/field/selector callee. Returns `None` for a bare
/// `name(...)` call (that is [`bare_callee_name`]'s job). Works across the five
/// grammars: Python `attribute`, Rust `field_expression`, Go
/// `selector_expression`, JS/TS `member_expression`.
pub(crate) fn method_callee_name(call: Node, source: &str) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    let trailing = match func.kind() {
        "attribute" => func.child_by_field_name("attribute"),
        "field_expression" => func.child_by_field_name("field"),
        "selector_expression" => func.child_by_field_name("field"),
        "member_expression" => func.child_by_field_name("property"),
        _ => None,
    }?;
    let name = trailing.utf8_text(source.as_bytes()).ok()?;
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// True if `def` is a *method* (associated with a class/impl/receiver) rather
/// than a free function. Method call sites take the `recv.name(...)` form, so
/// this selects how we discover call sites and (for languages with an implicit
/// receiver parameter) how many leading parameters to treat as the receiver.
pub(crate) fn is_method_def(def: Node, lang: Lang) -> bool {
    match lang {
        // Go methods are their own node kind (receiver in a separate field).
        Lang::Go => def.kind() == "method_declaration",
        // JS/TS class methods.
        Lang::JavaScript | Lang::TypeScript => def.kind() == "method_definition",
        // Rust: a `self` receiver, or defined inside an `impl` block.
        Lang::Rust => {
            if let Some(params) = def.child_by_field_name("parameters") {
                if named_kids(params)
                    .iter()
                    .any(|p| p.kind() == "self_parameter")
                {
                    return true;
                }
            }
            let mut cur = def.parent();
            while let Some(n) = cur {
                if n.kind() == "impl_item" {
                    return true;
                }
                cur = n.parent();
            }
            false
        }
        // Python: defined (transitively) inside a `class_definition`.
        Lang::Python => {
            let mut cur = def.parent();
            while let Some(n) = cur {
                if n.kind() == "class_definition" {
                    return true;
                }
                cur = n.parent();
            }
            false
        }
    }
}

/// Number of leading entries in the `parameters` node that are *receivers*
/// (never supplied at call sites): the Rust `self_parameter`, or a Python
/// method's conventional first `self`/`cls` parameter. Go receivers live in a
/// separate `receiver` field, and JS/TS methods have no receiver parameter, so
/// both return 0. Value parameters are `params[receiver_param_count..]`.
pub(crate) fn receiver_param_count(def: Node, lang: Lang, source: &str) -> usize {
    if !is_method_def(def, lang) {
        return 0;
    }
    let Some(params) = def.child_by_field_name("parameters") else {
        return 0;
    };
    match lang {
        Lang::Rust => {
            if named_kids(params).first().map(|p| p.kind()) == Some("self_parameter") {
                1
            } else {
                0
            }
        }
        Lang::Python => {
            let first = named_kids(params)
                .into_iter()
                .find(|p| p.kind() != "comment");
            match first {
                Some(p) if p.kind() == "identifier" => {
                    let t = p.utf8_text(source.as_bytes()).unwrap_or("");
                    if t == "self" || t == "cls" {
                        1
                    } else {
                        0
                    }
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Every `recv.method(...)` call whose trailing method name is `method`,
/// reachable from `root`. Complements [`collect_calls`], which only matches
/// bare `name(...)` calls. Receiver type is *not* verified — callers must cap
/// confidence and warn accordingly.
pub(crate) fn collect_method_calls<'a>(
    root: Node<'a>,
    lang: Lang,
    method: &str,
    source: &str,
) -> Vec<Node<'a>> {
    let ck = call_kind(lang);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == ck && method_callee_name(n, source).as_deref() == Some(method) {
            out.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

/// The unique same-file function definition named `name`.
pub(crate) fn find_callee_def<'a>(
    root: Node<'a>,
    lang: Lang,
    name: &str,
    source: &str,
) -> Result<Node<'a>, RefactorError> {
    let kinds = lang.function_kinds();
    let mut hits: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) {
            if let Some(id) = n.child_by_field_name("name") {
                if id.utf8_text(source.as_bytes()).ok() == Some(name) {
                    hits.push(n);
                }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    match hits.len() {
        0 => Err(RefactorError::Rejected(format!(
            "`{name}` is not defined in this file — cross-file inline is out of scope"
        ))),
        1 => Ok(hits[0]),
        _ => Err(RefactorError::Rejected(format!(
            "`{name}` is defined {} times in this file — inline is ambiguous",
            hits.len()
        ))),
    }
}

/// True if `callee`'s body contains a call to `name` (recursion).
fn body_calls(callee: Node, lang: Lang, name: &str, source: &str) -> bool {
    let ck = call_kind(lang);
    let Some(body) = callee.child_by_field_name("body") else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == ck {
            if let Some(f) = n.child_by_field_name("function") {
                if f.kind() == "identifier" && f.utf8_text(source.as_bytes()).ok() == Some(name) {
                    return true;
                }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    false
}

/// Parameter binding names in source order, taken from the locals definitions
/// that sit inside the parameter-list node.
fn param_names_in_order(idx: &LocalsIndex, params: Node) -> Vec<String> {
    let (ps, pe) = (params.start_byte(), params.end_byte());
    let mut defs: Vec<&Occurrence> = idx
        .definitions
        .iter()
        .filter(|d| d.start_byte >= ps && d.end_byte <= pe)
        .collect();
    defs.sort_by_key(|d| d.start_byte);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for d in defs {
        if seen.insert(d.start_byte) {
            out.push(d.name.clone());
        }
    }
    out
}

/// The argument expression nodes of `call`, in order.
pub(crate) fn arg_nodes(call: Node) -> Vec<Node> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    named_kids(args)
}

/// Named children of `n` via index access (returned nodes carry the tree's
/// lifetime, not a cursor's, so they can outlive local iteration).
pub(crate) fn named_kids(n: Node) -> Vec<Node> {
    (0..n.named_child_count())
        .filter_map(|i| n.named_child(i))
        .collect()
}

/// The statement/expression children that make up a function body, unwrapping
/// Go's `statement_list` and dropping comments.
fn body_statements(body: Node, lang: Lang) -> Vec<Node> {
    let container = if matches!(lang, Lang::Go) {
        named_kids(body)
            .into_iter()
            .find(|n| n.kind() == "statement_list")
            .unwrap_or(body)
    } else {
        body
    };
    named_kids(container)
        .into_iter()
        .filter(|n| !n.kind().contains("comment"))
        .collect()
}

/// All return nodes inside `body`.
fn return_nodes(body: Node, lang: Lang) -> Vec<Node> {
    let ret_kind = match lang {
        Lang::Rust => "return_expression",
        _ => "return_statement",
    };
    let mut out = Vec::new();
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind() == ret_kind {
            out.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

/// True if a return node carries a value (vs a bare `return`).
fn return_has_value(ret: Node, lang: Lang) -> bool {
    match lang {
        Lang::Rust => ret.named_child(0).is_some(),
        _ => ret.named_child(0).is_some(),
    }
}

/// The statement node (in `stmts` order) that owns `ret`; falls back to `last`.
fn ret_stmt_of<'a>(ret: Node<'a>, last: Node<'a>) -> Node<'a> {
    // For py/js/go/ts the return *is* the statement; for rust the return_expression
    // sits inside an expression_statement — either way its start lies within the
    // last statement, so `last` is the right anchor for the trailing-return test.
    let _ = ret;
    last
}

fn within(inner: Node, outer: Node) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}

/// If `body` reduces to exactly one result expression, return that expression
/// node (unwrapping `return`/Go `expression_list`/Rust trailing expr).
fn single_result_expr<'a>(
    body: Node<'a>,
    lang: Lang,
    returns: &[Node<'a>],
    stmts: &[Node<'a>],
) -> Option<Node<'a>> {
    let _ = body;
    if stmts.len() != 1 {
        return None;
    }
    let only = stmts[0];
    match lang {
        Lang::Rust => {
            // Either `{ return EXPR }` (expression_statement→return_expression) or a
            // bare trailing expression `{ EXPR }`.
            if only.kind() == "expression_statement" {
                let inner = only.named_child(0)?;
                if inner.kind() == "return_expression" {
                    return inner.named_child(0);
                }
                return None;
            }
            if returns.is_empty()
                && only.is_named()
                && !only.kind().ends_with("_declaration")
                && only.kind() != "expression_statement"
            {
                // A trailing expression node is the implicit result.
                return Some(only);
            }
            None
        }
        _ => {
            if only.kind() != "return_statement" {
                return None;
            }
            let val = only.named_child(0)?;
            // Go wraps the value(s) in an expression_list; a single value unwraps,
            // multiple values are a tuple return we decline for value inline.
            if val.kind() == "expression_list" {
                let kids = named_kids(val);
                if kids.len() == 1 {
                    return Some(kids[0]);
                }
                return None;
            }
            Some(val)
        }
    }
}

fn is_trivial_arg(n: Node) -> bool {
    let k = n.kind();
    k == "identifier"
        || k.ends_with("_literal")
        || k == "integer"
        || k == "float"
        || k == "number"
        || k == "string"
        || k == "true"
        || k == "false"
        || k == "nil"
        || k == "none"
        || k == "null"
        || k.ends_with("expression") && k.starts_with("field")
        || k == "member_expression"
        || k == "selector_expression"
        || k == "attribute"
}

/// Warn when a callee local (a body definition that is not a parameter) shares a
/// name with a caller local — textual inline would then shadow/collide.
fn collision_warn(
    idx: &LocalsIndex,
    _source: &str,
    call: Node,
    kept: &[Node],
    param_names: &[String],
    callee_name: &str,
    warnings: &mut Vec<String>,
) {
    let root = idx.tree.root_node();
    let Some(caller_fn) = enclosing_function(root, idx.lang, call.start_byte(), call.end_byte())
    else {
        return;
    };
    let (cs, ce) = (caller_fn.start_byte(), caller_fn.end_byte());
    let caller_locals: BTreeSet<&str> = idx
        .definitions
        .iter()
        .filter(|d| d.start_byte >= cs && d.start_byte < ce)
        .map(|d| d.name.as_str())
        .collect();
    let (bs, be) = (
        kept.first().unwrap().start_byte(),
        kept.last().unwrap().end_byte(),
    );
    let params: BTreeSet<&str> = param_names.iter().map(|s| s.as_str()).collect();
    let mut clashes: BTreeSet<&str> = BTreeSet::new();
    for d in &idx.definitions {
        if d.start_byte >= bs
            && d.end_byte <= be
            && !params.contains(d.name.as_str())
            && caller_locals.contains(d.name.as_str())
        {
            clashes.insert(d.name.as_str());
        }
    }
    for name in clashes {
        warnings.push(format!(
            "`{callee_name}` declares a local `{name}` that already exists at the call site — \
             inlining may shadow or collide"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_nav::Confidence;
    use shuttle_protocol::EditOp;

    // (search, replace) of the single SearchReplace op in the plan.
    fn op(plan: &RefactorPlan) -> (&str, &str) {
        match &plan.plan.ops[0] {
            EditOp::SearchReplace {
                search, replace, ..
            } => (search.as_str(), replace.as_str()),
            _ => panic!("expected SearchReplace"),
        }
    }

    // Byte column (0-based) of the first occurrence of `needle` on 1-based `line`.
    fn col_of(src: &str, line: usize, needle: &str) -> usize {
        src.split('\n').nth(line - 1).unwrap().find(needle).unwrap()
    }

    #[test]
    fn python_value_inline() {
        let src = "def add(a, b):\n    return a + b\n\ndef main():\n    x = add(1, 2)\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.py", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "x = (1 + 2)");
        assert!(matches!(plan.confidence, Confidence::High));
    }

    #[test]
    fn python_statement_void_inline() {
        let src = "def log(x):\n    print(x)\n\ndef main():\n    log(42)\n";
        let c = col_of(src, 5, "log");
        let plan = inline_method("m.py", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r, "    print(42)");
        assert!(matches!(plan.confidence, Confidence::High));
    }

    #[test]
    fn javascript_value_inline() {
        let src = "function add(a, b) {\n  return a + b;\n}\nfunction main() {\n  const x = add(1, 2);\n}\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.js", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "const x = (1 + 2);");
    }

    #[test]
    fn go_value_inline_single_return() {
        let src =
            "func add(a int, b int) int {\n\treturn a + b\n}\nfunc main() {\n\tx := add(1, 2)\n}\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.go", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "x := (1 + 2)");
    }

    #[test]
    fn rust_value_inline_trailing_expr() {
        let src = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\nfn main() {\n    let x = add(1, 2);\n}\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.rs", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "let x = (1 + 2);");
    }

    #[test]
    fn rust_value_inline_return_expr() {
        let src = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\nfn main() {\n    let x = add(1, 2);\n}\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.rs", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "let x = (1 + 2);");
    }

    #[test]
    fn typescript_value_inline() {
        let src = "function add(a: number, b: number): number {\n  return a + b;\n}\nfunction main() {\n  const x = add(1, 2);\n}\n";
        let c = col_of(src, 5, "add");
        let plan = inline_method("m.ts", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "const x = (1 + 2);");
    }

    #[test]
    fn multi_statement_void_inline_reindents() {
        let src = "def setup(n):\n    a = n + 1\n    print(a)\n\ndef main():\n    setup(5)\n";
        let c = col_of(src, 6, "setup");
        let plan = inline_method("m.py", src, 6, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r, "    a = 5 + 1\n    print(a)");
    }

    #[test]
    fn nontrivial_arg_used_twice_is_medium() {
        // `g()` (non-trivial) bound to `a` which appears twice → double-eval warning.
        let src = "def sq(a):\n    return a * a\n\ndef main():\n    x = sq(g())\n";
        let c = col_of(src, 5, "sq");
        let plan = inline_method("m.py", src, 5, c).unwrap();
        let (_s, r) = op(&plan);
        assert_eq!(r.trim(), "x = (g() * g())");
        assert!(matches!(plan.confidence, Confidence::Medium));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("evaluated repeatedly")));
    }

    #[test]
    fn reject_multiple_returns() {
        let src = "def pick(a):\n    if a:\n        return 1\n    return 2\n\ndef main():\n    x = pick(True)\n";
        let c = col_of(src, 7, "pick");
        let err = inline_method("m.py", src, 7, c).unwrap_err();
        assert!(format!("{err}").contains("return"));
    }

    #[test]
    fn reject_value_context_for_void_callee() {
        let src = "def log(x):\n    print(x)\n\ndef main():\n    y = log(1)\n";
        let c = col_of(src, 5, "log");
        let err = inline_method("m.py", src, 5, c).unwrap_err();
        assert!(format!("{err}").contains("single return expression"));
    }

    #[test]
    fn reject_value_returning_callee_as_statement() {
        let src = "def add(a, b):\n    return a + b\n\ndef main():\n    add(1, 2)\n";
        let c = col_of(src, 5, "add");
        let err = inline_method("m.py", src, 5, c).unwrap_err();
        assert!(format!("{err}").contains("ignores it"));
    }

    #[test]
    fn reject_unknown_callee() {
        let src = "def main():\n    x = missing(1)\n";
        let c = col_of(src, 2, "missing");
        let err = inline_method("m.py", src, 2, c).unwrap_err();
        assert!(format!("{err}").contains("not defined in this file"));
    }

    #[test]
    fn reject_method_call() {
        let src = "def main():\n    x = obj.f(1)\n";
        let c = col_of(src, 2, "f(");
        let err = inline_method("m.py", src, 2, c).unwrap_err();
        assert!(format!("{err}").contains("method"));
    }

    #[test]
    fn reject_arity_mismatch() {
        let src = "def add(a, b):\n    return a + b\n\ndef main():\n    x = add(1)\n";
        let c = col_of(src, 5, "add");
        let err = inline_method("m.py", src, 5, c).unwrap_err();
        assert!(format!("{err}").contains("parameter"));
    }

    #[test]
    fn reject_recursion() {
        let src = "def fac(n):\n    return n * fac(n)\n\ndef main():\n    x = fac(3)\n";
        let c = col_of(src, 5, "fac");
        let err = inline_method("m.py", src, 5, c).unwrap_err();
        assert!(format!("{err}").contains("recursive"));
    }

    #[test]
    fn local_collision_is_medium() {
        // callee declares `a`; caller already has `a` in scope → collision warn.
        let src =
            "def setup(n):\n    a = n + 1\n    print(a)\n\ndef main():\n    a = 99\n    setup(5)\n";
        let c = col_of(src, 7, "setup");
        let plan = inline_method("m.py", src, 7, c).unwrap();
        assert!(matches!(plan.confidence, Confidence::Medium));
        assert!(plan.warnings.iter().any(|w| w.contains("collide")));
    }
}
