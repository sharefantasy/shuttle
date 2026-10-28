//! G9·D3 — confidence calibration: turning the *design-time* High/Medium/Low
//! thresholds into a **data-driven** verdict grounded in execution truth.
//!
//! # Why this exists
//!  — today the confidence a graph edge
//! carries (High / Medium / Low) is a threshold *someone picked at design time*.
//! D3 backfills it with the G5 **execution truth**: of the edges we labeled
//! `Medium`, how many did `shuttle verify` actually prove out; of the weak `Low`
//! name-match edges, how many turned out to be really coupled. From those
//! observed hit rates it re-calibrates the per-tier floor — so the *degradation
//! itself* is measured, not guessed.
//!
//! # The three data sources (§4.2)
//! A [`TierSample`] is the **join product of exactly the two named telemetry
//! sources**: the confidence label an edge carried in a
//! [`crate::telemetry::SplitTelemetryLedger`] (which tier it was), crossed with
//! the pass/fail it earned in a [`crate::telemetry::VerifyTelemetry`] run (its
//! execution truth). The G7 co-change signal enters as the weak-signal
//! *control*: its observations are **pooled into the `Low` tier** so a genuinely
//! weak axis can gather enough samples to cross the min-sample gate without ever
//! borrowing statistical power from the stronger tiers.
//!
//! # §7 «绝不瞎猜» (§2 pit #15 anti-overfit)
//! Three hard rules, each a distinct [`TierVerdict`]:
//!   * **sample below the minimum ⇒ freeze the default floor** — a handful of
//!     observations never moves a threshold ([`TierVerdict::FrozenInsufficient`]).
//!   * **a provable tier is pinned** — `High` (the home of the provable directed
//!     edge kinds `ImportDirected` / `ContractDirected`) is NEVER down-adjusted,
//!     no matter how its sample reads ([`TierVerdict::PinnedProvable`]). A
//!     provable edge cannot be talked out of its proof by a noisy verify run.
//!   * **the ordering invariant holds** — after calibration
//!     `0 <= low <= medium <= high(==1)`; a candidate that would cross a
//!     neighbour is clamped, and the clamp is recorded verbatim so the rationale
//!     never lies about what actually landed.
//!
//! # Layering
//! Pure `shuttle-protocol`: no I/O, no CLI, no symbol-graph dependency — a
//! deterministic projection like [`crate::telemetry`] and [`crate::metrics`].
//! Feed it the same samples and it yields the same thresholds, byte for byte.

use serde::{Deserialize, Serialize};

use crate::tool_outcome::OutcomeConfidence;

/// Schema version stamped into a persisted calibration so a later consumer can
/// reject or migrate an incompatible verdict.
pub const CALIBRATION_VERSION: u32 = 1;

/// The minimum number of *executed* observations a tier needs before its floor
/// may move. Below this the default is frozen (§7 never-extrapolate).
pub const MIN_SAMPLE: usize = 20;

/// The design-time floor for the provable `High` tier: a provable directed edge
/// is expected to hold every time. Pinned — calibration never lowers it.
pub const DEFAULT_HIGH_FLOOR: f64 = 1.0;

/// The design-time floor for the `Medium` tier (typed / dataflow evidence).
pub const DEFAULT_MEDIUM_FLOOR: f64 = 0.50;

/// The design-time floor for the weak `Low` tier (name-match / co-change).
pub const DEFAULT_LOW_FLOOR: f64 = 0.10;

/// One tier's worth of execution-truth evidence — the join of a split ledger's
/// confidence label with a verify run's pass/fail.
///
/// `observed` counts the edges of this tier that verify *actually ran* (never a
/// re-verify set that was skipped — a skip is not evidence); `proven` is how
/// many of those were borne out (passed / proved really coupled). `provable`
/// marks a tier drawn from the provable edge kinds (`ImportDirected` /
/// `ContractDirected`): such a sample is pinned and can never *down*-adjust its
/// floor (§4.3 row 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierSample {
    /// Which confidence tier these observations were labeled at.
    pub tier: OutcomeConfidence,
    /// Edges of this tier that verify actually exercised.
    pub observed: usize,
    /// Of `observed`, how many were proven true (passed / really coupled).
    pub proven: usize,
    /// True when the sample is drawn from a provable edge kind — pinned,
    /// never used to down-adjust its floor (§7).
    #[serde(default)]
    pub provable: bool,
}

impl TierSample {
    /// The empirical hit rate `proven / observed`, guarding an empty sample
    /// (a zero-observation tier is rate 0.0, never a divide-by-zero panic and
    /// never an invented rate).
    pub fn hit_rate(&self) -> f64 {
        if self.observed == 0 {
            0.0
        } else {
            self.proven as f64 / self.observed as f64
        }
    }
}

