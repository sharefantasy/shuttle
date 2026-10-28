//! extract-method (docs §3): the refactor that most directly exercises
//! "scope + data-flow". We resolve the innermost enclosing function's locals,
//! classify variables around the selection into def_before / def_in / ref_in /
//! ref_after (§3.1), derive params (§3.2) and returns (§3.3), run the legality
//! gates (§3.4), and lower to an `EditPlan` of two SearchReplace ops: insert the
//! new function above the enclosing one, and replace the selection with a call.
//!
//! Honesty (§7): locals decides scope/binding (High). Type annotations for
//! go/rust/ts are NOT inferred here — we emit placeholder types + a TODO warning
//! and downgrade confidence. Complex control flow in the selection is rejected,
//! not silently mis-extracted.

use std::collections::BTreeSet;

use shuttle_nav::Confidence;
use shuttle_symbols::SymbolTable;
use tree_sitter::Node;

use crate::locals::{Lang, LocalsIndex, Occurrence};
use crate::{
    plan_from_ops, search_replace, ParamInfo, RefactorError, RefactorKind, RefactorPlan, ReturnInfo,
};

/// Placeholder emitted when a typed language needs an annotation we can't infer.
const TODO_TYPE: &str = "TODO_TYPE";

/// Extract lines `[start_line, end_line]` (1-based, inclusive) of the innermost
/// enclosing function into a new function `new_name`, replacing the selection
/// with a call to it.
pub fn extract_method(
    path: &str,
    source: &str,
    start_line: usize,
    end_line: usize,
    new_name: &str,
) -> Result<RefactorPlan, RefactorError> {
    // Level-1 only (pure tree-sitter, daemonless, sub-ms): no repo symbol table,
    // so the ctags return-type fallback (§5 level 2) is skipped. This is the
    // zero-dependency entry point the latency bench and library callers use.
    extract_method_with_symbols(path, source, start_line, end_line, new_name, None)
}

