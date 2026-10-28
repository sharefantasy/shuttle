//! End-to-end latency benchmark for `move_function` (docs §9: "端到端延迟进
//! benches/", aligned with the change-signature / introduce-parameter benches
//! beside it).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the unique top-level
//! definition, extract its exact block, and lower the origin-delete +
//! destination-append into ops. `dest` points at a guaranteed-absent path so
//! the CreateFile branch is taken (no destination read) and `repo_root` is empty
//! so no `shuttle callers` scan is included — this isolates the in-file hot path. One
//! representative top-level function per supported language so a regression in
//! any grammar's function-boundary detection shows separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::move_function;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    func: &'static str,
    dest: &'static str,
}

fn cases() -> Vec<Case> {
    let py = "import os\n\ndef helper(a):\n    return a + 1\n\ndef main():\n    return helper(2)\n"
        .to_string();
    let js =
        "function helper(a) {\n  return a + 1;\n}\nfunction main() {\n  return helper(2);\n}\n"
            .to_string();
    let go = "package p\n\nfunc helper(a int) int {\n\treturn a + 1\n}\nfunc main() {\n\t_ = helper(2)\n}\n".to_string();
    let rs = "fn helper(a: i32) -> i32 {\n    a + 1\n}\nfn main() {\n    let _ = helper(2);\n}\n"
        .to_string();
    let ts = "function helper(a: number): number {\n  return a + 1;\n}\nfunction main() {\n  return helper(2);\n}\n".to_string();
    vec![
        Case {
            name: "python_move",
            path: "m.py",
            source: py,
            func: "helper",
            dest: "/nonexistent_shuttle_bench_dest.py",
        },
        Case {
            name: "javascript_move",
            path: "m.js",
            source: js,
            func: "helper",
            dest: "/nonexistent_shuttle_bench_dest.js",
        },
        Case {
            name: "go_move",
            path: "m.go",
            source: go,
            func: "helper",
            dest: "/nonexistent_shuttle_bench_dest.go",
        },
        Case {
            name: "rust_move",
            path: "m.rs",
            source: rs,
            func: "helper",
            dest: "/nonexistent_shuttle_bench_dest.rs",
        },
        Case {
            name: "typescript_move",
            path: "m.ts",
            source: ts,
            func: "helper",
            dest: "/nonexistent_shuttle_bench_dest.ts",
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
    move_function(
        case.path,
        &case.source,
        case.func,
        case.dest,
        "",
        false,
        false,
        false,
    )
    .unwrap_or_else(|e| panic!("case {} should produce a plan, got {e:?}", case.name));

    for _ in 0..(iters / 10).max(1) {
        let _ = move_function(
            case.path,
            &case.source,
            case.func,
            case.dest,
            "",
            false,
            false,
            false,
        );
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = move_function(
            case.path,
            &case.source,
            case.func,
            case.dest,
            "",
            false,
            false,
            false,
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
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(5_000);

    println!(
        "shuttle-refactor move_function end-to-end latency (parse + locals + resolve + extract + lower)\n"
    );
    for case in cases() {
        bench_case(&case, iters);
    }
}