/// The evidence handed to the calibrator: one sample per tier plus the optional
/// G7 co-change weak-signal control (pooled into `Low`, §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationInput {
    /// Provable-tier observations (`ImportDirected` / `ContractDirected`).
    pub high: TierSample,
    /// Medium-tier observations (typed / dataflow evidence).
    pub medium: TierSample,
    /// Low-tier observations (name-match weak edges).
    pub low: TierSample,
    /// G7 co-change control: pure co-change-only partner observations, pooled
    /// into the `Low` tier so the weak axis can reach the min-sample gate on
    /// its own evidence. Absent ⇒ the `Low` tier stands on its own sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cochange_control: Option<TierSample>,
}

/// What the calibrator decided for one tier — and *why*, so a reviewer can
/// audit every move (or non-move).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum TierVerdict {
    /// The sample met [`MIN_SAMPLE`]; the floor was recalibrated toward the
    /// observed hit rate. `to` is the value that actually landed (after the
    /// ordering clamp); `observed_rate` is the raw empirical rate before it.
    Adjusted {
        from: f64,
        to: f64,
        observed_rate: f64,
        sample: usize,
    },
    /// The sample was below [`MIN_SAMPLE`]; the default floor is frozen — a
    /// few observations never move a threshold (§7 never-extrapolate).
    FrozenInsufficient { default: f64, sample: usize },
    /// A provable tier: pinned at its floor, never down-adjusted regardless of
    /// the observed rate (§4.3 — a proof can't be argued away by a noisy run).
    PinnedProvable { floor: f64, observed_rate: f64 },
}

/// The calibrated per-tier floors plus the per-tier rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibratedThresholds {
    pub version: u32,
    /// Provable tier floor — always [`DEFAULT_HIGH_FLOOR`], pinned.
    pub high_floor: f64,
    pub medium_floor: f64,
    pub low_floor: f64,
    pub high: TierVerdict,
    pub medium: TierVerdict,
    pub low: TierVerdict,
}

/// Calibrate the High/Medium/Low floors from execution-truth samples.
///
/// The `High` tier is pinned (provable, never down-adjusted). `Medium` and
/// `Low` move toward their observed hit rate *only* when the sample crosses
/// [`MIN_SAMPLE`]; otherwise their default floor is frozen. The G7 co-change
/// control is pooled into `Low`. Finally the ordering invariant
/// `0 <= low <= medium <= high(==1)` is enforced by clamping, and the clamp is
/// recorded in each verdict's `to`.
pub fn calibrate(input: &CalibrationInput) -> CalibratedThresholds {
    // High is provable → pinned. Even a low-reading High sample never lowers
    // the floor (§4.3 row 3): a proof outranks a noisy verify outcome.
    let high_floor = DEFAULT_HIGH_FLOOR;
    let high = TierVerdict::PinnedProvable {
        floor: high_floor,
        observed_rate: input.high.hit_rate(),
    };

    // Medium: adjust toward observed iff the sample is sufficient; else freeze.
    let (medium_candidate, medium_verdict_raw) = calibrate_tier(
        &input.medium,
        DEFAULT_MEDIUM_FLOOR,
        input.medium.observed,
        input.medium.hit_rate(),
    );

    // Low: pool the co-change control (§4.2) so the weak axis can reach the
    // gate on its own combined evidence.
    let (low_observed, low_proven) = match &input.cochange_control {
        Some(c) => (input.low.observed + c.observed, input.low.proven + c.proven),
        None => (input.low.observed, input.low.proven),
    };
    let low_rate = if low_observed == 0 {
        0.0
    } else {
        low_proven as f64 / low_observed as f64
    };
    let (low_candidate, low_verdict_raw) =
        calibrate_tier(&input.low, DEFAULT_LOW_FLOOR, low_observed, low_rate);

    // Enforce the ordering invariant by clamping, then fold the clamp back into
    // each verdict's landed `to` so the rationale never overstates the move.
    let medium_floor = medium_candidate.clamp(0.0, high_floor);
    let low_floor = low_candidate.clamp(0.0, medium_floor);

    let medium = finalize_verdict(medium_verdict_raw, medium_floor);
    let low = finalize_verdict(low_verdict_raw, low_floor);

    CalibratedThresholds {
        version: CALIBRATION_VERSION,
        high_floor,
        medium_floor,
        low_floor,
        high,
        medium,
        low,
    }
}

