//! shuttle-codemod — mechanical, rule-based edits that never touch the model.
//!
//! Wraps `ast-grep scan` against the project's `rules/` directory. A codemod
//! is the cheapest possible edit: deterministic, reviewable, whole-repo, and
//! costs zero model tokens. The tier ladder in the design doc is:
//!   symbol-table edit (shuttle-symbols) > ast-grep rule > comby > tree-sitter query > model.
//!
//! Two entry points: [`dry_run`] previews matches (parsed from `ast-grep`'s
//! `--json` stream into structured [`CodemodMatch`] rows) and [`apply`] rewrites
//! in place with `--update-all`. [`list_rules`] enumerates the rule set so the
//! planner can pick a rule by id without hard-coding paths.

use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::Duration;

const CODEMOD_SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CODEMOD_STDOUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CODEMOD_STDERR_BYTES: u64 = 64 * 1024;
const MAX_CODEMOD_MATCHES: usize = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum CodemodError {
    #[error("ast-grep not found on PATH")]
    MissingBackend,
    #[error("scan failed: {0}")]
    Scan(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One rule match: a site the codemod would rewrite, with the proposed
/// replacement. Produced by [`dry_run`] before anything is written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodMatch {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub rule_id: String,
    /// The matched source text.
    pub text: String,
    /// The rewritten text ast-grep would splice in, if the rule has a `fix`.
    pub replacement: Option<String>,
    /// The rule's human-readable message, if any.
    pub message: Option<String>,
}

/// Preview a rule without writing (dry-run): run `ast-grep scan --json` and
/// parse the match stream into structured [`CodemodMatch`] rows.
pub fn dry_run(
    rule_id: &str,
    rules_dir: &str,
    root: &str,
) -> Result<Vec<CodemodMatch>, CodemodError> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::AstGrep)
        .map_err(|_| CodemodError::MissingBackend)?;
    let mut command = Command::new(binary);
    command.args([
        "scan",
        "--rule",
        &rule_path(rules_dir, rule_id),
        "--json",
        root,
    ]);
    let out = shuttle_runtime_tools::output_bounded(
        &mut command,
        CODEMOD_SCAN_TIMEOUT,
        MAX_CODEMOD_STDOUT_BYTES,
        MAX_CODEMOD_STDERR_BYTES,
    )
    .map_err(CodemodError::Scan)?;
    if !out.status.success() {
        return Err(CodemodError::Scan(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    parse_scan_json(&String::from_utf8_lossy(&out.stdout))
}

/// Apply a rule in place (`--update-all`). Should only run after a dry-run
/// confirmation in the pipeline. Returns the number of rewritten sites.
pub fn apply(rule_id: &str, rules_dir: &str, root: &str) -> Result<usize, CodemodError> {
    // Count first (dry-run), so the caller learns how many sites changed.
    let matched = dry_run(rule_id, rules_dir, root)?.len();
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::AstGrep)
        .map_err(|_| CodemodError::MissingBackend)?;
    let status = Command::new(binary)
        .args([
            "scan",
            "--rule",
            &rule_path(rules_dir, rule_id),
            "--update-all",
            root,
        ])
        .status()
        .map_err(|_| CodemodError::MissingBackend)?;
    if !status.success() {
        return Err(CodemodError::Scan(format!("rule {rule_id} failed")));
    }
    Ok(matched)
}

/// Enumerate available rule ids in `rules_dir` (each `*.yml` / `*.yaml` file).
/// Sorted for deterministic output. The filename stem is the rule id.
pub fn list_rules(rules_dir: &str) -> Result<Vec<String>, CodemodError> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(rules_dir)? {
        let path = entry?.path();
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "yml" || e == "yaml")
            .unwrap_or(false);
        if is_yaml {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

fn rule_path(rules_dir: &str, rule_id: &str) -> String {
    format!("{rules_dir}/{rule_id}.yml")
}

/// Parse `ast-grep scan --json` output (a JSON array of match objects) into
/// [`CodemodMatch`] rows. Kept pure (no process spawn) so it is unit-testable
/// against captured fixtures. Empty / whitespace input yields no matches.
fn parse_scan_json(stdout: &str) -> Result<Vec<CodemodMatch>, CodemodError> {
    use serde_json::Value;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|e| CodemodError::Parse(e.to_string()))?;
    let arr = value
        .as_array()
        .ok_or_else(|| CodemodError::Parse("expected a JSON array".into()))?;

    if arr.len() > MAX_CODEMOD_MATCHES {
        return Err(CodemodError::Parse(format!(
            "match count {} exceeds {} limit",
            arr.len(),
            MAX_CODEMOD_MATCHES
        )));
    }
    let mut matches = Vec::with_capacity(arr.len());
    for m in arr {
        let path = m
            .get("file")
            .and_then(Value::as_str)
            .ok_or_else(|| CodemodError::Parse("match missing `file`".into()))?
            .to_string();
        let start = m
            .get("range")
            .and_then(|r| r.get("start"))
            .ok_or_else(|| CodemodError::Parse("match missing `range.start`".into()))?;
        // ast-grep reports 0-indexed line/column; normalize to 1-indexed.
        let line = start.get("line").and_then(Value::as_u64).unwrap_or(0) as usize + 1;
        let column = start.get("column").and_then(Value::as_u64).unwrap_or(0) as usize + 1;
        let text = m
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let rule_id = m
            .get("ruleId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let replacement = m
            .get("replacement")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = m
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        matches.push(CodemodMatch {
            path,
            line,
            column,
            rule_id,
            text,
            replacement,
            message,
        });
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from `ast-grep scan --rule ... --json` (trimmed to the fields we
    // parse). Two matches, both with a fix.
    const SCAN_JSON: &str = r#"[
      {
        "text": "requests.get(url)",
        "range": { "start": { "line": 1, "column": 4 }, "end": { "line": 1, "column": 21 } },
        "file": "a.py",
        "replacement": "requests.get(url, timeout=10)",
        "ruleId": "py-requests-add-timeout",
        "message": "requests.get without timeout — add a default timeout"
      },
      {
        "text": "requests.get(other)",
        "range": { "start": { "line": 2, "column": 5 }, "end": { "line": 2, "column": 24 } },
        "file": "a.py",
        "replacement": "requests.get(other, timeout=10)",
        "ruleId": "py-requests-add-timeout",
        "message": "requests.get without timeout — add a default timeout"
      }
    ]"#;

    #[test]
    fn parses_matches_and_normalizes_1_indexed() {
        let ms = parse_scan_json(SCAN_JSON).unwrap();
        assert_eq!(ms.len(), 2);
        let first = &ms[0];
        assert_eq!(first.path, "a.py");
        assert_eq!(first.line, 2); // 0-indexed 1 -> 1-indexed 2
        assert_eq!(first.column, 5); // 0-indexed 4 -> 1-indexed 5
        assert_eq!(first.rule_id, "py-requests-add-timeout");
        assert_eq!(first.text, "requests.get(url)");
        assert_eq!(
            first.replacement.as_deref(),
            Some("requests.get(url, timeout=10)")
        );
        assert!(first.message.as_deref().unwrap().contains("timeout"));
    }

    #[test]
    fn empty_output_yields_no_matches() {
        assert!(parse_scan_json("").unwrap().is_empty());
        assert!(parse_scan_json("   \n").unwrap().is_empty());
        assert!(parse_scan_json("[]").unwrap().is_empty());
    }

    #[test]
    fn match_without_fix_has_no_replacement() {
        let json = r#"[
          {
            "text": "eval(x)",
            "range": { "start": { "line": 0, "column": 0 } },
            "file": "b.py",
            "ruleId": "py-no-eval"
          }
        ]"#;
        let ms = parse_scan_json(json).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].replacement, None);
        assert_eq!(ms[0].message, None);
        assert_eq!(ms[0].line, 1);
        assert_eq!(ms[0].column, 1);
    }

    #[test]
    fn rejects_malformed_and_non_array() {
        assert!(parse_scan_json("{not json}").is_err());
        assert!(parse_scan_json(r#"{"file":"a"}"#).is_err()); // object, not array
    }

    #[test]
    fn match_count_is_bounded() {
        let item = serde_json::json!({
            "text": "x",
            "range": {"start": {"line": 0, "column": 0}},
            "file": "a.py",
            "ruleId": "bounded"
        });
        let body = serde_json::to_string(&vec![item; MAX_CODEMOD_MATCHES + 1]).unwrap();
        let error = parse_scan_json(&body).unwrap_err();
        assert!(error.to_string().contains("match count"));
    }

    #[test]
    fn rule_path_joins_id() {
        assert_eq!(rule_path("rules", "foo"), "rules/foo.yml");
    }
}
