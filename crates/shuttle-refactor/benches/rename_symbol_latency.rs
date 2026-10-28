//! End-to-end latency benchmark for `rename_symbol` (docs §9: "端到端延迟进
//! benches/"). Measures the same-file textual path (no repo root): read the
//! origin file, word-boundary rewrite every occurrence line, and lower into
//! SearchReplace ops. Cross-file discovery via `shuttle references` depends on
//! external backends (ctags/ast-grep) and repo layout, so it is out of scope
//! for this in-process micro-bench.
//!
//! std-only (no criterion), `harness = false`.

use shuttle_refactor::rename_symbol;
use std::time::Instant;

fn percentile(sorted_ns: &[u128], p: f64) -> u128 {
    if sorted_ns.is_empty() {
        return 0;
    }
    let idx = ((sorted_ns.len() as f64 - 1.0) * p).round() as usize;
    sorted_ns[idx]
}

fn main() {
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(5_000);

    let dir = std::env::var("SHUTTLE_TEST_TMP").unwrap_or_else(|_| ".".into());
    let path = format!("{dir}/rename_symbol_bench.py");
    let src = "def widget(x):\n    return widget_helper(x) + 1\n\ndef widget_helper(y):\n    return y\n\nfor i in range(10):\n    print(widget(i))\n    print(widget(i * 2))\n";
    std::fs::write(&path, src).expect("write bench fixture");

    // Warm up.
    for _ in 0..(iters / 10).max(1) {
        let _ = rename_symbol(&path, "widget", "gadget", "");
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let plan = rename_symbol(&path, "widget", "gadget", "");
        let ns = t.elapsed().as_nanos();
        std::hint::black_box(&plan);
        samples.push(ns);
    }
    let _ = std::fs::remove_file(&path);

    samples.sort_unstable();
    let sum: u128 = samples.iter().sum();
    let mean = sum / samples.len() as u128;
    println!("shuttle-refactor rename_symbol same-file end-to-end latency\n");
    println!(
        "{:<22} n={:<6} mean={:>8}ns  p50={:>8}ns  p90={:>8}ns  p99={:>8}ns  max={:>8}ns",
        "python_same_file",
        iters,
        mean,
        percentile(&samples, 0.50),
        percentile(&samples, 0.90),
        percentile(&samples, 0.99),
        samples.last().copied().unwrap_or(0),
    );
}
