//! record-projection (§9 E7): synthesize a **field-projection "record" builder** for a
//! data type — the mechanical `build_record`-style function the domain
//! tooling hand-writes to project a run/entity into a stable Bitable-upsert
//! payload (`{"kind": "<t>_record_v1", "fields": {<label>: value, ...}}`).
//!
//! This is the archetypal §7-compliant creation: every emitted line is a
//! deterministic projection of the type's *declared fields*. We default each
//! field's Bitable label to the field name verbatim — we do NOT guess the
//! Chinese display labels the domain uses by hand (that is free authoring, the
//! LLM's job), and we warn the caller to rename labels before relying on it.
//!
//! Per language the projection is emitted idiomatically:
//!   * Python — module-level `def build_<snake>_record(obj) -> dict` returning
//!     the `{kind, fields}` envelope (mirrors `render_bitable_record.build_record`).
//!   * TypeScript — a `toRecord(): { kind: string; fields: Record<string, unknown> }`
//!     method spliced inside the class body.
//!   * Rust — a sibling `impl T { pub fn to_record(&self) -> Vec<(&'static str, String)> }`
//!     (no dynamic map; a labeled vec is the honest projection).
//!   * Go — a sibling `func (t *T) Record() map[string]any`.
//!
//! Rust/Go builders are emitted as a sibling item after the type; TS is spliced
//! inside the class body; Python is appended after the class. Every edit is
//! anchored on the type's unique verbatim block (the E2/E3/E4 trick), so the
//! range is never guessed and the plan is High confidence.

use shuttle_nav::Confidence;

use crate::generate_boilerplate::{collect_fields, find_named_container, splice_inside, Field};
use crate::locals::{Lang, LocalsIndex};
use crate::{plan_from_ops, search_replace, RefactorError, RefactorKind, RefactorPlan};

/// Generate a field-projection record builder for `type_name` defined in
/// `source`, writing it into `path`. Fails if the type is not found / not
/// unique, declares no fields, or the language is unsupported (JS: untyped).
pub fn record_projection(
    path: &str,
    source: &str,
    type_name: &str,
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
            "`{type_name}` declares no fields — nothing to project into a record"
        )));
    }

    let (member, inside) = render_record(lang, type_name, &fields)?;

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
        "{path}:{} (record-projection for `{type_name}`)",
        container.start_position().row + 1
    )];
    let warnings = vec![
        format!(
            "the record projects `{type_name}`'s fields 1:1; Bitable labels default to the field \
             names — rename them to the real column titles before syncing"
        ),
        "field values are projected verbatim (or debug-stringified in Rust); wire real \
         serialization/normalization if the columns need it"
            .to_string(),
    ];

    let intent = format!("record-projection: build_record for {type_name} ({path})");
    let plan = plan_from_ops(RefactorKind::RecordProjection, path, ops, &intent);
    Ok(RefactorPlan {
        op: RefactorKind::RecordProjection,
        plan,
        signature: Some(format!("record @ {type_name}")),
        params: Vec::new(),
        returns: Vec::new(),
        confidence: Confidence::High,
        warnings,
        sites,
    })
}

/// Render the record builder. Returns (rendered_code, insert_inside_body).
fn render_record(
    lang: Lang,
    type_name: &str,
    fields: &[Field],
) -> Result<(String, bool), RefactorError> {
    match lang {
        Lang::Rust => Ok((rust_record(type_name, fields), false)),
        Lang::Go => Ok((go_record(type_name, fields), false)),
        Lang::TypeScript => Ok((ts_record(type_name, fields), true)),
        Lang::Python => Ok((python_record(type_name, fields), false)),
        Lang::JavaScript => Err(RefactorError::Rejected(
            "JavaScript fields are untyped/undeclared — no fields to project into a record (§7)"
                .into(),
        )),
    }
}

