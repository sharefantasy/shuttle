//! Form C machine-readable canvas underlay — `canvas_underlay/v1`.
//!
//! Design §4.3 / §8-P8. A "Figma 类型的画板" is reached by **overlaying the
//! Form A SVG skeleton with the Form B high-fidelity layers**. The canvas itself
//! is a *render artifact*, never a truth source — the truth stays in the
//! `coord_atom` propositions (red-line #7). shuttle does not build the canvas UI
//! (that is the host/front-end's job) and never owns the engine's coordinate
//! space or the pixels (red-line #6 / #11). shuttle only provides the canvas's
//! **machine-readable 底稿**: the assignment (赋值 — node geometry), the
//! propositions (命题 — atoms), and their provenance, so a front-end can render
//! an interactive canvas from it.
//!
//! This module is the *schema* for that underlay: pure, serde-closed,
//! digest-bound types with construction-time validation, mirroring
//! [`crate::ui_projection`] and [`crate::ui_adapter`]. The skeleton→fidelity
//! composition *logic* lives in the `shuttle-ui` crate's canvas arm — this crate
//! carries no geometry engine.
//!
//! Composition discipline (§4.3 landing order「先 A → 再 B → 画板 C 最后」):
//!
//! * The **skeleton layer** (Form A) is the geometric backbone: every canvas
//!   node carries the constraint-satisfying box assignment the projector found
//!   admissible. Geometry is a render input, not a truth.
//! * The **fidelity layer** (Form B), if composed, binds to the exact
//!   [`crate::AdapterReceipt`] by its `receipt_id` and disposition. A node may
//!   only carry a fidelity mapping (`token_ref` + `fidelity_verified`) when the
//!   fidelity layer is **admissible** (not `Reject`).
//! * The underlay **never silently accepts or drops** a fidelity layer
//!   (§4.2「绝不静默失败」): an inadmissible (`Reject`) fidelity layer degrades
//!   the readiness to `Blocked` and forces the review gate; a `Drop` disposition
//!   degrades to `Degraded`; a blind-spot (`null`) atom always surfaces on the
//!   gate rather than being fabricated into a pass (§7/§20「绝不瞎猜」).

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::{invalid, validate_text_id};
use crate::ui_adapter::AdapterDisposition;
use crate::ui_atom::{CoordAtom, Holds};
use serde::{Deserialize, Serialize};

pub const CANVAS_UNDERLAY_SCHEMA: &str = "canvas_underlay/v1";
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// The frames of reference the underlay composes: the Form A skeleton artifact
/// (an SVG export) and, optionally, the Form B fidelity artifact (a layer tree).
/// Both are content-addressed — shuttle fingerprints and reasons over them, it never
/// owns the artifact bytes (red-line #11).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanvasBasis {
    /// A stable id for the surface under composition (a screen/component id).
    pub surface_id: String,
    /// Content digest of the Form A skeleton artifact (the SVG the nodes were
    /// laid from).
    pub skeleton_digest: Sha256Digest,
    /// Content digest of the Form B fidelity artifact, present iff a fidelity
    /// layer was composed (whatever its disposition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity_digest: Option<Sha256Digest>,
    /// The shuttle-ui evaluator version that composed the underlay.
    pub evaluator_version: String,
}

impl CanvasBasis {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("basis.surface_id", &self.surface_id)?;
        validate_text_id("basis.evaluator_version", &self.evaluator_version)
    }
}

/// A half-open bounding box assignment on the `x` (horizontal) and `y`
/// (vertical) axes — the 赋值 the front-end renders. It is a render input, not a
/// proposition; the truth of a node's placement lives in the `coord_atom`s.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CanvasBox {
    pub x_start: f64,
    pub x_end: f64,
    pub y_start: f64,
    pub y_end: f64,
}

impl CanvasBox {
    fn validate(&self) -> Result<(), ClosedContractError> {
        for (name, v) in [
            ("bounds.x_start", self.x_start),
            ("bounds.x_end", self.x_end),
            ("bounds.y_start", self.y_start),
            ("bounds.y_end", self.y_end),
        ] {
            if !v.is_finite() {
                return Err(invalid(format!("{name} must be a finite number")));
            }
        }
        if self.x_end < self.x_start || self.y_end < self.y_start {
            return Err(invalid(
                "bounds must have end >= start on both axes (half-open box)",
            ));
        }
        Ok(())
    }
}

