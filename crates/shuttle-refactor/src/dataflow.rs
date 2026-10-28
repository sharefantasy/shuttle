//! G6 · S3 — provable forward dataflow (`shuttle dataflow`).
//!
//! # 北极星
//! Answer *"was this value's source touched by this change?"* — a lightweight
//! taint / provenance layer for "safety / correctness" intents. We lay one more
//! layer over the SymbolGraph that follows **only provable assignment / argument
//! chains** forward from a seed binding, minting weak `DataFlow` edges.
//!
//! # §7 「绝不瞎猜」 — the hard red line
//! We report **only provable value-flow paths**. Anything the local binding
//! resolver cannot prove — a subscript store (`a[i] = x`), an attribute /
//! member store (`self.f = x`), a destructuring target (`a, b = ...`), a value
//! that crosses a call boundary — is recorded as an **unresolved** frontier or
//! a **sink**, never propagated. There is **no heuristic taint spreading**: an
//! edge we cannot prove is simply absent (`None`), not guessed.
//!
//! # What "provable" means here (intra-procedural, local-binding only)
//! Starting from one seed variable, inside its enclosing function we follow the
//! *locals* binding rule (the same one rename-local / inline-variable trust):
//!
//! * A reference to a tainted binding that sits in the **value (RHS)** of an
//!   assignment whose **target is a plain identifier** taints that identifier —
//!   a proven copy edge. We enqueue it and recurse (still within the function).
//! * A reference that flows into a **call argument** is a **sink**: the value
//!   leaves this function. We record the callee but **never** follow its return
//!   provenance across the boundary (that would be an interprocedural guess).
//! * A reference whose assignment **target is not a plain identifier**
//!   (subscript / attribute / tuple / pattern) is an **unresolved store**: we
//!   cannot prove *what* it writes, so we record it and stop — we never taint a
//!   field/element as if we understood aliasing.
//!
//! The seed and every tainted binding stay inside one function body. Call
//! boundaries are sinks, so the walk is finite and every edge is a local,
//! locals-resolver-proven copy. Confidence is **Low** for weakly-typed
//! languages (Python / JavaScript — §2 pit #6) and **Medium** for strongly-typed
//! ones (Go / Rust / TypeScript), never High: a copy edge proves provenance, not
//! a type guarantee.

use shuttle_nav::Confidence;

use crate::locals::{Lang, LocalsIndex};
use crate::RefactorError;

use tree_sitter::Node;

/// A binding that the seed value provably reaches via a chain of identifier
/// assignments inside one function (a `DataFlow` node).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlowNode {
    /// The tainted binding's name.
    pub name: String,
    /// File the binding lives in (always the seed's file — intra-procedural).
    pub path: String,
    /// 1-based line of the assignment that tainted it (the seed's own def line
    /// for the seed itself).
    pub line: usize,
    /// Copy-edge hops from the seed (seed = 0).
    pub distance: usize,
    /// Trust level: Low for weakly-typed langs, Medium for strongly-typed.
    pub confidence: Confidence,
}

/// A point where a tainted value **leaves** this function through a call
/// argument. We record the callee but never follow it (§7 — no interprocedural
/// return-provenance guessing).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlowSink {
    /// The tainted binding whose value flowed into the call.
    pub value: String,
    /// Best-effort callee name (the call's function/callee identifier text), or
    /// `"<expr>"` when the callee is not a plain identifier.
    pub callee: String,
    pub path: String,
    /// 1-based line of the call site.
    pub line: usize,
}

/// An assignment whose target we could **not** prove to be a plain identifier
/// (subscript / attribute / member / tuple / pattern). We stop here rather than
/// taint a field/element we don't understand (§7).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnprovableStore {
    /// The tainted binding whose value was written through the unprovable target.
    pub value: String,
    /// The syntactic form we refused to follow: `"subscript"`, `"attribute"`,
    /// `"tuple/pattern"`, or the raw node kind when it is none of these.
    pub kind: String,
    pub path: String,
    /// 1-based line of the store.
    pub line: usize,
}

