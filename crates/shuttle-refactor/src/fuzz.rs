//! §9 D3 — property-based / fuzz safety net for the refactor engine.
//!
//! No external proptest/quickcheck dependency (the daemonless build stays
//! dependency-light): a tiny deterministic xorshift PRNG drives a generator of
//! *syntactically valid* Python and Rust programs, then throws every
//! source-driven refactor at random-but-plausible arguments.
//!
//! The invariant under test is the one the whole engine promises (§7, "绝不瞎
//! 猜" — never guess): **a refactor must never silently produce broken code.**
//! Concretely, for every plan a refactor returns `Ok` for, applying its ops to
//! the original source must not *increase* the tree-sitter error-node count …
//! unless the plan honestly downgraded itself to `Low` confidence (the
//! edit-then-reparse safety net from task A). A refactor is free to *reject*
//! (`Err`) or to *warn* — what it may not do is hand back a High/Medium plan
//! that corrupts the syntax tree.
//!
//! Runs thousands of cases in milliseconds and is fully reproducible: every
//! failing case prints its seed + generated source so it can be replayed.

#![cfg(test)]

use crate::locals::Lang;
use crate::verify::error_count;
use crate::{
    change_signature, compose_ops, inline_parameter, introduce_parameter, remove_parameter,
    NewParam, RefactorPlan, Vfs,
};
use shuttle_nav::Confidence;

/// Deterministic xorshift64* PRNG — reproducible, zero-dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[0, n)` (n > 0).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Which language a generated case is in — decides syntax + which asserts run.
#[derive(Clone, Copy)]
enum Target {
    Python,
    Rust,
}

impl Target {
    fn path(self) -> &'static str {
        match self {
            Target::Python => "fuzz.py",
            Target::Rust => "fuzz.rs",
        }
    }
    fn lang(self) -> Lang {
        match self {
            Target::Python => Lang::Python,
            Target::Rust => Lang::Rust,
        }
    }
}

const NAMES: &[&str] = &["a", "b", "c", "d", "e", "x", "y", "z", "n", "m"];
const FNS: &[&str] = &["f", "g", "h", "compute", "run", "step"];

/// A generated case: the function name, its params, and the source text.
struct Case {
    target: Target,
    func: String,
    params: Vec<String>,
    source: String,
}

