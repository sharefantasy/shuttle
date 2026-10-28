//! implement-members (§9 E2): given an *abstract contract* — a Rust `trait`, a
//! Go `interface`, a TypeScript `interface`, or a Python abstract base (methods
//! decorated `@abstractmethod`) — generate a concrete implementation scaffold
//! for a target type, one stub per unimplemented member.
//!
//! This is a **pure AST projection**: each member's signature is copied
//! verbatim and wrapped in a placeholder body (`todo!()` / `panic` /
//! `throw new Error` / `raise NotImplementedError`). We never invent
//! signatures, and we never fabricate a body — the body is an explicit,
//! honest "not implemented yet" marker the reviewer must fill (§7). Because the
//! scaffold is a faithful, re-parseable projection inserted into the same file
//! immediately after the contract, the plan is High confidence.
//!
//! Members that already carry a body (Rust `fn ... { }` default methods, Python
//! methods with no `@abstractmethod`) are *not* stubbed — re-declaring them
//! would shadow a working default, which we refuse to guess about (§7).

use shuttle_nav::Confidence;

use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

use tree_sitter::Node;

/// Generate a concrete implementation scaffold for `target` that implements the
/// abstract contract `interface` defined in `source`. The scaffold is inserted
/// into `path` immediately after the contract's definition. Fails if the
/// contract is not found / not unique, or if it declares no members to stub.
pub fn implement_members(
    path: &str,
    source: &str,
    interface: &str,
    target: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();

    let contract = find_named_container(root, lang, interface, source)?;
    let block = contract
        .utf8_text(source.as_bytes())
        .map_err(|e| RefactorError::Parse(e.to_string()))?
        .to_string();

    let scaffold = render_scaffold(lang, &contract, interface, target, source)?;

    // Anchor on the contract's exact source block (guaranteed unique for a
    // top-level item) and re-emit it followed by the scaffold. This is the same
    // deterministic same-file insertion trick E4 uses, so the range never has
    // to be guessed (§7).
    let replacement = format!("{block}\n\n{scaffold}");
    let ops = vec![search_replace(
        path,
        block,
        replacement,
        contract.start_position().row + 1,
    )];
    let sites = vec![format!(
        "{path}:{} (scaffold `{target}` implementing `{interface}`)",
        contract.end_position().row + 1
    )];
    let warnings = vec![format!(
        "each member of `{interface}` was stubbed with a placeholder body — fill in the real \
         implementation before use"
    )];

    let intent = format!("implement-members: {target} implements {interface} ({path})");
    let plan = plan_from_ops(RefactorKind::ImplementMembers, path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::ImplementMembers,
        plan,
        signature: Some(format!("{target}: {interface}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::High,
        warnings,
        sites,
    })
}

/// Locate the unique top-level container named `name` (a trait / interface /
/// class / type declaration). Rejects 0 or >1 matches (§7).
fn find_named_container<'a>(
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
            "`{name}` is not defined as a trait/interface/class in this file"
        ))),
        1 => Ok(hits[0]),
        _ => Err(RefactorError::Rejected(format!(
            "`{name}` is defined {} times in this file — ambiguous",
            hits.len()
        ))),
    }
}

/// Best-effort name of a container node (same rule as extract-to-new-file).
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

