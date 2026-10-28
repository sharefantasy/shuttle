//! `shuttle impact --base <oid> --head <oid>` — the land-time incremental re-verify
//! set (T2).
//!
//! # What this is
//! At land time (the moment a branch merges back), a lander does not need to
//! re-run the whole test/build universe — only the part of it a `base..head`
//! diff can actually perturb. This module computes exactly that **minimal
//! re-verify set** at compile-unit (file) granularity:
//!
//! > re-verify set = *changed units* ∪ *their reverse-dependency closure* (C12)
//!
//! The changed units come straight from `git diff --name-only base..head` — they
//! are commit-authoritative and never depend on any index. The closure is walked
//! backwards over the **directional** symbol graph (T3): "who transitively
//! depends on a changed unit and therefore must be re-verified too."
//!
//! # Staleness watermark (C11) — why this refuses to guess
//! The reverse-closure is only as good as the symbol graph it walks, and that
//! graph reflects whatever is *on disk* (the worktree HEAD), not necessarily the
//! `--head` commit the diff lands. So the detail carries a watermark triple —
//! `base_oid`, `head_oid`, and `index_base_oid` (the commit the live graph
//! actually reflects). When `index_base_oid != head_oid` the graph is **stale
//! relative to the requested head**: this module then refuses to reason off it.
//! It still emits the authoritative changed set (git diff never lies), marks the
//! detail `degraded`, and blocks with [`reason::STALE_INDEX`] whose one next
//! action is "re-index at `head_oid`, then re-run." It never silently trusts a
//! stale closure and never fabricates a set it cannot prove (§7 "绝不瞎猜").
//!
//! # shuttle never lands (#11)
//! This module's sole output is the *set to re-verify*. The land action itself —
//! credentials, physical merge, CI trigger — belongs to the calling control
//! plane. `shuttle impact` computes and hands off; it does not merge, push, or run
//! the re-verification. The [`ToolOutcome`] is the handoff envelope.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use shuttle_symbols::graph::{GraphConfidence, SymbolGraph};
use shuttle_symbols::SymbolTable;

use shuttle_protocol::{
    reason, OutcomeConfidence, ReverifyDetail, ReverifyUnit, ToolOutcome, TypedDetail,
};

/// Tool name stamped into every reverify-produced envelope.
pub const TOOL_IMPACT: &str = "impact";

/// Granularity label for the units this module emits. Go package alignment is
/// a separate follow-up (T4); everything here is file-granular.
pub const GRANULARITY_FILE: &str = "file";

// ---------------------------------------------------------------------------
// Path helpers (kept local so reverify.rs does not couple to lib-private fns).
// ---------------------------------------------------------------------------

/// Normalize a path for comparison: `\\` → `/`, strip a leading `./`.
fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// A symbol's def file `sym_path` belongs to a changed-file set when the two are
/// equal or one is a `/`-boundary suffix of the other (handles repo-relative vs
/// differently-rooted ctags paths — the same rule shuttle-nav's impact uses).
fn path_owned_by(sym_path: &str, files: &BTreeSet<String>) -> bool {
    let sp = norm(sym_path);
    files.iter().any(|w| {
        let w = norm(w);
        sp == w || sp.ends_with(&format!("/{w}")) || w.ends_with(&format!("/{sp}"))
    })
}

/// Map an [`shuttle-symbols`] graph confidence onto the protocol mirror. 1:1.
fn lower_conf(c: GraphConfidence) -> OutcomeConfidence {
    match c {
        GraphConfidence::High => OutcomeConfidence::High,
        GraphConfidence::Medium => OutcomeConfidence::Medium,
        GraphConfidence::Low => OutcomeConfidence::Low,
    }
}

// ---------------------------------------------------------------------------
// The pure computation (no I/O, deterministic).
// ---------------------------------------------------------------------------