/// Same as [`extract_method`], plus an optional repo [`SymbolTable`] enabling the
/// level-2 type fallback (§5): when a go/rust/ts variable's nearest declaration
/// has no explicit type but is initialized by a call, read the callee's return
/// type from ctags. `None` ⇒ identical to [`extract_method`].
pub fn extract_method_with_symbols(
    path: &str,
    source: &str,
    start_line: usize,
    end_line: usize,
    new_name: &str,
    symbols: Option<&SymbolTable>,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;

    if end_line < start_line {
        return Err(RefactorError::Rejected("empty line range".into()));
    }
    let lines: Vec<&str> = source.split('\n').collect();
    if end_line > lines.len() {
        return Err(RefactorError::Rejected(format!(
            "selection lines {start_line}-{end_line} out of range (file has {} lines)",
            lines.len()
        )));
    }

    // Byte range of the selection: [sel_start, sel_end).
    let line_starts = line_start_offsets(source);
    let sel_start = line_starts[start_line - 1];
    let sel_end = if end_line < line_starts.len() {
        line_starts[end_line]
    } else {
        source.len()
    };

    // Content-trimmed boundaries: node ranges start at the first token (after
    // indentation), so the gate checks below must compare against the selection's
    // first/last *non-whitespace* byte, not the raw line-start offsets. Otherwise
    // the enclosing block always looks like it "straddles" the selection.
    let (content_start, content_end) = content_bounds(source, sel_start, sel_end)
        .ok_or_else(|| RefactorError::Rejected("empty selection".into()))?;

    // 1. innermost enclosing function that fully contains the selection.
    let root = idx.tree.root_node();
    let fn_node = enclosing_function(root, lang, content_start, content_end).ok_or_else(|| {
        RefactorError::Rejected(
            "selection is not inside a function body (extract-method is function-scoped)".into(),
        )
    })?;
    let (fn_start, fn_end) = (fn_node.start_byte(), fn_node.end_byte());

    // §3.4 gate: complex control flow inside the selection changes semantics.
    // Checked before straddle so a fully-selected `if { return }` is reported as
    // a control-flow issue rather than a boundary split.
    if let Some(kind) = control_flow_exit(root, lang, content_start, content_end) {
        return Err(RefactorError::Rejected(format!(
            "selection contains `{kind}` — extracting would change control flow (rejected)"
        )));
    }

    // §3.4 gate: selection must be whole statements (no straddled syntax node).
    if let Some(kind) = straddled_node(root, content_start, content_end) {
        return Err(RefactorError::Rejected(format!(
            "selection boundary splits a `{kind}` node — select whole statements only"
        )));
    }

    // 2. Build the variable sets (§3.1).
    let in_sel = |o: &Occurrence| sel_start <= o.start_byte && o.start_byte < sel_end;
    let after_sel = |o: &Occurrence| o.start_byte >= sel_end && o.start_byte < fn_end;
    let before_sel_in_fn = |o: &Occurrence| o.start_byte >= fn_start && o.start_byte < sel_start;

    let def_in_names: BTreeSet<String> = idx
        .definitions
        .iter()
        .filter(|d| in_sel(d))
        .map(|d| d.name.clone())
        .collect();

    // §3.2 params = { v in ref_in | binding def is *before* S in the function }.
    let mut params: Vec<String> = Vec::new();
    let mut seen_params: BTreeSet<String> = BTreeSet::new();
    for r in idx.references.iter().filter(|r| in_sel(r)) {
        if def_in_names.contains(&r.name) {
            continue;
        }
        let Some(def) = idx.binding_def(r) else {
            continue;
        };
        if before_sel_in_fn(def) && seen_params.insert(r.name.clone()) {
            params.push(r.name.clone());
        }
    }

    // §3.3 returns = { v in def_in | v referenced after S }.
    let ref_after_names: BTreeSet<String> = idx
        .references
        .iter()
        .filter(|r| after_sel(r))
        .map(|r| r.name.clone())
        .collect();
    let mut returns: Vec<String> = def_in_names
        .iter()
        .filter(|n| ref_after_names.contains(*n))
        .cloned()
        .collect();
    returns.sort();

    // 3. Legality gate: new name must not clash with an existing local (§3.4).
    let mut warnings: Vec<String> = Vec::new();
    let name_clash = idx
        .definitions
        .iter()
        .any(|d| d.name == new_name && d.start_byte >= fn_start && d.start_byte < fn_end);
    if name_clash {
        warnings.push(format!(
            "`{new_name}` already names a local in the enclosing function — pick another name"
        ));
    }

    // 4. Type annotations (§5/§7), two-level heuristic. py/js never need them.
    // For go/rust/ts: level 1 reads each variable's declared type from its
    // binding declaration (nearest-declaration, pure tree-sitter); level 2, when
    // that declaration is untyped but initialized by a call `f(...)`, reads the
    // callee's return type from the repo ctags table (`symbols`). Neither
    // resolves → a `TODO_TYPE` placeholder (we never guess, §7). Fully resolved
    // keeps High; any placeholder downgrades to Medium.
    let needs_types = matches!(lang, Lang::Go | Lang::Rust | Lang::TypeScript);
    let has_sig = !params.is_empty() || !returns.is_empty();
    let resolve = |d: &Occurrence| declared_type(&idx, d).or_else(|| ctags_type(&idx, d, symbols));
    // `mut` is only exercised by the optional `ts-morph` fill below.
    #[cfg_attr(not(feature = "ts-morph"), allow(unused_mut))]
    let mut param_tys: Vec<String> = if needs_types {
        params
            .iter()
            .map(|n| {
                idx.definitions
                    .iter()
                    .find(|d| d.name == *n && before_sel_in_fn(d))
                    .and_then(resolve)
                    .unwrap_or_else(|| TODO_TYPE.to_string())
            })
            .collect()
    } else {
        Vec::new()
    };
    #[cfg_attr(not(feature = "ts-morph"), allow(unused_mut))]
    let mut return_tys: Vec<String> = if needs_types {
        returns
            .iter()
            .map(|n| {
                idx.definitions
                    .iter()
                    .find(|d| d.name == *n && in_sel(d))
                    .and_then(resolve)
                    .unwrap_or_else(|| TODO_TYPE.to_string())
            })
            .collect()
    } else {
        Vec::new()
    };
    // §5 batch 3: TypeScript is the only language that gains from a type-level
    // enhancement. When locals + ctags left a real `TODO_TYPE` and the optional
    // `ts-morph` feature is compiled in, ask ts-morph (once, non-daemon) to type
    // the still-unresolved locals. Genuine placeholders only; the bridge discards
    // non-informative inferences (§7), so anything it can't type stays TODO_TYPE.
    #[cfg(feature = "ts-morph")]
    if matches!(lang, Lang::TypeScript) {
        fill_ts_types(
            &params,
            &mut param_tys,
            &returns,
            &mut return_tys,
            source,
            path,
        );
    }

    let any_placeholder = param_tys
        .iter()
        .chain(return_tys.iter())
        .any(|t| t == TODO_TYPE);
    if needs_types && has_sig && any_placeholder {
        warnings.push(format!(
            "{lang:?}: could not infer some types from nearby declarations; \
             emitted `{TODO_TYPE}` placeholders — confirm before applying"
        ));
    }
    // Rust: a reference/mutable-reference parameter carries borrow semantics the
    // locals layer cannot verify across the new call boundary (§5). Keep it
    // honest — downgrade and flag rather than claim the extraction is safe.
    let rust_borrow = matches!(lang, Lang::Rust) && param_tys.iter().any(|t| t.starts_with('&'));
    if rust_borrow {
        warnings.push(
            "rust: a parameter is a reference — verify borrow/lifetime semantics \
             across the new function boundary"
                .into(),
        );
    }
    if matches!(lang, Lang::Rust) && returns.len() > 1 {
        warnings.push("rust: multiple return values emitted as a tuple — consider a struct".into());
    }

    // 5. Render the new function + call site.
    let sel_lines: Vec<&str> = lines[start_line - 1..end_line].to_vec();
    let fn_header_line = lines[fn_node.start_position().row];
    let fn_indent = leading_ws(fn_header_line);
    let sel_indent = leading_ws(sel_lines.first().copied().unwrap_or(""));
    let step = indent_step(lang);
    let body_indent = format!("{fn_indent}{step}");

    let body = reindent_block(&sel_lines, &body_indent);
    let new_fn = render_function(
        lang,
        new_name,
        &params,
        &param_tys,
        &returns,
        &return_tys,
        &fn_indent,
        &body_indent,
        &body,
    );
    let call_stmt = render_call(lang, new_name, &params, &returns, &sel_indent);

    // 6. EditPlan: (a) insert new function above the enclosing function header,
    // (b) replace the selection block with the call statement.
    let insert_search = fn_header_line.to_string();
    let insert_replace = format!("{new_fn}\n{fn_header_line}");
    let sel_block = sel_lines.join("\n");

    // Op order matters for sequential apply: replace the selection FIRST (its
    // search text is unique in the original file), THEN insert the new function.
    // If we inserted first, the new function body would duplicate the selection
    // text and the call-site search could match the wrong (inserted) copy.
    let ops = vec![
        search_replace(path, sel_block, call_stmt.clone(), start_line),
        search_replace(
            path,
            insert_search,
            insert_replace,
            fn_node.start_position().row + 1,
        ),
    ];

    // 7. Confidence (§7): Low on name clash; Medium when any needed type is an
    // unresolved placeholder or a Rust borrow we can't verify; otherwise High
    // (py/js need no types; go/rust/ts had every needed type resolved from a
    // nearby declaration).
    let confidence = if name_clash {
        Confidence::Low
    } else if needs_types && has_sig && (any_placeholder || rust_borrow) {
        Confidence::Medium
    } else {
        Confidence::High
    };

    let param_info: Vec<ParamInfo> = params
        .iter()
        .enumerate()
        .map(|(i, n)| ParamInfo {
            name: n.clone(),
            ty: param_tys.get(i).cloned(),
            written: def_in_names.contains(n),
        })
        .collect();
    let return_info: Vec<ReturnInfo> = returns
        .iter()
        .enumerate()
        .map(|(i, n)| ReturnInfo {
            name: n.clone(),
            ty: return_tys.get(i).cloned(),
        })
        .collect();

    let signature = Some(signature_line(lang, new_name, &param_info, &return_info));

    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::ExtractMethod,
            plan: plan_from_ops(
                RefactorKind::ExtractMethod,
                path,
                ops,
                &format!("extract-method {new_name}"),
            ),
            signature,
            params: param_info,
            returns: return_info,
            confidence,
            warnings,
            sites: vec![
                format!(
                    "{path}:{} (insert {new_name})",
                    fn_node.start_position().row + 1
                ),
                format!("{path}:{start_line}-{end_line} (call)"),
            ],
        },
        path,
        source,
    ))
}

