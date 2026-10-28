//! Atomic geometric UI proposition wire contract — `coord_atom/v1`.
//!
//! An atom states that, on one or more named axes and inside a coordinate
//! frame, an ordered tuple of UI elements stands in a geometric relation — an
//! Allen interval relation read through one of six UI-semantic families. The
//! atom's *identity* is the proposition; its *truth* is a separate,
//! three-valued (Kleene) evaluation that must carry grounding provenance.
//!
//! Never-guess discipline (§7/§20): a boolean truth with no grounding
//! provenance is illegal and is rejected at construction time. A *fabricated*
//! (ungrounded, model-asserted) provenance can never ground a boolean truth;
//! such a claim must degrade to [`Holds::Null`]. Geometry is at best a
//! sufficient-condition proxy — a `true` may be trusted, a `false` is a
//! blind-spot that must not be asserted as a refutation from geometry alone.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::{invalid, validate_text_id};
use serde::{Deserialize, Serialize};

pub const COORD_ATOM_SCHEMA: &str = "coord_atom/v1";

/// Three-valued Kleene truth. `Null` is a genuine unknown, never a soft false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holds {
    True,
    False,
    Null,
}

impl Holds {
    pub fn as_str(self) -> &'static str {
        match self {
            Holds::True => "true",
            Holds::False => "false",
            Holds::Null => "null",
        }
    }

    /// Kleene strong conjunction: `false` dominates, `null` is contagious over
    /// `true`.
    pub fn and(self, other: Holds) -> Holds {
        match (self, other) {
            (Holds::False, _) | (_, Holds::False) => Holds::False,
            (Holds::True, Holds::True) => Holds::True,
            _ => Holds::Null,
        }
    }

    /// Kleene strong disjunction: `true` dominates, `null` is contagious over
    /// `false`.
    pub fn or(self, other: Holds) -> Holds {
        match (self, other) {
            (Holds::True, _) | (_, Holds::True) => Holds::True,
            (Holds::False, Holds::False) => Holds::False,
            _ => Holds::Null,
        }
    }

    /// Kleene negation: `null` negates to `null`. Named `negate` to avoid
    /// clashing with the `std::ops::Not::not` trait method.
    pub fn negate(self) -> Holds {
        match self {
            Holds::True => Holds::False,
            Holds::False => Holds::True,
            Holds::Null => Holds::Null,
        }
    }

    /// Whether the truth is decided (`true`/`false`) as opposed to `null`.
    pub fn is_known(self) -> bool {
        !matches!(self, Holds::Null)
    }
}

/// Whether an axis carries a *metric* (continuously measurable magnitude, so
/// comparisons and thresholds are meaningful) or only an *ordinal* ordering
/// (position/precedence with no magnitude, e.g. a static font's weight index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisKind {
    Metric,
    Ordinal,
}

/// The temporal scope of the proposition over the interaction timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeQuantifier {
    /// Holds at one packaged instant / FSM state.
    At,
    /// Holds for every sampled/continuous instant (`∀t`).
    Forall,
    /// Holds for at least one instant (`∃t`).
    Exists,
}

/// Verification effort tier. Higher tiers cost more but close time blind-spots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationTier {
    /// Tier 0: FSM + packaged transition checking (~95% of UI).
    Fsm,
    /// Tier 1: keyframe sampling across a transition.
    Keyframe,
    /// Tier 2: continuous `∀t` checking.
    Continuous,
}

impl VerificationTier {
    pub fn level(self) -> u8 {
        match self {
            VerificationTier::Fsm => 0,
            VerificationTier::Keyframe => 1,
            VerificationTier::Continuous => 2,
        }
    }
}

/// The six UI-semantic families a geometric relation is read through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationFamily {
    Order,
    Adjacency,
    Alignment,
    Containment,
    Overlap,
    Metric,
}

impl RelationFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationFamily::Order => "order",
            RelationFamily::Adjacency => "adjacency",
            RelationFamily::Alignment => "alignment",
            RelationFamily::Containment => "containment",
            RelationFamily::Overlap => "overlap",
            RelationFamily::Metric => "metric",
        }
    }
}