/// Compute the land-time re-verify [`ReverifyDetail`] for a `base..head` diff.
///
/// `changed_files` is the authoritative diff (from `git diff --name-only`).
/// `graph_table` is `Some((graph, table))` **only when the live graph is fresh**
/// (its `index_base_oid == head_oid`); pass `None` when the graph is stale
/// relative to `head_oid`. That single choice drives the staleness watermark:
///
/// * `Some(..)` → walk the reverse-dependency closure and union it with the
///   changed set. `degraded = false`.
/// * `None`     → the graph is stale, so the closure would be untrustworthy.
///   Per C11 this module refuses to walk it and emits only the authoritative
///   changed set, with `degraded = true`.
///
/// Pure and deterministic: same inputs → same units in the same order
/// (changed-first, then by path).
pub fn compute_reverify(
    changed_files: &[String],
    base_oid: &str,
    head_oid: &str,
    index_base_oid: &str,
    max_depth: usize,
    graph_table: Option<(&SymbolGraph, &SymbolTable)>,
) -> ReverifyDetail {
    let degraded = graph_table.is_none();

    // Normalized, deduplicated changed set — the commit-authoritative core.
    let changed: BTreeSet<String> = changed_files
        .iter()
        .filter(|c| !c.trim().is_empty())
        .map(|c| norm(c))
        .collect();

    // Accumulate reverse-dependency units keyed by file path, tracking the
    // shortest distance and weakest confidence along the reaching path. A file
    // that is itself in the changed set is never a "reverse-dep" — it is a
    // "changed" unit and takes precedence.
    let mut reverse: BTreeMap<String, (usize, GraphConfidence)> = BTreeMap::new();

    if let Some((graph, table)) = graph_table {
        // Seeds = every symbol defined in a changed file.
        let seeds: Vec<String> = table
            .symbols
            .iter()
            .filter(|s| path_owned_by(&s.path, &changed))
            .map(|s| s.name.clone())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();

        if !seeds.is_empty() {
            // Directed closure only (T3): follow trusted import-directed edges so
            // the re-verify set is the acyclic zero-model DAG, never inflated by
            // weak name-match recall.
            for hit in graph.blast_radius_directed(&seeds, max_depth) {
                if hit.path.is_empty() {
                    continue; // no known def site — nothing actionable to point at
                }
                let p = norm(&hit.path);
                if changed.contains(&p) || path_owned_by(&p, &changed) {
                    continue; // a changed unit, not a reverse-dep
                }
                reverse
                    .entry(p)
                    .and_modify(|(d, c)| {
                        if hit.distance < *d || (hit.distance == *d && hit.confidence < *c) {
                            *d = hit.distance;
                            *c = hit.confidence;
                        }
                    })
                    .or_insert((hit.distance, hit.confidence));
            }
        }
    }

    // Lower to the typed unit list: changed-first (distance 0, High), then the
    // reverse-dependency closure sorted by path.
    let mut units: Vec<ReverifyUnit> = Vec::new();
    for path in &changed {
        units.push(ReverifyUnit {
            path: path.clone(),
            reason: "changed".to_string(),
            confidence: OutcomeConfidence::High,
            distance: 0,
        });
    }
    for (path, (distance, conf)) in reverse {
        units.push(ReverifyUnit {
            path,
            reason: "reverse-dep".to_string(),
            confidence: lower_conf(conf),
            distance,
        });
    }

    // Deterministic order: changed units (distance 0) first, then by distance,
    // then by path.
    units.sort_by(|a, b| {
        let ak = (a.reason != "changed", a.distance, a.path.as_str());
        let bk = (b.reason != "changed", b.distance, b.path.as_str());
        ak.cmp(&bk)
    });

    let reverify_size = units.len();
    ReverifyDetail {
        base_oid: base_oid.to_string(),
        head_oid: head_oid.to_string(),
        index_base_oid: index_base_oid.to_string(),
        changed_files: changed.into_iter().collect(),
        reverify: units,
        granularity: GRANULARITY_FILE.to_string(),
        degraded,
        reverify_size,
    }
}

