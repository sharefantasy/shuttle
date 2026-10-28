//! End-to-end latency benchmark for `change_signature` (docs §9: "端到端延迟进
//! benches/", aligned with the extract-method / inline-method benches beside it).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the same-file callee
//! definition, read its formal parameters, validate the transform, and rewrite
//! the signature + every same-file call site into SearchReplace ops. `root` is
//! empty so no cross-file `shuttle callers` scan is included — this isolates the
//! in-file hot path. One representative parameter reorder per supported language
//! so a regression in any grammar's parameter/call handling shows separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::{change_signature, NewParam};
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    func: &'static str,
}

fn cases() -> Vec<Case> {
    // Each swaps the two parameters of a same-file function with one call site.
    let py = "def sub(a, b):\n    return a - b\n\ndef main():\n    x = sub(1, 2)\n".to_string();
    let js =
        "function sub(a, b) {\n  return a - b;\n}\nfunction main() {\n  const x = sub(1, 2);\n}\n"
            .to_string();
    let go = "package p\n\nfunc sub(a int, b int) int {\n\treturn a - b\n}\nfunc main() {\n\t_ = sub(1, 2)\n}\n".to_string();
    let rs =
        "fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\nfn main() {\n    let _ = sub(1, 2);\n}\n"
            .to_string();
    let ts = "function sub(a: number, b: number): number {\n  return a - b;\n}\nfunction main() {\n  const x = sub(1, 2);\n}\n".to_string();
    vec![
        Case {
            name: "python_reorder",
            path: "m.py",
            source: py,
            func: "sub",
        },
        Case {
            name: "javascript_reorder",
            path: "m.js",
            source: js,
            func: "sub",
        },
        Case {
            name: "go_reorder",
            path: "m.go",
            source: go,
            func: "sub",
        },
        Case {
            name: "rust_reorder",
            path: "m.rs",
            source: rs,
            func: "sub",
        },
        Case {
            name: "typescript_reorder",
            path: "m.ts",
            source: ts,
            func: "sub",
        },
    ]
}

fn transform() -> Vec<NewParam> {
    vec![NewParam::keep(1), NewParam::keep(0)]
}

fn percentile(sorted_ns: &[u128], p: f64) -> u128 {
    if sorted_ns.is_empty() {
        return 0;
    }
    let idx = ((sorted_ns.len() as f64 - 1.0) * p).round() as usize;
    sorted_ns[idx]
}

fn bench_case(case: &Case, iters: usize) {
    change_signature(case.path, &case.source, case.func, &transform(), "")
        .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = change_signature(case.path, &case.source, case.func, &transform(), "");
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = change_signature(case.path, &case.source, case.func, &transform(), "");
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
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(5_000);

    println!(
        "shuttle-refactor change_signature end-to-end latency (parse + locals + resolve + validate + rewrite)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
