//! Form B external visual-engine adapter receipt — `adapter_receipt/v1`.
//!
//! Design §4.2 / §8-P7. shuttle never generates pixels (red-line #6). For a
//! high-fidelity design it delegates the pixel work to an external engine (v0 /
//! Galileo / Figma-AI class) and does exactly three things:
//!
//! 1. feeds structured input + token hard constraints to the engine boundary;
//! 2. takes the engine's produced artifact (a layer tree / figma-node export),
//!    **reverse-measures the atom holds** against it, and gates the product with
//!    the §3.3 hard/soft dichotomy;
//! 3. emits a **component-mapping receipt** (which layer realizes which design
//!    token / component) with a §3.3 disposition — adopt / sidecar / drop /
//!    reject — so the artifact is **never silently accepted or dropped**
//!    (capability negotiation + graceful degrade).
//!
//! This module is the *schema* for that receipt: pure, serde-closed,
//! digest-bound types with construction-time validation, mirroring
//! [`crate::ui_projection`]. The reverse-measurement and gate *logic* lives in
//! the `shuttle-ui` crate's adapter arm — this crate carries no geometry.
//!
//! Discipline (§3.3 hard/soft): a11y / 可区分性 / token 硬门 / capability
//! mismatch are **hard** → a non-droppable hard miss fails closed (`Reject`); a
//! droppable hard miss degrades to `Drop`. The engine's untrusted aesthetic
//! self-report is **soft** → it can flag for review (`Sidecar`) but never
//! blocks. A blind-spot (`null`) atom always surfaces on the review gate rather
//! than being fabricated into a pass (§7/§20 「绝不瞎猜」).

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::{invalid, validate_text_id};
use crate::ui_atom::{CoordAtom, Holds};
use serde::{Deserialize, Serialize};

pub const ADAPTER_RECEIPT_SCHEMA: &str = "adapter_receipt/v1";
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// The engine + produced artifact a receipt is bound to. The artifact digest is
/// a content address of the layer-tree export shuttle reverse-measured — shuttle never
/// owns the artifact bytes, it only fingerprints and reasons over them
/// (red-line #11).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdapterBasis {
    /// A stable id for the surface under judgment (a screen/component id).
    pub surface_id: String,
    /// The external engine backend id (e.g. `v0`, `galileo`, `figma-ai`).
    pub engine_id: String,
    /// The engine's self-reported version, for reproducibility.
    pub engine_version: String,
    /// Content digest of the engine's produced artifact (layer tree export).
    pub artifact_digest: Sha256Digest,
    /// The shuttle-ui evaluator version that reverse-measured the artifact.
    pub evaluator_version: String,
}

impl AdapterBasis {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("basis.surface_id", &self.surface_id)?;
        validate_text_id("basis.engine_id", &self.engine_id)?;
        validate_text_id("basis.engine_version", &self.engine_version)?;
        validate_text_id("basis.evaluator_version", &self.evaluator_version)
    }
}

/// Which hard gate a row belongs to. All three are §3.3 *hard* concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardGateKind {
    /// Accessibility / 可区分性 (contrast, tap-target size, …).
    A11y,
    /// A token 硬门 reverse-measured from the artifact geometry (a metric atom).
    Token,
    /// A capability the engine did not advertise it could honor (negotiation).
    Capability,
}

impl HardGateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HardGateKind::A11y => "a11y",
            HardGateKind::Token => "token",
            HardGateKind::Capability => "capability",
        }
    }
}

/// The three-valued outcome of a hard gate. Unlike a geometric proxy there is no
/// "trust the false" degradation — `Blocked` (could not decide) fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardGateVerdict {
    Pass,
    Fail,
    /// Could not decide (blind spot / unmeasured): fails closed.
    Blocked,
}

