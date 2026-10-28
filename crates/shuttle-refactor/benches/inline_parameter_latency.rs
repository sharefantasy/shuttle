//! End-to-end latency benchmark for `inline_parameter` (docs §9: "端到端延迟进
//! benches/", aligned with the introduce-parameter / change-signature benches
//! beside it — inline-parameter is the honest inverse of introduce-parameter).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the same-file callee,
//! locate + trailing-check the target parameter, collect every call site and
//! extract its trailing argument, run the §7 gate (all trivial, all agree, arity
//! matches), substitute the constant into the body, drop the parameter from the
//! signature, and drop the trailing argument at each call into SearchReplace
//! ops. `root` is empty so no cross-file `shuttle callers` scan is included — this
//! isolates the in-file hot path. One representative inline per supported
//! language so a regression in any grammar's parameter/body/call handling shows
//! separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::inline_parameter;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    func: &'static str,
    param: &'static str,
}

fn cases() -> Vec<Case> {
    // Each inlines the trailing parameter `factor`, passed as `10` at both calls.
    let py = "def area(w, factor):\n    return w * factor\n\ndef main():\n    x = area(3, 10)\n    y = area(5, 10)\n".to_string();
    let js = "function area(w, factor) {\n  return w * factor;\n}\nfunction main() {\n  const x = area(3, 10);\n  const y = area(5, 10);\n}\n".to_string();
    let go = "package p\n\nfunc area(w int, factor int) int {\n\treturn w * factor\n}\nfunc main() {\n\t_ = area(3, 10)\n\t_ = area(5, 10)\n}\n".to_string();
    let rs = "fn area(w: i32, factor: i32) -> i32 {\n    w * factor\n}\nfn main() {\n    let _ = area(3, 10);\n    let _ = area(5, 10);\n}\n".to_string();
    let ts = "function area(w: number, factor: number): number {\n  return w * factor;\n}\nfunction main() {\n  const x = area(3, 10);\n  const y = area(5, 10);\n}\n".to_string();
    vec![
        Case {
            name: "python_inline",
            path: "m.py",
            source: py,
            func: "area",
            param: "factor",
        },
        Case {
            name: "javascript_inline",
            path: "m.js",
            source: js,
            func: "area",
            param: "factor",
        },
        Case {
            name: "go_inline",
            path: "m.go",
            source: go,
            func: "area",
            param: "factor",
        },
        Case {
            name: "rust_inline",
            path: "m.rs",
            source: rs,
            func: "area",
            param: "factor",
        },
        Case {
            name: "typescript_inline",
            path: "m.ts",
            source: ts,
            func: "area",
            param: "factor",
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
    inline_parameter(case.path, &case.source, case.func, case.param, "")
        .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = inline_parameter(case.path, &case.source, case.func, case.param, "");
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = inline_parameter(case.path, &case.source, case.func, case.param, "");
        let ns = t.elapsed().as_nanos();
        std::hint::black_box(&plan);
        samples.push(ns);
    }

    samples.sort_unstable();
    let sum: u128 = samples.iter().sum();
    let mean = sum / samples.len() as u128;
    println!(
        "{:<22} n={:<6} mean={:>8}ns  p50={:>8}ns  p90={:>8}ns  p99={:>8}ns  max={:>8}ns",
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
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(5_000);

    println!(
        "shuttle-refactor inline_parameter end-to-end latency (parse + locals + resolve + gate + rewrite)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