/// Decide one (non-provable) tier: adjust toward `observed_rate` when
/// `sample >= MIN_SAMPLE`, else freeze `default`. Returns the *pre-clamp*
/// candidate floor and a verdict still carrying the pre-clamp `to`.
fn calibrate_tier(
    _sample: &TierSample,
    default: f64,
    sample_n: usize,
    observed_rate: f64,
) -> (f64, TierVerdict) {
    if sample_n >= MIN_SAMPLE {
        (
            observed_rate,
            TierVerdict::Adjusted {
                from: default,
                to: observed_rate,
                observed_rate,
                sample: sample_n,
            },
        )
    } else {
        (
            default,
            TierVerdict::FrozenInsufficient {
                default,
                sample: sample_n,
            },
        )
    }
}

/// Fold the ordering clamp back into an `Adjusted` verdict's landed `to` (a
/// frozen/pinned verdict is unaffected — its floor never moved).
fn finalize_verdict(v: TierVerdict, landed_floor: f64) -> TierVerdict {
    match v {
        TierVerdict::Adjusted {
            from,
            observed_rate,
            sample,
            ..
        } => TierVerdict::Adjusted {
            from,
            to: landed_floor,
            observed_rate,
            sample,
        },
        other => other,
    }
}

impl Default for CalibratedThresholds {
    /// The all-defaults (fully frozen) calibration — the honest verdict when
    /// there is no execution truth to learn from at all.
    fn default() -> Self {
        CalibratedThresholds {
            version: CALIBRATION_VERSION,
            high_floor: DEFAULT_HIGH_FLOOR,
            medium_floor: DEFAULT_MEDIUM_FLOOR,
            low_floor: DEFAULT_LOW_FLOOR,
            high: TierVerdict::PinnedProvable {
                floor: DEFAULT_HIGH_FLOOR,
                observed_rate: 0.0,
            },
            medium: TierVerdict::FrozenInsufficient {
                default: DEFAULT_MEDIUM_FLOOR,
                sample: 0,
            },
            low: TierVerdict::FrozenInsufficient {
                default: DEFAULT_LOW_FLOOR,
                sample: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        tier: OutcomeConfidence,
        observed: usize,
        proven: usize,
        provable: bool,
    ) -> TierSample {
        TierSample {
            tier,
            observed,
            proven,
            provable,
        }
    }

    fn input(high: TierSample, medium: TierSample, low: TierSample) -> CalibrationInput {
        CalibrationInput {
            high,
            medium,
            low,
            cochange_control: None,
        }
    }

    #[test]
    fn hit_rate_guards_empty_sample() {
        let s = sample(OutcomeConfidence::Low, 0, 0, false);
        assert_eq!(s.hit_rate(), 0.0, "an empty tier is rate 0, never a panic");
    }

    #[test]
    fn insufficient_sample_freezes_the_default() {
        // Medium below the gate (5 < 20) → the default floor is frozen even
        // though the observed rate (5/5 = 1.0) would otherwise raise it.
        let c = calibrate(&input(
            sample(OutcomeConfidence::High, 100, 100, true),
            sample(OutcomeConfidence::Medium, 5, 5, false),
            sample(OutcomeConfidence::Low, 3, 0, false),
        ));
        assert_eq!(
            c.medium_floor, DEFAULT_MEDIUM_FLOOR,
            "few samples never move it"
        );
        assert!(matches!(
            c.medium,
            TierVerdict::FrozenInsufficient { sample: 5, .. }
        ));
        assert_eq!(c.low_floor, DEFAULT_LOW_FLOOR);
        assert!(matches!(
            c.low,
            TierVerdict::FrozenInsufficient { sample: 3, .. }
        ));
    }

    #[test]
    fn sufficient_sample_adjusts_toward_observed() {
        // Medium: 50 observed, 30 proven → 0.6, above the 0.5 default → raised.
        let c = calibrate(&input(
            sample(OutcomeConfidence::High, 100, 100, true),
            sample(OutcomeConfidence::Medium, 50, 30, false),
            sample(OutcomeConfidence::Low, 40, 8, false),
        ));
        assert!(
            (c.medium_floor - 0.6).abs() < 1e-9,
            "medium calibrated to observed 0.6: {c:?}"
        );
        match c.medium {
            TierVerdict::Adjusted {
                from,
                to,
                observed_rate,
                sample,
            } => {
                assert_eq!(from, DEFAULT_MEDIUM_FLOOR);
                assert!((to - 0.6).abs() < 1e-9);
                assert!((observed_rate - 0.6).abs() < 1e-9);
                assert_eq!(sample, 50);
            }
            other => panic!("expected Adjusted, got {other:?}"),
        }
        // Low: 40 observed, 8 proven → 0.2, above 0.1 default → raised.
        assert!(
            (c.low_floor - 0.2).abs() < 1e-9,
            "low calibrated to 0.2: {c:?}"
        );
    }

    #[test]
    fn provable_high_tier_is_never_down_adjusted() {
        // A spuriously low High sample (40/100 = 0.4) must NOT lower the floor:
        // a provable edge cannot be argued out of its proof by a noisy run.
        let c = calibrate(&input(
            sample(OutcomeConfidence::High, 100, 40, true),
            sample(OutcomeConfidence::Medium, 50, 30, false),
            sample(OutcomeConfidence::Low, 40, 8, false),
        ));
        assert_eq!(
            c.high_floor, DEFAULT_HIGH_FLOOR,
            "provable tier pinned at 1.0"
        );
        match c.high {
            TierVerdict::PinnedProvable {
                floor,
                observed_rate,
            } => {
                assert_eq!(floor, 1.0);
                assert!(
                    (observed_rate - 0.4).abs() < 1e-9,
                    "records the raw rate honestly"
                );
            }
            other => panic!("expected PinnedProvable, got {other:?}"),
        }
    }

    #[test]
    fn cochange_control_pools_into_low_to_cross_the_gate() {
        // Low alone (8 obs) is below the gate; pooling the co-change control
        // (14 obs) crosses 20 and calibrates the weak tier on its combined
        // evidence: (2+3)/(8+14) = 5/22 ≈ 0.2273.
        let mut inp = input(
            sample(OutcomeConfidence::High, 100, 100, true),
            sample(OutcomeConfidence::Medium, 50, 30, false),
            sample(OutcomeConfidence::Low, 8, 2, false),
        );
        inp.cochange_control = Some(sample(OutcomeConfidence::Low, 14, 3, false));
        let c = calibrate(&inp);
        match c.low {
            TierVerdict::Adjusted {
                sample,
                observed_rate,
                ..
            } => {
                assert_eq!(sample, 22, "pooled Low+co-change sample crosses the gate");
                assert!((observed_rate - (5.0 / 22.0)).abs() < 1e-9);
            }
            other => panic!("expected pooled Adjusted, got {other:?}"),
        }
    }

    #[test]
    fn ordering_invariant_is_enforced_by_clamping() {
        // Noisy data: Low reads HIGHER than Medium (0.9 vs 0.3). The clamp keeps
        // low_floor <= medium_floor and records the landed (clamped) `to`.
        let c = calibrate(&input(
            sample(OutcomeConfidence::High, 100, 100, true),
            sample(OutcomeConfidence::Medium, 100, 30, false), // 0.3
            sample(OutcomeConfidence::Low, 100, 90, false),    // 0.9
        ));
        assert!(c.low_floor <= c.medium_floor, "invariant held: {c:?}");
        assert!((c.medium_floor - 0.3).abs() < 1e-9);
        assert!(
            (c.low_floor - 0.3).abs() < 1e-9,
            "low clamped down to medium: {c:?}"
        );
        match c.low {
            TierVerdict::Adjusted {
                to, observed_rate, ..
            } => {
                assert!(
                    (observed_rate - 0.9).abs() < 1e-9,
                    "raw rate recorded honestly"
                );
                assert!((to - 0.3).abs() < 1e-9, "landed value is the clamp");
            }
            other => panic!("expected Adjusted, got {other:?}"),
        }
        // The full invariant.
        assert!(
            0.0 <= c.low_floor && c.low_floor <= c.medium_floor && c.medium_floor <= c.high_floor
        );
        assert_eq!(c.high_floor, 1.0);
    }

    #[test]
    fn default_is_a_fully_frozen_calibration() {
        let d = CalibratedThresholds::default();
        assert_eq!(d.high_floor, DEFAULT_HIGH_FLOOR);
        assert_eq!(d.medium_floor, DEFAULT_MEDIUM_FLOOR);
        assert_eq!(d.low_floor, DEFAULT_LOW_FLOOR);
        assert!(matches!(d.medium, TierVerdict::FrozenInsufficient { .. }));
    }

    #[test]
    fn calibration_roundtrips_through_json() {
        let c = calibrate(&input(
            sample(OutcomeConfidence::High, 100, 100, true),
            sample(OutcomeConfidence::Medium, 50, 30, false),
            sample(OutcomeConfidence::Low, 40, 8, false),
        ));
        let json = serde_json::to_string(&c).unwrap();
        let back: CalibratedThresholds = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn input_roundtrips_through_json() {
        let inp = input(
            sample(OutcomeConfidence::High, 10, 10, true),
            sample(OutcomeConfidence::Medium, 50, 30, false),
            sample(OutcomeConfidence::Low, 40, 8, false),
        );
        let json = serde_json::to_string(&inp).unwrap();
        let back: CalibrationInput = serde_json::from_str(&json).unwrap();
        assert_eq!(inp, back);
    }
}
