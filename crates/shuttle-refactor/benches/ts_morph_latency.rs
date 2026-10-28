//! End-to-end latency benchmark for the OPTIONAL `ts-morph` TypeScript type
//! bridge (docs §2.2/§5/§6, batch 3; §9: "端到端延迟进 benches/"). Unlike the
//! sibling sub-ms benches, this one measures the deliberate slow path: spawning a
//! non-daemon `node` process, piping a small JSON payload, running ts-morph's TS
//! Compiler API on a single in-memory source, and parsing the reply. It exists to
//! keep that "按需拉起、用完即走" cost visible and honest — this is why the bridge
//! is off by default and only pulled in when a real `TODO_TYPE` needs filling.
//!
//! Compiled only under `--features ts-morph`. Without the feature (the default),
//! `main` prints a skip line and exits — the daemonless build carries no node
//! dependency. When enabled but ts-morph is not installed (`Bridge::Unavailable`),
//! it also skips: the bench can't measure a round-trip that can't happen.
//!
//! Run: `cargo bench -p shuttle-refactor --features ts-morph --bench ts_morph_latency`
//! (append `-- <iters>`; default 200 — node spawns are expensive, so far fewer
//! samples than the µs-scale benches).
//!
//! std-only, `harness = false`, matching the sibling latency benches.

#[cfg(not(feature = "ts-morph"))]
fn main() {
    println!("ts_morph_latency: skipped (build with --features ts-morph to measure)");
}

#[cfg(feature = "ts-morph")]
fn main() {
    use shuttle_refactor::ts_morph::{infer_types, Bridge};
    use std::time::Instant;

    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(200);

    // A same-file callee whose object return type ts-morph can infer but neither
    // locals nor ctags could — the exact case the bridge exists to serve.
    let source = "const w = makeWidget();\n\
                  function makeWidget() { return { id: 1, label: \"x\" }; }\n";
    let names = vec!["w".to_string()];

    // Probe once: if ts-morph isn't installed, there's nothing to time.
    match infer_types(source, "bench.ts", &names) {
        Bridge::Resolved(m) if m.contains_key("w") => {}
        Bridge::Unavailable => {
            println!("ts_morph_latency: skipped (ts-morph not installed; `npm i ts-morph`)");
            return;
        }
        other => {
            println!("ts_morph_latency: skipped (bridge returned {other:?})");
            return;
        }
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let out = infer_types(source, "bench.ts", &names);
        let ns = t.elapsed().as_nanos();
        std::hint::black_box(&out);
        samples.push(ns);
    }
    samples.sort_unstable();

    let sum: u128 = samples.iter().sum();
    let mean = sum / samples.len() as u128;
    let pct = |p: f64| {
        let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
        samples[idx]
    };
    println!(
        "shuttle-refactor ts-morph bridge end-to-end latency (node spawn + ts-morph infer + parse)\n"
    );
    println!(
        "ts_morph_infer      n={:<6} mean={:>10}ns  p50={:>10}ns  p90={:>10}ns  p99={:>10}ns  max={:>10}ns",
        iters,
        mean,
        pct(0.50),
        pct(0.90),
        pct(0.99),
        samples.last().copied().unwrap_or(0),
    );
}
