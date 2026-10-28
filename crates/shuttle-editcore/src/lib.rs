//! shuttle-editcore — the latency-critical hot path.
//!
//! Applies content-anchored search/replace blocks with a 4-tier fuzzy
//! ladder. Every tier that fails is cheap; only a total miss is expensive
//! (it forces a model round-trip), so the ladder is tuned to almost never
//! miss on real, slightly-drifted files.
//!
//!   Tier 0  exact substring match
//!   Tier 1  whitespace-normalized match (trim trailing ws, unify tabs)
//!   Tier 2  similarity match over a sliding window (>= THRESHOLD)
//!   Tier 3  give up -> AnchorMiss (caller bounces to the model)

use shuttle_protocol::{ApplyOutcome, ApplyStatus, EditOp};

/// Minimum similarity (0.0-1.0) for a Tier-2 fuzzy accept.
pub const FUZZY_THRESHOLD: f32 = 0.90;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported op for editcore")]
    Unsupported,
}

/// Apply a single SearchReplace op to `source`, returning new content + outcome.
pub fn apply_search_replace(
    source: &str,
    search: &str,
    replace: &str,
    line_hint: Option<usize>,
) -> (String, ApplyOutcome) {
    // Tier 0: exact.
    match find_exact(source, search) {
        MatchResult::Unique(pos) => return (splice(source, pos, search.len(), replace), ok(0)),
        MatchResult::Multiple(positions) => {
            if let Some(pos) = disambiguate(&positions, source, line_hint) {
                return (splice(source, pos, search.len(), replace), ok(0));
            }
            return (source.to_string(), status(ApplyStatus::Ambiguous, 0));
        }
        MatchResult::None => {}
    }

    // Tier 1: whitespace-normalized. Re-indent `replace` to the source's real
    // indentation before splicing, since the match ignored leading whitespace.
    if let Some((start, len)) = find_ws_normalized(source, search) {
        let reindented = reindent(source, start, search, replace);
        return (splice(source, start, len, &reindented), ok(1));
    }

    // Tier 2: similarity window.
    if let Some((start, len)) = find_fuzzy(source, search, FUZZY_THRESHOLD) {
        return (splice(source, start, len, replace), ok(2));
    }

    // Tier 3: miss.
    (source.to_string(), status(ApplyStatus::AnchorMiss, 3))
}

enum MatchResult {
    None,
    Unique(usize),
    Multiple(Vec<usize>),
}

fn find_exact(hay: &str, needle: &str) -> MatchResult {
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        hits.push(from + rel);
        from += rel + needle.len().max(1);
    }
    match hits.len() {
        0 => MatchResult::None,
        1 => MatchResult::Unique(hits[0]),
        _ => MatchResult::Multiple(hits),
    }
}

fn disambiguate(positions: &[usize], source: &str, line_hint: Option<usize>) -> Option<usize> {
    let hint = line_hint?;
    positions
        .iter()
        .min_by_key(|&&p| {
            let line = source[..p].bytes().filter(|&b| b == b'\n').count() + 1;
            (line as isize - hint as isize).unsigned_abs()
        })
        .copied()
}

/// Byte `(start, end)` of each line's *content* (newline excluded).
///
/// A trailing `\n` yields a final empty span `(len, len)`, mirroring the way
/// most editors treat a file that ends in a newline.
fn line_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            spans.push((start, i));
            start = i + 1;
        }
    }
    spans.push((start, s.len()));
    spans
}

