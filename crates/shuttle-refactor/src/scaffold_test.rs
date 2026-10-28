//! scaffold-test (§9 E5): from a function's *declared signature*, synthesize a
//! table-driven / parametrized **test skeleton** — the mechanical scaffold every
//! table test shares (a case table, a loop, a call, an assertion slot) — leaving
//! the actual cases and the assertion as explicit TODOs.
//!
//! This is a "creation that is a deterministic projection of existing code"
//! (§7「绝不瞎猜」): arity and parameter names come from the signature, parameter
//! and return *types* are echoed verbatim where the grammar states them, and
//! everything we *cannot* know from a signature (the inputs, the expected
//! output, the equality semantics) is a labelled placeholder rather than a
//! guess. Because the body logic is intentionally unwritten, the plan is capped
//! at **Medium** confidence and every placeholder is called out in warnings.
//!
//! The skeleton is appended **after the top-level item** that contains the
//! function (so a method's test lands after its whole `impl`/class, never inside
//! it), anchored on that item's unique verbatim block — the same trick E1/E3/E4
//! use, so the edit range is never guessed. Idiom per language: Rust
//! `#[cfg(test)] mod`, Go `func TestXxx(t *testing.T)` table test, TS/JS Jest
//! `describe` + `it.each`, Python `@pytest.mark.parametrize`.

use shuttle_nav::Confidence;

use crate::inline_method::find_callee_def;
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use tree_sitter::Node;

/// A signature parameter: its binding name and (where declared) its type.
struct Param {
    name: String,
    ty: Option<String>,
}

/// Generate a table-driven test skeleton for the uniquely-defined function
/// `fn_name` in `source`, appending it to `path`. Fails if `fn_name` is not
/// defined / not unique, or across an unsupported language.
pub fn scaffold_test(
    path: &str,
    source: &str,
    fn_name: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    // The function must already exist (we scaffold a test *for* it). Unique by
    // name, same file — ambiguity is rejected rather than guessed (§7).
    let func = find_callee_def(root, lang, fn_name, source)?;

    let params = signature_params(lang, func, source);
    let ret = return_type(lang, func, source);

    // A method / receiver-bound function can't be called as `fn_name(..)` — the
    // skeleton still projects the shape but flags the invocation for fix-up.
    let anchor = top_level_ancestor(func, root);
    let is_method = anchor.id() != func.id();

    let scaffold = render_scaffold(lang, fn_name, &params, ret.as_deref());

    let block = anchor
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();
    let replacement = format!("{}\n\n{scaffold}", block.trim_end());
    let ops = vec![search_replace(
        path,
        block,
        replacement,
        anchor.start_position().row + 1,
    )];
    let sites = vec![format!(
        "{path}:{} (scaffold test for `{fn_name}`)",
        anchor.start_position().row + 1
    )];

    let mut warnings: Vec<String> = vec![format!(
        "the test body is a skeleton — the case table and the assertion for `{fn_name}` are TODO \
         placeholders; fill them in before relying on it"
    )];
    match lang {
        Lang::Rust => warnings.push(
            "reference-typed params (e.g. `&str`) in the case struct may need explicit lifetimes \
             — adjust before compiling"
                .into(),
        ),
        Lang::Go => warnings.push(
            "add `import \"testing\"` and place this in a `_test.go` file for `go test` to pick it up"
                .into(),
        ),
        Lang::Python => warnings
            .push("add `import pytest` (or adapt to your test framework) before running".into()),
        Lang::TypeScript | Lang::JavaScript => warnings.push(
            "assumes Jest-style `describe`/`it.each` — adapt to your test runner if different"
                .into(),
        ),
    }
    if is_method {
        warnings.push(format!(
            "`{fn_name}` is a method/receiver-bound function — the skeleton calls it as a free \
             function; wire up the receiver and fix the invocation"
        ));
    }

    let intent = format!("scaffold-test: {fn_name} ({path})");
    let plan = plan_from_ops(RefactorKind::ScaffoldTest, path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::ScaffoldTest,
        plan,
        signature: Some(format!("test for {fn_name}({} params)", params.len())),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::Medium,
        warnings,
        sites,
    })
}