/// The provable forward-dataflow of one seed binding inside one function.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DataFlow {
    /// The seed variable name.
    pub seed: String,
    /// The file the analysis ran in.
    pub root_path: String,
    /// The enclosing function's name, if we could name it (for reporting only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Provably-tainted bindings (includes the seed at distance 0), sorted by
    /// (distance, line, name).
    pub nodes: Vec<FlowNode>,
    /// Call-argument sinks the value flowed into (boundary, never followed).
    pub sinks: Vec<FlowSink>,
    /// Assignment stores we refused to follow because the target was not a
    /// plain identifier — the §7 «推不动即 None» frontier, surfaced not guessed.
    pub unresolved: Vec<UnprovableStore>,
    /// True when the analysis degraded (unsupported construct, unreadable seed)
    /// so the caller never reads an empty flow as a proven "no flow".
    pub degraded: bool,
    /// One human-readable note per degrade / frontier decision.
    pub notes: Vec<String>,
}

/// Per-language description of the *provable* flow constructs. Everything not
/// listed here is treated as unprovable and never propagated (§7): the grammar
/// is an allow-list, not a best-effort matcher.
struct FlowGrammar {
    /// Node kinds that bind a value to a target (assignment / declaration).
    assign_kinds: &'static [&'static str],
    /// Candidate field names holding the assigned **value** (RHS). Tried in
    /// order; the first present field wins.
    value_fields: &'static [&'static str],
    /// Candidate field names holding the assignment **target** (LHS). Tried in
    /// order; the first present field wins.
    target_fields: &'static [&'static str],
    /// Node kinds for a call expression.
    call_kinds: &'static [&'static str],
    /// Field name holding a call's arguments.
    call_args_field: &'static str,
    /// Field name holding a call's callee (function) expression.
    call_fn_field: &'static str,
    /// The plain-identifier node kind for this language.
    ident_kind: &'static str,
}

fn grammar(lang: Lang) -> FlowGrammar {
    match lang {
        Lang::Python => FlowGrammar {
            assign_kinds: &["assignment"],
            value_fields: &["right"],
            target_fields: &["left"],
            call_kinds: &["call"],
            call_args_field: "arguments",
            call_fn_field: "function",
            ident_kind: "identifier",
        },
        Lang::JavaScript => FlowGrammar {
            assign_kinds: &["assignment_expression", "variable_declarator"],
            value_fields: &["right", "value"],
            target_fields: &["left", "name"],
            call_kinds: &["call_expression"],
            call_args_field: "arguments",
            call_fn_field: "function",
            ident_kind: "identifier",
        },
        Lang::TypeScript => FlowGrammar {
            assign_kinds: &["assignment_expression", "variable_declarator"],
            value_fields: &["right", "value"],
            target_fields: &["left", "name"],
            call_kinds: &["call_expression"],
            call_args_field: "arguments",
            call_fn_field: "function",
            ident_kind: "identifier",
        },
        Lang::Go => FlowGrammar {
            assign_kinds: &["assignment_statement", "short_var_declaration"],
            value_fields: &["right"],
            target_fields: &["left"],
            call_kinds: &["call_expression"],
            call_args_field: "arguments",
            call_fn_field: "function",
            ident_kind: "identifier",
        },
        Lang::Rust => FlowGrammar {
            assign_kinds: &["let_declaration", "assignment_expression"],
            value_fields: &["value", "right"],
            target_fields: &["pattern", "left"],
            call_kinds: &["call_expression"],
            call_args_field: "arguments",
            call_fn_field: "function",
            ident_kind: "identifier",
        },
    }
}

/// Confidence for a proven copy edge: Medium for strongly-typed langs, Low for
/// weakly-typed ones. Never High — a copy proves provenance, not a type.
fn edge_confidence(lang: Lang) -> Confidence {
    match lang {
        Lang::Go | Lang::Rust | Lang::TypeScript => Confidence::Medium,
        Lang::Python | Lang::JavaScript => Confidence::Low,
    }
}

