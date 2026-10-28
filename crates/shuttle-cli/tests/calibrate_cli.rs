//! Binary-level CLI integration tests for the G9·D3 `shuttle calibrate` subcommand
//! . The calibrator reads a
//! `CalibrationInput` JSON — one `TierSample` per tier plus an optional G7
//! co-change control pooled into Low — and emits a `CalibratedThresholds`
//! verdict. The three §7 anti-overfit red lines are asserted end-to-end:
//!
//!   1. **freeze on insufficient sample** — a tier below the `MIN_SAMPLE`
//!      gate keeps its default floor and reports `frozen_insufficient`; the
//!      calibrator NEVER extrapolates a threshold from a handful of points.
//!   2. **adjust on sufficient sample** — a tier at/above the gate moves its
//!      floor toward the observed hit-rate and reports `adjusted`.
//!   3. **provable High is pinned, never down-adjusted** — the `High` tier is
//!      pinned at 1.0 and reports `pinned_provable` EVEN when its observed
//!      hit-rate is poor; a provable directed edge is never demoted by noisy
//!      execution truth.
//!
//! Pure projection: the calibrator reads ONE input file and touches no repo,
//! so these cases need neither git nor ctags — they always run.

use std::io::Write;
use std::process::Command;

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("shuttle_calibrate_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_input(dir: &std::path::Path, json: &serde_json::Value) -> std::path::PathBuf {
    let p = dir.join("calibration.json");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(serde_json::to_string_pretty(json).unwrap().as_bytes())
        .unwrap();
    p
}