/// Allen's thirteen interval relations. The `Metric` family carries none of
/// these — magnitude is not an interval relation — so a metric atom keeps
/// `allen = None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllenRelation {
    Before,
    After,
    Meets,
    MetBy,
    Overlaps,
    OverlappedBy,
    Starts,
    StartedBy,
    Finishes,
    FinishedBy,
    During,
    Contains,
    Equals,
}

impl AllenRelation {
    /// The UI-semantic family this interval relation reads as. Shared-endpoint
    /// relations read as alignment; nesting reads as containment.
    pub fn family(self) -> RelationFamily {
        match self {
            AllenRelation::Before | AllenRelation::After => RelationFamily::Order,
            AllenRelation::Meets | AllenRelation::MetBy => RelationFamily::Adjacency,
            AllenRelation::Overlaps | AllenRelation::OverlappedBy => RelationFamily::Overlap,
            AllenRelation::Starts
            | AllenRelation::StartedBy
            | AllenRelation::Finishes
            | AllenRelation::FinishedBy
            | AllenRelation::Equals => RelationFamily::Alignment,
            AllenRelation::During | AllenRelation::Contains => RelationFamily::Containment,
        }
    }
}

/// The five grounding provenance classes. Only these may back a boolean truth,
/// and `Fabricated` — an ungrounded model/planner assertion — may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceClass {
    /// Authored by a design system token / spec.
    DesignSystem,
    /// Reverse-engineered from a rendered artifact.
    Reversed,
    /// Model/planner asserted with no grounding evidence. Cannot ground truth.
    Fabricated,
    /// Derived from a font binary metric (the fourth provenance class).
    FontBinaryMetric,
    /// Derived from an external compliance spec, e.g. a11y (the fifth class).
    ExternalComplianceSpec,
}

impl ProvenanceClass {
    /// Whether this class may back a decided (`true`/`false`) truth. A
    /// fabricated assertion never can (§7/§20 — never guess).
    pub fn can_ground_boolean(self) -> bool {
        !matches!(self, ProvenanceClass::Fabricated)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProvenanceClass::DesignSystem => "design_system",
            ProvenanceClass::Reversed => "reversed",
            ProvenanceClass::Fabricated => "fabricated",
            ProvenanceClass::FontBinaryMetric => "font_binary_metric",
            ProvenanceClass::ExternalComplianceSpec => "external_compliance_spec",
        }
    }
}

/// The evidence backing a decided truth.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub class: ProvenanceClass,
    /// A reference to the grounding source (a `token://`, `spec://`, artifact
    /// digest, etc.). Non-empty and control-free.
    pub source_ref: String,
}

impl Provenance {
    pub fn new(class: ProvenanceClass, source_ref: impl Into<String>) -> Self {
        Self {
            class,
            source_ref: source_ref.into(),
        }
    }

    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("provenance.source_ref", &self.source_ref)
    }
}

/// A named axis and its measurement kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    pub name: String,
    pub kind: AxisKind,
}

impl Axis {
    pub fn new(name: impl Into<String>, kind: AxisKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// An atomic geometric UI proposition. Its `atom_id` is a content address of
/// the *proposition* (family/relation/args/axes/allen/threshold/frame/time),
/// deliberately excluding the mutable evaluation fields (`holds`,
/// `provenance`, `verification_tier`) so the same proposition re-evaluates to
/// the same identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordAtom {
    pub schema_version: String,
    pub atom_id: Sha256Digest,
    pub family: RelationFamily,
    /// The concrete predicate name inside the family, e.g. `left_of`,
    /// `aligned_top`, `contains`, `min_width`.
    pub relation: String,
    /// Ordered element references. Order is semantically significant.
    pub args: Vec<String>,
    /// The axes the relation is evaluated on, strictly sorted and unique.
    pub axes: Vec<Axis>,
    /// The underlying Allen interval relation, when the family is interval-based.
    /// Always `None` for the `Metric` family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allen: Option<AllenRelation>,
    /// A relativized threshold token (`token://...`) — thresholds are contracts,
    /// not coincidences. Required for a decided metric relation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_ref: Option<String>,
    pub frame: String,
    pub time_quantifier: TimeQuantifier,
    /// Evaluation outcome (three-valued).
    pub holds: Holds,
    /// Grounding evidence. Required whenever `holds` is decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    pub verification_tier: VerificationTier,
}

#[derive(Serialize)]
struct CoordAtomIdentity<'a> {
    family: RelationFamily,
    relation: &'a str,
    args: &'a [String],
    axes: &'a [Axis],
    #[serde(skip_serializing_if = "Option::is_none")]
    allen: Option<AllenRelation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold_ref: Option<&'a str>,
    frame: &'a str,
    time_quantifier: TimeQuantifier,
}