// --------------------------------------------------------------------------
// tree-sitter helpers
// --------------------------------------------------------------------------

/// First/last non-whitespace byte within `[raw_start, raw_end)`, as an
/// exclusive-end range. `None` if the range is entirely whitespace.
fn content_bounds(source: &str, raw_start: usize, raw_end: usize) -> Option<(usize, usize)> {
    let slice = &source.as_bytes()[raw_start..raw_end];
    let first = slice.iter().position(|b| !b.is_ascii_whitespace())?;
    let last = slice.iter().rposition(|b| !b.is_ascii_whitespace())?;
    Some((raw_start + first, raw_start + last + 1))
}

/// Byte offset at the start of each line (line 0 = byte 0).
pub(crate) fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offs = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            offs.push(i + 1);
        }
    }
    offs
}

/// The smallest function node (per `lang.function_kinds`) whose byte range
/// contains `[start, end)`.
pub(crate) fn enclosing_function(root: Node, lang: Lang, start: usize, end: usize) -> Option<Node> {
    let kinds = lang.function_kinds();
    let mut best: Option<Node> = None;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.start_byte() <= start && end <= n.end_byte() {
            if kinds.contains(&n.kind()) {
                match best {
                    Some(b)
                        if (b.end_byte() - b.start_byte()) <= (n.end_byte() - n.start_byte()) => {}
                    _ => best = Some(n),
                }
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                stack.push(child);
            }
        }
    }
    best
}

