//! generate-boilerplate (§9 E3): synthesize the mechanical, field-derived
//! boilerplate for a data type — a constructor, a `Default`/repr/equality
//! helper — from the type's *declared fields*. This is the archetypal
//! "creation that is a deterministic projection of existing code": every line
//! of output is a mechanical function of the field names and (where declared)
//! their types, so we never invent anything the source doesn't already state
//! (§7「绝不瞎猜」).
//!
//! Supported `what` selectors, per language:
//!   * `constructor` — Rust `impl T { pub fn new(..) -> Self }`, Go
//!     `func NewT(..) *T`, TS `constructor(..)`, Python `__init__`.
//!   * `default`     — Rust only: `impl Default for T` filling every field
//!     with `Default::default()`.
//!   * `repr`        — Python only: `__repr__` echoing every field.
//!   * `eq`          — Python only: `__eq__` comparing every field tuple.
//!
//! Rust/Go boilerplate is emitted as a **sibling item** right after the type;
//! TS/Python boilerplate is spliced **inside** the type body (a constructor /
//! dunder has to live there). Either way the edit is anchored on the type's
//! unique verbatim block (the same trick E2/E4 use), so the range is never
//! guessed and the plan is High confidence.

use shuttle_nav::Confidence;

use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use tree_sitter::Node;

/// A declared field: its name and, when the grammar states it, its type.
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) ty: Option<String>,
}