/// Render the full implementation scaffold for one language.
fn render_scaffold(
    lang: Lang,
    contract: &Node,
    interface: &str,
    target: &str,
    source: &str,
) -> Result<String, RefactorError> {
    let stubs = match lang {
        Lang::Rust => rust_stubs(contract, source),
        Lang::Go => go_stubs(contract, target, source),
        Lang::TypeScript => ts_stubs(contract, source),
        Lang::Python => python_stubs(contract, source),
        Lang::JavaScript => {
            return Err(RefactorError::Rejected(
                "JavaScript has no interface/abstract-method contract to implement".into(),
            ))
        }
    };
    if stubs.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{interface}` declares no unimplemented members to stub"
        )));
    }
    Ok(match lang {
        Lang::Rust => format!(
            "impl {interface} for {target} {{\n{}\n}}",
            stubs.join("\n\n")
        ),
        Lang::Go => stubs.join("\n\n"),
        Lang::TypeScript => format!(
            "class {target} implements {interface} {{\n{}\n}}",
            stubs.join("\n\n")
        ),
        Lang::Python => format!("class {target}({interface}):\n{}", stubs.join("\n")),
        Lang::JavaScript => unreachable!(),
    })
}

/// Rust: each `function_signature_item` (no body) becomes `<sig> { todo!() }`.
/// Default methods (`function_item`, already have a body) are left alone.
fn rust_stubs(contract: &Node, source: &str) -> Vec<String> {
    let Some(body) = contract.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        if ch.kind() == "function_signature_item" {
            if let Ok(t) = ch.utf8_text(source.as_bytes()) {
                let sig = t.trim().trim_end_matches(';').trim_end();
                out.push(format!("    {sig} {{\n        todo!()\n    }}"));
            }
        }
    }
    out
}

/// Go: each `method_elem` in the interface becomes a method on `target` with a
/// `panic(...)` body. The receiver is a value receiver named after the type's
/// first letter (lowercased), matching Go convention.
fn go_stubs(contract: &Node, target: &str, source: &str) -> Vec<String> {
    let recv = target
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase().to_string())
        .unwrap_or_else(|| "r".into());
    let mut out = Vec::new();
    // contract is a `type_declaration`; descend type_spec → interface_type.
    let iface = descend_to_kind(contract, "interface_type");
    let Some(iface) = iface else {
        return out;
    };
    let mut c = iface.walk();
    for ch in iface.children(&mut c) {
        if ch.kind() == "method_elem" {
            if let Ok(t) = ch.utf8_text(source.as_bytes()) {
                let name = ch
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .unwrap_or("method");
                out.push(format!(
                    "func ({recv} {target}) {} {{\n\tpanic(\"not implemented: {name}\")\n}}",
                    t.trim()
                ));
            }
        }
    }
    out
}

/// TypeScript: each `method_signature` becomes a method with a `throw` body.
fn ts_stubs(contract: &Node, source: &str) -> Vec<String> {
    let Some(body) = contract.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        if ch.kind() == "method_signature" {
            if let Ok(t) = ch.utf8_text(source.as_bytes()) {
                let name = ch
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .unwrap_or("method");
                out.push(format!(
                    "  {} {{\n    throw new Error(\"not implemented: {name}\");\n  }}",
                    t.trim()
                ));
            }
        }
    }
    out
}

/// Python: each method decorated with `@abstractmethod` becomes a stub whose
/// body is `raise NotImplementedError`. Concrete (non-abstract) methods are
/// skipped — overriding a working method would be a guess (§7).
fn python_stubs(contract: &Node, source: &str) -> Vec<String> {
    let Some(body) = contract.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut c = body.walk();
    for ch in body.children(&mut c) {
        // Only `@abstractmethod`-decorated definitions are stubbed.
        if ch.kind() == "decorated_definition" && has_abstractmethod(&ch, source) {
            if let Some(def) = ch.child_by_field_name("definition") {
                if let Some(header) = python_def_header(&def, source) {
                    out.push(format!("    {header}\n        raise NotImplementedError"));
                }
            }
        }
    }
    out
}

/// Does a `decorated_definition` carry an `@abstractmethod` decorator?
fn has_abstractmethod(dec: &Node, source: &str) -> bool {
    let mut c = dec.walk();
    for ch in dec.children(&mut c) {
        if ch.kind() == "decorator" {
            if let Ok(t) = ch.utf8_text(source.as_bytes()) {
                if t.contains("abstractmethod") {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract a Python `def ...:` header (up to but excluding the body block).
fn python_def_header(def: &Node, source: &str) -> Option<String> {
    let body = def.child_by_field_name("body")?;
    let header = &source[def.start_byte()..body.start_byte()];
    Some(header.trim_end().to_string())
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
    fn rust_trait_scaffold() {
        let src = "trait Greeter {\n    fn hello(&self, name: &str) -> String;\n    fn bye(&self);\n    fn done(&self) -> bool { true }\n}\n";
        let plan = implement_members("lib.rs", src, "Greeter", "Robot").unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        let r = replacement(&plan);
        assert!(r.contains("impl Greeter for Robot {"));
        assert!(r.contains("fn hello(&self, name: &str) -> String {\n        todo!()\n    }"));
        assert!(r.contains("fn bye(&self) {\n        todo!()\n    }"));
        // The default method `done` must NOT be re-stubbed in the scaffold.
        let scaffold = &r[r.find("impl Greeter for Robot").unwrap()..];
        assert!(!scaffold.contains("fn done"));
    }

    #[test]
    fn go_interface_scaffold() {
        let src = "type Greeter interface {\n\tHello(name string) string\n\tBye()\n}\n";
        let plan = implement_members("g.go", src, "Greeter", "Robot").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("func (r Robot) Hello(name string) string {"));
        assert!(r.contains("panic(\"not implemented: Hello\")"));
        assert!(r.contains("func (r Robot) Bye() {"));
    }

    #[test]
    fn ts_interface_scaffold() {
        let src = "interface Greeter {\n  hello(name: string): string;\n  bye(): void;\n}\n";
        let plan = implement_members("g.ts", src, "Greeter", "Robot").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("class Robot implements Greeter {"));
        assert!(r.contains("hello(name: string): string {"));
        assert!(r.contains("throw new Error(\"not implemented: hello\");"));
    }

    #[test]
    fn python_abc_scaffold() {
        let src = "class Greeter(ABC):\n    @abstractmethod\n    def hello(self, name: str) -> str: ...\n    @abstractmethod\n    def bye(self): ...\n    def concrete(self):\n        return 1\n";
        let plan = implement_members("g.py", src, "Greeter", "Robot").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("class Robot(Greeter):"));
        assert!(r.contains("def hello(self, name: str) -> str:\n        raise NotImplementedError"));
        assert!(r.contains("def bye(self):\n        raise NotImplementedError"));
        // Concrete method must not be stubbed in the scaffold.
        let scaffold = &r[r.find("class Robot(Greeter):").unwrap()..];
        assert!(!scaffold.contains("def concrete"));
    }

    #[test]
    fn rejects_unknown_interface() {
        let src = "trait Greeter {\n    fn hello(&self);\n}\n";
        let err = implement_members("lib.rs", src, "Ghost", "Robot").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_contract_with_no_members() {
        let src = "trait Empty {\n}\n";
        let err = implement_members("lib.rs", src, "Empty", "Robot").unwrap_err();
        match err {
            RefactorError::Rejected(m) => assert!(m.contains("no unimplemented members")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejects_javascript() {
        let src = "class Foo {}\n";
        let err = implement_members("a.js", src, "Foo", "Bar").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }
}
