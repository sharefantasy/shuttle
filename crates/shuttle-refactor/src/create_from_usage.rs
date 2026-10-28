//! create-from-usage (§9 E1): given a function that is *called* somewhere in a
//! file but not yet *defined*, synthesize a stub definition whose shape is
//! projected from the call site — arity from the argument count, parameter
//! names from identifier arguments, and best-effort parameter types inferred
//! from literal arguments.
//!
//! Unlike E2/E3 (which project from a fully-declared source of truth), a call
//! site under-determines the definition: the return type is never stated, and
//! non-literal arguments carry no type. So this refactor is honest about the
//! guessing (§7「绝不瞎猜」): every inferred-or-placeholder piece is called out
//! in warnings, the body is an explicit "not implemented" marker, and the plan
//! is capped at **Medium** confidence. The stub is inserted immediately before
//! the top-level item that contains the call, anchored on that item's unique
//! verbatim block (the E2/E4 trick), so the edit range itself is never guessed.

use shuttle_nav::Confidence;

use crate::inline_method::find_callee_def;
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use tree_sitter::Node;

/// A parameter projected from one call argument.
struct Param {
    name: String,
    /// An inferred type (from a literal), or `None` when the argument is a
    /// non-literal we refuse to guess the type of.
    ty: Option<String>,
}

/// Synthesize a stub definition for `name`, which is called but not defined in
/// `source`, and insert it into `path`. Fails if `name` has no call site, if it
/// is already defined (then there is nothing to *create*), or across an
/// unsupported language.
pub fn create_from_usage(
    path: &str,
    source: &str,
    name: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    // Refuse if the function already exists — this refactor *creates*, it does
    // not edit an existing definition (that would be a different operation).
    if find_callee_def(root, lang, name, source).is_ok() {
        return Err(RefactorError::Rejected(format!(
            "`{name}` is already defined in this file — create-from-usage only stubs \
             *undefined* functions"
        )));
    }

    // Collect every call site of `name`.
    let calls = collect_calls(root, lang, name, source);
    if calls.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "no call to `{name}` found in this file — nothing to project a stub from"
        )));
    }

    let mut warnings: Vec<String> = Vec::new();

    // Pick the first call site; warn if others disagree on arity (we can only
    // project one signature, and guessing which is canonical is out of scope).
    let arg_lists: Vec<Vec<Node>> = calls.iter().map(|c| call_args(*c, lang)).collect();
    let arity = arg_lists[0].len();
    if arg_lists.iter().any(|a| a.len() != arity) {
        warnings.push(format!(
            "`{name}` is called with differing argument counts — stubbed from the first call \
             site ({arity} args); reconcile the others by hand"
        ));
    }

    // Project parameters from the chosen call's arguments.
    let params = project_params(lang, &arg_lists[0], source);
    if params.iter().any(|p| p.ty.is_none()) && !matches!(lang, Lang::Python | Lang::JavaScript) {
        warnings.push(format!(
            "some parameters of `{name}` could not be typed from the call site (non-literal \
             arguments) — a `{}` placeholder was used; fill in real types",
            unknown_type(lang)
        ));
    }
    if !matches!(lang, Lang::Python | Lang::JavaScript) {
        warnings.push(format!(
            "the return type of `{name}` is not knowable from a call site — a placeholder was \
             used; set the real return type"
        ));
    }
    warnings.push(format!(
        "the body of `{name}` is a placeholder — implement it before use"
    ));

    // Insert the stub right before the top-level item that contains the call.
    let call = calls[0];
    let anchor = top_level_ancestor(call, root);
    let block = anchor
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();

    let stub = render_stub(lang, name, &params);
    let replacement = format!("{stub}\n\n{block}");
    let ops = vec![search_replace(
        path,
        block,
        replacement,
        anchor.start_position().row + 1,
    )];
    let sites = vec![format!(
        "{path}:{} (create stub `{name}` before its caller)",
        anchor.start_position().row + 1
    )];

    let intent = format!("create-from-usage: stub {name} ({path})");
    let plan = plan_from_ops(RefactorKind::CreateFromUsage, path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::CreateFromUsage,
        plan,
        signature: Some(format!("{name}({} args)", params.len())),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::Medium,
        warnings,
        sites,
    })
}

