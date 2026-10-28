//! Latency benchmark for the executor hot path (`apply_search_replace`).
//!
//! This is the "fast small model's hands" — the code that runs on *every* edit
//! the upstream planner emits. Tail latency here is the product metric, so we
//! measure each fuzzy tier separately and report the p50/p90/p99/max the caller
//! actually feels, not just a mean.
//!
//! std-only (no criterion): keeps the bench dependency-free and instant to
//! compile, so `cargo bench -p shuttle-editcore` stays in the latency-first spirit.
//! Registered with `harness = false`, so this `main` is the entry point.

use shuttle_editcore::apply_search_replace;
use std::time::Instant;

/// One synthetic source body of `lines` lines, with a unique anchor near the end.
fn make_source(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 24);
    for i in 0..lines {
        s.push_str(&format!("    let v{i} = compute({i});\n"));
    }
    // A unique anchor the search will target.
    s.push_str("    return finalize(result);\n");
    s
}

struct Case {
    name: &'static str,
    source: String,
    search: String,
    replace: String,
    /// Tier we expect to hit (asserted once, for correctness of the bench).
    expect_tier: u8,
}

fn cases() -> Vec<Case> {
    let src = make_source(400);
    vec![
        // Tier 0: exact unique substring.
        Case {
            name: "tier0_exact",
            source: src.clone(),
            search: "    return finalize(result);\n".into(),
            replace: "    return finalize(result_v2);\n".into(),
            expect_tier: 0,
        },
        // Tier 1: trailing whitespace differs (source lacks it) -> exact fails, ws-normalized hits.
        Case {
            name: "tier1_ws_normalized",
            source: src.clone(),
            search: "    return finalize(result);   ".into(),
            replace: "    return finalize(result_v2);".into(),
            expect_tier: 1,
        },
        // Tier 2: a 12-line block where exactly one line drifted (v206 remembers
        // an older name) -> ~11/12 line similarity clears the 0.90 window while
        // Tier 0/1 (every line identical) both miss.
        Case {
            name: "tier2_fuzzy",
            source: src,
            search: {
                let mut ls: Vec<String> = (200..212)
                    .map(|i| format!("    let v{i} = compute({i});"))
                    .collect();
                ls[6] = "    let v206 = compute_renamed(206);".to_string();
                ls.join("\n")
            },
            replace: "    // collapsed\n    done();\n".into(),
            expect_tier: 2,
        },
    ]
}

fn percentile(sorted_ns: &[u128], p: f64) -> u128 {
    if sorted_ns.is_empty() {
        return 0;
    }
    let idx = ((sorted_ns.len() as f64 - 1.0) * p).round() as usize;
    sorted_ns[idx]
}

fn bench_case(case: &Case, iters: usize) {
    // Warm up (populate caches, JIT the allocator path) — excluded from stats.
    for _ in 0..(iters / 10).max(1) {
        let _ = apply_search_replace(&case.source, &case.search, &case.replace, None);
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let (out, oc) = apply_search_replace(&case.source, &case.search, &case.replace, None);
        let ns = t.elapsed().as_nanos();
        // Prevent the optimizer from eliding the call.
        std::hint::black_box(&out);
        std::hint::black_box(&oc);
        samples.push(ns);
    }
    // Correctness gate: a latency number for the wrong tier is meaningless.
    let (_, oc) = apply_search_replace(&case.source, &case.search, &case.replace, None);
    assert_eq!(
        oc.match_tier, case.expect_tier,
        "case {} expected tier {}, got {}",
        case.name, case.expect_tier, oc.match_tier
    );

    samples.sort_unstable();
    let sum: u128 = samples.iter().sum();
    let mean = sum / samples.len() as u128;
    println!(
        "{:<20} n={:<6} mean={:>7}ns  p50={:>7}ns  p90={:>7}ns  p99={:>7}ns  max={:>7}ns",
        case.name,
        iters,
        mean,
        percentile(&samples, 0.50),
        percentile(&samples, 0.90),
        percentile(&samples, 0.99),
        samples.last().copied().unwrap_or(0),
    );
}

fn main() {
    // Allow `cargo bench -- 50000` to override the iteration count.
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(20_000);

    println!("shuttle-editcore apply_search_replace latency (source = 400 lines)\n");
    for case in cases() {
        bench_case(&case, iters);
    }
}
