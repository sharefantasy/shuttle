//! shuttle-refactor — the daemonless tree-sitter `locals` refactor engine
//! .
//!
//! Decision layer only: it resolves function-local scope + name bindings via
//! native tree-sitter `locals` (see [`locals`]) and lowers each refactor into an
//! [`shuttle_protocol::EditPlan`] of content-anchored `SearchReplace` ops. Writing is
//! reused from `shuttle-editcore`; confidence tagging is reused from `shuttle-nav`.
//!
//! Batch 1 (this file): `extract-variable`, `inline-variable`, `rename-local`
//! for python / javascript — the three refactors that need no type annotations
//! and lean directly on locals name-binding.

pub mod batch;
pub mod change_signature;
pub mod constant;
pub mod create_from_usage;
pub mod dataflow;
pub mod extract_method;
pub mod extract_to_new_file;
#[cfg(test)]
mod fuzz;
pub mod generate_boilerplate;
pub mod implement_members;
pub mod inline_method;
pub mod inline_parameter;
pub mod introduce_parameter;
pub mod locals;
pub mod move_function;
pub mod new_module_wiring;
pub mod outcome;
pub mod query;
pub mod recipe;
pub mod record_projection;
pub mod remove_parameter;
pub mod rename_symbol;
pub mod scaffold_test;
/// Optional TS type bridge (docs §2.2/§5/§6). Compiled only under the
/// `ts-morph` feature; the default daemonless build omits it entirely.
#[cfg(feature = "ts-morph")]
pub mod ts_morph;
pub mod type_closure;
pub mod typed_impact;
pub mod verify;

use serde::{Deserialize, Serialize};
use shuttle_nav::Confidence;
use shuttle_protocol::{EditOp, EditPlan};

pub use batch::{apply_batch, compose_ops, BatchError, BatchOutcome, BatchStep, Vfs};
pub use change_signature::{change_signature, NewParam};
pub use constant::{extract_constant, inline_constant};
pub use create_from_usage::create_from_usage;
pub use dataflow::{dataflow, DataFlow, DataFlowOptions, FlowNode, FlowSink, UnprovableStore};
pub use extract_method::{extract_method, extract_method_with_symbols};
pub use extract_to_new_file::extract_to_new_file;
pub use generate_boilerplate::generate_boilerplate;
pub use implement_members::implement_members;
pub use inline_method::inline_method;
pub use inline_parameter::inline_parameter;
pub use introduce_parameter::introduce_parameter;
pub use locals::{Lang, LocalsIndex};
pub use move_function::move_function;
pub use new_module_wiring::new_module_wiring;
pub use query::{select, QueryCapture};
pub use recipe::{
    expand, find as find_recipe, lint_recipe, lint_registry, BatchSpec, BatchStepSpec, Recipe,
    RecipeError, RecipeParam, RecipeStep, BATCHABLE_OPS, RECIPES,
};
pub use record_projection::record_projection;
pub use remove_parameter::remove_parameter;
pub use rename_symbol::rename_symbol;
pub use scaffold_test::scaffold_test;
pub use typed_impact::{typed_impact, DroppedCaller, TypedImpact};

#[derive(Debug, thiserror::Error)]
pub enum RefactorError {
    #[error("unsupported language for path: {0}")]
    UnsupportedLang(String),
    #[error("tree-sitter parse error: {0}")]
    Parse(String),
    #[error("refactor rejected: {0}")]
    Rejected(String),
    #[error("io error: {0}")]
    Io(String),
}

/// Which refactor produced a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefactorKind {
    ExtractVariable,
    InlineVariable,
    RenameLocal,
    ExtractMethod,
    InlineMethod,
    ChangeSignature,
    IntroduceParameter,
    InlineParameter,
    ExtractConstant,
    InlineConstant,
    RemoveParameter,
    RenameSymbol,
    MoveFunction,
    ExtractToNewFile,
    ImplementMembers,
    GenerateBoilerplate,
    CreateFromUsage,
    ScaffoldTest,
    NewModuleWiring,
    RecordProjection,
}

/// A parameter of an extracted method (unused in batch 1, kept for the model).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    pub written: bool,
}

/// A return value of an extracted method (unused in batch 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReturnInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
}

/// A complete, self-contained refactor plan for the planner to review (§6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefactorPlan {
    pub op: RefactorKind,
    pub plan: EditPlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns: Vec<ReturnInfo>,
    pub confidence: Confidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Human-readable per-site preview lines (for the non-JSON CLI view).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<String>,
}

