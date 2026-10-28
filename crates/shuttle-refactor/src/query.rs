//! G3: tree-sitter query select — the safe, structured node-selection primitive.
//!
//! shuttle already runs `locals.scm` internally (see `locals.rs`), but that entry
//! point is hard-wired to the scope/definition/reference roles. This module
//! generalizes that one capability: run an ARBITRARY tree-sitter query against a
//! parsed file and return the matched nodes as structured data (capture name +
//! byte range + 1-based line / 0-based col + the matched text).
//!
//! Why this exists (docs — the "safe seed" for extensibility):
//!   · It lowers the cost of writing the NEXT operator in Rust — an operator's
//!     "find the nodes to touch" step becomes one `select()` call instead of a
//!     bespoke cursor walk.
//!   · It is the ONLY shape in which a future scripting layer could ever be
//!     safe: a script may PROPOSE (select nodes + emit structured ops) but never
//!     ADJUDICATE — the reparse + confidence + scope gates stay in Rust, and this
//!     primitive returns *data*, never a disk write. Nothing here touches the fs.
//!
//! §7: a malformed query is a hard error (we never guess what the caller meant).

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::locals::Lang;
use crate::RefactorError;

/// One captured node from a query match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryCapture {
    /// The capture name from the query, e.g. `function.name` for `@function.name`.
    pub capture: String,
    /// Byte range of the node in the source.
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line, 0-based column (tree-sitter Point is 0-based row/col) —
    /// matches the `Occurrence` convention in `locals.rs`.
    pub line: usize,
    pub col: usize,
    /// The node's kind (grammar node type), useful for callers that dispatch on it.
    pub kind: String,
    /// The exact source text of the node.
    pub text: String,
}

/// Run `query_scm` against `source` parsed as `lang`, returning every capture in
/// match order. A syntax error in the query is a hard `Rejected` — we do not
/// silently drop it (§7). An empty result is a valid, non-error outcome.
pub fn select(
    lang: Lang,
    source: &str,
    query_scm: &str,
) -> Result<Vec<QueryCapture>, RefactorError> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.language())
        .map_err(|e| RefactorError::Parse(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| RefactorError::Parse("tree-sitter returned no tree".into()))?;

    let query = Query::new(&lang.language(), query_scm)
        .map_err(|e| RefactorError::Rejected(format!("bad tree-sitter query: {e:?}")))?;
    let names = query.capture_names();

    let src = source.as_bytes();
    let mut out: Vec<QueryCapture> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        for cap in m.captures {
            let node = cap.node;
            let sp = node.start_position();
            out.push(QueryCapture {
                capture: names[cap.index as usize].to_string(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                line: sp.row + 1,
                col: sp.column,
                kind: node.kind().to_string(),
                text: node.utf8_text(src).unwrap_or("").to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_rust_fn_names() {
        let src = "fn alpha() {}\nfn beta(x: u64) -> u64 { x }\n";
        let caps = select(
            Lang::Rust,
            src,
            "(function_item name: (identifier) @fn.name)",
        )
        .unwrap();
        let names: Vec<&str> = caps.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(caps[0].capture, "fn.name");
        assert_eq!(caps[0].line, 1);
        assert_eq!(caps[1].line, 2);
    }

    #[test]
    fn selects_python_defs() {
        let src = "def foo():\n    pass\n\ndef bar(a):\n    return a\n";
        let caps = select(
            Lang::Python,
            src,
            "(function_definition name: (identifier) @def)",
        )
        .unwrap();
        let names: Vec<&str> = caps.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn empty_result_is_ok_not_error() {
        let src = "fn only() {}\n";
        let caps = select(Lang::Rust, src, "(struct_item name: (type_identifier) @s)").unwrap();
        assert!(caps.is_empty());
    }

    #[test]
    fn malformed_query_is_rejected() {
        let src = "fn a() {}\n";
        let err = select(Lang::Rust, src, "(this is not valid scm").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn reports_kind_and_range() {
        let src = "fn a() {}\n";
        let caps = select(Lang::Rust, src, "(function_item) @f").unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].kind, "function_item");
        assert_eq!(caps[0].start_byte, 0);
        assert_eq!(caps[0].end_byte, src.trim_end().len());
    }
}