/// Text of a node as an owned `String`, if valid UTF-8.
fn ntext(n: Node, source: &str) -> Option<String> {
    n.utf8_text(source.as_bytes()).ok().map(|s| s.to_string())
}

/// The declared parameters of `func`, per-language grammar. Receivers (`self`,
/// `cls`, Go/Rust `self`) are skipped — a test drives value parameters.
fn signature_params(lang: Lang, func: Node, source: &str) -> Vec<Param> {
    let Some(pnode) = func.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut c = pnode.walk();
    for ch in pnode.named_children(&mut c) {
        if let Some(p) = param_from(lang, ch, source) {
            out.push(p);
        }
    }
    out
}

/// Project one `Param` from a single parameter node, or `None` for a receiver /
/// unsupported node kind.
fn param_from(lang: Lang, node: Node, source: &str) -> Option<Param> {
    match lang {
        Lang::Rust => {
            if node.kind() != "parameter" {
                return None; // self_parameter and others are skipped
            }
            let name = node
                .child_by_field_name("pattern")
                .and_then(|n| ntext(n, source))?;
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| ntext(n, source));
            Some(Param { name, ty })
        }
        Lang::Go => {
            if !matches!(
                node.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            ) {
                return None;
            }
            let name = node
                .child_by_field_name("name")
                .and_then(|n| ntext(n, source))?;
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| ntext(n, source));
            Some(Param { name, ty })
        }
        Lang::TypeScript => {
            if !matches!(node.kind(), "required_parameter" | "optional_parameter") {
                return None;
            }
            let name = node
                .child_by_field_name("pattern")
                .and_then(|n| ntext(n, source))?;
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| ntext(n, source))
                .map(|s| s.trim_start_matches(':').trim().to_string());
            Some(Param { name, ty })
        }
        Lang::Python => match node.kind() {
            "identifier" => {
                let n = ntext(node, source)?;
                if n == "self" || n == "cls" {
                    return None;
                }
                Some(Param { name: n, ty: None })
            }
            "typed_parameter" => {
                let mut cc = node.walk();
                let name = node
                    .named_children(&mut cc)
                    .find(|k| k.kind() == "identifier")
                    .and_then(|n| ntext(n, source))?;
                if name == "self" || name == "cls" {
                    return None;
                }
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|n| ntext(n, source));
                Some(Param { name, ty })
            }
            "default_parameter" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| ntext(n, source))?;
                if name == "self" || name == "cls" {
                    return None;
                }
                Some(Param { name, ty: None })
            }
            "typed_default_parameter" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| ntext(n, source))?;
                if name == "self" || name == "cls" {
                    return None;
                }
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|n| ntext(n, source));
                Some(Param { name, ty })
            }
            _ => None,
        },
        Lang::JavaScript => {
            if node.kind() != "identifier" {
                return None;
            }
            Some(Param {
                name: ntext(node, source)?,
                ty: None,
            })
        }
    }
}

/// The declared return type of `func`, verbatim (TS annotation `:` stripped).
/// JS has none; Go uses the `result` field, everyone else `return_type`.
fn return_type(lang: Lang, func: Node, source: &str) -> Option<String> {
    let field = match lang {
        Lang::JavaScript => return None,
        Lang::Go => "result",
        _ => "return_type",
    };
    let n = func.child_by_field_name(field)?;
    let mut t = ntext(n, source)?;
    if matches!(lang, Lang::TypeScript) {
        t = t.trim_start_matches(':').trim().to_string();
    }
    Some(t)
}

