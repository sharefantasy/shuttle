//! End-to-end latency benchmark for `extract_method` (docs §9: "端到端延迟进
//! benches/", aligned with architecture.md §10 M6 and capability_plan §9.12).
//!
//! Unlike `shuttle-editcore/apply_latency` (which measures only the write-out hot
//! path), this measures the *whole* refactor decision the caller pays for on a
//! single invocation: parse the source with tree-sitter, build the locals index
//! (scope/def/ref), classify def_before/def_in/ref_in/ref_after, derive
//! params/returns, run the §3.4 legality gates, and lower to an `EditPlan`. That
//! full pipeline runs per user request with no warm engine (the daemonless
//! boundary), so its tail latency IS the product metric.
//!
//! We bench one representative extraction per supported language so a regression
//! in any grammar's locals resolution or template rendering shows up separately.
//!
//! std-only (no criterion), `harness = false`: keeps the bench dependency-free
//! and instant to compile, matching `shuttle-editcore`'s latency-first convention.

use shuttle_refactor::extract_method;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    start_line: usize,
    end_line: usize,
    new_name: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        // Python: High confidence (no type annotations needed).
        Case {
            name: "python_high",
            path: "m.py",
            source: "def g(a):\n    b = a + 1\n    c = b * 2\n    return c\n".into(),
            start_line: 3,
            end_line: 3,
            new_name: "compute",
        },
        // JavaScript: High confidence, const-declared return.
        Case {
            name: "javascript_high",
            path: "m.js",
            source: "function g(a) {\n  let b = a + 1;\n  let c = b * 2;\n  return c;\n}\n".into(),
            start_line: 3,
            end_line: 3,
            new_name: "compute",
        },
        // Go: Medium (emits TODO_TYPE placeholder + downgrade).
        Case {
            name: "go_medium",
            path: "m.go",
            source: "func g(a int) int {\n\tb := a + 1\n\tc := b * 2\n\treturn c\n}\n".into(),
            start_line: 3,
            end_line: 3,
            new_name: "compute",
        },
        // Rust: Medium, tail-expression return.
        Case {
            name: "rust_medium",
            path: "m.rs",
            source: "fn g(a: i32) -> i32 {\n    let b = a + 1;\n    let c = b * 2;\n    c\n}\n".into(),
            start_line: 3,
            end_line: 3,
            new_name: "compute",
        },
        // TypeScript: Medium, typed params + return annotation placeholder.
        Case {
            name: "typescript_medium",
            path: "m.ts",
            source: "function g(a: number): number {\n  const b = a + 1;\n  const c = b * 2;\n  return c;\n}\n".into(),
            start_line: 3,
            end_line: 3,
            new_name: "compute",
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
    // Correctness gate: a latency number for a rejected plan is meaningless.
    extract_method(
        case.path,
        &case.source,
        case.start_line,
        case.end_line,
        case.new_name,
    )
    .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    // Warm up (populate the allocator path) — excluded from stats.
    for _ in 0..(iters / 10).max(1) {
        let _ = extract_method(
            case.path,
            &case.source,
            case.start_line,
            case.end_line,
            case.new_name,
        );
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = extract_method(
            case.path,
            &case.source,
            case.start_line,
            case.end_line,
            case.new_name,
        );
        let ns = t.elapsed().as_nanos();
        std::hint::black_box(&plan);
        samples.push(ns);
    }

    samples.sort_unstable();
    let sum: u128 = samples.iter().sum();
    let mean = sum / samples.len() as u128;
    println!(
        "{:<20} n={:<6} mean={:>8}ns  p50={:>8}ns  p90={:>8}ns  p99={:>8}ns  max={:>8}ns",
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
    // Allow `cargo bench -- 5000` to override the iteration count.
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(5_000);

    println!(
        "shuttle-refactor extract_method end-to-end latency (parse + locals + derive + gate + lower)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