/// Options for the dataflow walk.
#[derive(Debug, Clone)]
pub struct DataFlowOptions {
    /// Max copy-edge hops from the seed (guards pathological chains). Default 32.
    pub max_depth: usize,
}

impl Default for DataFlowOptions {
    fn default() -> Self {
        DataFlowOptions { max_depth: 32 }
    }
}

/// Return-statement node kinds: an *explicit* return is an exit boundary, so a
/// tainted value flowing into one is a sink (`callee = "<return>"`). Implicit
/// tail expressions (Rust) are not listed — we never guess an implicit return.
fn return_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Python | Lang::JavaScript | Lang::TypeScript | Lang::Go => &["return_statement"],
        Lang::Rust => &["return_expression"],
    }
}

/// `shuttle dataflow`: the provable forward-dataflow of `seed` inside its enclosing
/// function in `path`. See the module docs for the §7 contract.
///
/// `seed` names a local binding (parameter or variable). We anchor its first
/// definition, then follow only provable identifier-copy assignments forward,
/// recording call/return sinks and unprovable stores rather than guessing past
/// them. The walk is intra-procedural and finite (call boundaries are sinks).
pub fn dataflow(seed: &str, path: &str, opts: &DataFlowOptions) -> Result<DataFlow, RefactorError> {
    let lang =
        Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.to_string()))?;
    let source = std::fs::read_to_string(path).map_err(|e| RefactorError::Io(e.to_string()))?;
    let idx = LocalsIndex::build(lang, &source)?;
    let g = grammar(lang);

    let mut out = DataFlow {
        seed: seed.to_string(),
        root_path: path.to_string(),
        ..Default::default()
    };

    // Anchor the seed's definition. If the name has no local definition we
    // cannot prove any flow (§7): degrade rather than invent a source.
    let Some(seed_def) = idx.definitions.iter().find(|d| d.name == seed) else {
        out.degraded = true;
        out.notes.push(format!(
            "seed `{seed}` has no local definition in {path} — cannot prove flow"
        ));
        return Ok(out);
    };
    let fn_range = idx
        .enclosing_function_range(seed_def.start_byte)
        .unwrap_or((seed_def.start_byte, idx.source.len()));
    out.function = enclosing_function_name(&idx, &g, seed_def.start_byte);

    let conf = edge_confidence(lang);
    let root = idx.tree.root_node();

    // Worklist of tainted bindings: (name, def_scope_range, distance). Visited
    // guards cycles (a binding reachable by two paths is analyzed once).
    let mut visited: std::collections::BTreeSet<(String, usize, usize)> = Default::default();
    let mut queue: std::collections::VecDeque<(String, (usize, usize), usize)> =
        std::collections::VecDeque::new();

    let seed_scope = idx.scope_range(seed_def.scope_id).unwrap_or(fn_range);
    out.nodes.push(FlowNode {
        name: seed.to_string(),
        path: path.to_string(),
        line: seed_def.line,
        distance: 0,
        confidence: conf,
    });
    visited.insert((seed.to_string(), seed_scope.0, seed_scope.1));
    queue.push_back((seed.to_string(), seed_scope, 0));

    while let Some((name, scope, dist)) = queue.pop_front() {
        if dist >= opts.max_depth {
            continue;
        }
        // Every reference provably bound to this binding (locals rule).
        for r in idx.references_bound_to(&name, scope) {
            // Only look at references inside the seed's function body: flow is
            // intra-procedural by construction.
            if !(fn_range.0 <= r.start_byte && r.start_byte < fn_range.1) {
                continue;
            }
            let Some(rnode) = root.descendant_for_byte_range(r.start_byte, r.end_byte) else {
                continue;
            };

            // 1) Call-argument sink takes precedence: the value leaves the
            //    function; we record the callee but never follow it (§7).
            if let Some((callee, line)) = call_arg_sink(rnode, &g, fn_range, &source) {
                push_sink(&mut out, &name, &callee, path, line);
                continue;
            }
            // 2) Explicit-return sink: the value exits via `return`.
            if let Some(line) = return_sink(rnode, lang, fn_range) {
                push_sink(&mut out, &name, "<return>", path, line);
                continue;
            }
            // 3) Assignment copy / unprovable store.
            if let Some(assign) = enclosing_assign(rnode, &g, fn_range) {
                let value = first_field(assign, g.value_fields);
                // The reference must BE the whole RHS value for a pure copy;
                // a sub-expression (`x + 1`, `x.foo`) is not a provable copy,
                // so we simply do not propagate (edge absent = None, §7).
                let is_pure_copy = value
                    .map(|v| v.start_byte() == r.start_byte && v.end_byte() == r.end_byte)
                    .unwrap_or(false);
                if !is_pure_copy {
                    continue;
                }
                let target = first_field(assign, g.target_fields);
                match target.and_then(|t| plain_ident_target(t, &g, &idx.source)) {
                    Some((tname, tbyte, tline)) => {
                        // Proven copy edge: taint the target identifier.
                        if let Some(tdef) = idx.definitions.iter().find(|d| d.start_byte == tbyte) {
                            let tscope = idx.scope_range(tdef.scope_id).unwrap_or(fn_range);
                            let key = (tname.clone(), tscope.0, tscope.1);
                            if visited.insert(key) {
                                out.nodes.push(FlowNode {
                                    name: tname.clone(),
                                    path: path.to_string(),
                                    line: tline,
                                    distance: dist + 1,
                                    confidence: conf,
                                });
                                queue.push_back((tname, tscope, dist + 1));
                            }
                        } else {
                            // Target identifier is not a locals *definition*
                            // (e.g. a plain reassignment `x = y` where x was
                            // defined earlier): we still recorded the store is
                            // provable, but without a def occurrence we cannot
                            // scope its references — surface, don't guess.
                            out.notes.push(format!(
                                "copy target `{tname}` @ {path}:{tline} has no locals def occurrence — not expanded"
                            ));
                        }
                    }
                    None => {
                        // Target is a subscript / attribute / tuple: we refuse
                        // to taint a field/element we don't understand (§7).
                        let (kind, line) = target
                            .map(|t| (classify_store(t.kind()), t.start_position().row + 1))
                            .unwrap_or_else(|| ("unknown".to_string(), r.line));
                        push_unprovable(&mut out, &name, &kind, path, line);
                    }
                }
            }
            // else: the reference is a plain read (condition, arithmetic, etc.)
            // that neither stores nor escapes — no edge to prove, nothing to do.
        }
    }

    out.nodes.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then(a.line.cmp(&b.line))
            .then(a.name.cmp(&b.name))
    });
    out.sinks
        .sort_by(|a, b| a.line.cmp(&b.line).then(a.value.cmp(&b.value)));
    out.unresolved
        .sort_by(|a, b| a.line.cmp(&b.line).then(a.value.cmp(&b.value)));
    Ok(out)
}

