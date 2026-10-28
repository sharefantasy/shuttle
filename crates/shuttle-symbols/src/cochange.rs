//! G7 · U1 — **co-change** derivation: a probabilistic-but-honest weak signal
//! mined from git history ().
//!
//! Two files that have *historically been edited in the same commits* carry an
//! **empirical coupling** the static symbol graph cannot see — two files with
//! no import relation but a real semantic bond (a schema and its validator, a
//! flag and the code that reads it). This module derives that coupling from
//! `git log --numstat` and hands it to the [`shuttle impact`](../../shuttle-cli) hint
//! layer.
//!
//! # The §7 red line, enforced structurally
//! Co-change is a **naturally weak signal**, and this module can only ever
//! produce a weak signal:
//! * It is **file-level**, never symbol-level — it never enters the symbol
//!   [`crate::graph::SymbolGraph`] closure and so can never grow the mechanical
//!   re-verify / impact set. It surfaces **only** as a review-surface hint.
//! * It proves **no direction and no dependency** — the graph never grows a
//!   walkable edge from it (the taxonomy label
//!   [`crate::graph::EdgeKind::CoChange`] is pinned `Low`, is never
//!   trusted-directed, and is skipped by both blast-radius closures).
//! * **History-insufficient → no edge.** A shallow clone / squashed history
//!   cannot establish co-change; we mark [`CoChangeGraph::insufficient_history`]
//!   and emit **nothing** rather than extrapolate (§2 坑 #8).
//!
//! # Commit-size decay (§2.2, 坑 #7)
//! A pair's weight is the sum, over commits touching **both** files, of a
//! `decay(commit_size)` that *shrinks as the commit grows*:
//!
//! ```text
//! co_change_weight(A, B) = Σ_over_commits_touching_both  decay(size(commit))
//! decay(n) = 1 / (n - 1)          // n = files in the commit, n ≥ 2
//! ```
//!
//! so a focused 2-file commit contributes a full `1.0` to its single pair,
//! while a 100-file reformat contributes `1/99 ≈ 0.01` to each of its pairs —
//! sweeping, mechanical megacommits (format / mass-rename) are suppressed and
//! never manufacture correlations. As a hard belt-and-suspenders (and to bound
//! the O(k²) pair enumeration a giant squash would otherwise cost — daemonless
//! latency matters), commits larger than [`CoChangeConfig::max_commit_size`]
//! are dropped wholesale before any pair is formed.
//!
//! Only pairs whose accumulated weight reaches
//! [`CoChangeConfig::weight_threshold`] become co-change hints; everything below
//! is noise and is discarded.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

const MAX_GIT_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: u64 = 64 * 1024;
const GIT_LOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Tunables for co-change derivation. [`CoChangeConfig::default`] is the honest,
/// conservative setting used by the CLI.
#[derive(Debug, Clone, Copy)]
pub struct CoChangeConfig {
    /// Minimum accumulated decayed weight for a pair to surface as a hint.
    /// Below this the correlation is treated as noise and dropped.
    pub weight_threshold: f64,
    /// Commits touching **more** files than this are dropped *wholesale* before
    /// any pair is formed — a format / mass-rename / squash megacommit proves no
    /// coupling and would otherwise cost O(k²) pair work. `0` disables the cap
    /// (decay alone then suppresses large commits).
    pub max_commit_size: usize,
    /// Fewer than this many usable commits in history → we cannot honestly
    /// establish co-change: emit nothing and flag `insufficient_history`.
    pub min_history_commits: usize,
}

impl Default for CoChangeConfig {
    fn default() -> Self {
        // threshold 2.0: a pair must be corroborated by the equivalent of two
        // focused 2-file commits (or many more large ones) before it is a hint.
        // max_commit_size 50: past this a commit is mechanical noise, not a
        // coupling. min_history 2: a single commit cannot establish co-change.
        CoChangeConfig {
            weight_threshold: 2.0,
            max_commit_size: 50,
            min_history_commits: 2,
        }
    }
}

/// The decayed contribution a commit of `size` files makes to *each* of its
/// file pairs. `size < 2` cannot form a pair and contributes nothing.
///
/// `decay(n) = 1 / (n - 1)`: strictly decreasing in `n`, so larger commits
/// contribute less per pair (§2.2). A 2-file commit contributes `1.0`.
pub fn decay(size: usize) -> f64 {
    if size < 2 {
        0.0
    } else {
        1.0 / (size as f64 - 1.0)
    }
}