/// One positioned node on the canvas: its skeleton geometry (always present),
/// optional carried render hints, and — only when an admissible fidelity layer
/// realized it — the design token it maps to plus the three-valued
/// reverse-measurement of that mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CanvasNode {
    /// The skeleton element id (stable, unique, id-sorted within the underlay).
    pub node_id: String,
    /// The skeleton box assignment (赋值).
    pub bounds: CanvasBox,
    /// Optional ARIA/semantic role carried from the skeleton.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Optional visible text carried from the skeleton.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The design token / component the fidelity layer mapped this node to, when
    /// (and only when) an admissible fidelity layer is composed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_ref: Option<String>,
    /// Whether the fidelity layer's reverse-measurement corroborated the mapping
    /// (three-valued; `null` for a blind spot). Present iff `token_ref` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity_verified: Option<Holds>,
}

impl CanvasNode {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("node.node_id", &self.node_id)?;
        self.bounds.validate()?;
        if let Some(role) = &self.role {
            validate_text_id("node.role", role)?;
        }
        // Text may be empty/whitespace as a render hint, but must be control-free
        // and bounded; reuse the id validator only when non-empty.
        if let Some(text) = &self.text {
            if text.len() > 4096 || text.chars().any(char::is_control) {
                return Err(invalid(
                    "node.text must be control-free and at most 4096 bytes",
                ));
            }
        }
        if let Some(token_ref) = &self.token_ref {
            validate_text_id("node.token_ref", token_ref)?;
        }
        // A fidelity mapping is all-or-nothing: token_ref and its measurement
        // travel together.
        if self.token_ref.is_some() != self.fidelity_verified.is_some() {
            return Err(invalid(
                "node.token_ref and node.fidelity_verified must both be present or both absent",
            ));
        }
        Ok(())
    }

    /// Whether this node carries a fidelity overlay.
    pub fn has_fidelity(&self) -> bool {
        self.token_ref.is_some()
    }
}

/// The Form B fidelity layer the underlay bound to, if any: a content-addressed
/// link back to the exact [`crate::AdapterReceipt`] and its §3.3 disposition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanvasFidelity {
    /// The Form B receipt this underlay overlaid (content address of that
    /// receipt's canonical bundle).
    pub receipt_id: Sha256Digest,
    /// The receipt's computed §3.3 disposition.
    pub disposition: AdapterDisposition,
    /// Whether the receipt itself demanded review.
    pub needs_review: bool,
}

/// The rolled-up composition state of the underlay — what the front-end may
/// render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanvasReadiness {
    /// No fidelity layer was composed: the canvas renders the structural
    /// skeleton only.
    SkeletonOnly,
    /// Skeleton + an admissible fidelity layer fully overlaid (`Adopt` /
    /// `Sidecar`).
    Composited,
    /// Skeleton + a partially-overlaid fidelity layer (`Drop`): some fidelity
    /// pieces were dropped, the canvas degrades gracefully.
    Degraded,
    /// The fidelity layer was inadmissible (`Reject`): the overlay is refused and
    /// the canvas must render skeleton-only until the fidelity artifact is fixed.
    Blocked,
}

impl CanvasReadiness {
    pub fn as_str(self) -> &'static str {
        match self {
            CanvasReadiness::SkeletonOnly => "skeleton_only",
            CanvasReadiness::Composited => "composited",
            CanvasReadiness::Degraded => "degraded",
            CanvasReadiness::Blocked => "blocked",
        }
    }

    /// Whether a fidelity overlay was admitted onto the canvas at all.
    pub fn fidelity_admitted(self) -> bool {
        matches!(
            self,
            CanvasReadiness::Composited | CanvasReadiness::Degraded
        )
    }
}

