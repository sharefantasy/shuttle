//! UI proposition projection envelope — `ui_projection/v1`.
//!
//! A projection bundles a frozen set of atoms and the principles evaluated over
//! them into one digest-bound artifact: the machine-readable "底稿" (the
//! render-agnostic ground truth) that the rendering arm draws from and that the
//! campaign layer receipts against. It is evidence and judgment, never a path,
//! credential, or land right (red-line #11): a projection reasons and proves;
//! it never mutates a workspace.
//!
//! The overall verdict is a Kleene fold over the principle verdicts, and any
//! principle that needs vetting forces the projection onto the review gate. A
//! projection never fabricates a decided verdict from unknowns.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::{invalid, validate_text_id};
use crate::ui_atom::{CoordAtom, Holds};
use crate::ui_principle::DesignPrinciple;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const UI_PROJECTION_SCHEMA: &str = "ui_projection/v1";
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// The frame of reference the projection is bound to: a source artifact
/// (design token set, reversed render, spec) at a content-addressed base.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectionBasis {
    /// A stable id for the surface under judgment (a screen/component id).
    pub surface_id: String,
    /// Content digest of the artifact the atoms were grounded against.
    pub artifact_digest: Sha256Digest,
    /// The evaluator's version (for reproducibility / calibration).
    pub evaluator_version: String,
}

impl ProjectionBasis {
    fn validate(&self) -> Result<(), ClosedContractError> {
        validate_text_id("basis.surface_id", &self.surface_id)?;
        validate_text_id("basis.evaluator_version", &self.evaluator_version)
    }
}

/// The rolled-up outcome of a projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionOutcome {
    /// Every principle decided true; nothing needs vetting.
    Satisfied,
    /// At least one principle is decided false (a hard refutation).
    Violated,
    /// Undecided or blind-spot: some principle is null or needs vetting.
    Indeterminate,
}

impl ProjectionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectionOutcome::Satisfied => "satisfied",
            ProjectionOutcome::Violated => "violated",
            ProjectionOutcome::Indeterminate => "indeterminate",
        }
    }
}

/// A digest-bound bundle of atoms, principles, and the computed verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UiProjection {
    pub schema_version: String,
    pub projection_id: Sha256Digest,
    pub basis: ProjectionBasis,
    /// Atoms sorted by `atom_id`, unique.
    pub atoms: Vec<CoordAtom>,
    /// Principles sorted by `principle_id`, unique.
    pub principles: Vec<DesignPrinciple>,
    pub outcome: ProjectionOutcome,
    /// Whether the projection must surface on the human/model review gate.
    pub needs_review: bool,
}

#[derive(Serialize)]
struct UiProjectionIdentity<'a> {
    basis: &'a ProjectionBasis,
    atom_ids: Vec<&'a Sha256Digest>,
    principle_ids: Vec<&'a Sha256Digest>,
}

impl UiProjection {
    /// Assemble a projection, computing the outcome/review flag from the atom
    /// truths and principle reductions. Fails closed: a principle that
    /// references an atom absent from the bundle is a contract violation.
    pub fn assemble(
        basis: ProjectionBasis,
        mut atoms: Vec<CoordAtom>,
        mut principles: Vec<DesignPrinciple>,
    ) -> Result<Self, ClosedContractError> {
        atoms.sort_by(|a, b| a.atom_id.cmp(&b.atom_id));
        principles.sort_by(|a, b| a.principle_id.cmp(&b.principle_id));

        // Validate members up front so the fold sees only sound truths.
        for atom in &atoms {
            atom.validate()?;
        }
        for principle in &principles {
            principle.validate()?;
        }

        let (outcome, needs_review) = Self::fold(&atoms, &principles)?;

        let mut projection = Self {
            schema_version: UI_PROJECTION_SCHEMA.to_string(),
            projection_id: placeholder_digest(),
            basis,
            atoms,
            principles,
            outcome,
            needs_review,
        };
        projection.validate_fields()?;
        projection.projection_id = projection.expected_projection_id()?;
        Ok(projection)
    }

