//! extract-constant / inline-constant (a §4-family pair, structural & High-
//! confidence for the literal-only subset).
//!
//! * **extract-constant** promotes a *literal* expression (a magic number,
//!   string, bool, or char) into a module-level named constant and rewrites
//!   every structurally-identical literal occurrence in the file to reference
//!   the new name. Because matching is done over complete tree-sitter *literal
//!   nodes* (never raw text), a `4` inside `40`, inside a comment, or inside a
//!   larger string never false-matches — the honesty guarantee of §7 holds
//!   without guessing.
//! * **inline-constant** is the inverse: given a cursor on a single-definition
//!   module constant, it substitutes the constant's value at every reference
//!   and deletes the declaration. It is the constant-aware sibling of
//!   inline-variable (it additionally parses Rust `const NAME: TY = v;` /
//!   `static` declarations that inline-variable's `= rhs` splitter cannot).
//!
//! Rust is the one language whose `const` requires an explicit type. For string
//! / bool / char literals the type is unambiguous (`&str` / `bool` / `char`) and
//! stays High. For integer / float literals the width is genuinely unknowable
//! from the literal alone, so we annotate a best-effort `i32` / `f64` and
//! downgrade to Medium with a warning rather than pretend certainty (§7).

use tree_sitter::Node;

use shuttle_nav::Confidence;

use crate::extract_method::line_start_offsets;
use crate::locals::{Lang, LocalsIndex};
use crate::{
    plan_from_ops, rewrite_spans, search_replace, RefactorError, RefactorKind, RefactorPlan, Span,
};

/// True for literal node kinds across the five grammars. Deliberately excludes
/// identifiers / member expressions: extract-constant promotes *values*, not
/// arbitrary sub-expressions (that is extract-variable's job).
fn is_literal_kind(kind: &str) -> bool {
    matches!(
        kind,
        // python / javascript / typescript
        "integer"
            | "float"
            | "string"
            | "true"
            | "false"
            | "none"
            | "null"
            | "number"
            // rust
            | "integer_literal"
            | "float_literal"
            | "string_literal"
            | "boolean_literal"
            | "char_literal"
            | "raw_string_literal"
            // go
            | "int_literal"
            | "interpreted_string_literal"
    )
}

/// Validate an identifier used as a fresh constant / parameter name.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Best-effort Rust type annotation for a promoted literal, plus whether it was
/// a *guess* (width-ambiguous integer / float). String / bool / char are exact.
fn rust_const_type(kind: &str) -> (&'static str, bool) {
    match kind {
        "string_literal" | "raw_string_literal" => ("&str", false),
        "char_literal" => ("char", false),
        "boolean_literal" | "true" | "false" => ("bool", false),
        "float_literal" => ("f64", true),
        _ => ("i32", true), // integer_literal and anything else numeric
    }
}