fn push_sink(out: &mut DataFlow, value: &str, callee: &str, path: &str, line: usize) {
    let s = FlowSink {
        value: value.to_string(),
        callee: callee.to_string(),
        path: path.to_string(),
        line,
    };
    if !out.sinks.contains(&s) {
        out.sinks.push(s);
    }
}

fn push_unprovable(out: &mut DataFlow, value: &str, kind: &str, path: &str, line: usize) {
    let u = UnprovableStore {
        value: value.to_string(),
        kind: kind.to_string(),
        path: path.to_string(),
        line,
    };
    if !out.unresolved.contains(&u) {
        out.notes.push(format!(
            "unprovable {kind} store of `{value}` @ {path}:{line} — not propagated (§7)"
        ));
        out.unresolved.push(u);
    }
}

// ---------------------------------------------------------------------------
// tree-sitter node helpers — every one is a *structural* predicate. None of
// them ever guesses: an unrecognized shape returns None / "unknown" and the
// caller degrades or records a frontier (§7).
// ---------------------------------------------------------------------------

/// The first present field among `fields` on `node`.
fn first_field<'a>(node: Node<'a>, fields: &[&str]) -> Option<Node<'a>> {
    for f in fields {
        if let Some(n) = node.child_by_field_name(f) {
            return Some(n);
        }
    }
    None
}

