//! End-to-end latency benchmark for `remove_parameter` (docs §9: "端到端延迟进
//! benches/", aligned with the inline-parameter bench beside it — remove-parameter
//! is the pure-deletion subset of inline-parameter).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the same-file callee,
//! locate + trailing-check the target parameter, confirm it is never read in the
//! body, collect every call site (arity + single-line gate), drop the parameter
//! from the signature, and drop the trailing argument at each call into
//! SearchReplace ops. `root` is empty so no cross-file `shuttle callers` scan is
//! included — this isolates the in-file hot path. One representative removal per
//! supported language.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::remove_parameter;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    func: &'static str,
    param: &'static str,
}

fn cases() -> Vec<Case> {
    // Each removes the unused trailing parameter `unused`.
    let py = "def area(w, unused):\n    return w * 2\n\ndef main():\n    x = area(3, 10)\n    y = area(5, 99)\n".to_string();
    let js = "function area(w, unused) {\n  return w * 2;\n}\nfunction main() {\n  const x = area(3, 10);\n  const y = area(5, 99);\n}\n".to_string();
    let go = "package p\n\nfunc area(w int, unused int) int {\n\treturn w * 2\n}\nfunc main() {\n\t_ = area(3, 10)\n\t_ = area(5, 99)\n}\n".to_string();
    let rs = "fn area(w: i32, unused: i32) -> i32 {\n    w * 2\n}\nfn main() {\n    let _ = area(3, 10);\n    let _ = area(5, 99);\n}\n".to_string();
    let ts = "function area(w: number, unused: number): number {\n  return w * 2;\n}\nfunction main() {\n  const x = area(3, 10);\n  const y = area(5, 99);\n}\n".to_string();
    vec![
        Case {
            name: "python_remove",
            path: "m.py",
            source: py,
            func: "area",
            param: "unused",
        },
        Case {
            name: "javascript_remove",
            path: "m.js",
            source: js,
            func: "area",
            param: "unused",
        },
        Case {
            name: "go_remove",
            path: "m.go",
            source: go,
            func: "area",
            param: "unused",
        },
        Case {
            name: "rust_remove",
            path: "m.rs",
            source: rs,
            func: "area",
            param: "unused",
        },
        Case {
            name: "typescript_remove",
            path: "m.ts",
            source: ts,
            func: "area",
            param: "unused",
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
    remove_parameter(case.path, &case.source, case.func, case.param, "")
        .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = remove_parameter(case.path, &case.source, case.func, case.param, "");
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = remove_parameter(case.path, &case.source, case.func, case.param, "");
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
        "shuttle-refactor remove_parameter end-to-end latency (parse + locals + resolve + gate + rewrite)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
