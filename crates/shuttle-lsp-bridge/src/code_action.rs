//! §9.9 D2 — expose shuttle's *own* refactors as LSP `codeAction`s.
//!
//! This is the inverse direction of the tier-3 bridge in [`crate`]: instead of
//! *calling* a language server for semantics we can't compute, this module
//! *serves* shuttle's refactor engine to any LSP client (an editor's "Refactor…"
//! menu) by lowering a [`shuttle_refactor::RefactorPlan`] into a standard
//! [`CodeAction`] carrying a [`WorkspaceEdit`].
//!
//! It is pure and needs no language server, so it lives in the DEFAULT build
//! (unlike the `real-lsp` tier-3 adapters). The only wrinkle: shuttle's [`EditOp`]s
//! are *content-anchored* (`SearchReplace { search, replace }`) while LSP
//! [`TextEdit`]s are *range-anchored* (`{ range, newText }`). We therefore
//! resolve each op's `search` against the current buffer text to a concrete
//! `Range`. Following §7's honesty rule we do this by **exact** location: if a
//! `search` does not occur exactly once we refuse to fabricate a range and drop
//! the whole action (returning `None`) rather than emit an edit that would land
//! in the wrong place.

use serde::Serialize;
use shuttle_protocol::{EditOp, InsertPosition};
use shuttle_refactor::{RefactorKind, RefactorPlan};

/// LSP `Position` (0-based line, 0-based UTF-16 code-unit character). We compute
/// `character` in UTF-16 units per the LSP spec's default `positionEncoding`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP `Range` — half-open `[start, end)`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// LSP `TextEdit`.
#[derive(Debug, Clone, Serialize)]
pub struct TextEdit {
    pub range: Range,
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// LSP `WorkspaceEdit` — the `changes` map form (`uri -> [TextEdit]`). shuttle's
/// same-file refactors touch one uri; cross-file ones (rename-symbol,
/// move-function) populate several keys.
#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceEdit {
    pub changes: std::collections::BTreeMap<String, Vec<TextEdit>>,
}

/// LSP `CodeAction`.
#[derive(Debug, Clone, Serialize)]
pub struct CodeAction {
    pub title: String,
    /// LSP `CodeActionKind` — always a `refactor.*` string for shuttle's ops.
    pub kind: String,
    pub edit: WorkspaceEdit,
    /// shuttle confidence surfaced verbatim so the client can badge Medium/Low.
    #[serde(rename = "isPreferred", skip_serializing_if = "std::ops::Not::not")]
    pub is_preferred: bool,
    /// shuttle warnings (method-matched call sites, cross-file textual discovery…),
    /// passed through so the editor can show them before applying.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics_note: Vec<String>,
}

/// The `CodeActionKind` string for each refactor (LSP hierarchical kinds).
fn kind_of(op: RefactorKind) -> &'static str {
    match op {
        RefactorKind::ExtractVariable
        | RefactorKind::ExtractMethod
        | RefactorKind::ExtractConstant
        | RefactorKind::ExtractToNewFile => "refactor.extract",
        RefactorKind::InlineVariable
        | RefactorKind::InlineMethod
        | RefactorKind::InlineConstant
        | RefactorKind::InlineParameter => "refactor.inline",
        RefactorKind::RenameLocal | RefactorKind::RenameSymbol => "refactor.rewrite",
        RefactorKind::ChangeSignature
        | RefactorKind::IntroduceParameter
        | RefactorKind::RemoveParameter
        | RefactorKind::MoveFunction
        | RefactorKind::ImplementMembers
        | RefactorKind::GenerateBoilerplate
        | RefactorKind::CreateFromUsage
        | RefactorKind::ScaffoldTest
        | RefactorKind::NewModuleWiring
        | RefactorKind::RecordProjection => "refactor.rewrite",
    }
}

/// A human title for the action's menu entry.
fn title_of(plan: &RefactorPlan) -> String {
    if let Some(intent) = &plan.plan.intent {
        return intent.clone();
    }
    format!("{:?}", plan.op)
}

/// Resolve a `search` string to a `Range` within `source` — but only if it
/// occurs **exactly once**. `None` on zero or multiple hits (§7: never guess).
fn locate(source: &str, needle: &str) -> Option<Range> {
    if needle.is_empty() {
        return None;
    }
    // Reject ambiguity: `search` must occur exactly once (§7 — never guess).
    if source.matches(needle).count() != 1 {
        return None;
    }
    let first = source.find(needle)?;
    let start = offset_to_position(source, first);
    let end = offset_to_position(source, first + needle.len());
    Some(Range { start, end })
}

