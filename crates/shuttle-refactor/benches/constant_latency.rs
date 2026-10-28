//! End-to-end latency benchmark for `extract_constant` (docs §9: "端到端延迟进
//! benches/", aligned with the sibling refactor benches beside it).
//!
//! Measures the whole per-invocation decision a daemonless caller pays for:
//! parse with tree-sitter, build the locals index, resolve the selected literal
//! node, scan the file for every structurally-identical literal, build the
//! module-level declaration (with the Rust type-guess honesty branch), and lower
//! the rewrites + declaration into SearchReplace ops (including the post-plan
//! re-parse safety net). One representative extract per supported language so a
//! regression in any grammar's literal handling shows separately.
//!
//! std-only (no criterion), `harness = false`: dependency-free and instant to
//! compile, matching the sibling latency benches.

use shuttle_refactor::extract_constant;
use std::time::Instant;

struct Case {
    name: &'static str,
    path: &'static str,
    source: String,
    line: usize,
    start_col: usize,
    end_col: usize,
    const_name: &'static str,
}

fn cases() -> Vec<Case> {
    // Each promotes the magic number `86400` used twice into a named constant.
    let py = "def a():\n    return x * 86400\n\ndef b():\n    return y + 86400\n".to_string();
    let js = "function a() {\n  return x * 86400;\n}\nfunction b() {\n  return y + 86400;\n}\n"
        .to_string();
    let go = "package p\n\nfunc a() int {\n\treturn 86400\n}\nfunc b() int {\n\treturn 86400\n}\n"
        .to_string();
    let rs = "fn a() -> i32 {\n    86400\n}\nfn b() -> i32 {\n    86400\n}\n".to_string();
    let ts =
        "function a(): number {\n  return 86400;\n}\nfunction b(): number {\n  return 86400;\n}\n"
            .to_string();
    // Column of the first `86400` occurrence on its line.
    let col = |src: &str, line: usize| -> (usize, usize) {
        let l = src.split('\n').nth(line - 1).unwrap();
        let sc = l.find("86400").unwrap();
        (sc, sc + 5)
    };
    let (psc, pec) = col(&py, 2);
    let (jsc, jec) = col(&js, 2);
    let (gsc, gec) = col(&go, 4);
    let (rsc, rec) = col(&rs, 2);
    let (tsc, tec) = col(&ts, 2);
    vec![
        Case {
            name: "python_extract",
            path: "m.py",
            source: py,
            line: 2,
            start_col: psc,
            end_col: pec,
            const_name: "SECONDS_PER_DAY",
        },
        Case {
            name: "javascript_extract",
            path: "m.js",
            source: js,
            line: 2,
            start_col: jsc,
            end_col: jec,
            const_name: "SECONDS_PER_DAY",
        },
        Case {
            name: "go_extract",
            path: "m.go",
            source: go,
            line: 4,
            start_col: gsc,
            end_col: gec,
            const_name: "SecondsPerDay",
        },
        Case {
            name: "rust_extract",
            path: "m.rs",
            source: rs,
            line: 2,
            start_col: rsc,
            end_col: rec,
            const_name: "SECONDS_PER_DAY",
        },
        Case {
            name: "typescript_extract",
            path: "m.ts",
            source: ts,
            line: 2,
            start_col: tsc,
            end_col: tec,
            const_name: "SECONDS_PER_DAY",
        },
    ]
}

fn main() {
    let cases = cases();
    let iters = 2000;
    println!("extract_constant latency ({iters} iters each)");
    for c in &cases {
        // Warm up + correctness sanity: the plan must be produced.
        let plan = extract_constant(
            c.path,
            &c.source,
            c.line,
            c.start_col,
            c.end_col,
            c.const_name,
        )
        .expect("bench case should produce a plan");
        assert!(!plan.plan.ops.is_empty());

        let start = Instant::now();
        for _ in 0..iters {
            let _ = extract_constant(
                c.path,
                &c.source,
                c.line,
                c.start_col,
                c.end_col,
                c.const_name,
            )
            .unwrap();
        }
        let elapsed = start.elapsed();
        let per = elapsed.as_secs_f64() * 1e6 / iters as f64;
        println!("  {:<20} {:>8.1} us/op", c.name, per);
    }
}