/// `shuttle calibrate --input <path> --json` → parsed `CalibratedThresholds` JSON.
/// Asserts exit 0 (pure projection never fails on well-formed input).
fn run_calibrate(input: &std::path::Path) -> serde_json::Value {
    let out = Command::new(shuttle_bin())
        .args(["calibrate", "--input", input.to_str().unwrap(), "--json"])
        .output()
        .expect("spawn shuttle calibrate");
    assert!(
        out.status.success(),
        "shuttle calibrate exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("calibrate stdout is not the JSON verdict: {e}\n---\n{stdout}"))
}

// ---------------------------------------------------------------------------
// §7 red line 1: insufficient sample FREEZES the default floor.
// ---------------------------------------------------------------------------

#[test]
fn insufficient_sample_freezes_and_never_extrapolates() {
    let ws = scratch_dir("freeze");
    // Medium/Low each carry only a handful of observations — well under the
    // MIN_SAMPLE gate. A naive fit would slam Low's floor to ~0 (0/2 proven),
    // which is exactly the overfit the freeze rule forbids.
    let input = write_input(
        &ws,
        &serde_json::json!({
            "high":   {"tier":"high","observed":3,"proven":3,"provable":true},
            "medium": {"tier":"medium","observed":5,"proven":1},
            "low":    {"tier":"low","observed":2,"proven":0}
        }),
    );
    let v = run_calibrate(&input);

    // Medium & Low froze at their defaults; the verdict names the reason and
    // reports the (too-small) sample it refused to fit.
    assert_eq!(v["medium"]["verdict"], "frozen_insufficient", "{v}");
    assert_eq!(v["medium"]["default"], 0.5, "{v}");
    assert_eq!(v["medium"]["sample"], 5, "{v}");
    assert_eq!(v["medium_floor"], 0.5, "{v}");

    assert_eq!(v["low"]["verdict"], "frozen_insufficient", "{v}");
    assert_eq!(v["low"]["default"], 0.1, "{v}");
    assert_eq!(v["low"]["sample"], 2, "{v}");
    assert_eq!(v["low_floor"], 0.1, "{v}");

    // High is provable and pinned even with a tiny sample (red line 3 preview).
    assert_eq!(v["high"]["verdict"], "pinned_provable", "{v}");
    assert_eq!(v["high_floor"], 1.0, "{v}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 red line 2: sufficient sample ADJUSTS the floor toward observed truth,
// and the G7 co-change control is pooled into the Low tier.
// ---------------------------------------------------------------------------

#[test]
fn sufficient_sample_adjusts_toward_observed_and_pools_cochange() {
    let ws = scratch_dir("adjust");
    // Medium: 21/30 proven → 0.70 observed, comfortably above the gate.
    // Low: 1/8 direct + a co-change control of 4/18 → pooled 5/26 ≈ 0.1923,
    // both the direct Low sample and the pooled total clear MIN_SAMPLE=20.
    let input = write_input(
        &ws,
        &serde_json::json!({
            "high":   {"tier":"high","observed":40,"proven":40,"provable":true},
            "medium": {"tier":"medium","observed":30,"proven":21},
            "low":    {"tier":"low","observed":8,"proven":1},
            "cochange_control": {"tier":"low","observed":18,"proven":4}
        }),
    );
    let v = run_calibrate(&input);

    assert_eq!(v["medium"]["verdict"], "adjusted", "{v}");
    assert_eq!(v["medium"]["from"], 0.5, "{v}");
    assert!(
        (v["medium"]["to"].as_f64().unwrap() - 0.70).abs() < 1e-9,
        "medium adjusted to observed 0.70: {v}"
    );
    assert!(
        (v["medium_floor"].as_f64().unwrap() - 0.70).abs() < 1e-9,
        "{v}"
    );
    assert_eq!(v["medium"]["sample"], 30, "{v}");

    // Low pooled the co-change control: 5 proven of 26 observed.
    assert_eq!(v["low"]["verdict"], "adjusted", "{v}");
    assert_eq!(v["low"]["sample"], 26, "{v}");
    let pooled = 5.0f64 / 26.0f64;
    assert!(
        (v["low"]["to"].as_f64().unwrap() - pooled).abs() < 1e-9,
        "low adjusted to pooled observed {pooled}: {v}"
    );
    assert!(
        (v["low_floor"].as_f64().unwrap() - pooled).abs() < 1e-9,
        "{v}"
    );

    // Ordering invariant preserved: 0 <= low <= medium <= high == 1.
    let (lo, md, hi) = (
        v["low_floor"].as_f64().unwrap(),
        v["medium_floor"].as_f64().unwrap(),
        v["high_floor"].as_f64().unwrap(),
    );
    assert!(
        0.0 <= lo && lo <= md && md <= hi && (hi - 1.0).abs() < 1e-9,
        "ordering: {v}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// §7 red line 3: a provable High tier is PINNED at 1.0 and never down-adjusted,
// even when execution truth for it looks poor.
// ---------------------------------------------------------------------------

#[test]
fn provable_high_is_pinned_never_down_adjusted() {
    let ws = scratch_dir("pin");
    // High has a large sample with a mediocre 30/50 = 0.60 observed rate. A
    // blind fit would drag its floor down to 0.60 — but a provable directed
    // edge (ImportDirected/ContractDirected) is ground truth, so it stays 1.0.
    let input = write_input(
        &ws,
        &serde_json::json!({
            "high":   {"tier":"high","observed":50,"proven":30,"provable":true},
            "medium": {"tier":"medium","observed":40,"proven":10},
            "low":    {"tier":"low","observed":40,"proven":2}
        }),
    );
    let v = run_calibrate(&input);

    assert_eq!(v["high"]["verdict"], "pinned_provable", "{v}");
    assert_eq!(v["high_floor"], 1.0, "{v}");
    // The verdict is honest about the poor observed rate it declined to apply.
    assert!(
        (v["high"]["observed_rate"].as_f64().unwrap() - 0.60).abs() < 1e-9,
        "pinned verdict still reports the true observed rate: {v}"
    );
    // The floor did NOT move down to the observed rate.
    assert!(v["high_floor"].as_f64().unwrap() > 0.60, "{v}");

    // Medium/Low did adjust down (they are not provable) — the invariant clamp
    // keeps them ordered under the pinned High.
    assert_eq!(v["medium"]["verdict"], "adjusted", "{v}");
    assert_eq!(v["low"]["verdict"], "adjusted", "{v}");
    let (lo, md, hi) = (
        v["low_floor"].as_f64().unwrap(),
        v["medium_floor"].as_f64().unwrap(),
        v["high_floor"].as_f64().unwrap(),
    );
    assert!(
        0.0 <= lo && lo <= md && md <= hi && (hi - 1.0).abs() < 1e-9,
        "ordering: {v}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------------------
// The human (non-JSON) rendering names every tier's verdict — a caller
// eyeballing the table still sees freeze/adjust/pin, never a bare number.
// ---------------------------------------------------------------------------

#[test]
fn human_output_labels_each_verdict() {
    let ws = scratch_dir("human");
    let input = write_input(
        &ws,
        &serde_json::json!({
            "high":   {"tier":"high","observed":3,"proven":3,"provable":true},
            "medium": {"tier":"medium","observed":30,"proven":21},
            "low":    {"tier":"low","observed":2,"proven":0}
        }),
    );
    let out = Command::new(shuttle_bin())
        .args(["calibrate", "--input", input.to_str().unwrap()])
        .output()
        .expect("spawn shuttle calibrate (human)");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains("pinned"), "high pinned label: {stdout}");
    assert!(
        stdout.contains("adjusted"),
        "medium adjusted label: {stdout}"
    );
    assert!(stdout.contains("frozen"), "low frozen label: {stdout}");
    // The provable tier is labelled provable, never fabricated as a fit.
    assert!(stdout.contains("provable"), "provable label: {stdout}");

    let _ = std::fs::remove_dir_all(&ws);
}