impl HardGateVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            HardGateVerdict::Pass => "pass",
            HardGateVerdict::Fail => "fail",
            HardGateVerdict::Blocked => "blocked",
        }
    }

    pub fn is_pass(self) -> bool {
        matches!(self, HardGateVerdict::Pass)
    }
}

/// One hard-gate verdict over the produced artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HardGateRow {
    /// A stable, control-free name for the gate (unique within its kind).
    pub name: String,
    pub kind: HardGateKind,
    pub verdict: HardGateVerdict,
    /// Whether a non-`Pass` verdict degrades to `Drop` (drop this piece, keep
    /// the rest) rather than `Reject` (fail closed on the whole artifact).
    pub droppable: bool,
}

impl HardGateRow {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("hard_gate.name", &self.name)
    }

    fn key(&self) -> (HardGateKind, &str) {
        (self.kind, self.name.as_str())
    }
}

/// The engine's untrusted aesthetic self-report — a §3.3 *soft* score. It never
/// blocks adoption; a score below its declared floor only flags the artifact for
/// review (a `Sidecar` disposition).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AestheticScore {
    /// A soft score in `[0, 1]`. Never gates adoption.
    pub score: f64,
    /// The soft floor in `[0, 1]` below which the artifact is flagged.
    pub floor: f64,
    /// A control-free reference to the scoring rationale.
    pub rationale_ref: String,
}

impl AestheticScore {
    fn validate(&self) -> Result<(), ClosedContractError> {
        for (name, v) in [
            ("aesthetic.score", self.score),
            ("aesthetic.floor", self.floor),
        ] {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(invalid(format!("{name} must be a finite value in [0, 1]")));
            }
        }
        validate_text_id("aesthetic.rationale_ref", &self.rationale_ref)
    }

    /// Whether the soft score falls below its floor (flag, do not block).
    pub fn flagged(&self) -> bool {
        self.score < self.floor
    }
}

/// One layer's mapping back to the design token / component it realizes — the
/// "组件映射回执". `verified` rolls up the reverse-measured atoms that reference
/// the layer (three-valued; `null` when geometry could not corroborate).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComponentMapping {
    /// The engine layer / figma-node id.
    pub layer_id: String,
    /// The design token or component the layer is claimed to realize.
    pub token_ref: String,
    /// Whether the layer's geometry corroborated the mapping.
    pub verified: Holds,
}

impl ComponentMapping {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("mapping.layer_id", &self.layer_id)?;
        validate_text_id("mapping.token_ref", &self.token_ref)
    }
}

/// The §3.3 disposition of the produced artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterDisposition {
    /// Every hard gate passed and no soft flag — adopt into the pipeline.
    Adopt,
    /// Adopt but flag for review (a soft aesthetic concern only).
    Sidecar,
    /// A recoverable hard miss confined to droppable gates — drop those, keep
    /// the rest, degrade gracefully.
    Drop,
    /// A non-droppable hard miss (a11y / token 硬门 / capability) — fail closed.
    Reject,
}

impl AdapterDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterDisposition::Adopt => "adopt",
            AdapterDisposition::Sidecar => "sidecar",
            AdapterDisposition::Drop => "drop",
            AdapterDisposition::Reject => "reject",
        }
    }

    /// Whether the artifact may be used at all (`Reject` is the only refusal).
    pub fn is_admissible(self) -> bool {
        !matches!(self, AdapterDisposition::Reject)
    }
}

/// A digest-bound Form B adapter receipt: the reverse-measured atoms, the
/// hard-gate verdicts, the component mapping, an optional soft aesthetic score,
/// and the computed §3.3 disposition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdapterReceipt {
    pub schema_version: String,
    pub receipt_id: Sha256Digest,
    pub basis: AdapterBasis,
    /// Reverse-measured atoms, strictly sorted by `atom_id` and unique.
    pub atoms: Vec<CoordAtom>,
    /// Hard-gate verdicts, strictly sorted by `(kind, name)` and unique.
    pub hard_gates: Vec<HardGateRow>,
    /// Layer ↔ token/component mappings, strictly sorted by `layer_id`, unique.
    pub mappings: Vec<ComponentMapping>,
    /// The soft aesthetic self-report, if the engine supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aesthetic: Option<AestheticScore>,
    /// The computed §3.3 disposition (a fold over the gates + soft score).
    pub disposition: AdapterDisposition,
    /// Whether the receipt must surface on the human/model review gate.
    pub needs_review: bool,
}

