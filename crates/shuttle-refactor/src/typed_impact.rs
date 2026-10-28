//! G6 · S2 — type-aware impact (`shuttle impact --typed`).
//!
//! The mechanical reverse-closure ([`shuttle_nav::impact_files`]) answers "who
//! references a symbol defined in the changed files?". But when what changed is
//! a **function signature / type**, the callers that actually break are the
//! ones that *type-depend* on it — not every importer. This module layers a
//! type-dependency proof on top of the **directed** closure and keeps only the
//! callers whose cross-file type closure ([`crate::type_closure`]) provably
//! references one of the changed symbols. The result is strictly **narrower**
//! than `shuttle impact --directed` .
//!
//! # §7 「绝不瞎猜」 — never pretend to understand types
//! We drop a directed caller **only** when we can positively prove it carries
//! *no* type dependency (its type closure fully resolved and contains no changed
//! seed). Every case where the proof is unavailable degrades to *keeping* the
//! directed edge, tagged [`Confidence::Medium`], and is surfaced in
//! [`TypedImpact::notes`] — never silently dropped:
//!
//! | caller situation | outcome |
//! |---|---|
//! | strong-typed lang, closure resolved, references a changed seed | **keep** (proven type dep) |
//! | strong-typed lang, closure resolved, no changed seed, no unresolved refs | **drop** (proven no type dep — the narrowing) |
//! | strong-typed lang, closure has unresolved type names | keep · Medium (can't prove absence) |
//! | weakly-typed lang (Python / JavaScript) | keep · Medium (types unreliable — §2 pit #6) |
//! | unsupported lang / closure resolution errored | keep · Medium (can't prove) |
//!
//! The changed *seeds* themselves (distance 0) are always kept as-is: they are
//! the definitions the caller asked about, not an inferred dependency.

use std::collections::BTreeSet;

use shuttle_nav::{Confidence, NavError, NavHit, NavKind, QueryOptions};
use shuttle_symbols::SymbolTable;

use crate::locals::Lang;
use crate::type_closure::{resolve_type_closure, ClosureOptions};

/// A directed caller that was proven to carry *no* type dependency on any
/// changed symbol, so `--typed` narrowed it out. Surfaced (not discarded) so a
/// caller can see exactly what the type proof removed relative to `--directed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DroppedCaller {
    pub symbol: String,
    pub path: String,
    /// Always `"no-type-dependency"` — the only reason we drop (§7): every
    /// unprovable case is kept, not dropped.
    pub reason: String,
}

/// The result of a type-aware impact query: the narrowed hit set plus every §7
/// degrade and every provable drop, so the answer is never mistaken for a
/// clean type proof when it isn't one.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TypedImpact {
    /// The narrowed impact: seeds + callers that provably type-depend on a
    /// changed symbol, plus every caller kept under a §7 degrade.
    pub hits: Vec<NavHit>,
    /// Directed callers proven to carry no type dependency (the narrowing).
    pub dropped: Vec<DroppedCaller>,
    /// True when any caller was kept under a degrade (a type proof was
    /// unavailable), so the caller must not read the set as a clean proof.
    pub degraded: bool,
    /// One human-readable note per degrade.
    pub notes: Vec<String>,
}

/// Whether a language carries type information reliable enough to *prove* a
/// type dependency. Python / JavaScript annotations are optional and sparse, so
/// per §7 we never infer type dependency from them — we degrade instead.
fn is_strongly_typed(lang: Lang) -> bool {
    matches!(lang, Lang::Go | Lang::Rust | Lang::TypeScript)
}