// ---------------------------------------------------------------------------
// Lowering to the universal ToolOutcome envelope.
// ---------------------------------------------------------------------------

/// Lower a [`ReverifyDetail`] into a [`ToolOutcome`].
///
/// * `degraded` (stale graph, C11) → `Blocked` (`stale-index`): the one next
///   action is to re-index at `head_oid` and re-run. The authoritative changed
///   set is still carried in the detail so the lander can proceed conservatively.
/// * Otherwise → `Green`: the re-verify set is the complete changed ∪ closure.
///
/// Either way the envelope is a *handoff*, not a land — shuttle never merges (#11).
pub fn reverify_outcome(detail: ReverifyDetail) -> ToolOutcome {
    let changed = detail.changed_files.len();
    let extra = detail.reverify_size.saturating_sub(changed);

    if detail.degraded {
        let head = detail.head_oid.clone();
        let idx = detail.index_base_oid.clone();
        let summary = format!(
            "reverify DEGRADED: symbol graph reflects {idx}, not head {head}; \
             emitting {changed} authoritative changed unit(s) only (closure withheld)"
        );
        ToolOutcome::blocked(
            TOOL_IMPACT,
            reason::STALE_INDEX,
            format!(
                "re-index the worktree at {head} (checkout/build the symbol graph there), \
                 then re-run `shuttle impact --base {} --head {head}`",
                detail.base_oid
            ),
            TypedDetail::Reverify(detail),
        )
        .with_summary(summary)
    } else {
        let summary = format!(
            "reverify set: {} unit(s) ({changed} changed + {extra} reverse-dep) — hand off to the caller to re-verify (shuttle does not land)",
            changed + extra
        );
        ToolOutcome::green(TOOL_IMPACT, TypedDetail::Reverify(detail)).with_summary(summary)
    }
}

// ---------------------------------------------------------------------------
// Git plumbing (mirrors shuttle-conflict's helper; scoped to the target root).
// ---------------------------------------------------------------------------

/// Run `git <args>` in `root`, returning trimmed stdout. On non-zero exit the
/// trimmed stderr is returned as the `Err` string (so the caller can lower it
/// into an `Error` outcome rather than panicking).
fn git(root: &str, args: &[&str]) -> Result<String, String> {
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)
        .map_err(|error| error.to_string())?;
    let out = Command::new(binary)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve a revspec to its full 40-hex commit oid.
fn rev_parse(root: &str, rev: &str) -> Result<String, String> {
    git(root, &["rev-parse", rev])
}

