//! The Form B adapter arm: reverse-measure an external engine's artifact and
//! gate it with the §3.3 hard/soft dichotomy into an `adapter_receipt/v1`.
//!
//! Design §4.2 / §8-P7. shuttle never generates pixels (red-line #6). A high-fidelity
//! design is delegated to an external pixel engine (v0 / Galileo / Figma-AI
//! class) that generates *inside* shuttle's token hard constraints. shuttle then does
//! exactly three things over the engine's produced artifact (a layer tree /
//! figma-node export whose bounding boxes this arm consumes):
//!
//! 1. **reverse-measures the atom holds** from the produced geometry via the
//!    same evidence arm ([`crate::geom`]) that grounds Form A — every measured
//!    atom carries `reversed` provenance, never `fabricated`;
//! 2. **gates** the artifact: a11y / 可区分性, the token 硬门 (metric atoms
//!    measured against their thresholds), and engine capability negotiation are
//!    §3.3 *hard* concerns that fail closed; the engine's untrusted aesthetic
//!    self-report is a §3.3 *soft* score that only flags, never blocks;
//! 3. emits a **component-mapping receipt** binding each layer to the design
//!    token / component it realizes, corroborated (three-valued) by the
//!    reverse-measured atoms.
//!
//! The disposition — adopt / sidecar / drop / reject — is computed by
//! [`shuttle_protocol::AdapterReceipt`]'s fold, so the artifact is **never silently
//! accepted or dropped** (capability negotiation + graceful degrade). A
//! blind-spot (`null`) atom or an unmeasurable gate surfaces on the review gate
//! rather than being fabricated into a pass (§7/§20 「绝不瞎猜」).
//!
//! This arm reasons and reports; it never mutates a workspace, owns the artifact
//! bytes, or drives the pixel engine itself (red-line #11).

use crate::constraint::{A11yConstraint, A11yVerdict};
use crate::geom::{self, Frame};
use shuttle_protocol::{
    AdapterBasis, AdapterReceipt, AestheticScore, ClosedContractError, ComponentMapping, CoordAtom,
    HardGateKind, HardGateRow, HardGateVerdict, Holds,
};
use std::collections::BTreeMap;

/// A token 硬门 stated as a metric/relational atom to be reverse-measured from
/// the engine's produced geometry. The reverse-measured Kleene truth becomes the
/// gate verdict: `true` passes, `false` fails, and a blind-spot (`null`) fails
/// closed (`Blocked`) — a token constraint shuttle could not corroborate must not be
/// silently assumed satisfied.
#[derive(Debug, Clone)]
pub struct TokenGate {
    /// A stable, control-free name, unique among token gates.
    pub name: String,
    /// The (unevaluated) atom encoding the token constraint.
    pub atom: CoordAtom,
    /// Whether a non-pass verdict degrades to `Drop` (drop this token feature,
    /// keep the rest) rather than `Reject` (fail closed on the whole artifact).
    pub droppable: bool,
}

/// An a11y / 可区分性 hard gate. Reuses the constraint arm's non-degradable
/// [`A11yConstraint`]: an unknown measurement blocks (fails closed). a11y is a
/// red-line, so it is non-droppable by default.
#[derive(Debug, Clone)]
pub struct A11yGate {
    pub constraint: A11yConstraint,
    /// a11y is a hard red-line; leave `false` unless a specific check is a
    /// genuinely optional enhancement.
    pub droppable: bool,
}

/// A capability the engine must honor for the artifact to be admissible
/// (§4.2 能力协商). `advertised` is three-valued: the engine confirmed support
/// (`Some(true)` → pass), denied it (`Some(false)` → fail), or shuttle could not
/// negotiate it (`None` → blocked, fail closed). A droppable capability degrades
/// gracefully to `Drop`; a required one to `Reject`.
#[derive(Debug, Clone)]
pub struct CapabilityGate {
    pub name: String,
    /// Whether this capability is required at all. A non-required capability
    /// always passes (it is informational).
    pub required: bool,
    /// Whether the engine advertised support: `Some(true)`/`Some(false)`/`None`.
    pub advertised: Option<bool>,
    pub droppable: bool,
}

/// A claimed layer ↔ token/component mapping. `corroborating_atom`, when set, is
/// an index into the input atom order (`token_gates` first, then `extra_atoms`)
/// whose reverse-measured hold corroborates the mapping; when unset or dangling
/// the mapping is `null` (uncorroborated — surfaced, never assumed true).
#[derive(Debug, Clone)]
pub struct MappingClaim {
    pub layer_id: String,
    pub token_ref: String,
    pub corroborating_atom: Option<usize>,
}

