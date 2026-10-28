//! The Form C composer arm: overlay the Form A SVG skeleton with an optional
//! Form B fidelity layer into a single machine-readable `canvas_underlay/v1`.
//!
//! Design §4.3 / §8-P8. A "Figma-class canvas" is **the Form A SVG skeleton +
//! the Form B high-fidelity layers**, overlaid. The canvas itself is a *render
//! product, not a truth source* (red-line #7): the truth stays in the
//! [`shuttle_protocol::CoordAtom`] propositions the underlay carries. shuttle does **not**
//! build the canvas UI (that is the host/front-end's job), nor does it own the
//! engine's coordinate space or pixels (red-lines #6 / #11). shuttle provides the
//! canvas's *machine-readable 底稿* — 赋值 (skeleton nodes) + 命题 (atoms with
//! provenance) + provenance (the Form B fidelity binding) — and the front-end
//! renders it into an interactive canvas.
//!
//! The composition never silently fails (§4.2): the underlay's readiness folds
//! from the Form B disposition, so an inadmissible fidelity layer (`Reject`)
//! surfaces as `Blocked` (skeleton-only, review-gated) rather than being
//! laundered into an overlay. A node is only bound to a design token when an
//! *admissible* fidelity layer actually mapped it; an unmapped node stays
//! skeleton-only, never fabricating a token binding (§7/§20 「绝不瞎猜」).
//!
//! Landing order is fixed by the design: **先形态 A → 再形态 B → 画板 C 最后**;
//! this arm is the last, composing what the first two already proved.
//!
//! This arm reasons and reports; it never mutates a workspace, owns the artifact
//! bytes, or drives a canvas engine (red-line #11). Persisting the underlay is
//! the caller/authority-owner's job.

use crate::render::Scene;
use shuttle_protocol::anchor::Sha256Digest;
use shuttle_protocol::{
    AdapterReceipt, CanvasBasis, CanvasBox, CanvasFidelity, CanvasNode, CanvasUnderlay,
    ClosedContractError, CoordAtom,
};
use std::collections::BTreeMap;

/// The structured input to a Form C composition.
#[derive(Debug, Clone)]
pub struct CanvasInput {
    /// The surface this canvas depicts (stable id, shared with Forms A/B).
    pub surface_id: String,
    /// The Form A skeleton: the constraint-satisfying box assignment (赋值).
    pub scene: Scene,
    /// The proposition bundle (命题) that is the underlay's sole truth source —
    /// typically the render=verify re-evaluated atoms, carrying `reversed`
    /// provenance. Passed through untouched and content-addressed.
    pub atoms: Vec<CoordAtom>,
    /// The optional Form B fidelity layer to overlay. When present, its §3.3
    /// disposition drives the underlay's readiness fold; when its layer is
    /// admissible, its mappings bind skeleton nodes to design tokens.
    pub fidelity: Option<AdapterReceipt>,
    /// shuttle's evaluator identity, stamped into the basis so a stored underlay is
    /// reproducible against the code that minted it.
    pub evaluator_version: String,
}

/// The result of a Form C composition: the deterministic SVG skeleton the
/// front-end overlays, and the digest-bound underlay 底稿 it renders.
#[derive(Debug, Clone)]
pub struct CanvasComposition {
    /// The Form A SVG skeleton the `basis.skeleton_digest` is bound to.
    pub svg: String,
    /// The assembled, content-addressed underlay.
    pub underlay: CanvasUnderlay,
}

/// Build the `sha256:<hex>` content address of a byte string.
fn digest_of(bytes: &[u8]) -> Result<Sha256Digest, ClosedContractError> {
    Sha256Digest::parse(format!("sha256:{}", shuttle_protocol::sha256_hex(bytes)))
        .map_err(|e| ClosedContractError::Invalid(e.to_string()))
}