/// Generate field-derived boilerplate (`what`) for the type `type_name` defined
/// in `source`, writing it into `path`. Fails if the type is not found / not
/// unique, declares no fields, or the `what` selector is unsupported for the
/// language.
pub fn generate_boilerplate(
    path: &str,
    source: &str,
    type_name: &str,
    what: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    let container = find_named_container(root, lang, type_name, source)?;
    let block = container
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();

    let fields = collect_fields(lang, &container, source);
    if fields.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{type_name}` declares no fields — nothing to generate boilerplate from"
        )));
    }

    let (member, inside) = render_boilerplate(lang, type_name, what, &fields)?;

    // Splice the generated code either after the type (Rust/Go sibling) or
    // inside the type body (TS/Python), anchoring on the unique block text.
    let replacement = if inside {
        splice_inside(lang, &block, &member)
    } else {
        format!("{block}\n\n{member}")
    };
    let ops = vec![search_replace(
        path,
        block,
        replacement,
        container.start_position().row + 1,
    )];
    let sites = vec![format!(
        "{path}:{} (generate {what} for `{type_name}`)",
        container.start_position().row + 1
    )];
    let warnings = vec![format!(
        "generated {what} is a mechanical projection of `{type_name}`'s fields — review field \
         order and types before relying on it"
    )];

    let intent = format!("generate-boilerplate: {what} for {type_name} ({path})");
    let plan = plan_from_ops(RefactorKind::GenerateBoilerplate, path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::GenerateBoilerplate,
        plan,
        signature: Some(format!("{what} @ {type_name}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::High,
        warnings,
        sites,
    })
}

/// Locate the unique top-level container named `name`. Rejects 0 / >1 (§7).
pub(crate) fn find_named_container<'a>(
    root: Node<'a>,
    lang: Lang,
    name: &str,
    source: &str,
) -> Result<Node<'a>, RefactorError> {
    let kinds = lang.container_kinds();
    let mut hits: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) && container_name(n, source).as_deref() == Some(name) {
            hits.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    match hits.len() {
        0 => Err(RefactorError::Rejected(format!(
            "`{name}` is not defined as a type/struct/class in this file"
        ))),
        1 => Ok(hits[0]),
        _ => Err(RefactorError::Rejected(format!(
            "`{name}` is defined {} times in this file — ambiguous",
            hits.len()
        ))),
    }
}

/// Best-effort container name (same rule as extract-to-new-file / E2).
fn container_name(node: Node, source: &str) -> Option<String> {
    if let Some(id) = node.child_by_field_name("name") {
        return id.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() == "type_spec" {
            if let Some(id) = ch.child_by_field_name("name") {
                return id.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
            }
        }
    }
    None
}

/// Collect the declared fields of a type, per-language grammar.
pub(crate) fn collect_fields(lang: Lang, container: &Node, source: &str) -> Vec<Field> {
    match lang {
        Lang::Rust => rust_fields(container, source),
        Lang::Go => go_fields(container, source),
        Lang::TypeScript => ts_fields(container, source),
        Lang::Python => python_fields(container, source),
        Lang::JavaScript => Vec::new(),
    }
}

/// Rust: struct_item → field_declaration_list → field_declaration{name,type}.
fn rust_fields(container: &Node, source: &str) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(list) = descend_to_kind(container, "field_declaration_list") else {
        return out;
    };
    let mut c = list.walk();
    for ch in list.children(&mut c) {
        if ch.kind() == "field_declaration" {
            let name = ch
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            let ty = ch
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            if let Some(name) = name {
                out.push(Field { name, ty });
            }
        }
    }
    out
}

/// Go: type_declaration → type_spec → struct_type → field_declaration_list →
/// field_declaration{name(field_identifier), type}.
fn go_fields(container: &Node, source: &str) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(list) = descend_to_kind(container, "field_declaration_list") else {
        return out;
    };
    let mut c = list.walk();
    for ch in list.children(&mut c) {
        if ch.kind() == "field_declaration" {
            let name = ch
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            let ty = ch
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            if let Some(name) = name {
                out.push(Field { name, ty });
            }
        }
    }
    out
}

/// TS: class_body → public_field_definition{name(property_identifier),
/// type_annotation}. The annotation carries a leading `: ` we strip.
fn ts_fields(container: &Node, source: &str) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(body) = container.child_by_field_name("body") else {
        return out;
    };
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        if ch.kind() == "public_field_definition" {
            let name = ch
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            let ty = descend_to_kind(&ch, "type_annotation")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.trim_start_matches(':').trim().to_string());
            if let Some(name) = name {
                out.push(Field { name, ty });
            }
        }
    }
    out
}

/// Python: class_definition → block → expression_statement → assignment of an
/// `identifier : type` (an annotated field). Bare assignments with a value are
/// class attributes we skip; only type-annotated field declarations count.
fn python_fields(container: &Node, source: &str) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(body) = container.child_by_field_name("body") else {
        return out;
    };
    let mut c = body.walk();
    for stmt in body.children(&mut c) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let mut sc = stmt.walk();
        for asn in stmt.children(&mut sc) {
            if asn.kind() != "assignment" {
                continue;
            }
            // Annotated field: assignment has a `type` field. A plain
            // assignment (value only, no annotation) is skipped — we don't
            // guess a field's type or whether it belongs in __init__ (§7).
            let ty = asn
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            if ty.is_none() {
                continue;
            }
            let name = asn
                .child_by_field_name("left")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(|s| s.to_string());
            if let Some(name) = name {
                out.push(Field { name, ty });
            }
        }
    }
    out
}

/// Render the boilerplate. Returns (rendered_code, insert_inside_body).
fn render_boilerplate(
    lang: Lang,
    type_name: &str,
    what: &str,
    fields: &[Field],
) -> Result<(String, bool), RefactorError> {
    match (lang, what) {
        (Lang::Rust, "constructor") => Ok((rust_new(type_name, fields), false)),
        (Lang::Rust, "default") => Ok((rust_default(type_name, fields), false)),
        (Lang::Go, "constructor") => Ok((go_new(type_name, fields), false)),
        (Lang::TypeScript, "constructor") => Ok((ts_ctor(fields), true)),
        (Lang::Python, "constructor") => Ok((python_init(fields), true)),
        (Lang::Python, "repr") => Ok((python_repr(type_name, fields), true)),
        (Lang::Python, "eq") => Ok((python_eq(type_name, fields), true)),
        (Lang::JavaScript, _) => Err(RefactorError::Rejected(
            "JavaScript fields are untyped/undeclared — no boilerplate to project (§7)".into(),
        )),
        (_, other) => Err(RefactorError::Rejected(format!(
            "`{other}` boilerplate is not supported for {lang:?} (supported: constructor; \
             plus default[Rust], repr/eq[Python])"
        ))),
    }
}

fn rust_new(type_name: &str, fields: &[Field]) -> String {
    let params = fields
        .iter()
        .map(|f| format!("{}: {}", f.name, f.ty.as_deref().unwrap_or("_")))
        .collect::<Vec<_>>()
        .join(", ");
    let names = fields
        .iter()
        .map(|f| f.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "impl {type_name} {{\n    pub fn new({params}) -> Self {{\n        Self {{ {names} }}\n    }}\n}}"
    )
}

fn rust_default(type_name: &str, fields: &[Field]) -> String {
    let body = fields
        .iter()
        .map(|f| format!("            {}: Default::default(),", f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "impl Default for {type_name} {{\n    fn default() -> Self {{\n        Self {{\n{body}\n        }}\n    }}\n}}"
    )
}

fn go_new(type_name: &str, fields: &[Field]) -> String {
    // Go convention: exported fields (X), lowercased param names (x).
    let params = fields
        .iter()
        .map(|f| {
            format!(
                "{} {}",
                lower_first(&f.name),
                f.ty.as_deref().unwrap_or("interface{}")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let assigns = fields
        .iter()
        .map(|f| format!("\t\t{}: {},", f.name, lower_first(&f.name)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "func New{type_name}({params}) *{type_name} {{\n\treturn &{type_name}{{\n{assigns}\n\t}}\n}}"
    )
}

fn ts_ctor(fields: &[Field]) -> String {
    let params = fields
        .iter()
        .map(|f| match &f.ty {
            Some(t) => format!("{}: {}", f.name, t),
            None => f.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let assigns = fields
        .iter()
        .map(|f| format!("    this.{n} = {n};", n = f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!("  constructor({params}) {{\n{assigns}\n  }}")
}

fn python_init(fields: &[Field]) -> String {
    let params = fields
        .iter()
        .map(|f| match &f.ty {
            Some(t) => format!("{}: {}", f.name, t),
            None => f.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let assigns = fields
        .iter()
        .map(|f| format!("        self.{n} = {n}", n = f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!("    def __init__(self, {params}):\n{assigns}")
}

fn python_repr(type_name: &str, fields: &[Field]) -> String {
    let parts = fields
        .iter()
        .map(|f| format!("{n}={{self.{n}!r}}", n = f.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("    def __repr__(self) -> str:\n        return f\"{type_name}({parts})\"")
}

fn python_eq(type_name: &str, fields: &[Field]) -> String {
    let self_tuple = fields
        .iter()
        .map(|f| format!("self.{}", f.name))
        .collect::<Vec<_>>()
        .join(", ");
    let other_tuple = fields
        .iter()
        .map(|f| format!("other.{}", f.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "    def __eq__(self, other) -> bool:\n        if not isinstance(other, {type_name}):\n            return NotImplemented\n        return ({self_tuple},) == ({other_tuple},)"
    )
}

/// Splice a member *inside* a container body. TS bodies close with `}` so we
/// insert before the final brace; Python bodies are indentation-based with no
/// closing token, so we append the (already-indented) member at the end.
pub(crate) fn splice_inside(lang: Lang, block: &str, member: &str) -> String {
    match lang {
        Lang::TypeScript => match block.rsplit_once('}') {
            Some((head, _)) => format!("{}\n{member}\n}}", head.trim_end()),
            None => format!("{block}\n\n{member}"),
        },
        _ => format!("{}\n\n{member}", block.trim_end()),
    }
}

/// Lowercase the first character of `s` (Go param-from-field convention).
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Descend the subtree of `node` to the first descendant of kind `kind`.
fn descend_to_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            return Some(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    None
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
    fn rust_constructor_from_fields() {
        let src = "struct Point {\n    x: i32,\n    y: String,\n}\n";
        let plan = generate_boilerplate("lib.rs", src, "Point", "constructor").unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        let r = replacement(&plan);
        assert!(r.contains("impl Point {"));
        assert!(r.contains("pub fn new(x: i32, y: String) -> Self {"));
        assert!(r.contains("Self { x, y }"));
    }

    #[test]
    fn rust_default_from_fields() {
        let src = "struct Config {\n    retries: u32,\n    name: String,\n}\n";
        let plan = generate_boilerplate("lib.rs", src, "Config", "default").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("impl Default for Config {"));
        assert!(r.contains("retries: Default::default(),"));
        assert!(r.contains("name: Default::default(),"));
    }

    #[test]
    fn go_constructor_from_fields() {
        let src = "type Point struct {\n\tX int\n\tY string\n}\n";
        let plan = generate_boilerplate("g.go", src, "Point", "constructor").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("func NewPoint(x int, y string) *Point {"));
        assert!(r.contains("X: x,"));
        assert!(r.contains("Y: y,"));
    }

    #[test]
    fn ts_constructor_spliced_inside_body() {
        let src = "class Point {\n  x: number;\n  y: string;\n}\n";
        let plan = generate_boilerplate("g.ts", src, "Point", "constructor").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("constructor(x: number, y: string) {"));
        assert!(r.contains("this.x = x;"));
        assert!(r.contains("this.y = y;"));
        // The constructor lives inside the class body (before the final brace).
        let ctor = r.find("constructor").unwrap();
        let last_brace = r.rfind('}').unwrap();
        assert!(ctor < last_brace);
    }

    #[test]
    fn python_init_from_annotated_fields() {
        let src = "class Point:\n    x: int\n    y: str\n";
        let plan = generate_boilerplate("g.py", src, "Point", "constructor").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("def __init__(self, x: int, y: str):"));
        assert!(r.contains("self.x = x"));
        assert!(r.contains("self.y = y"));
    }

    #[test]
    fn python_repr_from_fields() {
        let src = "class Point:\n    x: int\n    y: str\n";
        let plan = generate_boilerplate("g.py", src, "Point", "repr").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("def __repr__(self) -> str:"));
        assert!(r.contains("return f\"Point(x={self.x!r}, y={self.y!r})\""));
    }

    #[test]
    fn python_eq_from_fields() {
        let src = "class Point:\n    x: int\n    y: str\n";
        let plan = generate_boilerplate("g.py", src, "Point", "eq").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("def __eq__(self, other) -> bool:"));
        assert!(r.contains("if not isinstance(other, Point):"));
        assert!(r.contains("return (self.x, self.y,) == (other.x, other.y,)"));
    }

    #[test]
    fn rejects_type_with_no_fields() {
        let src = "struct Marker {\n}\n";
        let err = generate_boilerplate("lib.rs", src, "Marker", "constructor").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("no fields")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_type() {
        let src = "struct Point {\n    x: i32,\n}\n";
        let err = generate_boilerplate("lib.rs", src, "Ghost", "constructor").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_unsupported_selector() {
        let src = "struct Point {\n    x: i32,\n}\n";
        // `repr` is Python-only; on Rust it must be rejected, not guessed.
        let err = generate_boilerplate("lib.rs", src, "Point", "repr").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("not supported")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn python_skips_unannotated_class_attrs() {
        // `count = 0` is an unannotated class attr — not a field we project.
        let src = "class Point:\n    count = 0\n    x: int\n";
        let plan = generate_boilerplate("g.py", src, "Point", "constructor").unwrap();
        let r = replacement(&plan);
        let member = &r[r.find("def __init__").unwrap()..];
        assert!(member.contains("def __init__(self, x: int):"));
        assert!(!member.contains("count"));
    }
}