/// The full structured input to a Form B assessment.
#[derive(Debug, Clone)]
pub struct AdapterInput {
    pub basis: AdapterBasis,
    /// The reverse-measurement frame built from the engine artifact's layer
    /// bounding boxes.
    pub frame: Frame,
    /// Token 硬门 atoms to reverse-measure and gate.
    pub token_gates: Vec<TokenGate>,
    /// Additional atoms to reverse-measure for evidence / mapping corroboration
    /// (not themselves gates).
    pub extra_atoms: Vec<CoordAtom>,
    /// a11y / 可区分性 hard gates.
    pub a11y_gates: Vec<A11yGate>,
    /// Engine capability negotiation gates.
    pub capability_gates: Vec<CapabilityGate>,
    /// Claimed layer ↔ token mappings.
    pub mappings: Vec<MappingClaim>,
    /// The engine's untrusted aesthetic self-report (soft, non-blocking).
    pub aesthetic: Option<AestheticScore>,
    /// `token:// -> numeric bound` resolver for metric token gates.
    pub thresholds: BTreeMap<String, f64>,
}

fn threshold_resolver(thresholds: &BTreeMap<String, f64>) -> impl Fn(&str) -> Option<f64> + '_ {
    move |t: &str| thresholds.get(t).copied()
}

/// Map a reverse-measured Kleene truth to a hard-gate verdict. A blind-spot
/// (`null`) fails closed — a token constraint shuttle could not corroborate is
/// `Blocked`, never a silent pass (§7/§20 「绝不瞎猜」).
fn holds_to_verdict(h: Holds) -> HardGateVerdict {
    match h {
        Holds::True => HardGateVerdict::Pass,
        Holds::False => HardGateVerdict::Fail,
        Holds::Null => HardGateVerdict::Blocked,
    }
}

fn a11y_to_verdict(v: A11yVerdict) -> HardGateVerdict {
    match v {
        A11yVerdict::Pass => HardGateVerdict::Pass,
        A11yVerdict::Fail => HardGateVerdict::Fail,
        A11yVerdict::Blocked => HardGateVerdict::Blocked,
    }
}

impl CapabilityGate {
    fn verdict(&self) -> HardGateVerdict {
        if !self.required {
            return HardGateVerdict::Pass;
        }
        match self.advertised {
            Some(true) => HardGateVerdict::Pass,
            Some(false) => HardGateVerdict::Fail,
            None => HardGateVerdict::Blocked,
        }
    }
}