/// The call-expression node kind for `lang` (Python uses `call`).
fn call_kind(lang: Lang) -> &'static str {
    match lang {
        Lang::Python => "call",
        _ => "call_expression",
    }
}

/// Every call node whose callee identifier is exactly `name`.
fn collect_calls<'a>(root: Node<'a>, lang: Lang, name: &str, source: &str) -> Vec<Node<'a>> {
    let ck = call_kind(lang);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == ck && callee_name(n, source).as_deref() == Some(name) {
            out.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

/// The callee name of a call node — the `function` field where the grammar
/// exposes one, else the first `identifier` child. Only bare-identifier callees
/// match (method / namespaced calls are out of scope).
fn callee_name(call: Node, source: &str) -> Option<String> {
    let callee = call
        .child_by_field_name("function")
        .or_else(|| call.named_child(0))?;
    if callee.kind() != "identifier" {
        return None;
    }
    callee
        .utf8_text(source.as_bytes())
        .ok()
        .map(|s| s.to_string())
}

/// The argument nodes of a call (the named children of its argument list).
fn call_args<'a>(call: Node<'a>, _lang: Lang) -> Vec<Node<'a>> {
    let args = call.child_by_field_name("arguments").or_else(|| {
        // Go/Python name the list differently; find it by kind.
        let mut c = call.walk();
        let found = call
            .children(&mut c)
            .find(|ch| matches!(ch.kind(), "argument_list" | "arguments"));
        found
    });
    let Some(args) = args else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut c = args.walk();
    for ch in args.named_children(&mut c) {
        out.push(ch);
    }
    out
}

/// Project one parameter per argument: identifier args keep their name, other
/// args become `argN`; the type is inferred from literal kinds where possible.
fn project_params(lang: Lang, args: &[Node], source: &str) -> Vec<Param> {
    let mut out: Vec<Param> = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let base = if arg.kind() == "identifier" {
            arg.utf8_text(source.as_bytes())
                .ok()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("arg{}", i + 1))
        } else {
            format!("arg{}", i + 1)
        };
        // Dedupe against earlier params (two identical identifier args).
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|p| p.name == name) {
            name = format!("{base}{n}");
            n += 1;
        }
        let ty = infer_type(lang, arg.kind());
        out.push(Param { name, ty });
    }
    out
}

/// Best-effort type from an argument node kind. `None` for anything that is not
/// an unambiguous literal (we refuse to guess — §7).
fn infer_type(lang: Lang, kind: &str) -> Option<String> {
    let t = match lang {
        Lang::Rust => match kind {
            "integer_literal" => "i64",
            "float_literal" => "f64",
            "string_literal" => "String",
            "boolean_literal" | "true" | "false" => "bool",
            "char_literal" => "char",
            _ => return None,
        },
        Lang::Go => match kind {
            "int_literal" => "int",
            "float_literal" => "float64",
            "interpreted_string_literal" | "raw_string_literal" => "string",
            "true" | "false" => "bool",
            _ => return None,
        },
        Lang::TypeScript => match kind {
            "number" => "number",
            "string" => "string",
            "true" | "false" => "boolean",
            _ => return None,
        },
        // Python / JS stubs are untyped; type inference is not applicable.
        Lang::Python | Lang::JavaScript => return None,
    };
    Some(t.to_string())
}

/// The placeholder type name for `lang` when inference fails.
fn unknown_type(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "Unknown",
        Lang::Go => "interface{}",
        Lang::TypeScript => "unknown",
        Lang::Python | Lang::JavaScript => "",
    }
}

/// Walk up from `node` to the direct child of `root` that contains it.
fn top_level_ancestor<'a>(node: Node<'a>, root: Node<'a>) -> Node<'a> {
    let mut cur = node;
    while let Some(p) = cur.parent() {
        if p.id() == root.id() {
            return cur;
        }
        cur = p;
    }
    cur
}