#[derive(Serialize)]
struct AdapterReceiptIdentity<'a> {
    basis: &'a AdapterBasis,
    atom_ids: Vec<&'a Sha256Digest>,
    hard_gates: &'a [HardGateRow],
    mappings: &'a [ComponentMapping],
    #[serde(skip_serializing_if = "Option::is_none")]
    aesthetic: Option<&'a AestheticScore>,
}

impl AdapterReceipt {
    /// Assemble a receipt, computing the disposition/review flag from the gates
    /// and the soft score. Fails closed on ill-formed members.
    pub fn assemble(
        basis: AdapterBasis,
        mut atoms: Vec<CoordAtom>,
        mut hard_gates: Vec<HardGateRow>,
        mut mappings: Vec<ComponentMapping>,
        aesthetic: Option<AestheticScore>,
    ) -> Result<Self, ClosedContractError> {
        atoms.sort_by(|a, b| a.atom_id.cmp(&b.atom_id));
        hard_gates.sort_by(|a, b| a.key().cmp(&b.key()));
        mappings.sort_by(|a, b| a.layer_id.cmp(&b.layer_id));

        for atom in &atoms {
            atom.validate()?;
        }
        for gate in &hard_gates {
            gate.validate()?;
        }
        for mapping in &mappings {
            mapping.validate()?;
        }
        if let Some(score) = &aesthetic {
            score.validate()?;
        }

        let (disposition, needs_review) = Self::fold(&atoms, &hard_gates, aesthetic.as_ref());

        let mut receipt = Self {
            schema_version: ADAPTER_RECEIPT_SCHEMA.to_string(),
            receipt_id: placeholder_digest(),
            basis,
            atoms,
            hard_gates,
            mappings,
            aesthetic,
            disposition,
            needs_review,
        };
        receipt.validate_fields()?;
        receipt.receipt_id = receipt.expected_receipt_id()?;
        Ok(receipt)
    }