/// One derived co-change hint between two files, in canonical order `a < b`.
#[derive(Debug, Clone, PartialEq)]
pub struct CoChangeEdge {
    /// Lexicographically-smaller file path (co-change is undirected).
    pub a: String,
    /// Lexicographically-larger file path.
    pub b: String,
    /// Accumulated decayed co-change weight (≥ [`CoChangeConfig::weight_threshold`]).
    pub weight: f64,
    /// Number of commits in which both files were touched together.
    pub commits: usize,
}

/// The file-level co-change graph: a set of undirected weak hints, plus the
/// honest `insufficient_history` flag. This is **never** a symbol graph and
/// **never** feeds the mechanical closure — it is a review-surface hint source.
#[derive(Debug, Clone, Default)]
pub struct CoChangeGraph {
    edges: BTreeMap<(String, String), CoChangeEdge>,
    /// True when history was too shallow / short to establish co-change; when
    /// set, `edges` is empty and callers must report `insufficient-history`
    /// rather than "no coupling" (§7 — absence of proof is not proof of absence).
    pub insufficient_history: bool,
}

impl CoChangeGraph {
    /// All derived hints, canonical order (`a`, then `b`).
    pub fn edges(&self) -> impl Iterator<Item = &CoChangeEdge> {
        self.edges.values()
    }

    /// Number of hints.
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Whether there are no hints.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// The co-change partners of `file`: every other file historically edited
    /// with it above threshold, as `(partner, weight)`, sorted by descending
    /// weight then path (strongest hint first, deterministic ties).
    pub fn partners(&self, file: &str) -> Vec<(&str, f64)> {
        let mut out: Vec<(&str, f64)> = Vec::new();
        for e in self.edges.values() {
            if e.a == file {
                out.push((e.b.as_str(), e.weight));
            } else if e.b == file {
                out.push((e.a.as_str(), e.weight));
            }
        }
        out.sort_by(|x, y| {
            y.1.partial_cmp(&x.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| x.0.cmp(y.0))
        });
        out
    }
}

/// Parse the raw output of the sentinel-delimited `git log --numstat` we run in
/// [`from_git`] into a per-commit list of touched file paths.
///
/// The log is emitted with `--pretty=format:<SENTINEL>` so each commit begins
/// with a line equal to [`COMMIT_SENTINEL`], followed by numstat rows
/// `added\tdeleted\tpath`. Binary files show `-\t-\tpath`; both count as a
/// touch. Rename rows (`old => new`, or the brace form `dir/{old => new}.rs`)
/// are normalized to the **post-image** path — the file as it exists at head is
/// what a future edit would touch. This is a **pure** function: no I/O, fully
/// unit-testable against fixture text.
pub fn parse_numstat_log(raw: &str) -> Vec<Vec<String>> {
    let mut commits: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in raw.lines() {
        let line = line.trim_end_matches(['\r']);
        if line == COMMIT_SENTINEL {
            if let Some(files) = current.take() {
                commits.push(files);
            }
            current = Some(Vec::new());
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        // numstat row: <added>\t<deleted>\t<path>
        let mut it = line.splitn(3, '\t');
        let (_added, _deleted, path) = match (it.next(), it.next(), it.next()) {
            (Some(a), Some(d), Some(p)) => (a, d, p),
            _ => continue, // not a numstat row (stray line) — ignore
        };
        let path = normalize_numstat_path(path);
        if path.is_empty() {
            continue;
        }
        if let Some(files) = current.as_mut() {
            files.push(path);
        }
        // A numstat row before any sentinel is impossible (the pretty format
        // always leads), so we simply drop it if `current` is None.
    }
    if let Some(files) = current.take() {
        commits.push(files);
    }
    commits
}

/// Normalize a numstat path field to the post-image path, resolving git's two
/// rename encodings: the plain `old => new` and the braced `pre/{a => b}/post`.
fn normalize_numstat_path(field: &str) -> String {
    let field = field.trim();
    // Braced form: pre{a => b}post  →  pre + b + post
    if let (Some(lo), Some(hi)) = (field.find('{'), field.find('}')) {
        if lo < hi {
            if let Some(arrow) = field[lo..hi].find("=>") {
                let pre = &field[..lo];
                let inner_new = field[lo + 1 + arrow + 2..hi].trim();
                let post = &field[hi + 1..];
                let joined = format!("{pre}{inner_new}{post}");
                // Collapse any doubled slash the brace edit may leave.
                return joined.replace("//", "/").trim().to_string();
            }
        }
    }
    // Plain form: old => new  →  new
    if let Some(arrow) = field.find("=>") {
        return field[arrow + 2..].trim().to_string();
    }
    field.to_string()
}

/// Derive the co-change graph from a list of per-commit file sets (the output
/// of [`parse_numstat_log`]) plus whether the repository is `shallow`. **Pure**
/// — the whole adjudication (decay, cap, threshold, insufficient-history) is
/// testable with no git present.
///
/// * `shallow == true` **or** fewer than `cfg.min_history_commits` usable
///   commits → `insufficient_history = true`, **no edges** (§7 — never
///   extrapolate from a truncated history).
/// * A commit larger than `cfg.max_commit_size` is dropped whole (megacommit
///   suppression / cost bound).
/// * Each surviving commit adds `decay(size)` to every unordered pair of its
///   files; pairs reaching `cfg.weight_threshold` become hints.
pub fn derive_cochange(
    commits: &[Vec<String>],
    shallow: bool,
    cfg: &CoChangeConfig,
) -> CoChangeGraph {
    // Usable commits: those that can form at least one pair after the megacommit
    // cap. (A 1-file commit forms no pair; it neither helps nor hurts, but it
    // also does not count toward "we have enough history to speak".)
    let capped: Vec<&Vec<String>> = commits
        .iter()
        .filter(|c| cfg.max_commit_size == 0 || c.len() <= cfg.max_commit_size)
        .collect();

    if shallow || commits.len() < cfg.min_history_commits {
        return CoChangeGraph {
            edges: BTreeMap::new(),
            insufficient_history: true,
        };
    }

    // Accumulate decayed weight and co-occurrence count per unordered pair.
    let mut acc: BTreeMap<(String, String), (f64, usize)> = BTreeMap::new();
    for commit in capped {
        let size = commit.len();
        if size < 2 {
            continue;
        }
        let w = decay(size);
        // Dedup files within a commit (a rename can list a path twice) and sort
        // for canonical pair order.
        let mut files: Vec<&String> = commit.iter().collect();
        files.sort();
        files.dedup();
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let key = (files[i].clone(), files[j].clone());
                let e = acc.entry(key).or_insert((0.0, 0));
                e.0 += w;
                e.1 += 1;
            }
        }
    }

    let edges: BTreeMap<(String, String), CoChangeEdge> = acc
        .into_iter()
        .filter(|(_, (w, _))| *w >= cfg.weight_threshold)
        .map(|((a, b), (weight, commits))| {
            (
                (a.clone(), b.clone()),
                CoChangeEdge {
                    a,
                    b,
                    weight,
                    commits,
                },
            )
        })
        .collect();

    CoChangeGraph {
        edges,
        insufficient_history: false,
    }
}