/// A digest-bound Form C canvas underlay: the skeleton nodes (赋值), the atom
/// bundle (命题, carrying provenance), the optional fidelity binding, and the
/// computed readiness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CanvasUnderlay {
    pub schema_version: String,
    pub underlay_id: Sha256Digest,
    pub basis: CanvasBasis,
    /// Canvas nodes, strictly sorted by `node_id` and unique.
    pub nodes: Vec<CanvasNode>,
    /// The proposition bundle, strictly sorted by `atom_id` and unique. These —
    /// not the geometry — are the sole truth source.
    pub atoms: Vec<CoordAtom>,
    /// The composed fidelity layer, present iff `basis.fidelity_digest` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<CanvasFidelity>,
    /// The computed composition state (a fold over the fidelity disposition).
    pub readiness: CanvasReadiness,
    /// Whether the underlay must surface on the human/model review gate.
    pub needs_review: bool,
}

#[derive(Serialize)]
struct CanvasUnderlayIdentity<'a> {
    basis: &'a CanvasBasis,
    nodes: &'a [CanvasNode],
    atom_ids: Vec<&'a Sha256Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fidelity: Option<&'a CanvasFidelity>,
}

impl CanvasUnderlay {
    /// Assemble an underlay, computing the readiness/review flag from the
    /// fidelity disposition and the atom truths. Fails closed on ill-formed
    /// members or an inconsistent skeleton/fidelity composition.
    pub fn assemble(
        basis: CanvasBasis,
        mut nodes: Vec<CanvasNode>,
        mut atoms: Vec<CoordAtom>,
        fidelity: Option<CanvasFidelity>,
    ) -> Result<Self, ClosedContractError> {
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        atoms.sort_by(|a, b| a.atom_id.cmp(&b.atom_id));

        for node in &nodes {
            node.validate()?;
        }
        for atom in &atoms {
            atom.validate()?;
        }

        Self::validate_composition(&basis, &nodes, fidelity.as_ref())?;

        let (readiness, needs_review) = Self::fold(&atoms, fidelity.as_ref());

        let mut underlay = Self {
            schema_version: CANVAS_UNDERLAY_SCHEMA.to_string(),
            underlay_id: placeholder_digest(),
            basis,
            nodes,
            atoms,
            fidelity,
            readiness,
            needs_review,
        };
        underlay.validate_fields()?;
        underlay.underlay_id = underlay.expected_underlay_id()?;
        Ok(underlay)
    }

    fn fold(atoms: &[CoordAtom], fidelity: Option<&CanvasFidelity>) -> (CanvasReadiness, bool) {
        let readiness = match fidelity {
            None => CanvasReadiness::SkeletonOnly,
            Some(f) => match f.disposition {
                AdapterDisposition::Adopt | AdapterDisposition::Sidecar => {
                    CanvasReadiness::Composited
                }
                AdapterDisposition::Drop => CanvasReadiness::Degraded,
                AdapterDisposition::Reject => CanvasReadiness::Blocked,
            },
        };

        // A degraded/blocked composition, a fidelity layer that itself wanted
        // review, or any blind-spot atom (绝不瞎猜) must reach the gate.
        let mut needs_review = matches!(
            readiness,
            CanvasReadiness::Degraded | CanvasReadiness::Blocked
        );
        needs_review |= fidelity.map(|f| f.needs_review).unwrap_or(false);
        needs_review |= atoms.iter().any(|a| a.holds == Holds::Null);
        (readiness, needs_review)
    }