    fn fold(
        atoms: &[CoordAtom],
        principles: &[DesignPrinciple],
    ) -> Result<(ProjectionOutcome, bool), ClosedContractError> {
        let truths: BTreeMap<Sha256Digest, Holds> =
            atoms.iter().map(|a| (a.atom_id.clone(), a.holds)).collect();
        let known: BTreeSet<&Sha256Digest> = truths.keys().collect();

        let mut any_false = false;
        let mut any_indeterminate = false;
        let mut needs_review = false;

        for principle in principles {
            for atom_ref in principle.atom_refs() {
                if !known.contains(&atom_ref) {
                    return Err(invalid(
                        "principle references an atom absent from the projection bundle",
                    ));
                }
            }
            let verdict = principle.reduce(&truths);
            match verdict.holds {
                Holds::False => any_false = true,
                Holds::Null => any_indeterminate = true,
                Holds::True => {}
            }
            needs_review |= verdict.needs_vetting;
        }

        let outcome = if any_false {
            ProjectionOutcome::Violated
        } else if any_indeterminate {
            ProjectionOutcome::Indeterminate
        } else {
            ProjectionOutcome::Satisfied
        };
        // A violation or any vetting requirement must reach the gate.
        needs_review |= outcome != ProjectionOutcome::Satisfied;
        Ok((outcome, needs_review))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 8MiB input limit"));
        }
        let projection: Self = serde_json::from_slice(bytes)?;
        projection.validate()?;
        Ok(projection)
    }

    pub fn expected_projection_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.identity())
    }

    fn identity(&self) -> UiProjectionIdentity<'_> {
        UiProjectionIdentity {
            basis: &self.basis,
            atom_ids: self.atoms.iter().map(|a| &a.atom_id).collect(),
            principle_ids: self.principles.iter().map(|p| &p.principle_id).collect(),
        }
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != UI_PROJECTION_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        self.validate_fields()?;
        // Recompute the fold and confirm the stored verdict matches — a tampered
        // outcome/review flag is rejected.
        let (outcome, needs_review) = Self::fold(&self.atoms, &self.principles)?;
        if outcome != self.outcome || needs_review != self.needs_review {
            return Err(invalid(
                "stored outcome/needs_review does not match the recomputed fold",
            ));
        }
        if self.projection_id != self.expected_projection_id()? {
            return Err(invalid("projection_id does not match the canonical bundle"));
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        self.basis.validate()?;
        for atom in &self.atoms {
            atom.validate()?;
        }
        for principle in &self.principles {
            principle.validate()?;
        }
        if !self.atoms.windows(2).all(|w| w[0].atom_id < w[1].atom_id) {
            return Err(invalid(
                "atoms must be strictly sorted by atom_id and unique",
            ));
        }
        if !self
            .principles
            .windows(2)
            .all(|w| w[0].principle_id < w[1].principle_id)
        {
            return Err(invalid(
                "principles must be strictly sorted by principle_id and unique",
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
    use crate::ui_principle::Formula;

    fn atom(relation: &str, holds: Holds) -> CoordAtom {
        let prop = CoordAtomProposition {
            family: RelationFamily::Alignment,
            relation: relation.to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![Axis::new("y", AxisKind::Metric)],
            allen: Some(crate::ui_atom::AllenRelation::Equals),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        };
        if holds.is_known() {
            CoordAtom::evaluated(
                prop,
                holds,
                Provenance::new(ProvenanceClass::DesignSystem, "token://align/baseline"),
                VerificationTier::Fsm,
            )
            .unwrap()
        } else {
            CoordAtom::unevaluated(prop).unwrap()
        }
    }

    fn principle_over(atom: &CoordAtom, proxy: bool) -> DesignPrinciple {
        let formula = Formula::Atom {
            atom_ref: atom.atom_id.clone(),
        };
        DesignPrinciple::new("Aligned", formula, proxy).unwrap()
    }

    #[test]
    fn satisfied_projection() {
        let a = atom("aligned_top", Holds::True);
        let p = principle_over(&a, false);
        let proj = UiProjection::assemble(basis(), vec![a], vec![p]).unwrap();
        assert_eq!(proj.outcome, ProjectionOutcome::Satisfied);
        assert!(!proj.needs_review);
    }

    #[test]
    fn violated_projection_reaches_gate() {
        let a = atom("aligned_top", Holds::False);
        let p = principle_over(&a, false);
        let proj = UiProjection::assemble(basis(), vec![a], vec![p]).unwrap();
        assert_eq!(proj.outcome, ProjectionOutcome::Violated);
        assert!(proj.needs_review);
    }

    #[test]
    fn indeterminate_when_atom_unknown() {
        let a = atom("aligned_top", Holds::Null);
        let p = principle_over(&a, true);
        let proj = UiProjection::assemble(basis(), vec![a], vec![p]).unwrap();
        assert_eq!(proj.outcome, ProjectionOutcome::Indeterminate);
        assert!(proj.needs_review);
    }

    #[test]
    fn dangling_principle_ref_is_rejected() {
        let bound = atom("aligned_top", Holds::True);
        let orphan = atom("aligned_left", Holds::True);
        let p = principle_over(&orphan, false);
        // Only `bound` is in the bundle, but the principle references `orphan`.
        let err = UiProjection::assemble(basis(), vec![bound], vec![p]);
        assert!(err.is_err());
    }

    #[test]
    fn round_trip_and_tamper_detection() {
        let a = atom("aligned_top", Holds::True);
        let p = principle_over(&a, false);
        let proj = UiProjection::assemble(basis(), vec![a], vec![p]).unwrap();
        let bytes = serde_json::to_vec(&proj).unwrap();
        assert_eq!(UiProjection::parse(&bytes).unwrap(), proj);

        // Tamper the outcome; the recompute must reject it.
        let mut value = serde_json::to_value(&proj).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("outcome".to_string(), serde_json::json!("violated"));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(UiProjection::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let a = atom("aligned_top", Holds::True);
        let p = principle_over(&a, false);
        let proj = UiProjection::assemble(basis(), vec![a], vec![p]).unwrap();
        let mut value = serde_json::to_value(&proj).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(UiProjection::parse(&bytes).is_err());
    }

    fn basis() -> ProjectionBasis {
        ProjectionBasis {
            surface_id: "screen:checkout".to_string(),
            artifact_digest: Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).unwrap(),
            evaluator_version: "shuttle-ui/0.1".to_string(),
        }
    }
}