impl RefactorPlan {
    pub fn has_low_confidence(&self) -> bool {
        matches!(self.confidence, Confidence::Low)
    }
}

// ---------------------------------------------------------------------------
// Line-splice helpers: every batch-1 refactor is expressed as raw-text
// SearchReplace ops (deletes/inserts included via embedded newlines), matching
// shuttle-nav's line-precise convention so writing reuses `shuttle-editcore` unchanged.
// ---------------------------------------------------------------------------

/// One identifier occurrence to rewrite: (0-based line index, byte col, len).
pub(crate) struct Span {
    pub(crate) line0: usize,
    pub(crate) col: usize,
    pub(crate) len: usize,
}

/// Splice `new_text` into every span, grouped per line, and return
/// `(line_no_1based, old_line, new_line)` for each changed line.
pub(crate) fn rewrite_spans(
    source: &str,
    mut spans: Vec<Span>,
    new_text: &str,
) -> Vec<(usize, String, String)> {
    let lines: Vec<&str> = source.split('\n').collect();
    // group spans by line
    spans.sort_by_key(|s| (s.line0, s.col));
    let mut out: Vec<(usize, String, String)> = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        let line0 = spans[i].line0;
        let mut group: Vec<&Span> = Vec::new();
        while i < spans.len() && spans[i].line0 == line0 {
            group.push(&spans[i]);
            i += 1;
        }
        let Some(orig) = lines.get(line0) else {
            continue;
        };
        let bytes = orig.as_bytes();
        // splice right-to-left so earlier offsets stay valid
        let mut new_line = orig.to_string();
        let mut new_bytes: Vec<u8> = bytes.to_vec();
        group.sort_by_key(|s| s.col);
        for s in group.iter().rev() {
            if s.col + s.len <= new_bytes.len() {
                new_bytes.splice(s.col..s.col + s.len, new_text.bytes());
            }
        }
        if let Ok(rebuilt) = String::from_utf8(new_bytes) {
            new_line = rebuilt;
        }
        out.push((line0 + 1, orig.to_string(), new_line));
    }
    out
}

fn search_replace(path: &str, search: String, replace: String, line: usize) -> EditOp {
    EditOp::SearchReplace {
        path: path.to_string(),
        search,
        replace,
        line_hint: Some(line),
    }
}

fn plan_from_ops(op: RefactorKind, path: &str, ops: Vec<EditOp>, intent: &str) -> EditPlan {
    let _ = (op, path);
    EditPlan {
        version: shuttle_protocol::CURRENT_VERSION,
        intent: Some(intent.to_string()),
        ops,
    }
}

// ---------------------------------------------------------------------------
// rename-local (§4): pure locals binding. Rename the local variable governing
// `line:col` and every reference bound to the same definition, function-scoped.
// ---------------------------------------------------------------------------