/// Sentinel line separating commits in the `git log` we run. Chosen to never
/// collide with a numstat row (which is always `\d|-\t\d|-\t...`).
pub const COMMIT_SENTINEL: &str = "@@shuttle-cochange-commit@@";

/// I/O wrapper: run `git log --numstat` over `root`'s history and derive the
/// co-change graph. Detects a shallow repository (which forces
/// `insufficient_history`). `max_commits` bounds how far back we read (`0` =
/// all history) so a huge repo stays within the daemonless latency budget.
///
/// Returns `Err(msg)` only for a genuine git failure (not a repo, git missing);
/// an empty/short history is **not** an error — it is an honest
/// `insufficient_history` graph.
pub fn from_git(
    root: &str,
    cfg: &CoChangeConfig,
    max_commits: usize,
) -> Result<CoChangeGraph, String> {
    let shallow = is_shallow(root);

    let fmt = format!("--pretty=format:{COMMIT_SENTINEL}");
    let mut args: Vec<String> = vec![
        "log".to_string(),
        "--numstat".to_string(),
        "--no-color".to_string(),
        fmt,
    ];
    if max_commits > 0 {
        args.push(format!("--max-count={max_commits}"));
    }
    let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| error.to_string())?;
    let mut command = Command::new(binary);
    command.args(&argrefs).current_dir(root);
    let out = shuttle_runtime_tools::output_bounded(
        &mut command,
        GIT_LOG_TIMEOUT,
        MAX_GIT_LOG_BYTES,
        MAX_GIT_STDERR_BYTES,
    )?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let commits = parse_numstat_log(&raw);
    Ok(derive_cochange(&commits, shallow, cfg))
}

