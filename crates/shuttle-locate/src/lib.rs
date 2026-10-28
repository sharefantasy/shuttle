//! shuttle-locate — find *where* to edit, cheaply and precisely.
//!
//! Two backends, picked by the caller:
//!   * text locate  -> ripgrep (`rg`), for literal / regex string hits
//!   * struct locate -> ast-grep (`sg`), for AST-pattern hits that never
//!     false-match inside strings/comments.
//!
//! Both shell out to the already-installed binaries; this crate only owns
//! argument construction and result parsing, so it stays tiny and fast.

use std::process::Command;
use std::time::Duration;

const LOCATE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOCATE_STDOUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LOCATE_STDERR_BYTES: u64 = 64 * 1024;
const MAX_LOCATE_HITS: usize = 20_000;

#[derive(Debug, Clone)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    #[error("backend `{0}` not found on PATH")]
    MissingBackend(&'static str),
    #[error("backend failed: {0}")]
    Backend(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Literal / regex text search via ripgrep. Returns JSON-parsed hits.
pub fn text_locate(pattern: &str, root: &str) -> Result<Vec<Hit>, LocateError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Rg)
        .map_err(|_| LocateError::MissingBackend("rg"))?;
    let mut command = Command::new(binary);
    command.args(["--json", "--", pattern, root]);
    let out = shuttle_runtime_tools::output_bounded(
        &mut command,
        LOCATE_TIMEOUT,
        MAX_LOCATE_STDOUT_BYTES,
        MAX_LOCATE_STDERR_BYTES,
    )
    .map_err(LocateError::Backend)?;
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(LocateError::Backend(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    parse_rg_json(&String::from_utf8_lossy(&out.stdout))
}

/// Structural search via ast-grep. `pattern` is an ast-grep pattern string.
pub fn struct_locate(pattern: &str, lang: &str, root: &str) -> Result<Vec<Hit>, LocateError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::AstGrep)
        .map_err(|_| LocateError::MissingBackend("ast-grep"))?;
    let mut command = Command::new(binary);
    command.args(["run", "--pattern", pattern, "--lang", lang, "--json", root]);
    let out = shuttle_runtime_tools::output_bounded(
        &mut command,
        LOCATE_TIMEOUT,
        MAX_LOCATE_STDOUT_BYTES,
        MAX_LOCATE_STDERR_BYTES,
    )
    .map_err(LocateError::Backend)?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(LocateError::Backend(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    parse_sg_json(&String::from_utf8_lossy(&out.stdout))
}

use serde_json::Value;

/// Parse ripgrep `--json` output. ripgrep emits one JSON object per line, each
/// tagged by `type` (`begin`/`match`/`end`/`summary`). We keep only `match`
/// events. Within a match, `line_number` is 1-indexed and each submatch carries
/// a 0-indexed byte `start` within the line, which we surface as a 1-indexed
/// column.
fn parse_rg_json(s: &str) -> Result<Vec<Hit>, LocateError> {
    let mut hits = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let data = &v["data"];
        let path = data["path"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let line_no = data["line_number"].as_u64().unwrap_or(0) as usize;
        let text = data["lines"]["text"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('\n')
            .to_string();
        // One Hit per submatch so multiple hits on a line are all reported.
        match data["submatches"].as_array() {
            Some(subs) if !subs.is_empty() => {
                for sm in subs {
                    let col = sm["start"].as_u64().unwrap_or(0) as usize + 1;
                    if hits.len() >= MAX_LOCATE_HITS {
                        return Err(LocateError::Backend(format!(
                            "match count exceeds {MAX_LOCATE_HITS} limit"
                        )));
                    }
                    hits.push(Hit {
                        path: path.clone(),
                        line: line_no,
                        column: col,
                        text: text.clone(),
                    });
                }
            }
            _ => {
                if hits.len() >= MAX_LOCATE_HITS {
                    return Err(LocateError::Backend(format!(
                        "match count exceeds {MAX_LOCATE_HITS} limit"
                    )));
                }
                hits.push(Hit {
                    path,
                    line: line_no,
                    column: 1,
                    text,
                })
            }
        }
    }
    Ok(hits)
}

/// Parse ast-grep `--json` output: a single JSON array of match objects. Each
/// object has `file`, `text`/`lines`, and a `range` whose `start` gives 0-indexed
/// `line`/`column` (normalized here to 1-indexed).
fn parse_sg_json(s: &str) -> Result<Vec<Hit>, LocateError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let root: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let arr = match root.as_array() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let hits = arr
        .iter()
        .take(MAX_LOCATE_HITS)
        .map(|m| {
            let path = m["file"].as_str().unwrap_or_default().to_string();
            let start = &m["range"]["start"];
            let line = start["line"].as_u64().unwrap_or(0) as usize + 1;
            let column = start["column"].as_u64().unwrap_or(0) as usize + 1;
            let text = m["lines"]
                .as_str()
                .or_else(|| m["text"].as_str())
                .unwrap_or_default()
                .trim_end_matches('\n')
                .to_string();
            Hit {
                path,
                line,
                column,
                text,
            }
        })
        .collect::<Vec<_>>();
    if arr.len() > MAX_LOCATE_HITS {
        return Err(LocateError::Backend(format!(
            "match count {} exceeds {MAX_LOCATE_HITS} limit",
            arr.len()
        )));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rg_parses_match_events_only() {
        // Two non-match events + one match with a submatch at byte 4.
        let out = concat!(
            r#"{"type":"begin","data":{"path":{"text":"a.rs"}}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"a.rs"},"line_number":10,"lines":{"text":"    foo();\n"},"submatches":[{"start":4,"end":7,"match":{"text":"foo"}}]}}"#,
            "\n",
            r#"{"type":"end","data":{"path":{"text":"a.rs"}}}"#,
            "\n",
            r#"{"type":"summary","data":{}}"#,
            "\n",
        );
        let hits = parse_rg_json(out).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.rs");
        assert_eq!(hits[0].line, 10);
        assert_eq!(hits[0].column, 5); // 0-indexed 4 -> 1-indexed 5
        assert_eq!(hits[0].text, "    foo();");
    }

    #[test]
    fn rg_multiple_submatches_yield_multiple_hits() {
        let out = r#"{"type":"match","data":{"path":{"text":"b.rs"},"line_number":3,"lines":{"text":"x x\n"},"submatches":[{"start":0,"end":1,"match":{"text":"x"}},{"start":2,"end":3,"match":{"text":"x"}}]}}"#;
        let hits = parse_rg_json(out).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].column, 1);
        assert_eq!(hits[1].column, 3);
    }

    #[test]
    fn rg_ignores_malformed_lines() {
        let out = "not json\n{\"type\":\"match\",\"data\":{}}\n";
        // Second line is a match with missing fields -> defaults, still one Hit.
        let hits = parse_rg_json(out).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 0);
    }

    #[test]
    fn sg_parses_array_with_zero_indexed_range() {
        let out = r#"[{"file":"src/x.rs","text":"foo","lines":"let foo = 1;","range":{"start":{"line":41,"column":4},"end":{"line":41,"column":7}}}]"#;
        let hits = parse_sg_json(out).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/x.rs");
        assert_eq!(hits[0].line, 42); // 0-indexed 41 -> 1-indexed 42
        assert_eq!(hits[0].column, 5);
        assert_eq!(hits[0].text, "let foo = 1;");
    }

    #[test]
    fn sg_empty_output_is_empty() {
        assert!(parse_sg_json("").unwrap().is_empty());
        assert!(parse_sg_json("[]").unwrap().is_empty());
    }

    #[test]
    fn rg_match_count_is_bounded() {
        let line = r#"{"type":"match","data":{"path":{"text":"a.rs"},"line_number":1,"lines":{"text":"x\n"},"submatches":[{"start":0,"end":1}]}}"#;
        let body = std::iter::repeat_n(line, MAX_LOCATE_HITS + 1)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_rg_json(&body).is_err());
    }

    #[test]
    fn sg_match_count_is_bounded() {
        let item = serde_json::json!({
            "file": "a.rs",
            "text": "x",
            "range": {"start": {"line": 0, "column": 0}}
        });
        let body = serde_json::to_string(&vec![item; MAX_LOCATE_HITS + 1]).unwrap();
        assert!(parse_sg_json(&body).is_err());
    }
}