/// The fields required to state a proposition, before evaluation.
pub struct CoordAtomProposition {
    pub family: RelationFamily,
    pub relation: String,
    pub args: Vec<String>,
    pub axes: Vec<Axis>,
    pub allen: Option<AllenRelation>,
    pub threshold_ref: Option<String>,
    pub frame: String,
    pub time_quantifier: TimeQuantifier,
}

impl CoordAtom {
    /// State a proposition without a decided truth (`holds = Null`). This is the
    /// only truth an atom may carry before it is grounded.
    pub fn unevaluated(prop: CoordAtomProposition) -> Result<Self, ClosedContractError> {
        Self::build(prop, Holds::Null, None, VerificationTier::Fsm)
    }

    /// Evaluate a proposition to a decided truth with grounding provenance. A
    /// fabricated provenance is rejected — it cannot ground a boolean.
    pub fn evaluated(
        prop: CoordAtomProposition,
        holds: Holds,
        provenance: Provenance,
        verification_tier: VerificationTier,
    ) -> Result<Self, ClosedContractError> {
        Self::build(prop, holds, Some(provenance), verification_tier)
    }

    fn build(
        prop: CoordAtomProposition,
        holds: Holds,
        provenance: Option<Provenance>,
        verification_tier: VerificationTier,
    ) -> Result<Self, ClosedContractError> {
        let mut axes = prop.axes;
        axes.sort();
        let mut atom = Self {
            schema_version: COORD_ATOM_SCHEMA.to_string(),
            atom_id: placeholder_digest(),
            family: prop.family,
            relation: prop.relation,
            args: prop.args,
            axes,
            allen: prop.allen,
            threshold_ref: prop.threshold_ref,
            frame: prop.frame,
            time_quantifier: prop.time_quantifier,
            holds,
            provenance,
            verification_tier,
        };
        atom.validate_fields()?;
        atom.atom_id = atom.expected_atom_id()?;
        Ok(atom)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        let atom: Self = serde_json::from_slice(bytes)?;
        atom.validate()?;
        Ok(atom)
    }