/// `Point` -> `point`, `DebugRun` -> `debug_run` (snake_case for the record kind).
fn to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn rust_record(type_name: &str, fields: &[Field]) -> String {
    let rows = fields
        .iter()
        .map(|f| {
            format!(
                "            (\"{n}\", format!(\"{{:?}}\", self.{n})),",
                n = f.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "impl {type_name} {{\n    /// Project every declared field into a stable (label, value) \
         record.\n    pub fn to_record(&self) -> Vec<(&'static str, String)> {{\n        \
         vec![\n{rows}\n        ]\n    }}\n}}"
    )
}

fn go_record(type_name: &str, fields: &[Field]) -> String {
    let recv = type_name.chars().next().unwrap_or('t').to_ascii_lowercase();
    let rows = fields
        .iter()
        .map(|f| format!("\t\t\"{n}\": {recv}.{n},", n = f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "// Record projects every declared field into a stable Bitable-upsert map.\nfunc ({recv} \
         *{type_name}) Record() map[string]any {{\n\treturn map[string]any{{\n{rows}\n\t}}\n}}"
    )
}

fn ts_record(type_name: &str, fields: &[Field]) -> String {
    let kind = to_snake(type_name);
    let rows = fields
        .iter()
        .map(|f| format!("        {n}: this.{n},", n = f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "  toRecord(): {{ kind: string; fields: Record<string, unknown> }} {{\n    return {{\n      \
         kind: \"{kind}_record_v1\",\n      fields: {{\n{rows}\n      }},\n    }};\n  }}"
    )
}

fn python_record(type_name: &str, fields: &[Field]) -> String {
    let kind = to_snake(type_name);
    let var = "obj";
    let rows = fields
        .iter()
        .map(|f| format!("            \"{n}\": {var}.{n},", n = f.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "def build_{kind}_record({var}) -> dict:\n    \"\"\"Project one {type_name} into a stable \
         record payload (field -> value).\"\"\"\n    return {{\n        \"kind\": \
         \"{kind}_record_v1\",\n        \"fields\": {{\n{rows}\n        }},\n    }}"
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
    fn rust_record_from_fields() {
        let src = "struct Point {\n    x: i32,\n    y: String,\n}\n";
        let plan = record_projection("lib.rs", src, "Point").unwrap();
        assert!(matches!(plan.confidence, Confidence::High));
        let r = replacement(&plan);
        assert!(r.contains("impl Point {"));
        assert!(r.contains("pub fn to_record(&self) -> Vec<(&'static str, String)> {"));
        assert!(r.contains("(\"x\", format!(\"{:?}\", self.x)),"));
        assert!(r.contains("(\"y\", format!(\"{:?}\", self.y)),"));
    }

    #[test]
    fn go_record_from_fields() {
        let src = "type DebugRun struct {\n\tRunID string\n\tStatus string\n}\n";
        let plan = record_projection("g.go", src, "DebugRun").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("func (d *DebugRun) Record() map[string]any {"));
        assert!(r.contains("\"RunID\": d.RunID,"));
        assert!(r.contains("\"Status\": d.Status,"));
    }

    #[test]
    fn ts_record_spliced_inside_body() {
        let src = "class DebugRun {\n  runId: string;\n  status: string;\n}\n";
        let plan = record_projection("g.ts", src, "DebugRun").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("toRecord(): { kind: string; fields: Record<string, unknown> } {"));
        assert!(r.contains("kind: \"debug_run_record_v1\","));
        assert!(r.contains("runId: this.runId,"));
        assert!(r.contains("status: this.status,"));
        // spliced before the closing brace
        let rec = r.find("toRecord").unwrap();
        let last_brace = r.rfind('}').unwrap();
        assert!(rec < last_brace);
    }

    #[test]
    fn python_record_from_annotated_fields() {
        let src = "class DebugRun:\n    run_id: str\n    status: str\n";
        let plan = record_projection("g.py", src, "DebugRun").unwrap();
        let r = replacement(&plan);
        assert!(r.contains("def build_debug_run_record(obj) -> dict:"));
        assert!(r.contains("\"kind\": \"debug_run_record_v1\","));
        assert!(r.contains("\"run_id\": obj.run_id,"));
        assert!(r.contains("\"status\": obj.status,"));
    }

    #[test]
    fn rejects_type_without_fields() {
        let src = "struct Empty {\n}\n";
        let err = record_projection("lib.rs", src, "Empty").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn rejects_undefined_type() {
        let src = "struct Point {\n    x: i32,\n}\n";
        let err = record_projection("lib.rs", src, "Nope").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn js_is_rejected_untyped() {
        let src = "class Point {\n  constructor() { this.x = 1; }\n}\n";
        let err = record_projection("g.js", src, "Point").unwrap_err();
        assert!(matches!(err, RefactorError::Rejected(_)));
    }

    #[test]
    fn warns_labels_default_to_field_names() {
        let src = "struct Point {\n    x: i32,\n}\n";
        let plan = record_projection("lib.rs", src, "Point").unwrap();
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("Bitable labels default")));
    }
}