/// Compose a Form A skeleton (+ optional Form B fidelity layer) into a
/// `canvas_underlay/v1`. The skeleton is projected to a deterministic SVG whose
/// digest binds the basis; each scene element becomes a canvas node; and, only
/// when an *admissible* fidelity layer mapped it, a node is bound to the design
/// token that layer realized (with the reverse-measured three-valued verdict).
///
/// The readiness/review flag are computed by [`CanvasUnderlay`]'s fold, so an
/// inadmissible fidelity layer cannot be laundered into an overlay.
pub fn compose(input: CanvasInput) -> Result<CanvasComposition, ClosedContractError> {
    let CanvasInput {
        surface_id,
        scene,
        atoms,
        fidelity,
        evaluator_version,
    } = input;

    // Bind the basis to the exact skeleton bytes the front-end overlays.
    let svg = scene.to_svg();
    let skeleton_digest = digest_of(svg.as_bytes())?;

    // A fidelity overlay is admitted onto the canvas only when its Form B
    // disposition is admissible (Adopt/Sidecar/Drop). A Reject layer is refused
    // (Blocked), and no node may carry a token binding from it.
    let admissible = fidelity
        .as_ref()
        .map(|r| r.disposition.is_admissible())
        .unwrap_or(false);

    // Index the fidelity layer's layer_id -> (token_ref, verified) mappings so a
    // skeleton node keyed by the same id can pick up its token binding.
    let mut mapping_by_layer: BTreeMap<&str, (&str, shuttle_protocol::Holds)> = BTreeMap::new();
    if admissible {
        if let Some(receipt) = &fidelity {
            for m in &receipt.mappings {
                mapping_by_layer.insert(m.layer_id.as_str(), (m.token_ref.as_str(), m.verified));
            }
        }
    }

    // Lower every skeleton element to a canvas node, attaching a token binding
    // only when an admissible fidelity layer mapped that exact node.
    let mut nodes = Vec::with_capacity(scene.elements.len());
    for e in &scene.elements {
        let bounds = CanvasBox {
            x_start: e.x.start,
            x_end: e.x.end,
            y_start: e.y.start,
            y_end: e.y.end,
        };
        let (token_ref, fidelity_verified) = match mapping_by_layer.get(e.id.as_str()) {
            Some((token_ref, verified)) => (Some((*token_ref).to_string()), Some(*verified)),
            None => (None, None),
        };
        nodes.push(CanvasNode {
            node_id: e.id.clone(),
            bounds,
            role: e.role.clone(),
            text: e.text.clone(),
            token_ref,
            fidelity_verified,
        });
    }

    // The fidelity binding is a content-addressed link back to the Form B
    // receipt, present iff a fidelity layer was supplied. Its digest also seeds
    // the basis so `validate_composition` can confirm they agree.
    let (basis_fidelity_digest, canvas_fidelity) = match &fidelity {
        Some(receipt) => (
            Some(receipt.receipt_id.clone()),
            Some(CanvasFidelity {
                receipt_id: receipt.receipt_id.clone(),
                disposition: receipt.disposition,
                needs_review: receipt.needs_review,
            }),
        ),
        None => (None, None),
    };

    let basis = CanvasBasis {
        surface_id,
        skeleton_digest,
        fidelity_digest: basis_fidelity_digest,
        evaluator_version,
    };

    let underlay = CanvasUnderlay::assemble(basis, nodes, atoms, canvas_fidelity)?;
    Ok(CanvasComposition { svg, underlay })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{self, AdapterInput, TokenGate};
    use crate::geom::{Frame, Interval};
    use crate::render::Element;
    use shuttle_protocol::{
        AdapterBasis, AdapterDisposition, Axis, AxisKind, CanvasReadiness, CoordAtomProposition,
        Holds, RelationFamily, TimeQuantifier,
    };
    use std::collections::BTreeMap;

    fn iv(s: f64, e: f64) -> Interval {
        Interval::new(s, e).unwrap()
    }

    fn scene_two() -> Scene {
        let mut s = Scene::new(320.0, 200.0, 0.5);
        s.push(Element::new("cta", iv(0.0, 120.0), iv(0.0, 44.0)).with_text("Buy"));
        s.push(Element::new("hint", iv(140.0, 300.0), iv(0.0, 44.0)).with_role("note"));
        s
    }

    fn order_atom() -> CoordAtom {
        CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Order,
            relation: "before".to_string(),
            args: vec!["cta".to_string(), "hint".to_string()],
            axes: vec![Axis::new("x", AxisKind::Metric)],
            allen: Some(shuttle_protocol::AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap()
    }

    /// The order atom reverse-measured against the skeleton frame -> decided
    /// `true`, so it does not itself force review.
    fn decided_order_atom() -> CoordAtom {
        let frame = scene_two().to_frame();
        crate::geom::evaluate_atom(&order_atom(), &frame, &|_| None).unwrap()
    }

    fn null_atom() -> CoordAtom {
        // Unevaluated -> holds is Null: a blind-spot proposition.
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

    fn cta_min_width_atom() -> CoordAtom {
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

    /// Build a Form B receipt over a `cta` layer with the given disposition by
    /// choosing a measured width relative to the 44px token bound.
    fn receipt(mapped_ok: bool, droppable: bool) -> AdapterReceipt {
        let width = if mapped_ok { 44.0 } else { 20.0 };
        let mut frame = Frame::new(0.0);
        frame.set("cta", "w", Interval::new(0.0, width).unwrap());
        let basis = AdapterBasis {
            surface_id: "screen:x".to_string(),
            engine_id: "v0".to_string(),
            engine_version: "2026.9".to_string(),
            artifact_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            evaluator_version: "shuttle-ui/test".to_string(),
        };
        let mut thresholds = BTreeMap::new();
        thresholds.insert("token://size/cta-min".to_string(), 44.0);
        let input = AdapterInput {
            basis,
            frame,
            token_gates: vec![TokenGate {
                name: "token:cta-min-width".to_string(),
                atom: cta_min_width_atom(),
                droppable,
            }],
            extra_atoms: vec![],
            a11y_gates: vec![],
            capability_gates: vec![],
            mappings: vec![adapter::MappingClaim {
                layer_id: "cta".to_string(),
                token_ref: "token://component/button".to_string(),
                corroborating_atom: Some(0),
            }],
            aesthetic: None,
            thresholds,
        };
        adapter::assess(input).unwrap()
    }

    fn input(atoms: Vec<CoordAtom>, fidelity: Option<AdapterReceipt>) -> CanvasInput {
        CanvasInput {
            surface_id: "surface:home".to_string(),
            scene: scene_two(),
            atoms,
            fidelity,
            evaluator_version: "shuttle-ui/test".to_string(),
        }
    }

    #[test]
    fn skeleton_only_without_fidelity() {
        let c = compose(input(vec![decided_order_atom()], None)).unwrap();
        assert_eq!(c.underlay.readiness, CanvasReadiness::SkeletonOnly);
        assert!(!c.underlay.needs_review);
        assert!(c.underlay.basis.fidelity_digest.is_none());
        assert!(c.underlay.fidelity.is_none());
        // Nodes carry no token binding under skeleton-only.
        assert!(c.underlay.nodes.iter().all(|n| !n.has_fidelity()));
        // The SVG skeleton is bound and non-empty.
        assert!(c.svg.contains("<svg"));
        assert!(c.svg.contains("id=\"cta\""));
        c.underlay.validate().unwrap();
    }

    #[test]
    fn svg_digest_binds_basis() {
        let c = compose(input(vec![order_atom()], None)).unwrap();
        let expected = digest_of(c.svg.as_bytes()).unwrap();
        assert_eq!(c.underlay.basis.skeleton_digest, expected);
    }

    #[test]
    fn adopted_fidelity_composites_and_binds_mapped_node() {
        let c = compose(input(vec![order_atom()], Some(receipt(true, false)))).unwrap();
        assert_eq!(c.underlay.readiness, CanvasReadiness::Composited);
        assert!(c.underlay.basis.fidelity_digest.is_some());
        assert_eq!(
            c.underlay.fidelity.as_ref().unwrap().disposition,
            AdapterDisposition::Adopt
        );
        // The mapped `cta` node picks up the token binding + verdict.
        let cta = c
            .underlay
            .nodes
            .iter()
            .find(|n| n.node_id == "cta")
            .unwrap();
        assert_eq!(cta.token_ref.as_deref(), Some("token://component/button"));
        assert_eq!(cta.fidelity_verified, Some(Holds::True));
        // The unmapped `hint` node stays skeleton-only.
        let hint = c
            .underlay
            .nodes
            .iter()
            .find(|n| n.node_id == "hint")
            .unwrap();
        assert!(!hint.has_fidelity());
        c.underlay.validate().unwrap();
    }

    #[test]
    fn droppable_fail_degrades_but_still_binds() {
        // droppable token failure -> Drop -> admissible -> Degraded overlay.
        let c = compose(input(vec![order_atom()], Some(receipt(false, true)))).unwrap();
        assert_eq!(c.underlay.readiness, CanvasReadiness::Degraded);
        assert!(c.underlay.needs_review);
        assert!(c.underlay.readiness.fidelity_admitted());
        let cta = c
            .underlay
            .nodes
            .iter()
            .find(|n| n.node_id == "cta")
            .unwrap();
        // Drop is admissible, so the mapping survives; its verdict is the
        // reverse-measured false.
        assert_eq!(cta.fidelity_verified, Some(Holds::False));
        c.underlay.validate().unwrap();
    }

    #[test]
    fn rejected_fidelity_blocks_and_drops_all_bindings() {
        // non-droppable token failure -> Reject -> inadmissible -> Blocked, and
        // NO node may carry a token binding (never laundered into an overlay).
        let c = compose(input(vec![order_atom()], Some(receipt(false, false)))).unwrap();
        assert_eq!(c.underlay.readiness, CanvasReadiness::Blocked);
        assert!(c.underlay.needs_review);
        assert!(!c.underlay.readiness.fidelity_admitted());
        assert!(c.underlay.nodes.iter().all(|n| !n.has_fidelity()));
        // The fidelity binding is still recorded (provenance), just not overlaid.
        assert_eq!(
            c.underlay.fidelity.as_ref().unwrap().disposition,
            AdapterDisposition::Reject
        );
        c.underlay.validate().unwrap();
    }

    #[test]
    fn blind_spot_atom_forces_review_even_skeleton_only() {
        // A null proposition surfaces on the review gate (绝不瞎猜).
        let c = compose(input(vec![null_atom()], None)).unwrap();
        assert_eq!(c.underlay.readiness, CanvasReadiness::SkeletonOnly);
        assert!(c.underlay.needs_review);
        c.underlay.validate().unwrap();
    }

    #[test]
    fn round_trip_through_parse() {
        let c = compose(input(vec![order_atom()], Some(receipt(true, false)))).unwrap();
        let bytes = serde_json::to_vec(&c.underlay).unwrap();
        let parsed = CanvasUnderlay::parse(&bytes).unwrap();
        assert_eq!(parsed, c.underlay);
    }
}