/// Promote the literal selected at `line:[start_col,end_col)` (1-based line,
/// 0-based byte columns) into a module-level constant `const_name`, rewriting
/// every structurally-identical literal in the file.
pub fn extract_constant(
    path: &str,
    source: &str,
    line: usize,
    start_col: usize,
    end_col: usize,
    const_name: &str,
) -> Result<RefactorPlan, RefactorError> {
    let const_name = const_name.trim();
    if !valid_name(const_name) {
        return Err(RefactorError::Rejected(format!(
            "`{const_name}` is not a valid constant name"
        )));
    }

    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;
    let root = idx.tree.root_node();
    let starts = line_start_offsets(source);

    // Resolve the selection to an absolute byte range and require it to name a
    // single, complete literal node (never guess a partial selection).
    let base = *starts
        .get(line - 1)
        .ok_or_else(|| RefactorError::Rejected(format!("line {line} out of range")))?;
    if start_col >= end_col {
        return Err(RefactorError::Rejected("empty selection".into()));
    }
    let abs_start = base + start_col;
    let abs_end = base + end_col;
    let node = root
        .descendant_for_byte_range(abs_start, abs_end)
        .ok_or_else(|| RefactorError::Rejected("no syntax node at selection".into()))?;
    let node_text = node
        .utf8_text(source.as_bytes())
        .map_err(|_| RefactorError::Rejected("selection is not valid utf-8".into()))?;
    let expr = node_text.trim().to_string();
    if node.start_byte() != abs_start || node.end_byte() != abs_end {
        return Err(RefactorError::Rejected(
            "selection does not cover a complete node — select the whole literal".into(),
        ));
    }
    if !is_literal_kind(node.kind()) {
        return Err(RefactorError::Rejected(format!(
            "selection is `{}`, not a literal — extract-constant promotes literal values only",
            node.kind()
        )));
    }

    // Every structurally-identical literal occurrence in the file.
    let mut occ: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.is_named()
            && is_literal_kind(n.kind())
            && n.utf8_text(source.as_bytes())
                .map(|t| t.trim() == expr)
                .unwrap_or(false)
        {
            occ.push(n);
            continue; // don't descend into a matched literal
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    if occ.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "no literal `{expr}` found"
        )));
    }
    occ.sort_by_key(|n| n.start_byte());

    // Build the module-level declaration, tracking Rust type-guess honesty.
    let mut warnings: Vec<String> = Vec::new();
    let mut confidence = Confidence::High;
    let decl = match lang {
        Lang::Python => format!("{const_name} = {expr}"),
        Lang::JavaScript | Lang::TypeScript => format!("const {const_name} = {expr};"),
        Lang::Go => format!("const {const_name} = {expr}"),
        Lang::Rust => {
            let (ty, guessed) = rust_const_type(node.kind());
            if guessed {
                confidence = Confidence::Medium;
                warnings.push(format!(
                    "Rust const requires a type; annotated `{ty}` as a best-effort guess for \
                     `{expr}` (the literal's width is ambiguous) — adjust if the context needs a \
                     different width"
                ));
            }
            format!("const {const_name}: {ty} = {expr};")
        }
    };

    // Occurrence rewrites, grouped per line.
    let spans: Vec<Span> = occ
        .iter()
        .map(|n| {
            let sp = n.start_position();
            Span {
                line0: sp.row,
                col: sp.column,
                len: n.end_byte() - n.start_byte(),
            }
        })
        .collect();
    let changed = rewrite_spans(source, spans, const_name);

    // Insertion anchor: the top-level item containing the first occurrence. The
    // declaration is prepended before that item so the constant sits at module
    // scope, just above first use.
    let first = occ[0];
    let anchor = top_level_ancestor(root, first);
    let anchor_line0 = anchor.start_position().row; // 0-based
    let lines: Vec<&str> = source.split('\n').collect();
    let anchor_text = lines
        .get(anchor_line0)
        .ok_or_else(|| RefactorError::Rejected("anchor line missing".into()))?
        .to_string();

    let mut ops = Vec::new();
    let mut sites = Vec::new();

    // If the anchor line is itself a rewritten occurrence line, merge the
    // declaration prepend into that line's rewrite to avoid a two-op conflict.
    let anchor_line1 = anchor_line0 + 1;
    let mut merged_anchor = false;
    for (ln, old, new) in &changed {
        if *ln == anchor_line1 {
            let replace = format!("{decl}\n{new}");
            ops.push(search_replace(path, old.clone(), replace, *ln));
            sites.push(format!(
                "{path}:{ln} (extract-constant {const_name} + declare)"
            ));
            merged_anchor = true;
        } else {
            ops.push(search_replace(path, old.clone(), new.clone(), *ln));
            sites.push(format!("{path}:{ln} (use {const_name})"));
        }
    }
    if !merged_anchor {
        let replace = format!("{decl}\n{anchor_text}");
        ops.push(search_replace(path, anchor_text, replace, anchor_line1));
        sites.push(format!("{path}:{anchor_line1} (declare {const_name})"));
    }

    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::ExtractConstant,
            plan: plan_from_ops(
                RefactorKind::ExtractConstant,
                path,
                ops,
                &format!(
                    "extract-constant {const_name} = {expr} ({} site(s))",
                    occ.len()
                ),
            ),
            signature: Some(decl),
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

/// The direct child of `root` that (transitively) contains `node`.
fn top_level_ancestor<'a>(root: Node<'a>, node: Node<'a>) -> Node<'a> {
    let mut cur = node;
    while let Some(p) = cur.parent() {
        if p.id() == root.id() {
            return cur;
        }
        cur = p;
    }
    cur
}