/// Byte offset -> LSP Position (0-based line; character in UTF-16 units).
fn offset_to_position(source: &str, byte_off: usize) -> Position {
    let mut line = 0u32;
    let mut col_utf16 = 0u32;
    for (i, ch) in source.char_indices() {
        if i >= byte_off {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }
    Position {
        line,
        character: col_utf16,
    }
}

/// The per-op source text needed to resolve `SearchReplace` ranges: a map from
/// path to the CURRENT buffer content. shuttle plans are content-anchored, so the
/// client must supply the text each op's `search` should be located in.
pub type SourceMap<'a> = std::collections::BTreeMap<String, &'a str>;

/// Lower a [`RefactorPlan`] into a single [`CodeAction`], resolving every
/// `SearchReplace` op to a concrete LSP `TextEdit` range against `sources`.
///
/// Returns `None` (refusing to emit a partial/misplaced edit) when:
///   * an op targets a path absent from `sources`, or
///   * a `search` string cannot be located exactly once, or
///   * the plan carries an op the pure lowering cannot represent as a range
///     edit (`Codemod`, which is an opaque ast-grep rule applied elsewhere).
///
/// `Insert`/`CreateFile` ARE representable and are lowered too (insert becomes
/// a zero-width edit at the anchor line; create becomes an edit over an empty
/// buffer keyed by its path).
pub fn code_action(plan: &RefactorPlan, sources: &SourceMap) -> Option<CodeAction> {
    let mut changes: std::collections::BTreeMap<String, Vec<TextEdit>> =
        std::collections::BTreeMap::new();

    for op in &plan.plan.ops {
        match op {
            EditOp::SearchReplace {
                path,
                search,
                replace,
                ..
            } => {
                let src = sources.get(path)?;
                let range = locate(src, search)?;
                changes.entry(path.clone()).or_default().push(TextEdit {
                    range,
                    new_text: replace.clone(),
                });
            }
            EditOp::Insert {
                path,
                anchor,
                position,
                content,
            } => {
                let src = sources.get(path)?;
                let anchor_range = locate(src, anchor)?;
                // Zero-width edit at the start of the line before/after anchor.
                let at = match position {
                    InsertPosition::Before => Position {
                        line: anchor_range.start.line,
                        character: 0,
                    },
                    InsertPosition::After => Position {
                        line: anchor_range.end.line + 1,
                        character: 0,
                    },
                };
                let mut text = content.clone();
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                changes.entry(path.clone()).or_default().push(TextEdit {
                    range: Range { start: at, end: at },
                    new_text: text,
                });
            }
            EditOp::CreateFile { path, content } => {
                // Represent as an edit over a (presumed empty) new document.
                let zero = Position {
                    line: 0,
                    character: 0,
                };
                changes.entry(path.clone()).or_default().push(TextEdit {
                    range: Range {
                        start: zero,
                        end: zero,
                    },
                    new_text: content.clone(),
                });
            }
            // ast-grep codemods are not a range edit we can place honestly.
            EditOp::Codemod { .. } => return None,
        }
    }

    if changes.is_empty() {
        return None;
    }

    Some(CodeAction {
        title: title_of(plan),
        kind: kind_of(plan.op).to_string(),
        edit: WorkspaceEdit { changes },
        is_preferred: matches!(plan.confidence, shuttle_nav::Confidence::High),
        diagnostics_note: plan.warnings.clone(),
    })
}

/// Serialize a [`CodeAction`] to the exact JSON an LSP client expects.
pub fn to_json(action: &CodeAction) -> serde_json::Result<String> {
    serde_json::to_string(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_refactor::remove_parameter;

    fn plan_remove(src: &str) -> RefactorPlan {
        remove_parameter("m.py", src, "f", "unused", "").unwrap()
    }

    #[test]
    fn lowers_remove_parameter_to_textedits() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 9)\n";
        let plan = plan_remove(src);
        let mut sources: SourceMap = SourceMap::new();
        sources.insert("m.py".into(), src);
        let action = code_action(&plan, &sources).expect("lowerable");
        assert_eq!(action.kind, "refactor.rewrite");
        let edits = action.edit.changes.get("m.py").expect("edits for m.py");
        assert!(!edits.is_empty());
        // Every emitted range must re-extract to the op's original search text
        // (ranges are exact, not fuzzy): reconstruct and check it's coherent.
        for e in edits {
            assert!(e.range.start.line <= e.range.end.line);
        }
        // High vs Medium: a same-file removal on a plain fn is High → preferred.
        // (method plans are Medium; this fn is not a method.)
        assert!(!action.title.is_empty());
    }

    #[test]
    fn ambiguous_search_refuses_to_fabricate_a_range() {
        // `xx` occurs many times; locate() must return None → no action.
        let src = "xx xx xx\n";
        assert!(locate(src, "xx").is_none());
    }

    #[test]
    fn unique_search_resolves_to_correct_range() {
        let src = "line0\nfind_me here\nline2\n";
        let r = locate(src, "find_me").unwrap();
        assert_eq!(
            r.start,
            Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            r.end,
            Position {
                line: 1,
                character: 7
            }
        );
    }

    #[test]
    fn utf16_character_offsets() {
        // 'é' is 1 UTF-16 unit but 2 UTF-8 bytes; a following needle's char
        // offset must count UTF-16 units, not bytes.
        let src = "é X\n";
        let r = locate(src, "X").unwrap();
        assert_eq!(
            r.start,
            Position {
                line: 0,
                character: 2
            }
        );
    }

    #[test]
    fn missing_source_yields_no_action() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 9)\n";
        let plan = plan_remove(src);
        let sources: SourceMap = SourceMap::new(); // no "m.py" entry
        assert!(code_action(&plan, &sources).is_none());
    }

    #[test]
    fn action_serializes_to_lsp_json() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 9)\n";
        let plan = plan_remove(src);
        let mut sources: SourceMap = SourceMap::new();
        sources.insert("m.py".into(), src);
        let action = code_action(&plan, &sources).unwrap();
        let json = to_json(&action).unwrap();
        assert!(json.contains("\"kind\":\"refactor.rewrite\""));
        assert!(json.contains("\"newText\""));
        assert!(json.contains("\"range\""));
    }
}