/// Rename the local variable at `line` (1-based) / `col` (0-based byte) to
/// `new_name`, across the function scope only.
pub fn rename_local(
    path: &str,
    source: &str,
    line: usize,
    col: usize,
    new_name: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;

    // Find the occurrence (def or ref) at the cursor to learn the name.
    let target_name = idx
        .definitions
        .iter()
        .chain(idx.references.iter())
        .find(|o| o.line == line && o.col <= col && col < o.col + o.name.len())
        .map(|o| o.name.clone())
        .ok_or_else(|| RefactorError::Rejected(format!("no local identifier at {line}:{col}")))?;

    // Which definition governs the cursor? Pick the definition of `target_name`
    // whose scope tightly encloses the cursor byte.
    let cursor_byte = idx
        .references
        .iter()
        .chain(idx.definitions.iter())
        .find(|o| o.line == line && o.col == col)
        .map(|o| o.start_byte)
        .or_else(|| {
            idx.definitions
                .iter()
                .chain(idx.references.iter())
                .find(|o| o.name == target_name && o.line == line)
                .map(|o| o.start_byte)
        })
        .ok_or_else(|| RefactorError::Rejected("could not locate cursor byte".into()))?;

    let def = idx
        .definitions
        .iter()
        .filter(|d| d.name == target_name)
        .filter_map(|d| idx.scope_range(d.scope_id).map(|r| (d, r)))
        .filter(|(_, (s, e))| *s <= cursor_byte && cursor_byte < *e)
        // tightest enclosing scope wins
        .min_by_key(|(_, (s, e))| e - s)
        .map(|(d, r)| (d.clone(), r));

    let (def, def_scope) = match def {
        Some(x) => x,
        None => {
            return Err(RefactorError::Rejected(format!(
                "`{target_name}` has no local definition governing {line}:{col} \
                 (may be a global / cross-file symbol — use `shuttle rename`)"
            )));
        }
    };

    // Reject if the new name is already defined in the same scope (shadowing).
    let clash = idx
        .definitions
        .iter()
        .any(|d| d.name == new_name && idx.scope_range(d.scope_id) == Some(def_scope));

    // Collect the definition + all references bound to it.
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span {
        line0: def.line - 1,
        col: def.col,
        len: def.name.len(),
    });
    for r in idx.references_bound_to(&target_name, def_scope) {
        // skip the definition site if the query also captured it as a reference
        if r.start_byte == def.start_byte {
            continue;
        }
        spans.push(Span {
            line0: r.line - 1,
            col: r.col,
            len: r.name.len(),
        });
    }

    let changed = rewrite_spans(source, spans, new_name);
    if changed.is_empty() {
        return Err(RefactorError::Rejected("nothing to rename".into()));
    }

    let mut ops = Vec::new();
    let mut sites = Vec::new();
    for (ln, old, new) in &changed {
        ops.push(search_replace(path, old.clone(), new.clone(), *ln));
        sites.push(format!("{path}:{ln}"));
    }

    let mut warnings = Vec::new();
    let confidence = if clash {
        warnings.push(format!(
            "`{new_name}` is already defined in the same scope — rename would shadow/collide"
        ));
        Confidence::Low
    } else {
        Confidence::High
    };

    Ok(verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::RenameLocal,
            plan: plan_from_ops(
                RefactorKind::RenameLocal,
                path,
                ops,
                &format!("rename-local {target_name} -> {new_name}"),
            ),
            signature: None,
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

// ---------------------------------------------------------------------------
// inline-variable (§4): replace every reference to a single-assignment local
// with its defining right-hand side, then delete the definition.
// ---------------------------------------------------------------------------

/// Inline the local variable at `line`/`col`: substitute its RHS at every
/// reference and delete the assignment. Rejects if the variable is reassigned.
pub fn inline_variable(
    path: &str,
    source: &str,
    line: usize,
    col: usize,
    _new: (),
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    let idx = LocalsIndex::build(lang, source)?;

    let name = idx
        .definitions
        .iter()
        .chain(idx.references.iter())
        .find(|o| o.line == line && o.col <= col && col < o.col + o.name.len())
        .map(|o| o.name.clone())
        .ok_or_else(|| RefactorError::Rejected(format!("no local identifier at {line}:{col}")))?;

    // The definition(s) of `name`. Reassignment (>1 definition in same scope)
    // makes inlining unsafe → reject.
    let defs: Vec<&locals::Occurrence> =
        idx.definitions.iter().filter(|d| d.name == name).collect();
    if defs.is_empty() {
        return Err(RefactorError::Rejected(format!(
            "`{name}` has no local definition"
        )));
    }
    if defs.len() > 1 {
        return Err(RefactorError::Rejected(format!(
            "`{name}` is assigned {} times — inline would change semantics",
            defs.len()
        )));
    }
    let def = defs[0];
    let def_scope = idx
        .scope_range(def.scope_id)
        .ok_or_else(|| RefactorError::Rejected("definition scope unknown".into()))?;

    // Extract the RHS text from the definition's line: `<name> = <rhs>` (py) or
    // `const/let/var <name> = <rhs>;` (js). Keep it simple and structural.
    let def_line = source
        .split('\n')
        .nth(def.line - 1)
        .ok_or_else(|| RefactorError::Rejected("definition line missing".into()))?;
    let rhs = extract_rhs(def_line, &name).ok_or_else(|| {
        RefactorError::Rejected(format!("could not parse a simple `= rhs` for `{name}`"))
    })?;

    // Replace each bound reference (not the def) with the RHS.
    let refs: Vec<&locals::Occurrence> = idx
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

    // Delete the whole definition line, including its trailing newline.
    let del_search = format!("{def_line}\n");
    ops.push(search_replace(path, del_search, String::new(), def.line));
    sites.push(format!("{path}:{} (delete def)", def.line));

    Ok(verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::InlineVariable,
            plan: plan_from_ops(
                RefactorKind::InlineVariable,
                path,
                ops,
                &format!("inline-variable {name}"),
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

/// Parse the right-hand side of a single-assignment line. Handles
/// `x = expr` / `const x = expr;` / `let x = expr` etc.
pub(crate) fn extract_rhs(line: &str, name: &str) -> Option<String> {
    let eq = line.find('=')?;
    // ensure the LHS is exactly `name` (ignoring keywords/whitespace)
    let lhs = &line[..eq];
    if !lhs.split_whitespace().any(|w| w == name) {
        return None;
    }
    let mut rhs = line[eq + 1..].trim().to_string();
    // strip a trailing `;` (js) and surrounding whitespace
    if let Some(stripped) = rhs.strip_suffix(';') {
        rhs = stripped.trim_end().to_string();
    }
    if rhs.is_empty() {
        return None;
    }
    Some(rhs)
}

// ---------------------------------------------------------------------------
// extract-variable (§4): hoist a selected expression into a fresh local, insert
// its declaration above the enclosing statement, and replace the selection with
// the new name.
// ---------------------------------------------------------------------------

/// Extract the expression at byte range `[start_byte, end_byte)` on `line`
/// (1-based) into a new local named `new_name`.
pub fn extract_variable(
    path: &str,
    source: &str,
    line: usize,
    start_col: usize,
    end_col: usize,
    new_name: &str,
) -> Result<RefactorPlan, RefactorError> {
    let lang = Lang::from_path(path).ok_or_else(|| RefactorError::UnsupportedLang(path.into()))?;
    // Parse just to validate the language / grammar loads; we work on the selected
    // line text structurally.
    let _idx = LocalsIndex::build(lang, source)?;

    let lines: Vec<&str> = source.split('\n').collect();
    let orig = lines
        .get(line - 1)
        .ok_or_else(|| RefactorError::Rejected(format!("line {line} out of range")))?;
    let bytes = orig.as_bytes();
    if end_col > bytes.len() || start_col >= end_col {
        return Err(RefactorError::Rejected("selection out of range".into()));
    }
    let expr = std::str::from_utf8(&bytes[start_col..end_col])
        .map_err(|_| RefactorError::Rejected("selection is not valid utf-8".into()))?
        .to_string();
    if expr.trim().is_empty() {
        return Err(RefactorError::Rejected("empty selection".into()));
    }

    // Indentation of the enclosing statement = leading whitespace of the line.
    let indent: String = orig.chars().take_while(|c| c.is_whitespace()).collect();
    let decl = match lang {
        Lang::Python => format!("{indent}{new_name} = {expr}"),
        // js / ts / rust use `const`/`let`; go uses `:=`. All are valid
        // single-line declarations that hoist above the enclosing statement.
        Lang::JavaScript | Lang::TypeScript => format!("{indent}const {new_name} = {expr};"),
        Lang::Rust => format!("{indent}let {new_name} = {expr};"),
        Lang::Go => format!("{indent}{new_name} := {expr}"),
    };

    // New line = original with the selected expr replaced by the new name.
    let mut new_line_bytes = bytes.to_vec();
    new_line_bytes.splice(start_col..end_col, new_name.bytes());
    let new_line = String::from_utf8(new_line_bytes)
        .map_err(|_| RefactorError::Rejected("rewrite produced invalid utf-8".into()))?;

    // Single SearchReplace: prepend the declaration line, then the rewritten
    // statement line. Embedded newline expresses the insert deterministically.
    let search = orig.to_string();
    let replace = format!("{decl}\n{new_line}");
    let ops = vec![search_replace(path, search, replace, line)];

    Ok(verify::verify_reparse(
        RefactorPlan {
            op: RefactorKind::ExtractVariable,
            plan: plan_from_ops(
                RefactorKind::ExtractVariable,
                path,
                ops,
                &format!("extract-variable {new_name}"),
            ),
            signature: Some(decl.trim().to_string()),
            params: Vec::new(),
            returns: Vec::new(),
            confidence: Confidence::High,
            warnings: Vec::new(),
            sites: vec![format!("{path}:{line}")],
        },
        path,
        source,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_from_path() {
        assert_eq!(Lang::from_path("a.py"), Some(Lang::Python));
        assert_eq!(Lang::from_path("a.js"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path("a.mjs"), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path("a.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("a.go"), Some(Lang::Go));
        assert_eq!(Lang::from_path("a.ts"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_path("a.txt"), None);
    }

    #[test]
    fn python_locals_parse() {
        let src = "def f(a):\n    b = a + 1\n    return b\n";
        let idx = LocalsIndex::build(Lang::Python, src).unwrap();
        assert!(idx.definitions.iter().any(|d| d.name == "b"));
        assert!(idx.references.iter().any(|r| r.name == "b"));
    }

    #[test]
    fn rename_local_python() {
        // rename `b` (defined line 2, col 4) -> `total`
        let src = "def f(a):\n    b = a + 1\n    return b\n";
        let plan = rename_local("m.py", src, 2, 4, "total").unwrap();
        assert_eq!(plan.op, RefactorKind::RenameLocal);
        assert!(matches!(plan.confidence, Confidence::High));
        // two lines change: the def and the return
        assert_eq!(plan.plan.ops.len(), 2);
    }

    #[test]
    fn rename_local_shadow_is_low() {
        // `a` already a parameter; renaming `b`->`a` collides in the same scope
        let src = "def f(a):\n    b = a + 1\n    return b\n";
        let plan = rename_local("m.py", src, 2, 4, "a").unwrap();
        assert!(matches!(plan.confidence, Confidence::Low));
        assert!(!plan.warnings.is_empty());
    }

    #[test]
    fn inline_variable_python() {
        let src = "def f(a):\n    b = a + 1\n    return b + b\n";
        let plan = inline_variable("m.py", src, 2, 4, ()).unwrap();
        assert_eq!(plan.op, RefactorKind::InlineVariable);
        // both `b` refs replaced (1 changed line) + 1 delete op
        assert!(plan.plan.ops.len() >= 2);
        assert_eq!(plan.signature.as_deref(), Some("b = a + 1"));
    }

    #[test]
    fn inline_variable_reassigned_rejected() {
        let src = "def f(a):\n    b = a + 1\n    b = b + 2\n    return b\n";
        let err = inline_variable("m.py", src, 2, 4, ()).unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn extract_variable_python() {
        // line 2: `    return a + 1` — extract `a + 1` (cols 11..16) as `s`
        let src = "def f(a):\n    return a + 1\n";
        let line2 = "    return a + 1";
        let start = line2.find("a + 1").unwrap();
        let end = start + "a + 1".len();
        let plan = extract_variable("m.py", src, 2, start, end, "s").unwrap();
        assert_eq!(plan.op, RefactorKind::ExtractVariable);
        assert_eq!(plan.plan.ops.len(), 1);
        if let EditOp::SearchReplace { replace, .. } = &plan.plan.ops[0] {
            assert!(replace.contains("s = a + 1"));
            assert!(replace.contains("return s"));
        } else {
            panic!("expected SearchReplace");
        }
    }

    #[test]
    fn extract_variable_js() {
        let src = "function f(a) {\n  return a + 1;\n}\n";
        let line2 = "  return a + 1;";
        let start = line2.find("a + 1").unwrap();
        let end = start + "a + 1".len();
        let plan = extract_variable("m.js", src, 2, start, end, "s").unwrap();
        if let EditOp::SearchReplace { replace, .. } = &plan.plan.ops[0] {
            assert!(replace.contains("const s = a + 1;"));
            assert!(replace.contains("return s;"));
        } else {
            panic!("expected SearchReplace");
        }
    }

    #[test]
    fn rename_local_javascript() {
        let src = "function f(a) {\n  let b = a + 1;\n  return b;\n}\n";
        let line2 = "  let b = a + 1;";
        let col = line2.find('b').unwrap();
        let plan = rename_local("m.js", src, 2, col, "total").unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        assert_eq!(plan.plan.ops.len(), 2);
    }
}

#[cfg(test)]
mod batch2_smoke {
    use super::*;
    use crate::locals::LocalsIndex;

    #[test]
    fn go_locals_parse() {
        let src = "func f(a int) int {\n\tb := a + 1\n\treturn b\n}\n";
        let idx = LocalsIndex::build(Lang::Go, src).unwrap();
        assert!(
            idx.definitions.iter().any(|d| d.name == "b"),
            "defs: {:?}",
            idx.definitions
        );
        assert!(idx.references.iter().any(|r| r.name == "b"));
    }

    #[test]
    fn rust_locals_parse() {
        let src = "fn f(a: i32) -> i32 {\n    let b = a + 1;\n    b\n}\n";
        let idx = LocalsIndex::build(Lang::Rust, src).unwrap();
        assert!(
            idx.definitions.iter().any(|d| d.name == "b"),
            "defs: {:?}",
            idx.definitions
        );
    }

    #[test]
    fn ts_locals_parse() {
        let src = "function f(a: number): number {\n  const b = a + 1;\n  return b;\n}\n";
        let idx = LocalsIndex::build(Lang::TypeScript, src).unwrap();
        assert!(
            idx.definitions.iter().any(|d| d.name == "b"),
            "defs: {:?}",
            idx.definitions
        );
    }
}
