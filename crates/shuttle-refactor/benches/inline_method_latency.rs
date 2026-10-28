//! End-to-end latency benchmark for `inline_method` (docs §9: "端到端延迟进
//! benches/", aligned with the extract-method bench next to it).
//!
//! Like `extract_method_latency`, this measures the whole per-invocation
//! decision the daemonless caller pays for: parse with tree-sitter, build the
//! locals index, locate the call under the cursor, resolve the same-file callee,
//! bind arguments to parameters, run the return-shape legality gates, and lower
//! to the single SearchReplace op. One representative inline per supported
//! language so a regression in any grammar's call/return handling shows up
//! separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency bench.

use shuttle_refactor::inline_method;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    line: usize,
    col: usize,
}

fn col_of(src: &str, line: usize, needle: &str) -> usize {
    src.split('\n').nth(line - 1).unwrap().find(needle).unwrap()
}

fn cases() -> Vec<Case> {
    // Each is a value-context inline of a same-file single-return callee.
    let py = "def add(a, b):\n    return a + b\n\ndef main():\n    x = add(1, 2)\n".to_string();
    let js =
        "function add(a, b) {\n  return a + b;\n}\nfunction main() {\n  const x = add(1, 2);\n}\n"
            .to_string();
    let go =
        "func add(a int, b int) int {\n\treturn a + b\n}\nfunc main() {\n\tx := add(1, 2)\n}\n"
            .to_string();
    let rs =
        "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\nfn main() {\n    let x = add(1, 2);\n}\n"
            .to_string();
    let ts = "function add(a: number, b: number): number {\n  return a + b;\n}\nfunction main() {\n  const x = add(1, 2);\n}\n".to_string();
    let c_py = col_of(&py, 5, "add");
    let c_js = col_of(&js, 5, "add");
    let c_go = col_of(&go, 5, "add");
    let c_rs = col_of(&rs, 5, "add");
    let c_ts = col_of(&ts, 5, "add");
    vec![
        Case {
            name: "python_value",
            path: "m.py",
            source: py,
            line: 5,
            col: c_py,
        },
        Case {
            name: "javascript_value",
            path: "m.js",
            source: js,
            line: 5,
            col: c_js,
        },
        Case {
            name: "go_value",
            path: "m.go",
            source: go,
            line: 5,
            col: c_go,
        },
        Case {
            name: "rust_value",
            path: "m.rs",
            source: rs,
            line: 5,
            col: c_rs,
        },
        Case {
            name: "typescript_value",
            path: "m.ts",
            source: ts,
            line: 5,
            col: c_ts,
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
    inline_method(case.path, &case.source, case.line, case.col)
        .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = inline_method(case.path, &case.source, case.line, case.col);
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = inline_method(case.path, &case.source, case.line, case.col);
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
        "shuttle-refactor inline_method end-to-end latency (parse + locals + resolve + bind + gate + lower)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