/// Render the stub definition for one language.
fn render_stub(lang: Lang, name: &str, params: &[Param]) -> String {
    match lang {
        Lang::Rust => {
            let ps = params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.ty.as_deref().unwrap_or("Unknown")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("fn {name}({ps}) -> Unknown {{\n    todo!()\n}}")
        }
        Lang::Go => {
            let ps = params
                .iter()
                .map(|p| format!("{} {}", p.name, p.ty.as_deref().unwrap_or("interface{}")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("func {name}({ps}) interface{{}} {{\n\tpanic(\"not implemented: {name}\")\n}}")
        }
        Lang::TypeScript => {
            let ps = params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.ty.as_deref().unwrap_or("unknown")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("function {name}({ps}): unknown {{\n  throw new Error(\"not implemented: {name}\");\n}}")
        }
        Lang::Python => {
            let ps = params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("def {name}({ps}):\n    raise NotImplementedError")
        }
        Lang::JavaScript => {
            let ps = params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("function {name}({ps}) {{\n  throw new Error(\"not implemented: {name}\");\n}}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::EditOp;

    fn replacement(plan: &RefactorPlan) -> String {
        plan.plan
            .ops
            .iter()
            .find_map(|o| match o {
                EditOp::SearchReplace { replace, .. } => Some(replace.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn rust_stub_from_call_infers_literal_types() {
        let src = "fn main() {\n    let r = compute(x, 3, \"hi\");\n}\n";
        let plan = create_from_usage("lib.rs", src, "compute").unwrap();
        assert!(matches!(plan.confidence, Confidence::Medium));
        let r = replacement(&plan);
        // x is a non-literal identifier → param name `x`, type Unknown.
        // 3 → arg2: i64, "hi" → arg3: String.
        assert!(r.contains("fn compute(x: Unknown, arg2: i64, arg3: String) -> Unknown {"));
        assert!(r.contains("todo!()"));
        // Stub inserted before the caller `fn main`.
        assert!(r.find("fn compute").unwrap() < r.find("fn main").unwrap());
    }

    #[test]
    fn go_stub_from_call() {
        let src = "func main() {\n\tr := Compute(x, 3, \"hi\")\n}\n";
        let plan = create_from_usage("g.go", src, "Compute").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("func Compute(x interface{}, arg2 int, arg3 string) interface{} {"));
        assert!(r.contains("panic(\"not implemented: Compute\")"));
    }

    #[test]
    fn ts_stub_from_call() {
        let src = "function main() {\n  const r = compute(x, 3, \"hi\");\n}\n";
        let plan = create_from_usage("g.ts", src, "compute").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("function compute(x: unknown, arg2: number, arg3: string): unknown {"));
        assert!(r.contains("throw new Error(\"not implemented: compute\");"));
    }

    #[test]
    fn python_stub_untyped() {
        let src = "def main():\n    r = compute(x, 3, \"hi\")\n";
        let plan = create_from_usage("g.py", src, "compute").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("def compute(x, arg2, arg3):"));
        assert!(r.contains("raise NotImplementedError"));
    }

    #[test]
    fn js_stub_untyped() {
        let src = "function main() {\n  const r = compute(x, y);\n}\n";
        let plan = create_from_usage("g.js", src, "compute").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("function compute(x, y) {"));
    }

    #[test]
    fn dedupes_repeated_identifier_args() {
        let src = "fn main() {\n    let r = f(x, x);\n}\n";
        let plan = create_from_usage("lib.rs", src, "f").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("fn f(x: Unknown, x2: Unknown) -> Unknown {"));
    }

    #[test]
    fn rejects_already_defined() {
        let src =
            "fn compute(a: i32) -> i32 {\n    a\n}\nfn main() {\n    let r = compute(1);\n}\n";
        let err = create_from_usage("lib.rs", src, "compute").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("already defined")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_no_call_site() {
        let src = "fn main() {\n    let r = 1;\n}\n";
        let err = create_from_usage("lib.rs", src, "ghost").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("no call")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn warns_on_untyped_params_and_return() {
        let src = "fn main() {\n    let r = compute(x);\n}\n";
        let plan = create_from_usage("lib.rs", src, "compute").unwrap();
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("could not be typed")));
        assert!(plan.warnings.iter().any(|w| w.contains("return type")));
        assert!(plan.warnings.iter().any(|w| w.contains("body")));
    }

    #[test]
    fn warns_on_arity_mismatch_across_calls() {
        let src = "fn main() {\n    let a = f(1);\n    let b = f(1, 2);\n}\n";
        let plan = create_from_usage("lib.rs", src, "f").unwrap();
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("differing argument counts")));
    }
}
