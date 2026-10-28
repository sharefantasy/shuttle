//! Optional TypeScript type-inference bridge (docs §2.2, §5, §6 "可选 feature",
//! batch 3). TypeScript is the ONLY one of the five languages that benefits from
//! a type-level enhancement: extract-method needs a return-type annotation, and
//! when neither the nearest declaration nor the ctags table supplies it we would
//! otherwise emit a `TODO_TYPE` placeholder (§7). This module fills *genuine*
//! placeholders by shelling out to `ts-morph` (the TS Compiler API) exactly once
//! per refactor — NON-DAEMON, use-and-exit, per the design's "按需拉起、用完即走".
//!
//! It is behind the `ts-morph` cargo feature and is NOT compiled by default:
//! the daemonless single-file hot path (py/js/go/rust, and TS when locals+ctags
//! already resolved) never pays for a node process. When the feature is off,
//! [`crate::extract_method`] keeps its pure tree-sitter behaviour untouched.
//!
//! Honesty (§7): a non-informative inference (`any`/`unknown`/an unresolved
//! cross-module `import(...)` type) is discarded on the node side, so the caller
//! keeps the visible `TODO_TYPE` placeholder rather than a fabricated type.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// The one-shot node driver, embedded so a feature build carries no loose file.
const INFER_JS: &str = include_str!("ts_morph_infer.js");

/// Outcome of a bridge attempt. Anything other than [`Resolved`] means the
/// caller must keep whatever type it already had (usually `TODO_TYPE`): we never
/// downgrade an already-known type and never invent one.
#[derive(Debug)]
pub enum Bridge {
    /// `name -> concrete TS type text`, only for the names ts-morph could type.
    Resolved(BTreeMap<String, String>),
    /// node or ts-morph is unavailable (exit 3 / spawn error) — soft-skip.
    Unavailable,
    /// node ran but failed (bad input, crash) — soft-skip, keep placeholders.
    Failed(String),
}

/// Infer the TypeScript type text for each of `names` as declared in `source`.
///
/// Best-effort and side-effect-free: spawns `node -e <embedded script>`, pipes a
/// small JSON payload on stdin, and parses the JSON reply. Never panics; every
/// failure mode collapses to a non-[`Bridge::Resolved`] variant so extract-method
/// can fall back to its placeholder + Medium-confidence path.
pub fn infer_types(source: &str, file_name: &str, names: &[String]) -> Bridge {
    if names.is_empty() {
        return Bridge::Resolved(BTreeMap::new());
    }

    let payload = serde_json::json!({
        "source": source,
        "fileName": file_name,
        "names": names,
    })
    .to_string();

    let mut child = match Command::new(node_bin())
        .arg("-e")
        .arg(INFER_JS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Bridge::Unavailable, // node not on PATH
    };

    if let Some(mut stdin) = child.stdin.take() {
        // Ignore a broken pipe: the child may exit early (e.g. no ts-morph).
        let _ = stdin.write_all(payload.as_bytes());
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return Bridge::Failed(e.to_string()),
    };

    match output.status.code() {
        Some(0) => {}
        Some(3) => return Bridge::Unavailable, // ts-morph not installed
        Some(code) => return Bridge::Failed(format!("node exit {code}")),
        None => return Bridge::Failed("node killed by signal".into()),
    }

    match serde_json::from_slice::<BTreeMap<String, String>>(&output.stdout) {
        Ok(map) => Bridge::Resolved(map),
        Err(e) => Bridge::Failed(format!("bad json from ts-morph bridge: {e}")),
    }
}

/// Honour `SHUTTLE_NODE_BIN` so hermetic environments can pin an interpreter; default
/// to `node` on PATH.
fn node_bin() -> String {
    std::env::var("SHUTTLE_NODE_BIN").unwrap_or_else(|_| "node".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_present() -> bool {
        Command::new(node_bin())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // The bridge must never hard-fail when ts-morph is absent: a fresh checkout
    // has no node_modules, so `require("ts-morph")` exits 3 and we soft-skip.
    // This is the property extract-method relies on to stay daemonless-safe.
    #[test]
    fn missing_ts_morph_is_unavailable_not_error() {
        if !node_present() {
            return; // no node in CI image -> Unavailable is also acceptable
        }
        let src = "const x = makeThing();";
        match infer_types(src, "t.ts", &["x".to_string()]) {
            Bridge::Unavailable => {}
            Bridge::Resolved(m) => {
                // Only acceptable if a global ts-morph happens to be installed;
                // then it must still not fabricate a type for an unknown callee.
                assert!(!m.contains_key("x"), "should not invent a type: {m:?}");
            }
            Bridge::Failed(e) => panic!("expected soft-skip, got failure: {e}"),
        }
    }

    // Empty request short-circuits without spawning node at all.
    #[test]
    fn empty_names_resolves_empty() {
        match infer_types("const x = 1;", "t.ts", &[]) {
            Bridge::Resolved(m) => assert!(m.is_empty()),
            other => panic!("expected empty Resolved, got {other:?}"),
        }
    }
}
