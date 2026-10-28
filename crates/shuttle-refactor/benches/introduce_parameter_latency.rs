//! End-to-end latency benchmark for `introduce_parameter` (docs §9: "端到端延迟
//! 进 benches/", aligned with the change-signature / extract-method benches
//! beside it).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the same-file callee
//! definition, find the body occurrences of the promoted expression, run the §7
//! legality gate (no function-internal name), append the parameter to the
//! signature, replace the body occurrences, and append the argument at every
//! same-file call site into SearchReplace ops. `root` is empty so no cross-file
//! `shuttle callers` scan is included — this isolates the in-file hot path. One
//! representative constant promotion per supported language so a regression in
//! any grammar's parameter/body/call handling shows separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::introduce_parameter;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    func: &'static str,
    expr: &'static str,
    param: &'static str,
}

fn cases() -> Vec<Case> {
    // Each promotes the literal `10` in a same-file function with one call site.
    let py = "def area(w):\n    return w * 10\n\ndef main():\n    x = area(3)\n".to_string();
    let js =
        "function area(w) {\n  return w * 10;\n}\nfunction main() {\n  const x = area(3);\n}\n"
            .to_string();
    let go = "package p\n\nfunc area(w int) int {\n\treturn w * 10\n}\nfunc main() {\n\t_ = area(3)\n}\n".to_string();
    let rs = "fn area(w: i32) -> i32 {\n    w * 10\n}\nfn main() {\n    let _ = area(3);\n}\n"
        .to_string();
    let ts = "function area(w: number): number {\n  return w * 10;\n}\nfunction main() {\n  const x = area(3);\n}\n".to_string();
    vec![
        Case {
            name: "python_promote",
            path: "m.py",
            source: py,
            func: "area",
            expr: "10",
            param: "factor",
        },
        Case {
            name: "javascript_promote",
            path: "m.js",
            source: js,
            func: "area",
            expr: "10",
            param: "factor",
        },
        Case {
            name: "go_promote",
            path: "m.go",
            source: go,
            func: "area",
            expr: "10",
            param: "factor",
        },
        Case {
            name: "rust_promote",
            path: "m.rs",
            source: rs,
            func: "area",
            expr: "10",
            param: "factor",
        },
        Case {
            name: "typescript_promote",
            path: "m.ts",
            source: ts,
            func: "area",
            expr: "10",
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
    introduce_parameter(
        case.path,
        &case.source,
        case.func,
        case.expr,
        case.param,
        "",
    )
    .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = introduce_parameter(
            case.path,
            &case.source,
            case.func,
            case.expr,
            case.param,
            "",
        );
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = introduce_parameter(
            case.path,
            &case.source,
            case.func,
            case.expr,
            case.param,
            "",
        );
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
        "shuttle-refactor introduce_parameter end-to-end latency (parse + locals + resolve + gate + rewrite)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