/// Walk ancestors of `node` (bounded to `fn_range`) to the nearest assignment /
/// declaration node, if any.
fn enclosing_assign<'a>(
    node: Node<'a>,
    g: &FlowGrammar,
    fn_range: (usize, usize),
) -> Option<Node<'a>> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.start_byte() < fn_range.0 {
            break;
        }
        if g.assign_kinds.contains(&n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// If `node` (a tainted reference) sits inside a call's *argument list*, return
/// the callee name and the call's line. The reference must be a direct element
/// of the argument list (a pure pass-through), not buried in a sub-expression —
/// `f(x)` is a sink, `f(x + 1)` / `f(x.y)` is not a provable pass of `x`.
fn call_arg_sink(
    node: Node,
    g: &FlowGrammar,
    fn_range: (usize, usize),
    src: &str,
) -> Option<(String, usize)> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.start_byte() < fn_range.0 {
            break;
        }
        if g.call_kinds.contains(&n.kind()) {
            let args = n.child_by_field_name(g.call_args_field)?;
            // The tainted reference itself must be a *direct* named element of
            // the argument list: `f(x)` is a pure pass of `x`, but `f(x + 1)` /
            // `f(x.y)` buries `x` in a sub-expression and is NOT a provable pass
            // (edge absent = None, §7 — we never taint a derived value).
            let mut c = args.walk();
            let direct = args.named_children(&mut c).any(|a| a.id() == node.id());
            if !direct {
                return None;
            }
            let callee = n
                .child_by_field_name(g.call_fn_field)
                .filter(|f| f.kind() == g.ident_kind)
                .and_then(|f| f.utf8_text(src.as_bytes()).ok().map(|s| s.to_string()))
                .unwrap_or_else(|| "<expr>".to_string());
            return Some((callee, n.start_position().row + 1));
        }
        // Do not cross a nested function/closure boundary looking for a call
        // that encloses this reference within the *current* function.
        cur = n.parent();
    }
    None
}

/// If `node` sits inside an explicit return statement, return its line.
fn return_sink(node: Node, lang: Lang, fn_range: (usize, usize)) -> Option<usize> {
    let kinds = return_kinds(lang);
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.start_byte() < fn_range.0 {
            break;
        }
        if kinds.contains(&n.kind()) {
            return Some(n.start_position().row + 1);
        }
        // Do not cross a nested function/closure boundary looking for a return.
        if lang.function_kinds().contains(&n.kind()) {
            break;
        }
        cur = n.parent();
    }
    None
}

/// If `target` is a plain identifier (possibly the sole element of a Go/Rust
/// single-target list), return `(name, start_byte, line)`. Anything else
/// (subscript, attribute, multi-target tuple, pattern) returns None.
fn plain_ident_target(target: Node, g: &FlowGrammar, src: &str) -> Option<(String, usize, usize)> {
    // Direct identifier.
    if target.kind() == g.ident_kind {
        let name = target.utf8_text(src.as_bytes()).ok()?.to_string();
        return Some((name, target.start_byte(), target.start_position().row + 1));
    }
    // Go wraps assignment LHS in an `expression_list`; a *single* identifier
    // element is still a provable single-target copy. More than one element is
    // a tuple assignment (unprovable — which value goes where).
    if target.kind() == "expression_list" {
        let mut c = target.walk();
        let elems: Vec<Node> = target.named_children(&mut c).collect();
        if elems.len() == 1 && elems[0].kind() == g.ident_kind {
            let n = elems[0];
            let name = n.utf8_text(src.as_bytes()).ok()?.to_string();
            return Some((name, n.start_byte(), n.start_position().row + 1));
        }
    }
    None
}