/// The set of files that differ between two commits (`git diff --name-only`).
fn changed_between(root: &str, base: &str, head: &str) -> Result<Vec<String>, String> {
    let out = git(root, &["diff", "--name-only", base, head])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------------------
// I/O wrapper (the shape the CLI speaks).
// ---------------------------------------------------------------------------

/// Compute the land-time re-verify set for `base..head` over `opts.root`.
///
/// Resolves both revs (and the worktree `HEAD`) to full oids, diffs them for the
/// authoritative changed set, and — **only if the worktree HEAD equals
/// `head`** — builds the directional symbol graph and unions in the
/// reverse-dependency closure. A stale worktree (HEAD ≠ head) degrades to the
/// changed-only set and blocks with `stale-index` (C11).
///
/// A bad revspec or a non-repo `root` becomes an `Error` outcome
/// (`invalid-input` / `io`) rather than an `Err`, so the CLI always has a
/// machine envelope to print. A genuine scan failure propagates as [`NavError`].
pub fn reverify_impact(
    base: &str,
    head: &str,
    max_depth: usize,
    opts: &crate::QueryOptions,
) -> Result<ToolOutcome, crate::NavError> {
    let root = opts.root.as_str();

    let base_oid = match rev_parse(root, base) {
        Ok(o) => o,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_IMPACT,
                reason::INVALID_INPUT,
                format!("cannot resolve base '{base}': {e}"),
            ));
        }
    };
    let head_oid = match rev_parse(root, head) {
        Ok(o) => o,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_IMPACT,
                reason::INVALID_INPUT,
                format!("cannot resolve head '{head}': {e}"),
            ));
        }
    };
    let index_base_oid = match rev_parse(root, "HEAD") {
        Ok(o) => o,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_IMPACT,
                reason::IO,
                format!("cannot read worktree HEAD in '{root}': {e}"),
            ));
        }
    };
    let changed = match changed_between(root, &base_oid, &head_oid) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_IMPACT,
                reason::IO,
                format!("git diff {base_oid}..{head_oid} failed: {e}"),
            ));
        }
    };

    // No diff → nothing to re-verify. Short-circuit before any scan: an empty
    // changed set has no seeds, so the reverse closure is empty regardless of
    // graph freshness, and nothing is being withheld — this is an honest Green,
    // not a degrade. Skipping the scan here keeps the empty-land case O(git-diff).
    if changed.is_empty() {
        let detail = ReverifyDetail {
            base_oid,
            head_oid,
            index_base_oid,
            changed_files: Vec::new(),
            reverify: Vec::new(),
            granularity: GRANULARITY_FILE.to_string(),
            degraded: false,
            reverify_size: 0,
        };
        return Ok(reverify_outcome(detail));
    }

    // Staleness watermark (C11): trust the closure only when the on-disk graph
    // actually reflects the requested head.
    let detail = if index_base_oid == head_oid {
        let table = crate::full_table(opts)?;
        let graph = SymbolGraph::try_from_scan_root(&table, root)?;
        compute_reverify(
            &changed,
            &base_oid,
            &head_oid,
            &index_base_oid,
            max_depth,
            Some((&graph, &table)),
        )
    } else {
        compute_reverify(
            &changed,
            &base_oid,
            &head_oid,
            &index_base_oid,
            max_depth,
            None,
        )
    };

    Ok(reverify_outcome(detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn ctags_present() -> bool {
        SymbolTable::scan(".").is_ok()
    }

    /// True only when we can actually *create* a git repo in the scratch area.
    /// A bare `git --version` is not enough: some sandboxes permit the binary
    /// but block writes to `.git/config`, in which case the isolated
    /// git-init e2e tests below must skip (they still run on any normal dev
    /// machine and in CI, which is where they earn their keep). The real-repo
    /// read-only e2e tests do not depend on this and always run.
    fn can_init_git() -> bool {
        let probe = scratch("initprobe");
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&probe)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            && probe.join(".git").join("config").exists();
        std::fs::remove_dir_all(&probe).ok();
        ok
    }

    /// The shuttle repo root (this crate is `<root>/crates/shuttle-nav`).
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/shuttle-nav has a repo root two levels up")
            .to_path_buf()
    }

    /// True when the repo root is a git worktree with at least a couple of
    /// commits of history (needed to diff HEAD~1..HEAD~2).
    fn real_repo_has_history() -> bool {
        let root = repo_root();
        Command::new("git")
            .args(["rev-parse", "HEAD~2"])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_reverify_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path) {
        // Empty template so a user's global `~/.gittemplates` hooks are not
        // copied into the scratch repo (that copy can fail under a sandboxed
        // TMPDIR and is irrelevant to the test).
        run_git(dir, &["init", "-q", "--template="]);
        run_git(dir, &["config", "user.email", "t@t"]);
        run_git(dir, &["config", "user.name", "t"]);
        run_git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn commit_all(dir: &Path, msg: &str) -> String {
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", msg]);
        run_git(dir, &["rev-parse", "HEAD"])
    }

    /// core.py defines core_fn; wrapper.py imports core and calls it, so the
    /// directional graph proves wrapper depends on core.
    fn seed_wrapper_core(dir: &Path) {
        write(
            dir,
            "core.py",
            "def core_fn():\n    return 1\n\nclass Core:\n    pass\n",
        );
        write(
            dir,
            "wrapper.py",
            "import core\n\ndef wrap():\n    return core.core_fn()\n",
        );
    }

    fn opts_for(dir: &Path) -> crate::QueryOptions {
        let mut o = crate::QueryOptions::new(dir.to_string_lossy().to_string());
        o.cache = false;
        o
    }

    // ---- pure computation ------------------------------------------------

    #[test]
    fn changed_units_are_always_high_distance_zero() {
        let d = compute_reverify(
            &["core.py".to_string(), "wrapper.py".to_string()],
            "base",
            "head",
            "head",
            64,
            None, // no graph → degraded, changed-only
        );
        assert!(d.degraded);
        assert_eq!(d.reverify_size, 2);
        for u in &d.reverify {
            assert_eq!(u.reason, "changed");
            assert_eq!(u.distance, 0);
            assert_eq!(u.confidence, OutcomeConfidence::High);
        }
        assert_eq!(d.granularity, GRANULARITY_FILE);
    }

    #[test]
    fn closure_pulls_in_reverse_dependents() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("closure");
        seed_wrapper_core(&dir);
        let table = SymbolTable::scan(&dir.to_string_lossy()).unwrap();
        let graph = SymbolGraph::from_scan(&table);
        // Change only core.py → wrapper.py must fall out of the reverse closure.
        let d = compute_reverify(
            &["core.py".to_string()],
            "b",
            "h",
            "h",
            64,
            Some((&graph, &table)),
        );
        assert!(!d.degraded);
        let changed: Vec<&str> = d
            .reverify
            .iter()
            .filter(|u| u.reason == "changed")
            .map(|u| u.path.as_str())
            .collect();
        assert_eq!(changed, vec!["core.py"]);
        let rev: Vec<&str> = d
            .reverify
            .iter()
            .filter(|u| u.reason == "reverse-dep")
            .map(|u| u.path.as_str())
            .collect();
        assert!(
            rev.iter().any(|p| p.ends_with("wrapper.py")),
            "wrapper.py should be a reverse-dep, got {rev:?} (detail {d:?})"
        );
        // A reverse-dep is never distance 0.
        for u in d.reverify.iter().filter(|u| u.reason == "reverse-dep") {
            assert!(u.distance >= 1);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changed_file_is_never_also_a_reverse_dep() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("dedup");
        seed_wrapper_core(&dir);
        let table = SymbolTable::scan(&dir.to_string_lossy()).unwrap();
        let graph = SymbolGraph::from_scan(&table);
        // Change BOTH files: each must appear exactly once, all as "changed".
        let d = compute_reverify(
            &["core.py".to_string(), "wrapper.py".to_string()],
            "b",
            "h",
            "h",
            64,
            Some((&graph, &table)),
        );
        let mut paths: Vec<&str> = d.reverify.iter().map(|u| u.path.as_str()).collect();
        paths.sort();
        let before = paths.len();
        paths.dedup();
        assert_eq!(before, paths.len(), "no duplicate units: {d:?}");
        assert!(d.reverify.iter().all(|u| u.reason == "changed"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- outcome lowering ------------------------------------------------

    #[test]
    fn degraded_detail_blocks_with_stale_index() {
        let d = compute_reverify(&["x.py".to_string()], "b", "h", "other", 64, None);
        let o = reverify_outcome(d);
        assert_eq!(o.state, shuttle_protocol::OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::STALE_INDEX);
        assert!(o.next_action.is_some());
    }

    #[test]
    fn fresh_detail_is_green() {
        let d = compute_reverify(&["x.py".to_string()], "b", "h", "h", 64, None);
        // index_base == head → not stale; but we passed None graph so closure is
        // empty. degraded still follows graph_table.is_none(), so this asserts the
        // *outcome* mapping on a non-degraded detail built directly.
        let fresh = ReverifyDetail {
            degraded: false,
            ..d
        };
        let o = reverify_outcome(fresh);
        assert_eq!(o.state, shuttle_protocol::OutcomeState::Green);
    }

    // ---- end-to-end over a real git repo ---------------------------------

    #[test]
    fn e2e_fresh_worktree_greens_with_closure() {
        if !ctags_present() || !can_init_git() {
            return;
        }
        let dir = scratch("e2e_fresh");
        init_repo(&dir);
        seed_wrapper_core(&dir);
        let base = commit_all(&dir, "base");
        // Edit core.py and commit → head reflects the worktree (fresh).
        write(
            &dir,
            "core.py",
            "def core_fn():\n    return 2\n\nclass Core:\n    pass\n",
        );
        let head = commit_all(&dir, "touch core");
        let o = reverify_impact(&base, &head, 64, &opts_for(&dir)).unwrap();
        assert_eq!(
            o.state,
            shuttle_protocol::OutcomeState::Green,
            "outcome {o:?}"
        );
        match o.detail {
            TypedDetail::Reverify(d) => {
                assert!(!d.degraded);
                assert_eq!(d.index_base_oid, head);
                assert!(d
                    .reverify
                    .iter()
                    .any(|u| u.path.ends_with("core.py") && u.reason == "changed"));
                assert!(
                    d.reverify
                        .iter()
                        .any(|u| u.path.ends_with("wrapper.py") && u.reason == "reverse-dep"),
                    "wrapper.py should be reverse-dep: {d:?}"
                );
            }
            other => panic!("expected reverify detail, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn e2e_stale_worktree_blocks_stale_index() {
        if !ctags_present() || !can_init_git() {
            return;
        }
        let dir = scratch("e2e_stale");
        init_repo(&dir);
        seed_wrapper_core(&dir);
        let base = commit_all(&dir, "base");
        write(
            &dir,
            "core.py",
            "def core_fn():\n    return 2\n\nclass Core:\n    pass\n",
        );
        let head = commit_all(&dir, "head commit");
        // Make a THIRD commit so the worktree HEAD no longer equals `head`.
        write(&dir, "core.py", "def core_fn():\n    return 3\n");
        let _newer = commit_all(&dir, "newer");
        // Ask for base..head while the worktree sits at `newer` → stale.
        let o = reverify_impact(&base, &head, 64, &opts_for(&dir)).unwrap();
        assert_eq!(
            o.state,
            shuttle_protocol::OutcomeState::Blocked,
            "outcome {o:?}"
        );
        assert_eq!(o.reason_code, reason::STALE_INDEX);
        match o.detail {
            TypedDetail::Reverify(d) => {
                assert!(d.degraded);
                assert_ne!(d.index_base_oid, d.head_oid);
                // The authoritative changed set is still emitted (git diff never
                // lies), all as "changed" — the closure is withheld.
                assert!(!d.reverify.is_empty());
                assert!(d.reverify.iter().all(|u| u.reason == "changed"));
            }
            other => panic!("expected reverify detail, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn e2e_bad_base_is_invalid_input() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("e2e_badbase");
        init_repo(&dir);
        seed_wrapper_core(&dir);
        let head = commit_all(&dir, "only");
        let o = reverify_impact("deadbeefdeadbeef", &head, 64, &opts_for(&dir)).unwrap();
        assert!(o.state.is_error());
        assert_eq!(o.reason_code, reason::INVALID_INPUT);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- real-repo read-only e2e (runs here; no scratch repo needed) ----
    //
    // These drive the full git plumbing (rev-parse + diff) and outcome lowering
    // against the actual shuttle worktree. They never write to git, so
    // they are unaffected by sandboxes that forbid creating a `.git/config`.

    #[test]
    fn real_repo_stale_head_blocks_stale_index() {
        if !real_repo_has_history() {
            return;
        }
        let root = repo_root();
        let mut opts = crate::QueryOptions::new(root.to_string_lossy().to_string());
        opts.cache = false;
        // Ask for a historical diff (HEAD~2..HEAD~1) while the worktree sits at
        // HEAD → the graph reflects HEAD, not HEAD~1, so this MUST degrade and
        // block with stale-index (C11). No scan happens on the degraded path,
        // so this is fast even on a big repo.
        let o = reverify_impact("HEAD~2", "HEAD~1", 64, &opts).unwrap();
        assert_eq!(
            o.state,
            shuttle_protocol::OutcomeState::Blocked,
            "outcome {o:?}"
        );
        assert_eq!(o.reason_code, reason::STALE_INDEX);
        assert!(o.next_action.is_some());
        match o.detail {
            TypedDetail::Reverify(d) => {
                assert!(d.degraded);
                assert_ne!(d.index_base_oid, d.head_oid);
                // git diff is authoritative even when the graph is stale: the
                // changed set is non-empty and every unit is "changed".
                assert!(!d.reverify.is_empty());
                assert!(d.reverify.iter().all(|u| u.reason == "changed"));
                assert!(d.reverify.iter().all(|u| u.distance == 0));
                // Full 40-hex oids resolved from the revspecs.
                assert_eq!(d.base_oid.len(), 40);
                assert_eq!(d.head_oid.len(), 40);
            }
            other => panic!("expected reverify detail, got {other:?}"),
        }
    }

    #[test]
    fn real_repo_empty_diff_is_green_empty() {
        if !real_repo_has_history() {
            return;
        }
        let root = repo_root();
        let mut opts = crate::QueryOptions::new(root.to_string_lossy().to_string());
        opts.cache = false;
        // base == head == HEAD → empty diff. index_base == head (both HEAD) so
        // the graph is *fresh*, but with zero changed files there are no seeds,
        // so no closure walk and no scan — Green with an empty set.
        let o = reverify_impact("HEAD", "HEAD", 64, &opts).unwrap();
        assert_eq!(
            o.state,
            shuttle_protocol::OutcomeState::Green,
            "outcome {o:?}"
        );
        match o.detail {
            TypedDetail::Reverify(d) => {
                assert!(!d.degraded);
                assert_eq!(d.index_base_oid, d.head_oid);
                assert_eq!(d.reverify_size, 0);
                assert!(d.changed_files.is_empty());
            }
            other => panic!("expected reverify detail, got {other:?}"),
        }
    }

    #[test]
    fn real_repo_bad_revspec_is_invalid_input() {
        if !real_repo_has_history() {
            return;
        }
        let root = repo_root();
        let mut opts = crate::QueryOptions::new(root.to_string_lossy().to_string());
        opts.cache = false;
        let o = reverify_impact("deadbeefdeadbeefdeadbeef", "HEAD", 64, &opts).unwrap();
        assert!(o.state.is_error());
        assert_eq!(o.reason_code, reason::INVALID_INPUT);
    }

    // ---- isolated git-init e2e (portable; skips where .git/config is unwritable) ----

    #[test]
    fn e2e_no_diff_is_green_empty() {
        if !ctags_present() || !can_init_git() {
            return;
        }
        let dir = scratch("e2e_nodiff");
        init_repo(&dir);
        seed_wrapper_core(&dir);
        let head = commit_all(&dir, "only");
        // base == head → empty diff → empty (but Green) re-verify set.
        let o = reverify_impact(&head, &head, 64, &opts_for(&dir)).unwrap();
        assert_eq!(
            o.state,
            shuttle_protocol::OutcomeState::Green,
            "outcome {o:?}"
        );
        match o.detail {
            TypedDetail::Reverify(d) => {
                assert_eq!(d.reverify_size, 0);
                assert!(!d.degraded);
            }
            other => panic!("expected reverify detail, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