/// Collapse every whitespace run to a single space and trim both ends, making
/// the comparison insensitive to tab-vs-space and trailing whitespace.
fn norm_ws(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Leading-whitespace prefix of a line (the indentation string).
fn leading_ws(line: &str) -> &str {
    let end = line
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(line.len());
    &line[..end]
}

/// Tier 1: match `search` against `source` line-by-line under whitespace
/// normalization, returning the byte region of the matched window. The caller
/// re-indents `replace` to the source's real indentation before splicing.
fn find_ws_normalized(source: &str, search: &str) -> Option<(usize, usize)> {
    let src = line_spans(source);
    let needle: Vec<String> = search.split('\n').map(norm_ws).collect();
    let n = needle.len();
    if n == 0 || n > src.len() {
        return None;
    }
    for i in 0..=(src.len() - n) {
        let matched = (0..n).all(|k| {
            let (s, e) = src[i + k];
            norm_ws(&source[s..e]) == needle[k]
        });
        if matched {
            let start = src[i].0;
            let end = src[i + n - 1].1;
            return Some((start, end - start));
        }
    }
    None
}

/// Re-base `replace`'s indentation onto the source's real indentation.
///
/// Tier 1 matched under whitespace normalization, so the source may indent with
/// tabs where the planner wrote spaces (or vice-versa). We strip the search
/// block's first-line indent from each replace line and prepend the source's
/// actual first-line indent, preserving relative nesting.
fn reindent(source: &str, region_start: usize, search: &str, replace: &str) -> String {
    let src_line = {
        let e = source[region_start..]
            .find('\n')
            .map(|d| region_start + d)
            .unwrap_or(source.len());
        &source[region_start..e]
    };
    let src_indent = leading_ws(src_line);
    let search_indent = leading_ws(search.split('\n').next().unwrap_or(""));

    replace
        .split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                return String::new();
            }
            let body = line.strip_prefix(search_indent).unwrap_or(line);
            format!("{src_indent}{body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tier 2: slide a line window over `source`, scoring each candidate against
/// `search` with `similar`'s diff ratio. Windows of `n-1..=n+1` lines absorb
/// small drift (an inserted/removed line). Returns the best region at or above
/// `threshold`.
fn find_fuzzy(source: &str, search: &str, threshold: f32) -> Option<(usize, usize)> {
    let src = line_spans(source);
    let n = search.split('\n').count();
    if n == 0 || src.is_empty() {
        return None;
    }
    let mut best: Option<(f32, usize, usize)> = None;
    let lo = n.saturating_sub(1).max(1);
    let hi = n + 1;
    for w in lo..=hi {
        if w > src.len() {
            break;
        }
        for i in 0..=(src.len() - w) {
            let start = src[i].0;
            let end = src[i + w - 1].1;
            let window = &source[start..end];
            let ratio = similar::TextDiff::from_lines(window, search).ratio();
            if best.is_none_or(|(b, _, _)| ratio > b) {
                best = Some((ratio, start, end - start));
            }
        }
    }
    match best {
        Some((ratio, start, len)) if ratio >= threshold => Some((start, len)),
        _ => None,
    }
}

fn splice(source: &str, at: usize, remove_len: usize, replace: &str) -> String {
    let mut out = String::with_capacity(source.len() + replace.len());
    out.push_str(&source[..at]);
    out.push_str(replace);
    out.push_str(&source[at + remove_len..]);
    out
}

fn ok(tier: u8) -> ApplyOutcome {
    status(ApplyStatus::Applied, tier)
}

fn status(status: ApplyStatus, tier: u8) -> ApplyOutcome {
    ApplyOutcome {
        path: String::new(),
        status,
        match_tier: tier,
        message: None,
    }
}

/// Route an [`EditOp`] to the right handler. CreateFile / Codemod are handled
/// by the CLI layer (terminal fast path / ast-grep), not here.
pub fn is_editcore_op(op: &EditOp) -> bool {
    matches!(op, EditOp::SearchReplace { .. } | EditOp::Insert { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(src: &str, search: &str, replace: &str) -> (String, ApplyOutcome) {
        apply_search_replace(src, search, replace, None)
    }

    #[test]
    fn tier0_exact_unique() {
        let src = "fn a() {}\nfn b() {}\n";
        let (out, oc) = apply(src, "fn b() {}", "fn b() { 1 }");
        assert_eq!(oc.status, ApplyStatus::Applied);
        assert_eq!(oc.match_tier, 0);
        assert_eq!(out, "fn a() {}\nfn b() { 1 }\n");
    }

    #[test]
    fn tier0_ambiguous_without_hint() {
        let src = "x\nx\n";
        let (out, oc) = apply(src, "x", "y");
        assert_eq!(oc.status, ApplyStatus::Ambiguous);
        assert_eq!(out, src, "ambiguous match must not mutate source");
    }

    #[test]
    fn tier0_ambiguous_resolved_by_line_hint() {
        let src = "x\nx\nx\n";
        let (out, oc) = apply_search_replace(src, "x", "y", Some(2));
        assert_eq!(oc.status, ApplyStatus::Applied);
        assert_eq!(out, "x\ny\nx\n");
    }

    #[test]
    fn tier1_tab_vs_spaces_reindents() {
        // Source indents with a tab; planner wrote 4 spaces.
        let src = "fn f() {\n\treturn 1;\n}\n";
        let search = "    return 1;";
        let replace = "    return 2;";
        let (out, oc) = apply(src, search, replace);
        assert_eq!(oc.status, ApplyStatus::Applied);
        assert_eq!(oc.match_tier, 1);
        // Replacement keeps the source's tab indentation.
        assert_eq!(out, "fn f() {\n\treturn 2;\n}\n");
    }

    #[test]
    fn tier1_trailing_whitespace_tolerated() {
        // Search carries trailing spaces the source lacks -> exact fails,
        // whitespace normalization (Tier 1) still matches.
        let src = "let a = 1;\n";
        let (out, oc) = apply(src, "let a = 1;   ", "let a = 2;");
        assert_eq!(oc.status, ApplyStatus::Applied);
        assert_eq!(oc.match_tier, 1);
        assert_eq!(out, "let a = 2;\n");
    }

    #[test]
    fn tier2_fuzzy_absorbs_small_drift() {
        // A 12-line block where exactly one line drifted. By line similarity
        // that is ~11/12 = 0.917, clearing the 0.90 Tier-2 threshold while
        // Tier 0/1 (which need every line identical) both miss.
        let mut lines: Vec<String> = (0..12).map(|i| format!("    step_{i}();")).collect();
        let src = lines.join("\n") + "\n";
        // The planner's search block remembers an older name for step 6.
        lines[6] = "    step_six_renamed();".to_string();
        let search = lines.join("\n");
        let replace = "    // collapsed\n    done();";
        let (out, oc) = apply(&src, &search, replace);
        assert_eq!(oc.status, ApplyStatus::Applied);
        assert_eq!(oc.match_tier, 2);
        assert!(out.contains("done();"));
    }

    #[test]
    fn tier3_total_miss_bounces() {
        let src = "completely unrelated content\n";
        let (out, oc) = apply(src, "fn nonexistent_symbol(a, b, c) -> Result<T>", "x");
        assert_eq!(oc.status, ApplyStatus::AnchorMiss);
        assert_eq!(oc.match_tier, 3);
        assert_eq!(out, src);
    }

    #[test]
    fn line_spans_handles_trailing_newline() {
        assert_eq!(line_spans("a\nb\n"), vec![(0, 1), (2, 3), (4, 4)]);
        assert_eq!(line_spans("a\nb"), vec![(0, 1), (2, 3)]);
    }
}