    pub fn expected_atom_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.identity())
    }

    fn identity(&self) -> CoordAtomIdentity<'_> {
        CoordAtomIdentity {
            family: self.family,
            relation: &self.relation,
            args: &self.args,
            axes: &self.axes,
            allen: self.allen,
            threshold_ref: self.threshold_ref.as_deref(),
            frame: &self.frame,
            time_quantifier: self.time_quantifier,
        }
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != COORD_ATOM_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        self.validate_fields()?;
        if self.atom_id != self.expected_atom_id()? {
            return Err(invalid("atom_id does not match the canonical proposition"));
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        validate_text_id("relation", &self.relation)?;
        validate_text_id("frame", &self.frame)?;

        if self.args.is_empty() {
            return Err(invalid("args must not be empty"));
        }
        for arg in &self.args {
            validate_text_id("args", arg)?;
        }
        // Relational families need at least two operands; a metric may be unary
        // (a magnitude of one element against a threshold).
        if self.family != RelationFamily::Metric && self.args.len() < 2 {
            return Err(invalid(
                "relational families require at least two element args",
            ));
        }

        if self.axes.is_empty() {
            return Err(invalid("axes must not be empty"));
        }
        for axis in &self.axes {
            validate_text_id("axes.name", &axis.name)?;
        }
        if !self.axes.windows(2).all(|w| w[0] < w[1]) {
            return Err(invalid("axes must be strictly sorted and unique"));
        }

        // Allen/family consistency.
        match (self.family, self.allen) {
            (RelationFamily::Metric, Some(_)) => {
                return Err(invalid("metric family must not carry an Allen relation"));
            }
            (family, Some(allen)) if allen.family() != family => {
                return Err(invalid(
                    "allen relation is inconsistent with the declared family",
                ));
            }
            _ => {}
        }

        // Threshold tokenization: consistency is a contract, not a coincidence.
        if let Some(token) = &self.threshold_ref {
            validate_text_id("threshold_ref", token)?;
            if !token.starts_with("token://") {
                return Err(invalid("threshold_ref must be a token:// reference"));
            }
        }
        if self.family == RelationFamily::Metric
            && self.holds.is_known()
            && self.threshold_ref.is_none()
        {
            return Err(invalid(
                "a decided metric relation requires a relativized threshold_ref",
            ));
        }

        // Never-guess provenance discipline.
        match (&self.provenance, self.holds) {
            (None, holds) if holds.is_known() => {
                return Err(invalid(
                    "a decided truth requires grounding provenance (no-provenance truth is illegal)",
                ));
            }
            (Some(provenance), _) => {
                provenance.validate()?;
                if self.holds.is_known() && !provenance.class.can_ground_boolean() {
                    return Err(invalid(
                        "fabricated provenance cannot ground a boolean truth; it must degrade to null",
                    ));
                }
            }
            (None, _) => {}
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

    fn prop() -> CoordAtomProposition {
        CoordAtomProposition {
            family: RelationFamily::Order,
            relation: "left_of".to_string(),
            args: vec!["label".to_string(), "field".to_string()],
            axes: vec![Axis::new("x", AxisKind::Metric)],
            allen: Some(AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        }
    }

    #[test]
    fn kleene_tables() {
        assert_eq!(Holds::True.and(Holds::Null), Holds::Null);
        assert_eq!(Holds::False.and(Holds::Null), Holds::False);
        assert_eq!(Holds::True.or(Holds::Null), Holds::True);
        assert_eq!(Holds::False.or(Holds::Null), Holds::Null);
        assert_eq!(Holds::Null.negate(), Holds::Null);
        assert_eq!(Holds::True.negate(), Holds::False);
    }

    #[test]
    fn round_trip_and_stable_identity() {
        let atom = CoordAtom::evaluated(
            prop(),
            Holds::True,
            Provenance::new(ProvenanceClass::DesignSystem, "token://spacing/gap-md"),
            VerificationTier::Fsm,
        )
        .unwrap();
        let bytes = serde_json::to_vec(&atom).unwrap();
        let parsed = CoordAtom::parse(&bytes).unwrap();
        assert_eq!(parsed, atom);
        // Identity is independent of the evaluation fields.
        let other = CoordAtom::unevaluated(prop()).unwrap();
        assert_eq!(other.atom_id, atom.atom_id);
    }

    #[test]
    fn rejects_unknown_fields() {
        let atom = CoordAtom::unevaluated(prop()).unwrap();
        let mut value = serde_json::to_value(&atom).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(CoordAtom::parse(&bytes).is_err());
    }

    #[test]
    fn no_provenance_truth_is_illegal() {
        let err = CoordAtom::build(prop(), Holds::True, None, VerificationTier::Fsm);
        assert!(err.is_err());
    }

    #[test]
    fn fabricated_cannot_ground_truth() {
        let err = CoordAtom::evaluated(
            prop(),
            Holds::True,
            Provenance::new(ProvenanceClass::Fabricated, "planner://guess"),
            VerificationTier::Fsm,
        );
        assert!(err.is_err());
        // But a fabricated claim may be recorded as an unknown (audit trail).
        let ok = CoordAtom::build(
            prop(),
            Holds::Null,
            Some(Provenance::new(
                ProvenanceClass::Fabricated,
                "planner://guess",
            )),
            VerificationTier::Fsm,
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn metric_needs_threshold_when_decided() {
        let metric = CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![Axis::new("w", AxisKind::Metric)],
            allen: None,
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        };
        let err = CoordAtom::evaluated(
            metric,
            Holds::True,
            Provenance::new(ProvenanceClass::DesignSystem, "token://size/cta-min"),
            VerificationTier::Fsm,
        );
        assert!(err.is_err());
    }

    #[test]
    fn allen_family_mismatch_rejected() {
        let mut p = prop();
        p.family = RelationFamily::Containment;
        p.allen = Some(AllenRelation::Before); // Before is Order, not Containment.
        assert!(CoordAtom::unevaluated(p).is_err());
    }

    #[test]
    fn threshold_must_be_token() {
        let mut p = prop();
        p.family = RelationFamily::Metric;
        p.allen = None;
        p.threshold_ref = Some("42px".to_string());
        assert!(CoordAtom::unevaluated(p).is_err());
    }
}