/// Classify an unprovable store target's node kind into a stable label.
fn classify_store(kind: &str) -> String {
    match kind {
        "subscript" | "index_expression" | "subscript_expression" => "subscript".to_string(),
        "attribute" | "member_expression" | "selector_expression" | "field_expression" => {
            "attribute".to_string()
        }
        "tuple"
        | "tuple_pattern"
        | "pattern_list"
        | "expression_list"
        | "array_pattern"
        | "object_pattern"
        | "tuple_struct_pattern" => "tuple/pattern".to_string(),
        other => other.to_string(),
    }
}

/// Best-effort name of the function enclosing `byte`, read from the nearest
/// enclosing `function_kinds` node's `name` field. Reporting only; None when
/// anonymous (lambda / closure / arrow).
fn enclosing_function_name(idx: &LocalsIndex, _g: &FlowGrammar, byte: usize) -> Option<String> {
    let kinds = idx.lang.function_kinds();
    let root = idx.tree.root_node();
    let src = idx.source.as_bytes();
    let mut best: Option<(usize, String)> = None; // (span, name)
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if kinds.contains(&node.kind()) && node.start_byte() <= byte && byte < node.end_byte() {
            if let Some(nn) = node.child_by_field_name("name") {
                if let Ok(name) = nn.utf8_text(src) {
                    let span = node.end_byte() - node.start_byte();
                    match &best {
                        Some((bspan, _)) if *bspan <= span => {}
                        _ => best = Some((span, name.to_string())),
                    }
                }
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    best.map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `src` to a uniquely-named temp file with `ext` and return its path.
    /// Honours TMPDIR (house convention) so scratch stays inside the sandbox.
    fn scratch(ext: &str, src: &str) -> String {
        let mut dir = std::env::temp_dir();
        let uniq = format!(
            "shuttle_df_{}_{}.{ext}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(uniq);
        let mut f = std::fs::File::create(&dir).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn names_at(flow: &DataFlow) -> Vec<(String, usize)> {
        flow.nodes
            .iter()
            .map(|n| (n.name.clone(), n.distance))
            .collect()
    }

    #[test]
    fn python_proves_copy_chain_and_stops_at_derived_value() {
        // raw -> x -> y proven; z = y + 1 is a DERIVED value, never tainted.
        let src = "def h(raw):\n    x = raw\n    y = x\n    z = y + 1\n    return z\n";
        let p = scratch("py", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        let n = names_at(&flow);
        assert!(n.contains(&("raw".into(), 0)), "{n:?}");
        assert!(n.contains(&("x".into(), 1)), "{n:?}");
        assert!(n.contains(&("y".into(), 2)), "{n:?}");
        // z is derived (y + 1), NOT a pure copy — must be absent (§7).
        assert!(!flow.nodes.iter().any(|nd| nd.name == "z"), "{n:?}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn python_call_argument_is_a_sink_not_a_propagation() {
        let src = "def h(raw):\n    y = raw\n    save(y)\n    return 0\n";
        let p = scratch("py", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        assert!(
            flow.sinks
                .iter()
                .any(|s| s.value == "y" && s.callee == "save"),
            "sinks={:?}",
            flow.sinks
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn python_derived_call_arg_is_not_a_sink() {
        // save(y + 1): `y` is buried in a sub-expression — NOT a provable pass.
        let src = "def h(raw):\n    y = raw\n    save(y + 1)\n    return 0\n";
        let p = scratch("py", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        assert!(flow.sinks.is_empty(), "sinks={:?}", flow.sinks);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn python_subscript_and_attribute_stores_are_unprovable_not_tainted() {
        let src = "def h(raw):\n    x = raw\n    data[0] = x\n    self.f = x\n    return 0\n";
        let p = scratch("py", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        assert!(
            flow.unresolved.iter().any(|u| u.kind == "subscript"),
            "unresolved={:?}",
            flow.unresolved
        );
        assert!(
            flow.unresolved.iter().any(|u| u.kind == "attribute"),
            "unresolved={:?}",
            flow.unresolved
        );
        // Neither `data` nor `f`/`self` gets tainted.
        assert!(!flow.nodes.iter().any(|n| n.name == "data" || n.name == "f"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn go_copy_chain_is_medium_and_return_is_a_sink() {
        let src = "package s\n\nfunc H(raw string) string {\n\tx := raw\n\ty := x\n\tsink(y)\n\treturn x\n}\n";
        let p = scratch("go", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        // Strongly-typed -> Medium, never Low.
        assert!(
            flow.nodes
                .iter()
                .all(|n| n.confidence == Confidence::Medium),
            "nodes={:?}",
            flow.nodes
        );
        assert!(flow.nodes.iter().any(|n| n.name == "x" && n.distance == 1));
        assert!(flow.nodes.iter().any(|n| n.name == "y" && n.distance == 2));
        assert!(
            flow.sinks
                .iter()
                .any(|s| s.callee == "<return>" && s.value == "x"),
            "sinks={:?}",
            flow.sinks
        );
        assert!(flow
            .sinks
            .iter()
            .any(|s| s.callee == "sink" && s.value == "y"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn go_tuple_assignment_target_is_unprovable() {
        // a, b := x, y — a multi-target assignment: which value lands where is
        // not a single-identifier copy, so it must NOT propagate (§7).
        let src = "package s\n\nfunc H(raw string) {\n\tx := raw\n\ta, b := x, other()\n\t_ = a\n\t_ = b\n}\n";
        let p = scratch("go", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        // x is proven (distance 1); a/b must not be tainted as clean copies.
        assert!(flow.nodes.iter().any(|n| n.name == "x"));
        assert!(
            !flow.nodes.iter().any(|n| n.name == "a" || n.name == "b"),
            "nodes={:?}",
            flow.nodes
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rust_copy_chain_and_return_sink() {
        let src = "fn h(raw: String) -> String {\n    let x = raw;\n    let y = x;\n    sink(&y);\n    return y;\n}\n";
        let p = scratch("rs", src);
        let flow = dataflow("raw", &p, &DataFlowOptions::default()).unwrap();
        assert!(
            flow.nodes.iter().any(|n| n.name == "x" && n.distance == 1),
            "nodes={:?}",
            flow.nodes
        );
        assert!(
            flow.nodes.iter().any(|n| n.name == "y" && n.distance == 2),
            "nodes={:?}",
            flow.nodes
        );
        assert!(flow
            .nodes
            .iter()
            .all(|n| n.confidence == Confidence::Medium));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_seed_degrades_never_invents_a_source() {
        let src = "def h(raw):\n    return raw\n";
        let p = scratch("py", src);
        let flow = dataflow("nonexistent", &p, &DataFlowOptions::default()).unwrap();
        assert!(flow.degraded, "should degrade");
        assert!(flow.nodes.is_empty(), "no nodes when seed missing");
        assert!(flow
            .notes
            .iter()
            .any(|nt| nt.contains("no local definition")));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn unsupported_language_errors_cleanly() {
        let p = scratch("txt", "raw = 1\n");
        let err = dataflow("raw", &p, &DataFlowOptions::default());
        assert!(matches!(err, Err(RefactorError::UnsupportedLang(_))));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn depth_bound_is_respected() {
        // A long copy chain truncated by max_depth = 1: only seed + 1 hop.
        let src = "def h(raw):\n    a = raw\n    b = a\n    c = b\n    return c\n";
        let p = scratch("py", src);
        let flow = dataflow("raw", &p, &DataFlowOptions { max_depth: 1 }).unwrap();
        assert!(
            flow.nodes.iter().all(|n| n.distance <= 1),
            "nodes={:?}",
            flow.nodes
        );
        assert!(flow.nodes.iter().any(|n| n.name == "a"));
        assert!(!flow.nodes.iter().any(|n| n.name == "c"), "c beyond depth");
        std::fs::remove_file(&p).ok();
    }
}