/// Reverse-measure the engine artifact and gate it into an `adapter_receipt/v1`.
///
/// The receipt's disposition/review flag are computed by [`AdapterReceipt`]'s
/// fold from the hard gates + soft score, so the caller cannot manufacture an
/// `Adopt` over a failing artifact.
pub fn assess(input: AdapterInput) -> Result<AdapterReceipt, ClosedContractError> {
    let AdapterInput {
        basis,
        frame,
        token_gates,
        extra_atoms,
        a11y_gates,
        capability_gates,
        mappings,
        aesthetic,
        thresholds,
    } = input;

    let resolver = threshold_resolver(&thresholds);

    // Reverse-measure every token gate and every extra atom, preserving input
    // order so mapping claims can address them by index.
    let mut measured: Vec<CoordAtom> = Vec::with_capacity(token_gates.len() + extra_atoms.len());
    let mut hard_gates: Vec<HardGateRow> = Vec::new();

    for gate in &token_gates {
        let evaluated = geom::evaluate_atom(&gate.atom, &frame, &resolver)?;
        hard_gates.push(HardGateRow {
            name: gate.name.clone(),
            kind: HardGateKind::Token,
            verdict: holds_to_verdict(evaluated.holds),
            droppable: gate.droppable,
        });
        measured.push(evaluated);
    }
    for atom in &extra_atoms {
        measured.push(geom::evaluate_atom(atom, &frame, &resolver)?);
    }

    for gate in &a11y_gates {
        hard_gates.push(HardGateRow {
            name: gate.constraint.name.clone(),
            kind: HardGateKind::A11y,
            verdict: a11y_to_verdict(gate.constraint.check()),
            droppable: gate.droppable,
        });
    }

    for gate in &capability_gates {
        hard_gates.push(HardGateRow {
            name: gate.name.clone(),
            kind: HardGateKind::Capability,
            verdict: gate.verdict(),
            droppable: gate.droppable,
        });
    }

    // Ground each mapping's `verified` in a reverse-measured atom (by input
    // order index); an unset or dangling reference stays `null` — surfaced,
    // never assumed true.
    let mappings: Vec<ComponentMapping> = mappings
        .into_iter()
        .map(|claim| {
            let verified = claim
                .corroborating_atom
                .and_then(|i| measured.get(i))
                .map(|a| a.holds)
                .unwrap_or(Holds::Null);
            ComponentMapping {
                layer_id: claim.layer_id,
                token_ref: claim.token_ref,
                verified,
            }
        })
        .collect();

    AdapterReceipt::assemble(basis, measured, hard_gates, mappings, aesthetic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Interval;
    use shuttle_protocol::{
        AdapterDisposition, Axis, AxisKind, CoordAtomProposition, ProvenanceClass, RelationFamily,
        TimeQuantifier,
    };

    fn basis() -> AdapterBasis {
        AdapterBasis {
            surface_id: "screen:checkout".to_string(),
            engine_id: "v0".to_string(),
            engine_version: "2026.9".to_string(),
            artifact_digest: shuttle_protocol::anchor::Sha256Digest::parse(format!(
                "sha256:{}",
                "a".repeat(64)
            ))
            .unwrap(),
            evaluator_version: "shuttle-ui/0.1".to_string(),
        }
    }

    /// A unary metric atom "cta width >= token" on axis `w`.
    fn cta_min_width() -> CoordAtom {
        CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![Axis::new("w", AxisKind::Metric)],
            allen: None,
            threshold_ref: Some("token://size/cta-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap()
    }

    fn frame_with_cta(width: f64) -> Frame {
        let mut f = Frame::new(0.0);
        f.set("cta", "w", Interval::new(0.0, width).unwrap());
        f
    }

    fn thresholds() -> BTreeMap<String, f64> {
        let mut t = BTreeMap::new();
        t.insert("token://size/cta-min".to_string(), 44.0);
        t
    }

    fn base_input() -> AdapterInput {
        AdapterInput {
            basis: basis(),
            frame: frame_with_cta(44.0),
            token_gates: vec![],
            extra_atoms: vec![],
            a11y_gates: vec![],
            capability_gates: vec![],
            mappings: vec![],
            aesthetic: None,
            thresholds: thresholds(),
        }
    }

    #[test]
    fn adopts_clean_artifact() {
        let mut input = base_input();
        input.token_gates = vec![TokenGate {
            name: "token:cta-min-width".to_string(),
            atom: cta_min_width(),
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
        assert!(!r.needs_review);
        // Reverse-measurement stamps `reversed` provenance, never fabricated.
        assert_eq!(
            r.atoms[0].provenance.as_ref().unwrap().class,
            ProvenanceClass::Reversed
        );
    }

    #[test]
    fn token_false_rejects_when_nondroppable() {
        let mut input = base_input();
        input.frame = frame_with_cta(20.0); // below the 44 bound -> false
        input.token_gates = vec![TokenGate {
            name: "token:cta-min-width".to_string(),
            atom: cta_min_width(),
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
        assert!(!r.disposition.is_admissible());
        assert_eq!(r.hard_gates[0].verdict, HardGateVerdict::Fail);
    }

    #[test]
    fn token_droppable_fail_degrades_to_drop() {
        let mut input = base_input();
        input.frame = frame_with_cta(20.0);
        input.token_gates = vec![TokenGate {
            name: "token:cta-min-width".to_string(),
            atom: cta_min_width(),
            droppable: true,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Drop);
        assert!(r.disposition.is_admissible());
        assert!(r.needs_review);
    }

    #[test]
    fn token_blind_spot_blocks_and_rejects() {
        // No threshold resolves -> the metric is a blind spot -> Blocked ->
        // fail closed (绝不瞎猜: never a silent pass).
        let mut input = base_input();
        input.thresholds = BTreeMap::new(); // resolver misses
        input.token_gates = vec![TokenGate {
            name: "token:cta-min-width".to_string(),
            atom: cta_min_width(),
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.hard_gates[0].verdict, HardGateVerdict::Blocked);
        assert_eq!(r.disposition, AdapterDisposition::Reject);
    }

    #[test]
    fn a11y_blocked_rejects() {
        let mut input = base_input();
        input.a11y_gates = vec![A11yGate {
            constraint: A11yConstraint {
                name: "contrast".to_string(),
                measured: None, // unknown -> blocked -> fail closed
                required_min: 4.5,
            },
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
        let g = r
            .hard_gates
            .iter()
            .find(|g| g.kind == HardGateKind::A11y)
            .unwrap();
        assert_eq!(g.verdict, HardGateVerdict::Blocked);
    }

    #[test]
    fn a11y_pass_adopts() {
        let mut input = base_input();
        input.a11y_gates = vec![A11yGate {
            constraint: A11yConstraint {
                name: "contrast".to_string(),
                measured: Some(5.0),
                required_min: 4.5,
            },
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
    }

    #[test]
    fn capability_not_advertised_rejects() {
        let mut input = base_input();
        input.capability_gates = vec![CapabilityGate {
            name: "token-grid-snap".to_string(),
            required: true,
            advertised: Some(false), // engine denies support -> fail
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
    }

    #[test]
    fn capability_unnegotiated_blocks() {
        let mut input = base_input();
        input.capability_gates = vec![CapabilityGate {
            name: "token-grid-snap".to_string(),
            required: true,
            advertised: None, // could not negotiate -> blocked -> fail closed
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
        let g = r
            .hard_gates
            .iter()
            .find(|g| g.kind == HardGateKind::Capability)
            .unwrap();
        assert_eq!(g.verdict, HardGateVerdict::Blocked);
    }

    #[test]
    fn capability_droppable_degrades_gracefully() {
        let mut input = base_input();
        input.capability_gates = vec![CapabilityGate {
            name: "fancy-shadow".to_string(),
            required: true,
            advertised: Some(false),
            droppable: true, // graceful degrade: drop the feature, keep the rest
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Drop);
        assert!(r.disposition.is_admissible());
    }

    #[test]
    fn capability_not_required_passes() {
        let mut input = base_input();
        input.capability_gates = vec![CapabilityGate {
            name: "nice-to-have".to_string(),
            required: false,
            advertised: Some(false),
            droppable: false,
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
    }

    #[test]
    fn aesthetic_soft_flag_sidecars_but_admissible() {
        let mut input = base_input();
        input.aesthetic = Some(AestheticScore {
            score: 0.4,
            floor: 0.7,
            rationale_ref: "engine://aesthetic/weak-hierarchy".to_string(),
        });
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Sidecar);
        assert!(r.disposition.is_admissible());
        assert!(r.needs_review);
    }

    #[test]
    fn soft_flag_never_overrides_hard_reject() {
        // A hard a11y block plus a soft flag: hard wins, disposition is Reject.
        let mut input = base_input();
        input.a11y_gates = vec![A11yGate {
            constraint: A11yConstraint {
                name: "contrast".to_string(),
                measured: Some(1.0),
                required_min: 4.5,
            },
            droppable: false,
        }];
        input.aesthetic = Some(AestheticScore {
            score: 0.4,
            floor: 0.7,
            rationale_ref: "engine://aesthetic/weak-hierarchy".to_string(),
        });
        let r = assess(input).unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
    }

    #[test]
    fn mapping_verified_reflects_reverse_measured_atom() {
        let mut input = base_input();
        input.token_gates = vec![TokenGate {
            name: "token:cta-min-width".to_string(),
            atom: cta_min_width(),
            droppable: false,
        }];
        input.mappings = vec![
            MappingClaim {
                layer_id: "layer:cta".to_string(),
                token_ref: "token://component/button".to_string(),
                corroborating_atom: Some(0), // the token-gate atom (True here)
            },
            MappingClaim {
                layer_id: "layer:ghost".to_string(),
                token_ref: "token://component/spacer".to_string(),
                corroborating_atom: None, // uncorroborated -> null
            },
        ];
        let r = assess(input).unwrap();
        let cta = r
            .mappings
            .iter()
            .find(|m| m.layer_id == "layer:cta")
            .unwrap();
        assert_eq!(cta.verified, Holds::True);
        let ghost = r
            .mappings
            .iter()
            .find(|m| m.layer_id == "layer:ghost")
            .unwrap();
        assert_eq!(ghost.verified, Holds::Null);
        // An uncorroborated mapping is a blind spot only in the mapping, not an
        // atom; the receipt still adopts but the mapping records `null` honestly.
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
    }

    #[test]
    fn dangling_corroborating_index_is_null() {
        let mut input = base_input();
        input.mappings = vec![MappingClaim {
            layer_id: "layer:x".to_string(),
            token_ref: "token://t".to_string(),
            corroborating_atom: Some(99), // out of range -> null, not a panic
        }];
        let r = assess(input).unwrap();
        assert_eq!(r.mappings[0].verified, Holds::Null);
    }
}