    /// Enforce the skeleton↔fidelity composition invariants (§4.3 / §4.2
    /// 绝不静默失败): fidelity presence agrees with the basis, and node fidelity
    /// overlays only exist when the fidelity layer is admissible.
    fn validate_composition(
        basis: &CanvasBasis,
        nodes: &[CanvasNode],
        fidelity: Option<&CanvasFidelity>,
    ) -> Result<(), ClosedContractError> {
        if basis.fidelity_digest.is_some() != fidelity.is_some() {
            return Err(invalid(
                "basis.fidelity_digest must be present iff a fidelity layer is composed",
            ));
        }
        let admissible = fidelity
            .map(|f| f.disposition.is_admissible())
            .unwrap_or(false);
        for node in nodes {
            if node.has_fidelity() && !admissible {
                return Err(invalid(
                    "a node carries a fidelity mapping but no admissible fidelity layer is composed",
                ));
            }
        }
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 8MiB input limit"));
        }
        let underlay: Self = serde_json::from_slice(bytes)?;
        underlay.validate()?;
        Ok(underlay)
    }

    pub fn expected_underlay_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.identity())
    }

    fn identity(&self) -> CanvasUnderlayIdentity<'_> {
        CanvasUnderlayIdentity {
            basis: &self.basis,
            nodes: &self.nodes,
            atom_ids: self.atoms.iter().map(|a| &a.atom_id).collect(),
            fidelity: self.fidelity.as_ref(),
        }
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != CANVAS_UNDERLAY_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        self.validate_fields()?;
        // Recompute the fold and confirm the stored state matches — a tampered
        // readiness/review flag is rejected.
        let (readiness, needs_review) = Self::fold(&self.atoms, self.fidelity.as_ref());
        if readiness != self.readiness || needs_review != self.needs_review {
            return Err(invalid(
                "stored readiness/needs_review does not match the recomputed fold",
            ));
        }
        if self.underlay_id != self.expected_underlay_id()? {
            return Err(invalid("underlay_id does not match the canonical bundle"));
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        self.basis.validate()?;
        for node in &self.nodes {
            node.validate()?;
        }
        for atom in &self.atoms {
            atom.validate()?;
        }
        if !self.nodes.windows(2).all(|w| w[0].node_id < w[1].node_id) {
            return Err(invalid(
                "nodes must be strictly sorted by node_id and unique",
            ));
        }
        if !self.atoms.windows(2).all(|w| w[0].atom_id < w[1].atom_id) {
            return Err(invalid(
                "atoms must be strictly sorted by atom_id and unique",
            ));
        }
        Self::validate_composition(&self.basis, &self.nodes, self.fidelity.as_ref())?;
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

    fn digest(byte: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn basis(with_fidelity: bool) -> CanvasBasis {
        CanvasBasis {
            surface_id: "screen:checkout".to_string(),
            skeleton_digest: digest('a'),
            fidelity_digest: with_fidelity.then(|| digest('b')),
            evaluator_version: "shuttle-ui/0.1".to_string(),
        }
    }

    fn node(id: &str, mapped: Option<Holds>) -> CanvasNode {
        CanvasNode {
            node_id: id.to_string(),
            bounds: CanvasBox {
                x_start: 0.0,
                x_end: 100.0,
                y_start: 0.0,
                y_end: 40.0,
            },
            role: Some("button".to_string()),
            text: Some("Buy".to_string()),
            token_ref: mapped.map(|_| "token://component/button".to_string()),
            fidelity_verified: mapped,
        }
    }

    fn atom(relation: &str, holds: Holds) -> CoordAtom {
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

    fn fidelity(disposition: AdapterDisposition, needs_review: bool) -> CanvasFidelity {
        CanvasFidelity {
            receipt_id: digest('c'),
            disposition,
            needs_review,
        }
    }

    #[test]
    fn skeleton_only_when_no_fidelity() {
        let u = CanvasUnderlay::assemble(
            basis(false),
            vec![node("a", None), node("b", None)],
            vec![atom("min_width", Holds::True)],
            None,
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::SkeletonOnly);
        assert!(!u.needs_review);
        assert!(!u.readiness.fidelity_admitted());
    }

    #[test]
    fn composited_when_fidelity_adopts() {
        let u = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", Some(Holds::True))],
            vec![atom("min_width", Holds::True)],
            Some(fidelity(AdapterDisposition::Adopt, false)),
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::Composited);
        assert!(!u.needs_review);
        assert!(u.readiness.fidelity_admitted());
    }

    #[test]
    fn sidecar_fidelity_composites_but_reviews() {
        let u = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", Some(Holds::True))],
            vec![atom("min_width", Holds::True)],
            Some(fidelity(AdapterDisposition::Sidecar, true)),
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::Composited);
        assert!(u.needs_review);
    }

    #[test]
    fn drop_fidelity_degrades() {
        let u = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", Some(Holds::True))],
            vec![],
            Some(fidelity(AdapterDisposition::Drop, true)),
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::Degraded);
        assert!(u.needs_review);
        assert!(u.readiness.fidelity_admitted());
    }

    #[test]
    fn rejected_fidelity_blocks_and_reviews() {
        // A rejected fidelity layer: the overlay is refused, so no node may carry
        // a mapping (validate_composition), and readiness is Blocked.
        let u = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", None)],
            vec![],
            Some(fidelity(AdapterDisposition::Reject, true)),
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::Blocked);
        assert!(u.needs_review);
        assert!(!u.readiness.fidelity_admitted());
    }

    #[test]
    fn blind_spot_atom_surfaces_under_skeleton_only() {
        let u = CanvasUnderlay::assemble(
            basis(false),
            vec![node("a", None)],
            vec![atom("min_width", Holds::Null)],
            None,
        )
        .unwrap();
        assert_eq!(u.readiness, CanvasReadiness::SkeletonOnly);
        assert!(u.needs_review);
    }

    #[test]
    fn fidelity_mapping_without_admissible_layer_is_rejected() {
        // A node claims a fidelity mapping but the fidelity layer was rejected.
        let err = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", Some(Holds::True))],
            vec![],
            Some(fidelity(AdapterDisposition::Reject, true)),
        );
        assert!(err.is_err());
    }

    #[test]
    fn fidelity_mapping_without_any_layer_is_rejected() {
        // A node claims a fidelity mapping but no fidelity layer is composed.
        let err = CanvasUnderlay::assemble(
            basis(false),
            vec![node("a", Some(Holds::True))],
            vec![],
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn basis_fidelity_digest_must_agree_with_layer() {
        // Basis advertises a fidelity digest but no layer is supplied.
        let err = CanvasUnderlay::assemble(basis(true), vec![node("a", None)], vec![], None);
        assert!(err.is_err());
        // Layer supplied but basis has no digest.
        let err = CanvasUnderlay::assemble(
            basis(false),
            vec![node("a", None)],
            vec![],
            Some(fidelity(AdapterDisposition::Adopt, false)),
        );
        assert!(err.is_err());
    }

    #[test]
    fn degenerate_box_is_rejected() {
        let mut bad = node("a", None);
        bad.bounds.x_end = -1.0; // end < start
        let err = CanvasUnderlay::assemble(basis(false), vec![bad], vec![], None);
        assert!(err.is_err());
    }

    #[test]
    fn round_trip_and_tamper_detection() {
        let u = CanvasUnderlay::assemble(
            basis(true),
            vec![node("a", Some(Holds::True)), node("b", None)],
            vec![atom("min_width", Holds::True)],
            Some(fidelity(AdapterDisposition::Adopt, false)),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&u).unwrap();
        assert_eq!(CanvasUnderlay::parse(&bytes).unwrap(), u);

        // Tamper the readiness; the recompute must reject it.
        let mut value = serde_json::to_value(&u).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("readiness".to_string(), serde_json::json!("blocked"));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(CanvasUnderlay::parse(&bytes).is_err());
    }

    #[test]
    fn tamper_node_geometry_breaks_digest() {
        let u =
            CanvasUnderlay::assemble(basis(false), vec![node("a", None)], vec![], None).unwrap();
        let mut value = serde_json::to_value(&u).unwrap();
        value["nodes"][0]["bounds"]["x_end"] = serde_json::json!(999.0);
        let bytes = serde_json::to_vec(&value).unwrap();
        // Geometry is part of the identity, so mutating it breaks the digest.
        assert!(CanvasUnderlay::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let u =
            CanvasUnderlay::assemble(basis(false), vec![node("a", None)], vec![], None).unwrap();
        let mut value = serde_json::to_value(&u).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(CanvasUnderlay::parse(&bytes).is_err());
    }

    #[test]
    fn duplicate_node_id_is_rejected() {
        let err = CanvasUnderlay::assemble(
            basis(false),
            vec![node("a", None), node("a", None)],
            vec![],
            None,
        );
        assert!(err.is_err());
    }
}