/// `shuttle impact --typed`: the directed reverse-closure of `changed`, narrowed to
/// the callers that provably type-depend on a changed symbol.
///
/// `changed` are the changed files (same seeding rule as [`shuttle_nav::impact_files`]);
/// `max_depth`/`tests_only` are forwarded to the directed closure. The type
/// proof reuses the *same* cached [`SymbolTable`] the closure walked, so there
/// is no second scan and no divergent repo view.
pub fn typed_impact(
    changed: &[String],
    max_depth: usize,
    tests_only: bool,
    opts: &QueryOptions,
) -> Result<TypedImpact, NavError> {
    // Base: the DIRECTED closure (trusted import/contract edges only). `--typed`
    // is defined as a refinement of `--directed`, so we always start there —
    // never from the broad symmetric name-match closure.
    let base = shuttle_nav::impact_files(
        changed, max_depth, tests_only, /*directed=*/ true, opts,
    )?;

    // One scan, shared with the proof. `symbols_defined_in` recovers the exact
    // seed set the closure derived, so the changed-symbol set the type proof
    // matches against is identical to the one that seeded the walk.
    let table: SymbolTable = shuttle_nav::scan_table(opts)?;
    let changed_seeds: BTreeSet<String> = shuttle_nav::symbols_defined_in(&table, changed)
        .into_iter()
        .map(|s| s.name.clone())
        .collect();

    let copts = ClosureOptions::default();
    let mut out = TypedImpact::default();

    for hit in base {
        // Seeds (the changed defs) are always kept verbatim — they are the
        // subject, not an inferred dependency.
        if hit.kind == NavKind::Definition {
            out.hits.push(hit);
            continue;
        }

        match classify_caller(&hit, &table, &changed_seeds, &copts) {
            CallerVerdict::KeepProven => out.hits.push(hit),
            CallerVerdict::KeepDegraded(reason) => {
                out.degraded = true;
                out.notes
                    .push(format!("{} @ {}: {reason}", hit.symbol, hit.path));
                out.hits.push(demote(hit));
            }
            CallerVerdict::Drop => out.dropped.push(DroppedCaller {
                symbol: hit.symbol,
                path: hit.path,
                reason: "no-type-dependency".to_string(),
            }),
        }
    }

    Ok(out)
}

enum CallerVerdict {
    /// Provably type-depends on a changed symbol — keep at its own confidence.
    KeepProven,
    /// A type proof was unavailable — keep, but tag Medium and note why.
    KeepDegraded(&'static str),
    /// Provably carries no type dependency — narrow it out.
    Drop,
}

fn classify_caller(
    hit: &NavHit,
    table: &SymbolTable,
    changed_seeds: &BTreeSet<String>,
    copts: &ClosureOptions,
) -> CallerVerdict {
    let Some(lang) = Lang::from_path(&hit.path) else {
        // Unsupported language: we cannot read a type closure at all (§7).
        return CallerVerdict::KeepDegraded("unsupported language — type proof unavailable");
    };
    if !is_strongly_typed(lang) {
        // Python / JavaScript: optional, sparse annotations — never infer.
        return CallerVerdict::KeepDegraded("weakly-typed language — types not inferable");
    }
    match resolve_type_closure(&hit.symbol, &hit.path, table, copts) {
        Err(_) => {
            // Could not anchor the caller fn / read its file — can't prove.
            CallerVerdict::KeepDegraded("type closure unresolved — proof unavailable")
        }
        Ok(graph) => {
            let carries_type_dep = graph
                .nodes
                .iter()
                .any(|n| changed_seeds.contains(&n.symbol));
            if carries_type_dep {
                CallerVerdict::KeepProven
            } else if !graph.unresolved.is_empty() {
                // Some type names could not be bound — we cannot prove ABSENCE
                // of a dependency, so we keep (never guess a clean drop, §7).
                CallerVerdict::KeepDegraded("unbound type refs — absence not provable")
            } else {
                // Fully resolved and no changed seed appears: proven no type dep.
                CallerVerdict::Drop
            }
        }
    }
}

/// Tag a degraded hit Medium, but never *raise* an already-weaker (Low) rating:
/// a degrade is a ceiling, not a floor.
fn demote(mut hit: NavHit) -> NavHit {
    hit.confidence = match hit.confidence {
        Confidence::High => Confidence::Medium,
        Confidence::Medium => Confidence::Medium,
        Confidence::Low => Confidence::Low,
    };
    hit
}