/// If a *named* node straddles either selection boundary (starts outside and
/// ends inside, or vice versa), return its kind. Container nodes that fully
/// enclose the selection are fine.
fn straddled_node(root: Node, start: usize, end: usize) -> Option<String> {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let (ns, ne) = (n.start_byte(), n.end_byte());
        if ne <= start || ns >= end {
            continue;
        }
        let container = ns <= start && ne >= end;
        if !container && n.is_named() {
            let cuts_start = ns < start && start < ne && ne <= end;
            let cuts_end = start <= ns && ns < end && end < ne;
            if cuts_start || cuts_end {
                return Some(n.kind().to_string());
            }
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    None
}

/// Return the kind of the first control-flow exit found strictly inside the
/// selection (return / break / continue / goto / labeled / yield).
fn control_flow_exit(root: Node, lang: Lang, start: usize, end: usize) -> Option<String> {
    let exits: &[&str] = match lang {
        Lang::Python => &[
            "return_statement",
            "break_statement",
            "continue_statement",
            "yield",
        ],
        Lang::JavaScript | Lang::TypeScript => &[
            "return_statement",
            "break_statement",
            "continue_statement",
            "yield_expression",
        ],
        Lang::Go => &[
            "return_statement",
            "break_statement",
            "continue_statement",
            "goto_statement",
            "labeled_statement",
        ],
        Lang::Rust => &[
            "return_expression",
            "break_expression",
            "continue_expression",
        ],
    };
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let (ns, ne) = (n.start_byte(), n.end_byte());
        if ne <= start || ns >= end {
            continue;
        }
        if ns >= start && ne <= end && exits.contains(&n.kind()) {
            return Some(n.kind().to_string());
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    None
}

// --------------------------------------------------------------------------
// rendering
// --------------------------------------------------------------------------

pub(crate) fn leading_ws(line: &str) -> String {
    line.chars().take_while(|c| c.is_whitespace()).collect()
}

pub(crate) fn indent_step(lang: Lang) -> &'static str {
    match lang {
        Lang::Go => "\t",
        _ => "    ",
    }
}

/// Strip the common leading indent of `lines` and re-prefix each with
/// `new_indent`; blank lines stay blank.
pub(crate) fn reindent_block(lines: &[&str], new_indent: &str) -> String {
    let common = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_ws(l).len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{new_indent}{}", &l[common.min(l.len())..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Nearest-declaration type heuristic (§5/§7): read the declared type token from
/// the binding declaration of `def`. Pure tree-sitter, function-local, zero extra
/// process. Returns `None` when the declaration carries no explicit type (Go `:=`,
/// untyped Rust/TS `let`/`const`) — the caller then emits a `TODO_TYPE`
/// placeholder rather than guessing (§7 「绝不瞎猜」).
fn declared_type(idx: &LocalsIndex, def: &Occurrence) -> Option<String> {
    let decl_kinds: &[&str] = match idx.lang {
        Lang::Go => &["parameter_declaration", "var_spec", "const_spec"],
        Lang::Rust => &["parameter", "let_declaration"],
        Lang::TypeScript => &[
            "required_parameter",
            "optional_parameter",
            "variable_declarator",
        ],
        _ => return None,
    };
    let root = idx.tree.root_node();
    let mut node = root.descendant_for_byte_range(def.start_byte, def.end_byte);
    while let Some(n) = node {
        if decl_kinds.contains(&n.kind()) {
            // Found the governing declaration. No `type` field means the author
            // did not annotate it → placeholder, never a guess.
            let ty = n.child_by_field_name("type")?;
            let raw = ty.utf8_text(idx.source.as_bytes()).unwrap_or("").trim();
            // TS exposes the annotation as `: T`; strip the leading colon.
            let raw = raw.trim_start_matches(':').trim();
            return if raw.is_empty() {
                None
            } else {
                Some(raw.to_string())
            };
        }
        node = n.parent();
    }
    None
}

/// Level-2 type fallback (§5/§7): when the nearest declaration carries no
/// explicit type, inspect its initializer. If it is a direct call `f(...)`, look
/// up `f`'s definition in the ctags [`SymbolTable`] and read the return type from
/// its signature. Cross-declaration and cross-file, still zero model tokens.
/// Returns `None` (→ keep the `TODO_TYPE` placeholder) whenever there is no table,
/// the initializer is not a plain call, the callee is unknown, or its return type
/// cannot be parsed unambiguously — we never guess (§7 「绝不瞎猜」).
fn ctags_type(
    idx: &LocalsIndex,
    def: &Occurrence,
    symbols: Option<&SymbolTable>,
) -> Option<String> {
    let symbols = symbols?;
    let decl_kinds: &[&str] = match idx.lang {
        Lang::Go => &["short_var_declaration", "var_spec", "const_spec"],
        Lang::Rust => &["let_declaration"],
        Lang::TypeScript => &["variable_declarator"],
        _ => return None,
    };
    let root = idx.tree.root_node();
    let mut node = root.descendant_for_byte_range(def.start_byte, def.end_byte);
    let mut decl = None;
    while let Some(n) = node {
        if decl_kinds.contains(&n.kind()) {
            decl = Some(n);
            break;
        }
        node = n.parent();
    }
    let decl = decl?;
    // Go `:=` exposes the initializer as `right` (an expression_list); Go `var`
    // and Rust/TS expose it as `value`.
    let init = decl
        .child_by_field_name("right")
        .or_else(|| decl.child_by_field_name("value"))?;
    let callee = call_callee_name(init, idx)?;
    let sym = symbols
        .defs(&callee)
        .into_iter()
        .find(|s| s.signature.is_some())?;
    return_type_from_signature(idx.lang, sym.signature.as_deref()?)
}

/// Name of the function called by `node`, but only for the unambiguous case: a
/// bare-identifier callee (`f(...)`). Method/associated calls (`x.f()`, `T::f()`)
/// resolve to types locals cannot verify, so we decline them (§7).
fn call_callee_name(node: Node, idx: &LocalsIndex) -> Option<String> {
    let node = if node.kind() == "expression_list" {
        node.named_child(0)?
    } else {
        node
    };
    if node.kind() != "call_expression" {
        return None;
    }
    let func = node.child_by_field_name("function")?;
    let name = func.utf8_text(idx.source.as_bytes()).ok()?;
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// Parse a single, unambiguous return type out of a ctags signature line. Only
/// the clean single-return forms are accepted; void, multi-return tuples, and
/// anything we cannot parse cleanly return `None` so the caller keeps the
/// placeholder rather than emit a wrong type (§7).
fn return_type_from_signature(lang: Lang, sig: &str) -> Option<String> {
    let sig = sig.trim();
    match lang {
        Lang::Go => {
            // `func name(params) RET {` — RET sits between the param `)` and `{`.
            let close = sig.rfind(')')?;
            let after = sig[close + 1..].trim().trim_end_matches('{').trim();
            // empty ⇒ void; leading `(` ⇒ multi-return tuple → decline.
            if after.is_empty() || after.starts_with('(') {
                return None;
            }
            Some(after.to_string())
        }
        Lang::Rust => {
            // `fn name(params) -> RET {`
            let arrow = sig.find("->")?;
            let after = sig[arrow + 2..].split('{').next()?.trim();
            let after = after
                .split_whitespace()
                .take_while(|w| *w != "where")
                .collect::<Vec<_>>()
                .join(" ");
            let after = after.trim();
            if after.is_empty() {
                return None;
            }
            Some(after.to_string())
        }
        Lang::TypeScript => {
            // `function name(params): RET {`
            let close = sig.rfind(')')?;
            let rest = &sig[close + 1..];
            let colon = rest.find(':')?;
            let after = rest[colon + 1..].split('{').next()?.trim();
            if after.is_empty() {
                return None;
            }
            Some(after.to_string())
        }
        _ => None,
    }
}

/// Render the parameter list. `param_tys` is empty for languages that need no
/// annotations (py/js); otherwise it is aligned 1:1 with `params`, each entry a
/// resolved type or the `TODO_TYPE` placeholder.
fn typed_params(lang: Lang, params: &[String], param_tys: &[String]) -> String {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if param_tys.is_empty() {
                return p.clone();
            }
            let t = &param_tys[i];
            match lang {
                Lang::Go => format!("{p} {t}"),
                Lang::Rust | Lang::TypeScript => format!("{p}: {t}"),
                _ => p.clone(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(clippy::too_many_arguments)]
fn render_function(
    lang: Lang,
    name: &str,
    params: &[String],
    param_tys: &[String],
    returns: &[String],
    return_tys: &[String],
    fn_indent: &str,
    body_indent: &str,
    body: &str,
) -> String {
    let ps = typed_params(lang, params, param_tys);
    match lang {
        Lang::Python => {
            let ret = python_return(returns);
            let tail = if ret.is_empty() {
                String::new()
            } else {
                format!("\n{body_indent}{ret}")
            };
            format!("{fn_indent}def {name}({ps}):\n{body}{tail}\n")
        }
        Lang::JavaScript | Lang::TypeScript => {
            let ret_ty = ts_return_type(return_tys);
            let ret = js_return(returns);
            let tail = if ret.is_empty() {
                String::new()
            } else {
                format!("\n{body_indent}{ret}")
            };
            format!("{fn_indent}function {name}({ps}){ret_ty} {{\n{body}{tail}\n{fn_indent}}}")
        }
        Lang::Go => {
            let ret_ty = go_return_types(return_tys);
            let ret = go_return(returns);
            let tail = if ret.is_empty() {
                String::new()
            } else {
                format!("\n{body_indent}{ret}")
            };
            format!("{fn_indent}func {name}({ps}){ret_ty} {{\n{body}{tail}\n{fn_indent}}}")
        }
        Lang::Rust => {
            let ret_ty = rust_return_type(return_tys);
            let tail = rust_tail(returns);
            let tail = if tail.is_empty() {
                String::new()
            } else {
                format!("\n{body_indent}{tail}")
            };
            format!("{fn_indent}fn {name}({ps}){ret_ty} {{\n{body}{tail}\n{fn_indent}}}")
        }
    }
}

fn render_call(
    lang: Lang,
    name: &str,
    params: &[String],
    returns: &[String],
    indent: &str,
) -> String {
    let args = params.join(", ");
    match lang {
        Lang::Python => match returns.len() {
            0 => format!("{indent}{name}({args})"),
            1 => format!("{indent}{} = {name}({args})", returns[0]),
            _ => format!("{indent}{} = {name}({args})", returns.join(", ")),
        },
        Lang::JavaScript | Lang::TypeScript => match returns.len() {
            0 => format!("{indent}{name}({args});"),
            1 => format!("{indent}const {} = {name}({args});", returns[0]),
            _ => format!("{indent}const [{}] = {name}({args});", returns.join(", ")),
        },
        Lang::Go => match returns.len() {
            0 => format!("{indent}{name}({args})"),
            _ => format!("{indent}{} := {name}({args})", returns.join(", ")),
        },
        Lang::Rust => match returns.len() {
            0 => format!("{indent}{name}({args});"),
            1 => format!("{indent}let {} = {name}({args});", returns[0]),
            _ => format!("{indent}let ({}) = {name}({args});", returns.join(", ")),
        },
    }
}

fn python_return(returns: &[String]) -> String {
    match returns.len() {
        0 => String::new(),
        1 => format!("return {}", returns[0]),
        _ => format!("return {}", returns.join(", ")),
    }
}

fn js_return(returns: &[String]) -> String {
    match returns.len() {
        0 => String::new(),
        1 => format!("return {};", returns[0]),
        _ => format!("return [{}];", returns.join(", ")),
    }
}

fn go_return(returns: &[String]) -> String {
    match returns.len() {
        0 => String::new(),
        _ => format!("return {}", returns.join(", ")),
    }
}

fn go_return_types(return_tys: &[String]) -> String {
    match return_tys.len() {
        0 => String::new(),
        1 => format!(" {}", return_tys[0]),
        _ => format!(" ({})", return_tys.join(", ")),
    }
}

fn rust_return_type(return_tys: &[String]) -> String {
    match return_tys.len() {
        0 => String::new(),
        1 => format!(" -> {}", return_tys[0]),
        _ => format!(" -> ({})", return_tys.join(", ")),
    }
}

/// (feature `ts-morph`) Fill still-unresolved TS parameter/return types by asking
/// the one-shot [`crate::ts_morph`] bridge to type the placeholder locals against
/// the whole source. Only slots that are still `TODO_TYPE` are eligible, and only
/// names the bridge could concretely type get replaced — everything else keeps
/// its placeholder so confidence still downgrades honestly (§7). Best-effort: any
/// bridge failure (no node, no ts-morph) is a silent no-op.
#[cfg(feature = "ts-morph")]
fn fill_ts_types(
    params: &[String],
    param_tys: &mut [String],
    returns: &[String],
    return_tys: &mut [String],
    source: &str,
    path: &str,
) {
    use std::collections::BTreeSet;

    // Collect the names whose annotation is still a placeholder.
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for (n, t) in params.iter().zip(param_tys.iter()) {
        if t == TODO_TYPE {
            wanted.insert(n.clone());
        }
    }
    for (n, t) in returns.iter().zip(return_tys.iter()) {
        if t == TODO_TYPE {
            wanted.insert(n.clone());
        }
    }
    if wanted.is_empty() {
        return;
    }

    let names: Vec<String> = wanted.into_iter().collect();
    let resolved = match crate::ts_morph::infer_types(source, path, &names) {
        crate::ts_morph::Bridge::Resolved(m) => m,
        // No node / no ts-morph / bridge error: keep placeholders untouched.
        _ => return,
    };
    if resolved.is_empty() {
        return;
    }

    for (n, t) in params.iter().zip(param_tys.iter_mut()) {
        if t == TODO_TYPE {
            if let Some(ty) = resolved.get(n) {
                *t = ty.clone();
            }
        }
    }
    for (n, t) in returns.iter().zip(return_tys.iter_mut()) {
        if t == TODO_TYPE {
            if let Some(ty) = resolved.get(n) {
                *t = ty.clone();
            }
        }
    }
}

/// TypeScript return annotation from resolved types (empty for JS, which has
/// none). A single return is `: T`; multiple map to the array destructuring the
/// call site uses, so the annotation is a tuple `: [T1, T2]`.
fn ts_return_type(return_tys: &[String]) -> String {
    match return_tys.len() {
        0 => String::new(),
        1 => format!(": {}", return_tys[0]),
        _ => format!(": [{}]", return_tys.join(", ")),
    }
}

fn rust_tail(returns: &[String]) -> String {
    match returns.len() {
        0 => String::new(),
        1 => returns[0].clone(),
        _ => format!("({})", returns.join(", ")),
    }
}

fn signature_line(lang: Lang, name: &str, params: &[ParamInfo], returns: &[ReturnInfo]) -> String {
    let ps = params
        .iter()
        .map(|p| match &p.ty {
            Some(t) => format!("{}: {t}", p.name),
            None => p.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let rs = returns
        .iter()
        .map(|r| r.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    match lang {
        Lang::Python => format!("def {name}({ps}) -> ({rs})"),
        Lang::Go => format!("func {name}({ps}) ({rs})"),
        Lang::Rust => format!("fn {name}({ps}) -> ({rs})"),
        _ => format!("function {name}({ps}) => ({rs})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_symbols::{Symbol, SymbolKind, SymbolTable};

    // Build an in-memory ctags-style symbol table (fields are all pub, so tests
    // never need to spawn ctags). Each entry is (name, raw signature line).
    fn sym_table(defs: &[(&str, &str)]) -> SymbolTable {
        SymbolTable {
            symbols: defs
                .iter()
                .map(|(name, sig)| Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    scope: None,
                    path: "repo.rs".to_string(),
                    line: 1,
                    signature: Some(sig.to_string()),
                })
                .collect(),
        }
    }

    fn op_replace(plan: &RefactorPlan, idx: usize) -> (&str, &str) {
        match &plan.plan.ops[idx] {
            shuttle_protocol::EditOp::SearchReplace {
                search, replace, ..
            } => (search, replace),
            _ => panic!("expected SearchReplace"),
        }
    }

    // Python: params derived from a var defined before the selection; a return
    // derived from a var defined inside and used after.
    #[test]
    fn python_params_and_return() {
        // def g(a):
        //     b = a + 1          <- line 2 (before selection)
        //     c = b * 2          <- line 3 (SELECTION: defines c, reads b)
        //     return c           <- line 4 (uses c after)
        let src = "def g(a):\n    b = a + 1\n    c = b * 2\n    return c\n";
        let plan = extract_method("m.py", src, 3, 3, "compute").unwrap();
        assert_eq!(plan.op, RefactorKind::ExtractMethod);
        assert!(matches!(plan.confidence, Confidence::High));
        // param = b (defined before, read inside)
        assert_eq!(
            plan.params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        // return = c (defined inside, used after)
        assert_eq!(
            plan.returns
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(new_fn.contains("def compute(b):"), "fn: {new_fn}");
        assert!(new_fn.contains("return c"), "fn: {new_fn}");
        let (_, call) = op_replace(&plan, 0);
        assert!(call.contains("c = compute(b)"), "call: {call}");
    }

    // Selection with no outputs used later → void extraction, no return.
    #[test]
    fn python_void_extraction() {
        let src = "def g(a):\n    print(a)\n    print(a + 1)\n    return a\n";
        let plan = extract_method("m.py", src, 2, 3, "show").unwrap();
        assert!(plan.returns.is_empty(), "returns: {:?}", plan.returns);
        assert_eq!(
            plan.params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        let (_, call) = op_replace(&plan, 0);
        assert!(call.trim_start().starts_with("show(a)"), "call: {call}");
    }

    // §3.4 gate: a mid-selection `return` is rejected (control-flow exit).
    #[test]
    fn reject_control_flow_return() {
        let src = "def g(a):\n    if a:\n        return 1\n    return 2\n";
        let err = extract_method("m.py", src, 3, 3, "x").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("control flow"), "{m}"),
            e => panic!("expected Rejected, got {e:?}"),
        }
    }

    // §3.4 gate: name clash with an existing local downgrades to Low + warning.
    #[test]
    fn name_clash_is_low() {
        // `compute` is already a local (param), so extracting as `compute` clashes.
        let src = "def g(compute):\n    b = compute + 1\n    return b\n";
        let plan = extract_method("m.py", src, 2, 2, "compute").unwrap();
        assert!(matches!(plan.confidence, Confidence::Low));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("already names a local")));
    }

    // JavaScript: const-declared return, object/array destructuring for multi.
    #[test]
    fn javascript_single_return() {
        let src = "function g(a) {\n  let b = a + 1;\n  let c = b * 2;\n  return c;\n}\n";
        let plan = extract_method("m.js", src, 3, 3, "compute").unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(new_fn.contains("function compute(b) {"), "fn: {new_fn}");
        assert!(new_fn.contains("return c;"), "fn: {new_fn}");
        let (_, call) = op_replace(&plan, 0);
        assert!(call.contains("const c = compute(b);"), "call: {call}");
    }

    // Go: typed placeholders + Medium confidence + `:=` call.
    #[test]
    fn go_typed_placeholder_is_medium() {
        let src = "func g(a int) int {\n\tb := a + 1\n\tc := b * 2\n\treturn c\n}\n";
        let plan = extract_method("m.go", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        assert!(plan.warnings.iter().any(|w| w.contains("TODO_TYPE")));
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) TODO_TYPE {"),
            "fn: {new_fn}"
        );
        let (_, call) = op_replace(&plan, 0);
        assert!(call.contains("c := compute(b)"), "call: {call}");
    }

    // Rust: typed placeholders, tail-expression return, `let` binding at call.
    #[test]
    fn rust_typed_placeholder() {
        let src = "fn g(a: i32) -> i32 {\n    let b = a + 1;\n    let c = b * 2;\n    c\n}\n";
        let plan = extract_method("m.rs", src, 3, 3, "compute").unwrap();
        assert!(matches!(plan.confidence, Confidence::Medium));
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("fn compute(b: TODO_TYPE) -> TODO_TYPE {"),
            "fn: {new_fn}"
        );
        // tail expression is the returned var
        assert!(
            new_fn.trim_end().ends_with("c\n}") || new_fn.contains("\n    c\n"),
            "fn: {new_fn}"
        );
        let (_, call) = op_replace(&plan, 0);
        assert!(call.contains("let c = compute(b);"), "call: {call}");
    }

    // TypeScript: typed params + return annotation placeholder. This is the
    // PURE tree-sitter path; when the optional `ts-morph` feature is compiled in
    // the placeholder is instead filled (see `typescript_tsmorph_fills_placeholder`).
    #[cfg(not(feature = "ts-morph"))]
    #[test]
    fn typescript_typed_placeholder() {
        let src = "function g(a: number): number {\n  const b = a + 1;\n  const c = b * 2;\n  return c;\n}\n";
        let plan = extract_method("m.ts", src, 3, 3, "compute").unwrap();
        assert!(matches!(plan.confidence, Confidence::Medium));
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("function compute(b: TODO_TYPE): TODO_TYPE {"),
            "fn: {new_fn}"
        );
    }

    // (feature `ts-morph`) The SAME source that leaves a `TODO_TYPE` placeholder
    // under the pure tree-sitter path is resolved to a real type when the
    // optional ts-morph bridge is compiled in AND installed: `b`/`c` are
    // number-typed by the TS compiler, so the annotation fills and confidence
    // climbs back to High. Skips (does not fail) when ts-morph is not installed —
    // the daemonless build must never depend on node being present.
    #[cfg(feature = "ts-morph")]
    #[test]
    fn typescript_tsmorph_fills_placeholder() {
        // Probe: is a usable ts-morph reachable? If not, this environment can't
        // exercise the bridge — skip rather than fail.
        let probe = crate::ts_morph::infer_types("const q = 1 + 2;", "p.ts", &["q".to_string()]);
        let available =
            matches!(&probe, crate::ts_morph::Bridge::Resolved(m) if m.contains_key("q"));
        if !available {
            return;
        }

        let src = "function g(a: number): number {\n  const b = a + 1;\n  const c = b * 2;\n  return c;\n}\n";
        let plan = extract_method("m.ts", src, 3, 3, "compute").unwrap();
        // ts-morph typed both b and c -> no placeholder -> High.
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        assert!(
            !plan.warnings.iter().any(|w| w.contains("TODO_TYPE")),
            "warns: {:?}",
            plan.warnings
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("function compute(b: number): number {"),
            "fn: {new_fn}"
        );
    }

    // Go: an explicitly typed declaration resolves to a real type -> High.
    #[test]
    fn go_typed_resolves_to_high() {
        let src = "func g(a int) int {\n\tvar b int = a + 1\n\tvar c int = b * 2\n\treturn c\n}\n";
        let plan = extract_method("m.go", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        assert!(
            !plan.warnings.iter().any(|w| w.contains("TODO_TYPE")),
            "warns: {:?}",
            plan.warnings
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(new_fn.contains("func compute(b int) int {"), "fn: {new_fn}");
    }

    // Rust: explicit `let b: i32` annotations resolve -> High, no placeholder.
    #[test]
    fn rust_typed_resolves_to_high() {
        let src =
            "fn g(a: i32) -> i32 {\n    let b: i32 = a + 1;\n    let c: i32 = b * 2;\n    c\n}\n";
        let plan = extract_method("m.rs", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("fn compute(b: i32) -> i32 {"),
            "fn: {new_fn}"
        );
    }

    // Rust: a reference-typed parameter downgrades to Medium with a borrow warning.
    #[test]
    fn rust_reference_param_is_medium() {
        let src = "fn g(a: &str) -> usize {\n    let b: &str = a;\n    let c: usize = b.len();\n    c\n}\n";
        let plan = extract_method("m.rs", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("reference")),
            "warns: {:?}",
            plan.warnings
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("fn compute(b: &str) -> usize {"),
            "fn: {new_fn}"
        );
    }

    // TypeScript: explicit `const b: number` annotations resolve -> High.
    #[test]
    fn typescript_typed_resolves_to_high() {
        let src = "function g(a: number): number {\n  const b: number = a + 1;\n  const c: number = b * 2;\n  return c;\n}\n";
        let plan = extract_method("m.ts", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("function compute(b: number): number {"),
            "fn: {new_fn}"
        );
    }

    // Level 2 (§5): Go `b := makeThing()` with an untyped `:=` decl resolves via
    // the callee's ctags return type -> High, no placeholder.
    #[test]
    fn go_ctags_return_type_resolves_to_high() {
        // b and c are both `:=` (untyped); their initializers are calls whose
        // return types come from the repo symbol table.
        let src = "func g(a int) int {\n\tb := makeWidget()\n\tc := scale(b)\n\treturn c\n}\n";
        let syms = sym_table(&[
            ("makeWidget", "func makeWidget() Widget {"),
            ("scale", "func scale(w Widget) int {"),
        ]);
        let plan = extract_method_with_symbols("m.go", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        assert!(
            !plan.warnings.iter().any(|w| w.contains("TODO_TYPE")),
            "warns: {:?}",
            plan.warnings
        );
        let (_, new_fn) = op_replace(&plan, 1);
        // b is a Widget (makeWidget's return), c is an int (scale's return).
        assert!(
            new_fn.contains("func compute(b Widget) int {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: without a symbol table the same source keeps the placeholder
    // (level 1 alone) -> Medium. Proves the fallback is opt-in.
    #[test]
    fn go_ctags_absent_keeps_placeholder() {
        let src = "func g(a int) int {\n\tb := makeWidget()\n\tc := scale(b)\n\treturn c\n}\n";
        let plan = extract_method("m.go", src, 3, 3, "compute").unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) TODO_TYPE {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: an unknown callee (not in the table) keeps the placeholder even
    // when a table is supplied -> Medium (§7 never guess).
    #[test]
    fn go_ctags_unknown_callee_keeps_placeholder() {
        let src = "func g(a int) int {\n\tb := makeWidget()\n\tc := scale(b)\n\treturn c\n}\n";
        // Table has scale but NOT makeWidget -> b stays TODO_TYPE, c resolves.
        let syms = sym_table(&[("scale", "func scale(w Widget) int {")]);
        let plan = extract_method_with_symbols("m.go", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) int {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: a method call `x.f()` is declined (§7) -> placeholder kept.
    #[test]
    fn go_ctags_method_call_declined() {
        let src = "func g(a int) int {\n\tb := factory.make()\n\tc := scale(b)\n\treturn c\n}\n";
        let syms = sym_table(&[
            ("make", "func make() Widget {"),
            ("scale", "func scale(w Widget) int {"),
        ]);
        let plan = extract_method_with_symbols("m.go", src, 3, 3, "compute", Some(&syms)).unwrap();
        // b came from a method call -> declined -> placeholder -> Medium.
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) int {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: Rust `let b = make();` resolves via the callee's `-> RET` -> High.
    #[test]
    fn rust_ctags_return_type_resolves_to_high() {
        let src =
            "fn g(a: i32) -> i32 {\n    let b = make_widget();\n    let c = scale(b);\n    c\n}\n";
        let syms = sym_table(&[
            ("make_widget", "fn make_widget() -> Widget {"),
            ("scale", "fn scale(w: Widget) -> i32 {"),
        ]);
        let plan = extract_method_with_symbols("m.rs", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("fn compute(b: Widget) -> i32 {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: TypeScript `const b = make();` resolves via the callee's `: RET`.
    #[test]
    fn typescript_ctags_return_type_resolves_to_high() {
        let src = "function g(a: number): number {\n  const b = makeWidget();\n  const c = scale(b);\n  return c;\n}\n";
        let syms = sym_table(&[
            ("makeWidget", "function makeWidget(): Widget {"),
            ("scale", "function scale(w: Widget): number {"),
        ]);
        let plan = extract_method_with_symbols("m.ts", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::High),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("function compute(b: Widget): number {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: a void callee (`func f() {`) yields no type -> placeholder kept.
    #[test]
    fn go_ctags_void_callee_keeps_placeholder() {
        let src = "func g(a int) int {\n\tb := setup()\n\tc := scale(b)\n\treturn c\n}\n";
        let syms = sym_table(&[
            ("setup", "func setup() {"),
            ("scale", "func scale(w Widget) int {"),
        ]);
        let plan = extract_method_with_symbols("m.go", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) int {"),
            "fn: {new_fn}"
        );
    }

    // Level 2: a multi-return Go tuple (`func f() (A, B) {`) is declined -> kept.
    #[test]
    fn go_ctags_multi_return_declined() {
        let src = "func g(a int) int {\n\tb := makePair()\n\tc := scale(b)\n\treturn c\n}\n";
        let syms = sym_table(&[
            ("makePair", "func makePair() (Widget, error) {"),
            ("scale", "func scale(w Widget) int {"),
        ]);
        let plan = extract_method_with_symbols("m.go", src, 3, 3, "compute", Some(&syms)).unwrap();
        assert!(
            matches!(plan.confidence, Confidence::Medium),
            "conf: {:?}",
            plan.confidence
        );
        let (_, new_fn) = op_replace(&plan, 1);
        assert!(
            new_fn.contains("func compute(b TODO_TYPE) int {"),
            "fn: {new_fn}"
        );
    }

    // Selection outside any function is rejected.
    #[test]
    fn reject_outside_function() {
        let src = "x = 1\ny = 2\n";
        let err = extract_method("m.py", src, 1, 1, "f").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }
}
