//! G7 · U1c — the `shuttle impact` **co-change hint** surface.
//!
//! This module is the *only* place co-change touches the impact command, and it
//! is deliberately a **sibling** of the mechanical reverse-dependency closure,
//! never a member of it. The partners surfaced here are a **review hint** mined
//! from git history ([`shuttle_symbols::cochange`]) — two files that have
//! *historically been edited together* — and they are **NEVER** folded into the
//! re-verify / blast-radius set.
//!
//! # The §7 red line, restated at the call site
//! Co-change is a naturally weak signal (file-level, undirected, no proven
//! dependency). It surfaces **only** as a hint the human skims before review; it
//! can never grow the mechanical impact set nor manufacture a walkable edge.
//! * **History-insufficient → no hint.** A shallow clone / squashed history
//!   sets [`CoChangeHints::insufficient_history`] and emits **no** partners
//!   rather than extrapolate — absence of proof is not proof of absence.
//! * The [`CoChangeHints`] envelope is intentionally distinct from [`NavHit`]:
//!   a caller can never accidentally treat a co-change partner as a re-verify
//!   target because it never arrives in the same list.

use serde::{Deserialize, Serialize};
use shuttle_symbols::cochange::{from_git, CoChangeConfig};

use crate::{normalize_path, NavError, QueryOptions};

/// One historical co-change partner of a changed file: the partner path, its
/// accumulated decayed weight, and the number of commits the two were touched
/// in together. Sorted strongest-first by the producer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoChangePartner {
    /// The partner file path (post-image, repo-relative as git reports it).
    pub file: String,
    /// Accumulated decayed co-change weight (≥ the config threshold).
    pub weight: f64,
    /// Commits in which both files were touched together.
    pub commits: usize,
}

/// The co-change partners of a single changed file. `partners` is empty when the
/// file has no above-threshold historical coupling — an honest "no hint", not a
/// failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoChangeFileHints {
    /// The changed file whose partners these are (normalized).
    pub changed: String,
    /// Its co-change partners, strongest weight first (deterministic ties on
    /// path). Empty ⇒ no above-threshold coupling in history.
    pub partners: Vec<CoChangePartner>,
}

/// The full co-change hint envelope for an `shuttle impact --cochange` run. This is a
/// **hint surface**, structurally separate from the [`NavHit`] re-verify set: it
/// is never merged into the mechanical closure and every consumer sees it as a
/// review aid only.
///
/// [`NavHit`]: crate::NavHit
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoChangeHints {
    /// Per-changed-file partner lists, in the order the changed files were given
    /// (normalized + deduped). A file with no partners still appears with an
    /// empty list so the caller sees it was considered.
    pub files: Vec<CoChangeFileHints>,
    /// True when git history was too shallow / short to establish co-change; when
    /// set, every `partners` list is empty and the caller must report
    /// `insufficient-history`, never "no coupling" (§7).
    pub insufficient_history: bool,
}

impl CoChangeHints {
    /// Whether any changed file surfaced at least one partner.
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.partners.is_empty())
    }
}

/// Derive the co-change **hints** for a set of changed files over `root`'s git
/// history. This runs [`shuttle_symbols::cochange::from_git`] once and, for each
/// changed file, collects its above-threshold historical partners.
///
/// The result is a [`CoChangeHints`] envelope — a review surface that is
/// **never** the re-verify set. `max_commits` bounds how far back we read
/// (`0` = all history) so a huge repo stays inside the daemonless latency
/// budget; `cfg` carries the honest decay/threshold/insufficient-history knobs.
///
/// Returns `Err` only on a genuine git failure (not a repo, git missing). An
/// empty / shallow history is **not** an error — it is an honest
/// `insufficient_history` envelope with no partners.
pub fn cochange_hints(
    changed: &[String],
    cfg: &CoChangeConfig,
    max_commits: usize,
    opts: &QueryOptions,
) -> Result<CoChangeHints, NavError> {
    let wanted: Vec<String> = changed
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| normalize_path(c))
        .collect::<std::collections::BTreeSet<String>>()
        .into_iter()
        .collect();
    if wanted.is_empty() {
        return Ok(CoChangeHints {
            files: Vec::new(),
            insufficient_history: false,
        });
    }

    let graph = from_git(&opts.root, cfg, max_commits)
        .map_err(|e| NavError::Io(std::io::Error::other(e)))?;

    if graph.insufficient_history {
        // §7: never extrapolate from a truncated history. Report every changed
        // file with an empty partner list under the insufficient-history flag.
        return Ok(CoChangeHints {
            files: wanted
                .into_iter()
                .map(|changed| CoChangeFileHints {
                    changed,
                    partners: Vec::new(),
                })
                .collect(),
            insufficient_history: true,
        });
    }

    // Collect partners per changed file. `partners()` already sorts by
    // descending weight then path; we look each partner's commit count up from
    // the undirected edge so the hint carries the same evidence the derivation
    // saw. Co-change paths are matched with the same suffix-tolerant rule the
    // symbol closure uses, so a repo-relative `--changed` still lines up with a
    // differently-rooted numstat path.
    let files: Vec<CoChangeFileHints> = wanted
        .into_iter()
        .map(|changed| {
            let mut partners: Vec<CoChangePartner> = Vec::new();
            for edge in graph.edges() {
                let (this, other) = if cochange_path_matches(&edge.a, &changed) {
                    (&edge.a, &edge.b)
                } else if cochange_path_matches(&edge.b, &changed) {
                    (&edge.b, &edge.a)
                } else {
                    continue;
                };
                let _ = this;
                partners.push(CoChangePartner {
                    file: other.clone(),
                    weight: edge.weight,
                    commits: edge.commits,
                });
            }
            // Strongest hint first, deterministic ties on path.
            partners.sort_by(|x, y| {
                y.weight
                    .partial_cmp(&x.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| x.file.cmp(&y.file))
            });
            CoChangeFileHints { changed, partners }
        })
        .collect();

    Ok(CoChangeHints {
        files,
        insufficient_history: false,
    })
}

/// Suffix-tolerant equality between a git-reported co-change path and a
/// caller-supplied changed path (both already `/`-normalized on the changed
/// side). Mirrors the symbol closure's `path_matches` so `src/a.rs` lines up
/// with a numstat `crate/src/a.rs` and vice-versa, with boundaries on `/` so
/// `latest.rs` never matches `test.rs`.
fn cochange_path_matches(git_path: &str, changed: &str) -> bool {
    let g = normalize_path(git_path);
    g == *changed || g.ends_with(&format!("/{changed}")) || changed.ends_with(&format!("/{g}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_changed_yields_empty_not_error() {
        let opts = QueryOptions::new(".");
        let h = cochange_hints(&[], &CoChangeConfig::default(), 0, &opts).unwrap();
        assert!(h.files.is_empty());
        assert!(!h.insufficient_history);
        assert!(h.is_empty());
    }

    #[test]
    fn path_match_is_suffix_tolerant_on_slash_boundary() {
        assert!(cochange_path_matches("crate/src/a.rs", "src/a.rs"));
        assert!(cochange_path_matches("src/a.rs", "crate/src/a.rs"));
        assert!(cochange_path_matches("src/a.rs", "src/a.rs"));
        // boundary is on `/`, so a bare suffix never spuriously matches.
        assert!(!cochange_path_matches("src/latest.rs", "test.rs"));
        assert!(!cochange_path_matches("src/a.rs", "b.rs"));
    }
}