    fn fold(
        atoms: &[CoordAtom],
        hard_gates: &[HardGateRow],
        aesthetic: Option<&AestheticScore>,
    ) -> (AdapterDisposition, bool) {
        let mut hard_fail_nondrop = false;
        let mut hard_fail_drop = false;
        for gate in hard_gates {
            if !gate.verdict.is_pass() {
                if gate.droppable {
                    hard_fail_drop = true;
                } else {
                    hard_fail_nondrop = true;
                }
            }
        }
        let soft_flag = aesthetic.map(AestheticScore::flagged).unwrap_or(false);

        let disposition = if hard_fail_nondrop {
            AdapterDisposition::Reject
        } else if hard_fail_drop {
            AdapterDisposition::Drop
        } else if soft_flag {
            AdapterDisposition::Sidecar
        } else {
            AdapterDisposition::Adopt
        };

        // Anything but a clean adopt reaches the gate; a blind-spot atom always
        // surfaces even under an otherwise-clean adopt (绝不瞎猜).
        let mut needs_review = disposition != AdapterDisposition::Adopt;
        needs_review |= atoms.iter().any(|a| a.holds == Holds::Null);
        (disposition, needs_review)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 8MiB input limit"));
        }
        let receipt: Self = serde_json::from_slice(bytes)?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn expected_receipt_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.identity())
    }

    fn identity(&self) -> AdapterReceiptIdentity<'_> {
        AdapterReceiptIdentity {
            basis: &self.basis,
            atom_ids: self.atoms.iter().map(|a| &a.atom_id).collect(),
            hard_gates: &self.hard_gates,
            mappings: &self.mappings,
            aesthetic: self.aesthetic.as_ref(),
        }
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != ADAPTER_RECEIPT_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        self.validate_fields()?;
        let (disposition, needs_review) =
            Self::fold(&self.atoms, &self.hard_gates, self.aesthetic.as_ref());
        if disposition != self.disposition || needs_review != self.needs_review {
            return Err(invalid(
                "stored disposition/needs_review does not match the recomputed fold",
            ));
        }
        if self.receipt_id != self.expected_receipt_id()? {
            return Err(invalid("receipt_id does not match the canonical bundle"));
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        self.basis.validate()?;
        for atom in &self.atoms {
            atom.validate()?;
        }
        for gate in &self.hard_gates {
            gate.validate()?;
        }
        for mapping in &self.mappings {
            mapping.validate()?;
        }
        if let Some(score) = &self.aesthetic {
            score.validate()?;
        }
        if !self.atoms.windows(2).all(|w| w[0].atom_id < w[1].atom_id) {
            return Err(invalid(
                "atoms must be strictly sorted by atom_id and unique",
            ));
        }
        if !self.hard_gates.windows(2).all(|w| w[0].key() < w[1].key()) {
            return Err(invalid(
                "hard_gates must be strictly sorted by (kind, name) and unique",
            ));
        }
        if !self
            .mappings
            .windows(2)
            .all(|w| w[0].layer_id < w[1].layer_id)
        {
            return Err(invalid(
                "mappings must be strictly sorted by layer_id and unique",
            ));
        }
        Ok(())
    }
}