/// Inline the module constant whose declaration/reference is under the cursor:
/// substitute its value at every reference and delete the declaration. Rejects
/// reassigned constants (more than one definition) — never guess which wins.
pub fn inline_constant(
    path: &str,
    source: &str,
    line: usize,
    col: usize,
    _reserved: (),
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;

    let name = idx
        .definitions
        .iter()
        .chain(idx.references.iter())
        .find(|o| o.line == line && o.col <= col && col < o.col + o.name.len())
        .map(|o| o.name.clone())
        .ok_or_else(|| RefactorError::Rejected(format!("no identifier at {line}:{col}")))?;

    let defs: Vec<&crate::locals::Occurrence> =
        idx.definitions.iter().filter(|d| d.name == name).collect();
    if defs.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{name}` has no definition in this file"
        )));
    }
    if defs.len() > 1 {
        return Err(RefactorError::Rejected(format!(
            "`{name}` is defined {} times — inline would change semantics",
            defs.len()
        )));
    }
    let def = defs[0];
    let def_scope = idx
        .scope_range(def.scope_id)
        .ok_or_else(|| RefactorError::Rejected("definition scope unknown".into()))?;

    let def_line = source
        .split('\n')
        .nth(def.line - 1)
        .ok_or_else(|| RefactorError::Rejected("definition line missing".into()))?;
    let rhs = const_rhs(def_line, &name).ok_or_else(|| {
        RefactorError::Rejected(format!("could not parse a constant value for `{name}`"))
    })?;

    let refs: Vec<&crate::locals::Occurrence> = idx
        .references_bound_to(&name, def_scope)
        .into_iter()
        .filter(|r| r.start_byte != def.start_byte)
        .collect();
    if refs.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{name}` is never read; nothing to inline"
        )));
    }

    let spans: Vec<Span> = refs
        .iter()
        .map(|r| Span {
            line0: r.line - 1,
            col: r.col,
            len: r.name.len(),
        })
        .collect();
    let changed = rewrite_spans(source, spans, &rhs);

    let mut ops = Vec::new();
    let mut sites = Vec::new();
    for (ln, old, new) in &changed {
        ops.push(search_replace(path, old.clone(), new.clone(), *ln));
        sites.push(format!("{path}:{ln} (inline {name})"));
    }
    let del_search = format!("{def_line}\n");
    ops.push(search_replace(path, del_search, String::new(), def.line));
    sites.push(format!("{path}:{} (delete const)", def.line));

    Ok(crate::verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::InlineConstant,
            plan: plan_from_ops(
                RefactorKind::InlineConstant,
                path,
                ops,
                &format!("inline-constant {name}"),
            ),
            signature: Some(format!("{name} = {rhs}")),
            params: Vec::new(),
            returns: Vec::new(),
            confidence: Confidence::High,
            warnings: Vec::new(),
            sites,
        },
        path,
        source,
    ))
}