/// A single, non-composite type token — safe to use as a struct field type
/// (no top-level comma / tuple / paren). Used to decide whether Go/Rust can
/// give the `expected` slot a real type or must fall back to a comment.
fn is_simple_type(t: &str) -> bool {
    !t.trim().is_empty() && !t.contains(',') && !t.trim_start().starts_with('(')
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

/// Render the language-idiomatic table-driven test skeleton.
fn render_scaffold(lang: Lang, name: &str, params: &[Param], ret: Option<&str>) -> String {
    match lang {
        Lang::Rust => rust_scaffold(name, params, ret),
        Lang::Go => go_scaffold(name, params, ret),
        Lang::TypeScript => ts_scaffold(name, params, ret, true),
        Lang::JavaScript => ts_scaffold(name, params, ret, false),
        Lang::Python => python_scaffold(name, params),
    }
}

fn rust_scaffold(name: &str, params: &[Param], ret: Option<&str>) -> String {
    let expected_ty = ret.filter(|r| !r.trim().is_empty()).unwrap_or("()");
    let mut fields: Vec<String> = params
        .iter()
        .map(|p| {
            format!(
                "            {}: {},",
                p.name,
                p.ty.as_deref().unwrap_or("()")
            )
        })
        .collect();
    fields.push(format!("            expected: {expected_ty},"));
    let field_block = fields.join("\n");
    let call_args = params
        .iter()
        .map(|p| format!("case.{}", p.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "#[cfg(test)]\nmod test_{name} {{\n    use super::*;\n\n    #[test]\n    fn {name}_cases() {{\n        struct Case {{\n{field_block}\n        }}\n        // TODO: add real (inputs, expected) rows.\n        let cases: Vec<Case> = vec![];\n        for case in cases {{\n            let _got = {name}({call_args});\n            // TODO: assert_eq!(_got, case.expected);\n            let _ = case.expected;\n        }}\n    }}\n}}"
    )
}

fn go_scaffold(name: &str, params: &[Param], ret: Option<&str>) -> String {
    let mut fields: Vec<String> = vec!["\t\tname     string".to_string()];
    for p in params {
        fields.push(format!(
            "\t\t{} {}",
            p.name,
            p.ty.as_deref().unwrap_or("interface{}")
        ));
    }
    let has_expected = ret.map(is_simple_type).unwrap_or(false);
    if has_expected {
        fields.push(format!("\t\texpected {}", ret.unwrap().trim()));
    }
    let field_block = fields.join("\n");
    let call_args = params
        .iter()
        .map(|p| format!("tt.{}", p.name))
        .collect::<Vec<_>>()
        .join(", ");
    let cmp = if has_expected {
        "\t\t\t// TODO: compare got with tt.expected".to_string()
    } else {
        format!(
            "\t\t\t// TODO: assert on got (return type {} is composite)",
            ret.unwrap_or("?")
        )
    };
    format!(
        "func Test{name}(t *testing.T) {{\n\ttests := []struct {{\n{field_block}\n\t}}{{\n\t\t// TODO: add cases\n\t}}\n\tfor _, tt := range tests {{\n\t\tt.Run(tt.name, func(t *testing.T) {{\n\t\t\tgot := {name}({call_args})\n\t\t\t_ = got\n{cmp}\n\t\t}})\n\t}}\n}}"
    )
}

fn ts_scaffold(name: &str, params: &[Param], ret: Option<&str>, typed: bool) -> String {
    let names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut cols = names.clone();
    cols.push("expected".to_string());
    let lambda = cols.join(", ");
    let call_args = names.join(", ");
    let placeholders = params.iter().map(|_| "%p").collect::<Vec<_>>().join(", ");
    let each = if typed {
        let mut tys: Vec<String> = params
            .iter()
            .map(|p| p.ty.clone().unwrap_or_else(|| "unknown".to_string()))
            .collect();
        tys.push(
            ret.map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        );
        format!("it.each<[{}]>([", tys.join(", "))
    } else {
        "it.each([".to_string()
    };
    format!(
        "describe(\"{name}\", () => {{\n  // TODO: fill in rows: [{}].\n  {each}\n    // [{}],\n  ])(\"{name}({placeholders})\", ({lambda}) => {{\n    const got = {name}({call_args});\n    void got;\n    // TODO: expect(got).toBe(expected);\n  }});\n}});",
        cols.join(", "),
        cols.join(", ")
    )
}

fn python_scaffold(name: &str, params: &[Param]) -> String {
    let names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut cols = names.clone();
    cols.push("expected".to_string());
    let ids = cols.join(", ");
    let call_args = names.join(", ");
    format!(
        "@pytest.mark.parametrize(\n    \"{ids}\",\n    [\n        # TODO: add cases ({ids})\n    ],\n)\ndef test_{name}({ids}):\n    result = {name}({call_args})\n    # TODO: assert result == expected\n    _ = result"
    )
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
    fn rust_table_test_from_signature() {
        let src = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let plan = scaffold_test("lib.rs", src, "add").unwrap();
        assert!(matches!(plan.confidence, Confidence::Medium));
        let r = replacement(&plan);
        let s = &r[r.find("#[cfg(test)]").unwrap()..];
        assert!(s.contains("mod test_add"));
        assert!(s.contains("a: i32,"));
        assert!(s.contains("b: i32,"));
        assert!(s.contains("expected: i32,"));
        assert!(s.contains("let _got = add(case.a, case.b);"));
        assert!(s.contains("assert_eq!(_got, case.expected)"));
    }

    #[test]
    fn go_table_test_from_signature() {
        let src = "func Add(a int, b int) int {\n\treturn a + b\n}\n";
        let plan = scaffold_test("m.go", src, "Add").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("func TestAdd").unwrap()..];
        assert!(s.contains("tests := []struct {"));
        assert!(s.contains("a int"));
        assert!(s.contains("expected int"));
        assert!(s.contains("got := Add(tt.a, tt.b)"));
        assert!(s.contains("t.Run(tt.name"));
    }

    #[test]
    fn go_composite_return_falls_back_to_comment() {
        let src = "func Parse(s string) (int, error) {\n\treturn 0, nil\n}\n";
        let plan = scaffold_test("m.go", src, "Parse").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("func TestParse").unwrap()..];
        assert!(!s.contains("expected (int, error)"));
        assert!(s.contains("composite"));
    }

    #[test]
    fn ts_it_each_typed() {
        let src = "export function add(a: number, b: string): number {\n  return a;\n}\n";
        let plan = scaffold_test("m.ts", src, "add").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("describe(").unwrap()..];
        assert!(s.contains("it.each<[number, string, number]>(["));
        assert!(s.contains("const got = add(a, b);"));
        assert!(s.contains("(a, b, expected) =>"));
    }

    #[test]
    fn python_parametrize() {
        let src = "def add(a: int, b: int) -> int:\n    return a + b\n";
        let plan = scaffold_test("m.py", src, "add").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("@pytest").unwrap()..];
        assert!(s.contains("@pytest.mark.parametrize("));
        assert!(s.contains("\"a, b, expected\""));
        assert!(s.contains("def test_add(a, b, expected):"));
        assert!(s.contains("result = add(a, b)"));
    }

    #[test]
    fn js_it_each_untyped() {
        let src = "function add(a, b) {\n  return a + b;\n}\n";
        let plan = scaffold_test("m.js", src, "add").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("describe(").unwrap()..];
        assert!(s.contains("it.each(["));
        assert!(!s.contains("<["));
        assert!(s.contains("const got = add(a, b);"));
    }

    #[test]
    fn python_skips_self_receiver() {
        let src = "class C:\n    def method(self, a: int) -> int:\n        return a\n";
        let plan = scaffold_test("m.py", src, "method").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("@pytest").unwrap()..];
        assert!(s.contains("\"a, expected\""));
        assert!(!s.contains("self"));
        // method → warned about receiver-bound invocation.
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("method/receiver-bound")));
    }

    #[test]
    fn zero_param_function() {
        let src = "fn now() -> u64 {\n    0\n}\n";
        let plan = scaffold_test("lib.rs", src, "now").unwrap();
        let r = replacement(&plan);
        let s = &r[r.find("#[cfg(test)]").unwrap()..];
        assert!(s.contains("expected: u64,"));
        assert!(s.contains("let _got = now();"));
    }

    #[test]
    fn rejects_undefined_function() {
        let src = "fn main() {}\n";
        let err = scaffold_test("lib.rs", src, "ghost").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("not defined")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn warns_body_is_placeholder() {
        let src = "fn f(a: i32) -> i32 {\n    a\n}\n";
        let plan = scaffold_test("lib.rs", src, "f").unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("skeleton")));
    }
}