/// Whether `root` is a shallow clone (`git rev-parse --is-shallow-repository`).
/// A parse failure conservatively returns `false` — we only *force*
/// insufficient-history when git positively reports shallow.
fn is_shallow(root: &str) -> bool {
    let Ok(binary) = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git) else {
        return false;
    };
    Command::new(binary)
        .args(["rev-parse", "--is-shallow-repository"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CoChangeConfig {
        CoChangeConfig::default()
    }

    #[test]
    fn decay_is_strictly_decreasing() {
        assert_eq!(decay(0), 0.0);
        assert_eq!(decay(1), 0.0);
        assert_eq!(decay(2), 1.0);
        assert!(decay(3) < decay(2));
        assert!(decay(100) < decay(3));
        assert!(decay(100) > 0.0);
    }

    #[test]
    fn parse_splits_commits_on_sentinel() {
        let raw = format!(
            "{S}\n10\t2\tsrc/a.rs\n3\t0\tsrc/b.rs\n\n{S}\n1\t1\tsrc/a.rs\n",
            S = COMMIT_SENTINEL
        );
        let commits = parse_numstat_log(&raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0], vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(commits[1], vec!["src/a.rs"]);
    }

    #[test]
    fn parse_handles_binary_and_renames() {
        let raw = format!(
            "{S}\n-\t-\tassets/logo.png\n5\t5\tsrc/{{old => new}}.rs\n2\t0\tlib/a => lib/b\n",
            S = COMMIT_SENTINEL
        );
        let commits = parse_numstat_log(&raw);
        assert_eq!(commits.len(), 1);
        // binary counts as a touch; renames normalize to the post-image path.
        assert_eq!(commits[0], vec!["assets/logo.png", "src/new.rs", "lib/b"]);
    }

    #[test]
    fn focused_repeated_pair_becomes_a_hint() {
        // three focused 2-file commits: weight = 3 * decay(2) = 3.0 >= 2.0.
        let commits = vec![
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
        ];
        let g = derive_cochange(&commits, false, &cfg());
        assert!(!g.insufficient_history);
        assert_eq!(g.len(), 1);
        let e = g.edges().next().unwrap();
        assert_eq!((e.a.as_str(), e.b.as_str()), ("a.rs", "b.rs"));
        assert_eq!(e.commits, 3);
        assert!((e.weight - 3.0).abs() < 1e-9);
        assert_eq!(g.partners("a.rs"), vec![("b.rs", 3.0)]);
        assert_eq!(g.partners("b.rs"), vec![("a.rs", 3.0)]);
    }

    #[test]
    fn single_cooccurrence_below_threshold_is_dropped() {
        // one 2-file commit: weight 1.0 < 2.0 → noise, no hint.
        let commits = vec![
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["x.rs".to_string()],
        ];
        let g = derive_cochange(&commits, false, &cfg());
        assert!(!g.insufficient_history);
        assert!(g.is_empty(), "a single co-occurrence is noise");
    }

    #[test]
    fn megacommit_is_dropped_wholesale() {
        // a 100-file "reformat" commit must not manufacture any hint, even
        // repeated: it exceeds max_commit_size and is dropped before pairing.
        let big: Vec<String> = (0..100).map(|i| format!("f{i}.rs")).collect();
        let commits = vec![big.clone(), big.clone(), big];
        let g = derive_cochange(&commits, false, &cfg());
        assert!(!g.insufficient_history);
        assert!(
            g.is_empty(),
            "a 100-file megacommit must not manufacture 100^2 co-change edges"
        );
    }

    #[test]
    fn decay_suppresses_large_but_uncapped_commits() {
        // With the cap disabled, decay alone must still keep a large commit's
        // per-pair contribution far below threshold.
        let cfg = CoChangeConfig {
            max_commit_size: 0,
            ..CoChangeConfig::default()
        };
        let big: Vec<String> = (0..40).map(|i| format!("f{i}.rs")).collect();
        let commits = vec![big.clone(), big];
        let g = derive_cochange(&commits, false, &cfg);
        // each pair got 2 * decay(40) = 2/39 ≈ 0.051 << 2.0 → nothing.
        assert!(g.is_empty(), "decay must suppress large commits: {:?}", g);
    }

    #[test]
    fn shallow_history_yields_insufficient_history_no_edges() {
        let commits = vec![
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
        ];
        let g = derive_cochange(&commits, true, &cfg());
        assert!(g.insufficient_history, "shallow → insufficient-history");
        assert!(g.is_empty(), "insufficient history emits no edges (§7)");
    }

    #[test]
    fn too_few_commits_is_insufficient_history() {
        let commits = vec![vec!["a.rs".to_string(), "b.rs".to_string()]];
        let g = derive_cochange(&commits, false, &cfg());
        assert!(g.insufficient_history);
        assert!(g.is_empty());
    }

    #[test]
    fn partners_sorted_by_descending_weight() {
        // a.rs co-changes strongly with b.rs, weakly (but above threshold) with c.rs.
        let commits = vec![
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["a.rs".to_string(), "c.rs".to_string()],
            vec!["a.rs".to_string(), "c.rs".to_string()],
        ];
        let g = derive_cochange(&commits, false, &cfg());
        let p = g.partners("a.rs");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].0, "b.rs"); // weight 3.0 first
        assert_eq!(p[1].0, "c.rs"); // weight 2.0 second
        assert!(p[0].1 > p[1].1);
    }
}