fn placeholder_digest() -> Sha256Digest {
    Sha256Digest::parse(format!("sha256:{}", "0".repeat(64)))
        .expect("fixed placeholder digest is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_atom::{
        Axis, AxisKind, CoordAtomProposition, Provenance, ProvenanceClass, RelationFamily,
        TimeQuantifier, VerificationTier,
    };

    fn basis() -> AdapterBasis {
        AdapterBasis {
            surface_id: "screen:checkout".to_string(),
            engine_id: "v0".to_string(),
            engine_version: "2026.9".to_string(),
            artifact_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            evaluator_version: "shuttle-ui/0.1".to_string(),
        }
    }

    fn metric_atom(relation: &str, holds: Holds) -> CoordAtom {
        let prop = CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: relation.to_string(),
            args: vec!["cta".to_string()],
            axes: vec![Axis::new("x", AxisKind::Metric)],
            allen: None,
            threshold_ref: Some("token://size/min-tap".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        };
        if holds.is_known() {
            CoordAtom::evaluated(
                prop,
                holds,
                Provenance::new(ProvenanceClass::Reversed, "reversed://frame-geometry"),
                VerificationTier::Fsm,
            )
            .unwrap()
        } else {
            CoordAtom::unevaluated(prop).unwrap()
        }
    }

    fn gate(
        name: &str,
        kind: HardGateKind,
        verdict: HardGateVerdict,
        droppable: bool,
    ) -> HardGateRow {
        HardGateRow {
            name: name.to_string(),
            kind,
            verdict,
            droppable,
        }
    }

    fn mapping(layer: &str, holds: Holds) -> ComponentMapping {
        ComponentMapping {
            layer_id: layer.to_string(),
            token_ref: "token://component/button".to_string(),
            verified: holds,
        }
    }

    #[test]
    fn adopt_when_all_pass() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![metric_atom("min_width", Holds::True)],
            vec![gate(
                "a11y:tap",
                HardGateKind::A11y,
                HardGateVerdict::Pass,
                false,
            )],
            vec![mapping("layer:cta", Holds::True)],
            None,
        )
        .unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
        assert!(!r.needs_review);
        assert!(r.disposition.is_admissible());
    }

    #[test]
    fn sidecar_on_soft_flag() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![metric_atom("min_width", Holds::True)],
            vec![gate(
                "a11y:tap",
                HardGateKind::A11y,
                HardGateVerdict::Pass,
                false,
            )],
            vec![],
            Some(AestheticScore {
                score: 0.4,
                floor: 0.7,
                rationale_ref: "engine://aesthetic/low-contrast-hierarchy".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Sidecar);
        assert!(r.needs_review);
        assert!(r.disposition.is_admissible());
    }

    #[test]
    fn drop_on_droppable_hard_fail() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![gate(
                "token:accent-shadow",
                HardGateKind::Token,
                HardGateVerdict::Fail,
                true,
            )],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Drop);
        assert!(r.needs_review);
    }

    #[test]
    fn reject_on_nondroppable_a11y_fail() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![gate(
                "a11y:contrast",
                HardGateKind::A11y,
                HardGateVerdict::Fail,
                false,
            )],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
        assert!(!r.disposition.is_admissible());
    }

    #[test]
    fn reject_on_blocked_capability() {
        // Capability negotiation: the engine could not honor a required kind, so
        // the gate is Blocked (fail-closed) — never silently accepted.
        let r = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![gate(
                "constraint:token-grid",
                HardGateKind::Capability,
                HardGateVerdict::Blocked,
                false,
            )],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(r.disposition, AdapterDisposition::Reject);
    }

    #[test]
    fn blind_spot_atom_surfaces_under_adopt() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![metric_atom("min_width", Holds::Null)],
            vec![],
            vec![],
            None,
        )
        .unwrap();
        // No hard gate failed, but the unknown atom must reach the review gate.
        assert_eq!(r.disposition, AdapterDisposition::Adopt);
        assert!(r.needs_review);
    }

    #[test]
    fn round_trip_and_tamper_detection() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![metric_atom("min_width", Holds::True)],
            vec![gate(
                "a11y:tap",
                HardGateKind::A11y,
                HardGateVerdict::Pass,
                false,
            )],
            vec![mapping("layer:cta", Holds::True)],
            None,
        )
        .unwrap();
        let bytes = serde_json::to_vec(&r).unwrap();
        assert_eq!(AdapterReceipt::parse(&bytes).unwrap(), r);

        // Tamper the disposition; the recompute must reject it.
        let mut value = serde_json::to_value(&r).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("disposition".to_string(), serde_json::json!("reject"));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(AdapterReceipt::parse(&bytes).is_err());
    }

    #[test]
    fn tamper_hard_gate_verdict_breaks_digest() {
        let r = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![gate(
                "a11y:tap",
                HardGateKind::A11y,
                HardGateVerdict::Pass,
                false,
            )],
            vec![],
            None,
        )
        .unwrap();
        let mut value = serde_json::to_value(&r).unwrap();
        value["hard_gates"][0]["verdict"] = serde_json::json!("fail");
        let bytes = serde_json::to_vec(&value).unwrap();
        // Flipping a gate verdict changes both the fold and the digest.
        assert!(AdapterReceipt::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let r = AdapterReceipt::assemble(basis(), vec![], vec![], vec![], None).unwrap();
        let mut value = serde_json::to_value(&r).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(AdapterReceipt::parse(&bytes).is_err());
    }

    #[test]
    fn duplicate_hard_gate_key_is_rejected() {
        let err = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![
                gate("a11y:tap", HardGateKind::A11y, HardGateVerdict::Pass, false),
                gate("a11y:tap", HardGateKind::A11y, HardGateVerdict::Fail, false),
            ],
            vec![],
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn out_of_range_aesthetic_is_rejected() {
        let err = AdapterReceipt::assemble(
            basis(),
            vec![],
            vec![],
            vec![],
            Some(AestheticScore {
                score: 1.5,
                floor: 0.5,
                rationale_ref: "engine://x".to_string(),
            }),
        );
        assert!(err.is_err());
    }
}