/// Extract the value of a constant declaration line. Handles the plain
/// `NAME = v` (py/js/go/ts) form *and* the type-annotated Rust/Go forms
/// `const NAME: TY = v;` / `static NAME: TY = v;` / `const NAME TY = v` that
/// inline-variable's `= rhs` splitter would mis-handle.
fn const_rhs(line: &str, name: &str) -> Option<String> {
    let eq = line.find('=')?;
    let lhs = &line[..eq];
    // The LHS must mention `name` as a whole token (splitting on the delimiters
    // that surround a declared name: whitespace, `:`, `,`).
    let names_lhs = lhs.split(|c: char| c.is_whitespace() || c == ':' || c == ',');
    if !names_lhs.into_iter().any(|w| w == name) {
        return None;
    }
    let mut rhs = line[eq + 1..].trim().to_string();
    if let Some(stripped) = rhs.strip_suffix(';') {
        rhs = stripped.trim_end().to_string();
    }
    if rhs.is_empty() {
        return None;
    }
    Some(rhs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- extract-constant ---------------------------------------------------

    #[test]
    fn python_extracts_and_rewrites_all_occurrences() {
        // Two identical magic numbers `86400` on different lines.
        let src = "def a():\n    return x * 86400\n\ndef b():\n    return y + 86400\n";
        // Select the first `86400`: line 2, cols of "86400" within "    return x * 86400".
        let l2 = "    return x * 86400";
        let sc = l2.find("86400").unwrap();
        let out = extract_constant("m.py", src, 2, sc, sc + 5, "SECONDS_PER_DAY").unwrap();
        assert_eq!(out.confidence, Confidence::High);
        // Declaration prepended, both uses rewritten.
        let joined = out.sites.join("\n");
        assert!(joined.contains("SECONDS_PER_DAY"));
        // The plan should contain the declaration text.
        assert_eq!(out.signature.as_deref(), Some("SECONDS_PER_DAY = 86400"));
        // 2 occurrences → at least 2 ops (one may be merged with the declare).
        assert!(out.plan.ops.len() >= 2);
    }

    #[test]
    fn rejects_non_literal_selection() {
        let src = "def a():\n    return foo + 1\n";
        let l2 = "    return foo + 1";
        let sc = l2.find("foo").unwrap();
        let err = extract_constant("m.py", src, 2, sc, sc + 3, "K").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_partial_selection() {
        // Selecting only `864` out of `86400` covers no complete node.
        let src = "def a():\n    return 86400\n";
        let l2 = "    return 86400";
        let sc = l2.find("86400").unwrap();
        let err = extract_constant("m.py", src, 2, sc, sc + 3, "K").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rust_string_stays_high_with_ref_str() {
        let src = "fn a() -> &'static str {\n    \"hello\"\n}\n";
        let l2 = "    \"hello\"";
        let sc = l2.find('"').unwrap();
        let out = extract_constant("m.rs", src, 2, sc, sc + 7, "GREETING").unwrap();
        assert_eq!(out.confidence, Confidence::High);
        assert_eq!(
            out.signature.as_deref(),
            Some("const GREETING: &str = \"hello\";")
        );
    }

    #[test]
    fn rust_integer_downgrades_to_medium_with_warning() {
        let src = "fn a() -> i32 {\n    42\n}\n";
        let l2 = "    42";
        let sc = l2.find("42").unwrap();
        let out = extract_constant("m.rs", src, 2, sc, sc + 2, "ANSWER").unwrap();
        assert_eq!(out.confidence, Confidence::Medium);
        assert!(out.warnings.iter().any(|w| w.contains("i32")));
        assert_eq!(out.signature.as_deref(), Some("const ANSWER: i32 = 42;"));
    }

    #[test]
    fn go_untyped_const() {
        let src = "package p\n\nfunc a() int {\n\treturn 42\n}\n";
        let l4 = "\treturn 42";
        let sc = l4.find("42").unwrap();
        let out = extract_constant("m.go", src, 4, sc, sc + 2, "Answer").unwrap();
        assert_eq!(out.confidence, Confidence::High);
        assert_eq!(out.signature.as_deref(), Some("const Answer = 42"));
    }

    #[test]
    fn rejects_invalid_name() {
        let src = "def a():\n    return 1\n";
        let err = extract_constant("m.py", src, 2, 11, 12, "1bad").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    // --- inline-constant ----------------------------------------------------

    #[test]
    fn python_inlines_module_constant() {
        let src = "MAX = 100\n\ndef a():\n    return MAX + MAX\n";
        // Cursor on the definition `MAX` at line 1 col 0.
        let out = inline_constant("m.py", src, 1, 0, ()).unwrap();
        assert_eq!(out.confidence, Confidence::High);
        // Delete def + rewrite the one use-line (two refs collapse to one line).
        assert!(out.sites.iter().any(|s| s.contains("delete const")));
        assert!(out.sites.iter().any(|s| s.contains("inline MAX")));
    }

    #[test]
    fn rust_inlines_typed_const() {
        let src = "const MAX: i32 = 100;\nfn a() -> i32 {\n    MAX + 1\n}\n";
        let out = inline_constant("m.rs", src, 1, 6, ()).unwrap();
        assert_eq!(out.confidence, Confidence::High);
        assert_eq!(out.signature.as_deref(), Some("MAX = 100"));
    }

    #[test]
    fn inline_rejects_unused_constant() {
        let src = "MAX = 100\n\ndef a():\n    return 1\n";
        let err = inline_constant("m.py", src, 1, 0, ()).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn const_rhs_parses_rust_typed_form() {
        assert_eq!(
            const_rhs("const MAX: i32 = 100;", "MAX").as_deref(),
            Some("100")
        );
        assert_eq!(
            const_rhs("static X: &str = \"hi\";", "X").as_deref(),
            Some("\"hi\"")
        );
        assert_eq!(const_rhs("MAX = 100", "MAX").as_deref(), Some("100"));
        assert_eq!(const_rhs("const MAX = 100;", "MAX").as_deref(), Some("100"));
        assert_eq!(const_rhs("y = 5", "MAX"), None);
    }
}