/// Generate one syntactically valid program with a single function `func`
/// taking `params` value parameters plus a couple of call sites, in `target`.
fn generate(rng: &mut Rng, target: Target) -> Case {
    let func = (*rng.pick(FNS)).to_string();
    let arity = 1 + rng.below(4); // 1..=4 params
                                  // Distinct param names.
    let mut params: Vec<String> = Vec::new();
    while params.len() < arity {
        let cand = (*rng.pick(NAMES)).to_string();
        if !params.contains(&cand) {
            params.push(cand);
        }
    }
    // Body references a random NON-EMPTY subset so at least one param is used
    // (keeps the code meaningful; the *last* param is often left unused so
    // remove-parameter has something legal to bite on).
    let used_upto = 1 + rng.below(params.len()); // use params[..used_upto]
    let call_args: Vec<String> = (0..arity).map(|i| (i as i64 + 1).to_string()).collect();

    let source = match target {
        Target::Python => {
            let sig = params.join(", ");
            let body_expr = params[..used_upto].join(" + ");
            let calls = format!(
                "{f}({a})\n{f}({b})\n",
                f = func,
                a = call_args.join(", "),
                b = call_args
                    .iter()
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            format!("def {func}({sig}):\n    return {body_expr}\n\n{calls}")
        }
        Target::Rust => {
            let sig = params
                .iter()
                .map(|p| format!("{p}: i64"))
                .collect::<Vec<_>>()
                .join(", ");
            let body_expr = params[..used_upto].join(" + ");
            let calls = format!(
                "    let _ = {f}({a});\n    let _ = {f}({b});\n",
                f = func,
                a = call_args.join(", "),
                b = call_args
                    .iter()
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            format!("fn {func}({sig}) -> i64 {{\n    {body_expr}\n}}\n\nfn main() {{\n{calls}}}\n")
        }
    };

    Case {
        target,
        func,
        params,
        source,
    }
}

/// Apply a plan's ops to `source` in memory and return the resulting text, or
/// `None` if any op failed to anchor (which is itself acceptable — the engine
/// is allowed to produce a plan that a later edit can't place, as long as it
/// isn't a corrupting one; a miss just means "no change to judge").
fn apply(source: &str, path: &str, plan: &RefactorPlan) -> Option<String> {
    let mut vfs: Vfs = Vfs::new();
    vfs.insert(path.to_string(), source.to_string());
    compose_ops(&mut vfs, &plan.plan.ops).ok()?;
    vfs.remove(path)
}

/// THE invariant: a non-Low plan must never increase the syntax-error count.
fn assert_no_corruption(seed: u64, op: &str, case: &Case, plan: &RefactorPlan) {
    let before = error_count(case.target.lang(), &case.source);
    let Some(after_src) = apply(&case.source, case.target.path(), plan) else {
        return; // anchor miss — nothing landed, nothing to corrupt.
    };
    let after = error_count(case.target.lang(), &after_src);
    if after > before && !matches!(plan.confidence, Confidence::Low) {
        panic!(
            "CORRUPTION: {op} produced a {:?}-confidence plan that raised syntax \
             errors {before} -> {after}\n--- seed {seed} ---\n--- BEFORE ---\n{}\n--- AFTER ---\n{}\n--- warnings: {:?}",
            plan.confidence, case.source, after_src, plan.warnings
        );
    }
}

/// Drive every source-driven refactor against one generated case.
fn exercise(seed: u64, case: &Case) {
    let path = case.target.path();
    let src = &case.source;
    let n = case.params.len();

    // remove-parameter: drop the last (often unused) parameter.
    if let Ok(plan) = remove_parameter(path, src, &case.func, case.params.last().unwrap(), "") {
        assert_no_corruption(seed, "remove-parameter", case, &plan);
    }

    // change-signature: reverse the parameter order (pure reorder, no adds).
    let reorder: Vec<NewParam> = (0..n).rev().map(NewParam::keep).collect();
    if let Ok(plan) = change_signature(path, src, &case.func, &reorder, "") {
        assert_no_corruption(seed, "change-signature", case, &plan);
    }

    // introduce-parameter: promote a literal that appears in the body.
    if let Ok(plan) = introduce_parameter(path, src, &case.func, "1", "promoted", "") {
        assert_no_corruption(seed, "introduce-parameter", case, &plan);
    }

    // inline-parameter: inline the first parameter back to a constant.
    if let Ok(plan) = inline_parameter(path, src, &case.func, &case.params[0], "") {
        assert_no_corruption(seed, "inline-parameter", case, &plan);
    }
}

/// Run `iters` cases starting from `base_seed` against one target language.
fn run_campaign(base_seed: u64, iters: u64, target: Target) {
    for i in 0..iters {
        let seed = base_seed.wrapping_add(i).wrapping_mul(0x100_0001);
        let mut rng = Rng::new(seed);
        let case = generate(&mut rng, target);
        // Sanity: the generator must emit valid code (0 errors) — otherwise the
        // "no increase" invariant would be measuring against garbage.
        assert_eq!(
            error_count(target.lang(), &case.source),
            0,
            "generator emitted invalid source (seed {seed}):\n{}",
            case.source
        );
        exercise(seed, &case);
    }
}

#[test]
fn fuzz_python_refactors_never_corrupt() {
    run_campaign(0xC0FF_EE00, 2000, Target::Python);
}

#[test]
fn fuzz_rust_refactors_never_corrupt() {
    run_campaign(0xBEEF_1234, 2000, Target::Rust);
}

#[test]
fn fuzz_is_reproducible() {
    // The PRNG is deterministic: the same seed yields byte-identical source.
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    let ca = generate(&mut a, Target::Python);
    let cb = generate(&mut b, Target::Python);
    assert_eq!(ca.source, cb.source);
}
