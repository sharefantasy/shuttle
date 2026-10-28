//! The serde wire boundary and orchestration surface for the formal-UI arms.
//!
//! The model types this crate reasons over — [`shuttle_protocol::CoordAtom`]'s
//! *proposition* fields, the constraint lexicon/coupling types in
//! [`crate::constraint`] — are deliberately serde-free (`CoordAtomProposition`
//! and every `constraint` model type carry no derive), because their identity
//! and validity are enforced by *constructors*, not by a permissive
//! deserializer. This module is the one place that owns the JSON DTOs, lowers
//! them through those constructors (so an ill-formed request fails closed at the
//! boundary), and exposes five orchestrators — [`verify`], [`adopt`],
//! [`couple`], [`render`] (Form A), and [`assess`] (Form B) — that every
//! delivery surface (the `shuttle ui` CLI, the MCP shim, and the pi extension) drives
//! *identically*. One request shape, one evaluation path, one response shape, no
//! surface-specific drift.
//!
//! The orchestrators reason and report; none of them writes a file, owns a
//! path, or holds land rights (red-line #11). Persisting a projected artifact
//! is the caller/authority-owner's job.

use crate::adapter::{self, A11yGate, AdapterInput, CapabilityGate, MappingClaim, TokenGate};
use crate::canvas::{self, CanvasInput};
use crate::constraint::{
    arbitrate, A11yConstraint, A11yVerdict, CouplingGraph, CouplingKind, DimValue, Dimension,
    LegalRange, LexiconVerdict, SolverClass,
};
use crate::geom::{self, Frame, Interval};
use crate::render::{outcome_of, render_verify, Element, Scene};
use serde::{Deserialize, Serialize};
use shuttle_protocol::anchor::Sha256Digest;
use shuttle_protocol::{
    AdapterBasis, AdapterReceipt, AestheticScore, Axis, CanvasReadiness, CanvasUnderlay,
    ClosedContractError, CoordAtom, CoordAtomProposition, DesignPrinciple, Formula, Holds,
    ProjectionBasis, ProjectionOutcome, RelationFamily, TimeQuantifier, UiProjection,
};
use std::collections::BTreeMap;

/// The evaluator version stamped into every projection basis so a stored
/// judgment is reproducible against the exact code that minted it.
pub const EVALUATOR_VERSION: &str = concat!("shuttle-ui/", env!("CARGO_PKG_VERSION"));

/// Build the canonical `sha256:<hex>` content address of a byte string. Used to
/// bind a projection to the exact artifact bytes the render arm emitted.
pub fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", shuttle_protocol::sha256_hex(bytes))
}

fn invalid(msg: impl Into<String>) -> ClosedContractError {
    ClosedContractError::Invalid(msg.into())
}

// ---------------------------------------------------------------------------
// Geometry DTOs
// ---------------------------------------------------------------------------

/// One element's extent on a named axis: the half-open interval `[start, end)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BoxDto {
    pub element: String,
    pub axis: String,
    pub start: f64,
    pub end: f64,
}

/// A frame of bounding boxes with an endpoint tolerance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FrameDto {
    #[serde(default)]
    pub tolerance: f64,
    #[serde(default)]
    pub boxes: Vec<BoxDto>,
}

impl FrameDto {
    /// Lower the DTO into a geometry [`Frame`], rejecting any degenerate
    /// interval (`end < start` / non-finite) at the boundary.
    pub fn build(&self) -> Result<Frame, ClosedContractError> {
        let mut frame = Frame::new(self.tolerance);
        for b in &self.boxes {
            let iv = Interval::new(b.start, b.end).ok_or_else(|| {
                invalid(format!(
                    "invalid interval [{}, {}) for {}/{}",
                    b.start, b.end, b.element, b.axis
                ))
            })?;
            frame.set(b.element.clone(), b.axis.clone(), iv);
        }
        Ok(frame)
    }
}

/// The proposition fields of a [`CoordAtom`], before evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtomSpecDto {
    pub family: RelationFamily,
    pub relation: String,
    pub args: Vec<String>,
    pub axes: Vec<Axis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allen: Option<shuttle_protocol::AllenRelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_ref: Option<String>,
    pub frame: String,
    pub time_quantifier: TimeQuantifier,
}

impl AtomSpecDto {
    /// Lower into an unevaluated [`CoordAtom`] via its validating constructor.
    pub fn build(&self) -> Result<CoordAtom, ClosedContractError> {
        CoordAtom::unevaluated(CoordAtomProposition {
            family: self.family,
            relation: self.relation.clone(),
            args: self.args.clone(),
            axes: self.axes.clone(),
            allen: self.allen,
            threshold_ref: self.threshold_ref.clone(),
            frame: self.frame.clone(),
            time_quantifier: self.time_quantifier,
        })
    }
}

// ---------------------------------------------------------------------------
// Principle DTO (formula over atom *indices*, lowered to atom ids)
// ---------------------------------------------------------------------------

/// A boolean formula over atom references *by index* into the request's atom
/// list. Lowering resolves each index to the built atom's content-addressed
/// `atom_id`; a dangling index fails closed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FormulaDto {
    Atom { atom: usize },
    All { all: Vec<FormulaDto> },
    Any { any: Vec<FormulaDto> },
    Not { not: Box<FormulaDto> },
}

impl FormulaDto {
    fn lower(&self, atoms: &[CoordAtom]) -> Result<Formula, ClosedContractError> {
        match self {
            FormulaDto::Atom { atom } => {
                let a = atoms.get(*atom).ok_or_else(|| {
                    invalid(format!(
                        "formula references atom index {atom} which is out of range (have {})",
                        atoms.len()
                    ))
                })?;
                Ok(Formula::Atom {
                    atom_ref: a.atom_id.clone(),
                })
            }
            FormulaDto::All { all } => Ok(Formula::All {
                all: all
                    .iter()
                    .map(|f| f.lower(atoms))
                    .collect::<Result<_, _>>()?,
            }),
            FormulaDto::Any { any } => Ok(Formula::Any {
                any: any
                    .iter()
                    .map(|f| f.lower(atoms))
                    .collect::<Result<_, _>>()?,
            }),
            FormulaDto::Not { not } => Ok(Formula::Not {
                not: Box::new(not.lower(atoms)?),
            }),
        }
    }
}

/// A named design principle whose formula is stated over atom indices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrincipleDto {
    pub name: String,
    pub formula: FormulaDto,
    #[serde(default)]
    pub is_geometric_proxy: bool,
}

impl PrincipleDto {
    fn lower(&self, atoms: &[CoordAtom]) -> Result<DesignPrinciple, ClosedContractError> {
        let formula = self.formula.lower(atoms)?;
        DesignPrinciple::new(self.name.clone(), formula, self.is_geometric_proxy)
    }
}

// ---------------------------------------------------------------------------
// Scene DTOs (render arm)
// ---------------------------------------------------------------------------

/// One positioned element in a scene; `x`/`y` are `[start, end]` extents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ElementDto {
    pub id: String,
    pub x: [f64; 2],
    pub y: [f64; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl ElementDto {
    fn build(&self) -> Result<Element, ClosedContractError> {
        let x = Interval::new(self.x[0], self.x[1])
            .ok_or_else(|| invalid(format!("invalid x interval for element {}", self.id)))?;
        let y = Interval::new(self.y[0], self.y[1])
            .ok_or_else(|| invalid(format!("invalid y interval for element {}", self.id)))?;
        let mut el = Element::new(self.id.clone(), x, y);
        if let Some(t) = &self.text {
            el = el.with_text(t.clone());
        }
        if let Some(r) = &self.role {
            el = el.with_role(r.clone());
        }
        Ok(el)
    }
}

/// A constraint-satisfying layout assignment ready to project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SceneDto {
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub tolerance: f64,
    #[serde(default)]
    pub elements: Vec<ElementDto>,
}

impl SceneDto {
    fn build(&self) -> Result<Scene, ClosedContractError> {
        let mut scene = Scene::new(self.width, self.height, self.tolerance);
        for e in &self.elements {
            scene.push(e.build()?);
        }
        Ok(scene)
    }
}

// ---------------------------------------------------------------------------
// Constraint DTOs (couple arm)
// ---------------------------------------------------------------------------

/// A dimension's concrete value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DimValueDto {
    Metric { value: f64 },
    Ordinal { value: i64 },
}

impl DimValueDto {
    fn build(self) -> DimValue {
        match self {
            DimValueDto::Metric { value } => DimValue::Metric(value),
            DimValueDto::Ordinal { value } => DimValue::Ordinal(value),
        }
    }
}

/// A dimension's legal range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegalRangeDto {
    Interval { min: f64, max: f64 },
    Ordinal { allowed: Vec<i64> },
}

impl LegalRangeDto {
    fn build(&self) -> LegalRange {
        match self {
            LegalRangeDto::Interval { min, max } => LegalRange::interval(*min, *max),
            LegalRangeDto::Ordinal { allowed } => LegalRange::ordinal(allowed.iter().copied()),
        }
    }
}

/// One dimension in the lexicon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DimensionDto {
    pub name: String,
    pub value: DimValueDto,
    pub range: LegalRangeDto,
}

impl DimensionDto {
    fn build(&self) -> Dimension {
        Dimension {
            name: self.name.clone(),
            value: self.value.build(),
            range: self.range.build(),
        }
    }
}

/// How two dimensions are coupled.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CouplingKindDto {
    Value,
    Constraint,
    Time,
}

impl CouplingKindDto {
    fn build(self) -> CouplingKind {
        match self {
            CouplingKindDto::Value => CouplingKind::Value,
            CouplingKindDto::Constraint => CouplingKind::Constraint,
            CouplingKindDto::Time => CouplingKind::Time,
        }
    }
}

/// A directed coupling edge `from -> to` of a given kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CouplingDto {
    pub from: String,
    pub to: String,
    pub kind: CouplingKindDto,
}

/// An accessibility hard constraint (non-degradable; an unknown blocks).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct A11yDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    pub required_min: f64,
}

impl A11yDto {
    fn build(&self) -> A11yConstraint {
        A11yConstraint {
            name: self.name.clone(),
            measured: self.measured,
            required_min: self.required_min,
        }
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Evaluate a set of atom propositions against a frame of bounding boxes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    pub frame: FrameDto,
    #[serde(default)]
    pub atoms: Vec<AtomSpecDto>,
    /// `token:// -> numeric bound` resolver for metric atoms.
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
}

/// The evaluated atoms and their three-valued roll-up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyResponse {
    pub atoms: Vec<CoordAtom>,
    pub outcome: ProjectionOutcome,
    pub decided_true: usize,
    pub decided_false: usize,
    pub unknown: usize,
}

fn threshold_resolver(thresholds: &BTreeMap<String, f64>) -> impl Fn(&str) -> Option<f64> + '_ {
    move |t: &str| thresholds.get(t).copied()
}

/// Re-derive each atom's Kleene truth from the frame geometry (the evidence arm).
pub fn verify(req: &VerifyRequest) -> Result<VerifyResponse, ClosedContractError> {
    let frame = req.frame.build()?;
    let resolver = threshold_resolver(&req.thresholds);
    let mut atoms = Vec::with_capacity(req.atoms.len());
    let (mut decided_true, mut decided_false, mut unknown) = (0usize, 0usize, 0usize);
    for spec in &req.atoms {
        let atom = spec.build()?;
        let evaluated = geom::evaluate_atom(&atom, &frame, &resolver)?;
        match evaluated.holds {
            Holds::True => decided_true += 1,
            Holds::False => decided_false += 1,
            Holds::Null => unknown += 1,
        }
        atoms.push(evaluated);
    }
    let outcome = if decided_false > 0 {
        ProjectionOutcome::Violated
    } else if unknown > 0 {
        ProjectionOutcome::Indeterminate
    } else {
        ProjectionOutcome::Satisfied
    };
    Ok(VerifyResponse {
        atoms,
        outcome,
        decided_true,
        decided_false,
        unknown,
    })
}

// ---------------------------------------------------------------------------
// adopt
// ---------------------------------------------------------------------------

/// Reverse-engineer atoms from a frame, reduce named principles over them, and
/// bundle the result into a signed [`UiProjection`] bound to an artifact digest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    pub surface_id: String,
    /// The `sha256:<hex>` digest of the artifact the atoms are grounded against.
    pub artifact_digest: String,
    pub frame: FrameDto,
    #[serde(default)]
    pub atoms: Vec<AtomSpecDto>,
    #[serde(default)]
    pub principles: Vec<PrincipleDto>,
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
}

/// The assembled projection plus its rolled-up verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdoptResponse {
    pub projection: UiProjection,
    pub projection_id: String,
    pub outcome: ProjectionOutcome,
    pub needs_review: bool,
}

/// Reverse-engineer + reduce + bundle a signed projection from a frame.
pub fn adopt(req: &AdoptRequest) -> Result<AdoptResponse, ClosedContractError> {
    let frame = req.frame.build()?;
    let resolver = threshold_resolver(&req.thresholds);
    let mut atoms = Vec::with_capacity(req.atoms.len());
    for spec in &req.atoms {
        let atom = spec.build()?;
        atoms.push(geom::evaluate_atom(&atom, &frame, &resolver)?);
    }
    let mut principles = Vec::with_capacity(req.principles.len());
    for p in &req.principles {
        principles.push(p.lower(&atoms)?);
    }
    let digest =
        Sha256Digest::parse(req.artifact_digest.clone()).map_err(|e| invalid(e.to_string()))?;
    let basis = ProjectionBasis {
        surface_id: req.surface_id.clone(),
        artifact_digest: digest,
        evaluator_version: EVALUATOR_VERSION.to_string(),
    };
    let projection = UiProjection::assemble(basis, atoms, principles)?;
    Ok(AdoptResponse {
        projection_id: projection.projection_id.to_string(),
        outcome: projection.outcome,
        needs_review: projection.needs_review,
        projection,
    })
}

// ---------------------------------------------------------------------------
// couple
// ---------------------------------------------------------------------------

/// Type-check a lexicon, classify the coupling topology, and run the a11y gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CoupleRequest {
    #[serde(default)]
    pub dimensions: Vec<DimensionDto>,
    #[serde(default)]
    pub couplings: Vec<CouplingDto>,
    #[serde(default)]
    pub a11y: Vec<A11yDto>,
}

/// The L1 verdict for one dimension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LexiconRow {
    pub dimension: String,
    pub status: String,
}

/// The a11y verdict for one constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct A11yRow {
    pub name: String,
    pub verdict: String,
}

/// The rolled-up arbitration report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoupleResponse {
    pub lexicon: Vec<LexiconRow>,
    pub solver_class: String,
    pub a11y: Vec<A11yRow>,
    pub admissible: bool,
}

fn lexicon_status(v: &LexiconVerdict) -> &'static str {
    match v {
        LexiconVerdict::Ok => "ok",
        LexiconVerdict::OutOfRange { .. } => "out_of_range",
        LexiconVerdict::TypeMismatch { .. } => "type_mismatch",
    }
}

fn a11y_status(v: A11yVerdict) -> &'static str {
    match v {
        A11yVerdict::Pass => "pass",
        A11yVerdict::Fail => "fail",
        A11yVerdict::Blocked => "blocked",
    }
}

fn solver_status(c: SolverClass) -> &'static str {
    match c {
        SolverClass::Lint => "lint",
        SolverClass::Solver => "solver",
    }
}

/// Run L1 typing + L2 topology classification + the a11y gate (constraint arm).
pub fn couple(req: &CoupleRequest) -> Result<CoupleResponse, ClosedContractError> {
    let dims: Vec<Dimension> = req.dimensions.iter().map(DimensionDto::build).collect();
    let mut graph = CouplingGraph::new();
    for d in &dims {
        graph.add_dimension(d.name.clone());
    }
    for c in &req.couplings {
        graph.couple(c.from.clone(), c.to.clone(), c.kind.build());
    }
    let a11y: Vec<A11yConstraint> = req.a11y.iter().map(A11yDto::build).collect();
    let report = arbitrate(&dims, &graph, &a11y);

    // Zip the lexicon verdicts back against the dimension names directly — the
    // report preserves input order — rather than re-deriving names from the
    // verdict payloads.
    let lexicon = dims
        .iter()
        .zip(report.lexicon.iter())
        .map(|(d, v)| LexiconRow {
            dimension: d.name.clone(),
            status: lexicon_status(v).to_string(),
        })
        .collect();
    let a11y = report
        .a11y
        .iter()
        .map(|(name, v)| A11yRow {
            name: name.clone(),
            verdict: a11y_status(*v).to_string(),
        })
        .collect();
    Ok(CoupleResponse {
        lexicon,
        solver_class: solver_status(report.solver_class).to_string(),
        a11y,
        admissible: report.admissible,
    })
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Project a scene to HTML and re-verify every declared atom against the
/// geometry that HTML encodes (the generation arm, Form A). When
/// `adopt_surface_id` is set, also assemble a signed projection bound to the
/// rendered artifact's digest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RenderRequest {
    pub scene: SceneDto,
    #[serde(default)]
    pub atoms: Vec<AtomSpecDto>,
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopt_surface_id: Option<String>,
    #[serde(default)]
    pub principles: Vec<PrincipleDto>,
}

/// The per-atom re-verification result of a render pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomCheckDto {
    pub atom: CoordAtom,
    pub holds: Holds,
    pub regression: bool,
    pub unverified: bool,
}

/// The projected artifact plus its render=verify roll-up (and optional signed
/// projection).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderResponse {
    pub html: String,
    pub artifact_digest: String,
    pub checks: Vec<AtomCheckDto>,
    pub outcome: ProjectionOutcome,
    pub consistent: bool,
    pub verified: usize,
    pub unverified: usize,
    pub regressions: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<UiProjection>,
}

/// Project + re-verify a scene; optionally adopt the result as a projection.
pub fn render(req: &RenderRequest) -> Result<RenderResponse, ClosedContractError> {
    let scene = req.scene.build()?;
    let resolver = threshold_resolver(&req.thresholds);
    let mut atoms = Vec::with_capacity(req.atoms.len());
    for spec in &req.atoms {
        atoms.push(spec.build()?);
    }
    let report = render_verify(&scene, &atoms, &resolver)?;
    let artifact_digest = digest_of(report.html.as_bytes());
    let outcome = outcome_of(&report);

    let checks: Vec<AtomCheckDto> = report
        .checks
        .iter()
        .map(|c| AtomCheckDto {
            atom: c.atom.clone(),
            holds: c.holds,
            regression: c.regression,
            unverified: c.unverified,
        })
        .collect();

    let projection = if let Some(surface_id) = &req.adopt_surface_id {
        let digest =
            Sha256Digest::parse(artifact_digest.clone()).map_err(|e| invalid(e.to_string()))?;
        // Principles reduce over the RE-EVALUATED atoms the projection carries,
        // so the bundle is bound to the exact geometry the artifact encodes.
        let evaluated: Vec<CoordAtom> = report.checks.iter().map(|c| c.atom.clone()).collect();
        let mut principles = Vec::with_capacity(req.principles.len());
        for p in &req.principles {
            principles.push(p.lower(&evaluated)?);
        }
        let basis = ProjectionBasis {
            surface_id: surface_id.clone(),
            artifact_digest: digest,
            evaluator_version: EVALUATOR_VERSION.to_string(),
        };
        Some(UiProjection::assemble(basis, evaluated, principles)?)
    } else {
        None
    };

    Ok(RenderResponse {
        html: report.html,
        artifact_digest,
        checks,
        outcome,
        consistent: report.consistent,
        verified: report.verified,
        unverified: report.unverified,
        regressions: report.regressions,
        projection,
    })
}

// ---------------------------------------------------------------------------
// assess (Form B: external visual-engine adapter)
// ---------------------------------------------------------------------------

/// The engine's untrusted aesthetic self-report. A §3.3 *soft* signal: a score
/// below its floor only flags the artifact for review (`Sidecar`); it never
/// blocks an otherwise-admissible artifact and never overrides a hard reject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AestheticDto {
    pub score: f64,
    pub floor: f64,
    pub rationale_ref: String,
}

impl AestheticDto {
    fn build(&self) -> AestheticScore {
        AestheticScore {
            score: self.score,
            floor: self.floor,
            rationale_ref: self.rationale_ref.clone(),
        }
    }
}

/// A token 硬门 stated as an atom to reverse-measure against the engine's
/// produced geometry. A non-pass verdict rejects the whole artifact unless
/// `droppable`, in which case the feature degrades to `Drop`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TokenGateDto {
    pub name: String,
    pub atom: AtomSpecDto,
    #[serde(default)]
    pub droppable: bool,
}

/// An a11y / 可区分性 hard gate over a non-degradable constraint. a11y is a
/// red-line, so `droppable` defaults to `false`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct A11yGateDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    pub required_min: f64,
    #[serde(default)]
    pub droppable: bool,
}

impl A11yGateDto {
    fn build(&self) -> A11yGate {
        A11yGate {
            constraint: A11yConstraint {
                name: self.name.clone(),
                measured: self.measured,
                required_min: self.required_min,
            },
            droppable: self.droppable,
        }
    }
}

/// A capability the engine must honor (§4.2 能力协商). `advertised` is
/// three-valued: `Some(true)` confirmed, `Some(false)` denied, `None` could not
/// be negotiated (blocks, fail closed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGateDto {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised: Option<bool>,
    #[serde(default)]
    pub droppable: bool,
}

/// A claimed layer ↔ token/component mapping. `corroborating_atom`, when set, is
/// an index into the assessed atom order (`token_gates` first, then
/// `extra_atoms`) whose reverse-measured hold corroborates the claim; an unset
/// or dangling index leaves the mapping `null` (uncorroborated — never assumed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MappingClaimDto {
    pub layer_id: String,
    pub token_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corroborating_atom: Option<usize>,
}

/// Reverse-measure an external engine's artifact and gate it into an
/// `adapter_receipt/v1`. shuttle never generates pixels (red-line #6); the engine
/// generates *inside* the token hard constraints, and this request feeds shuttle the
/// produced artifact's layer geometry (`frame`) plus the gates to adjudicate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AssessRequest {
    pub surface_id: String,
    pub engine_id: String,
    pub engine_version: String,
    /// The `sha256:<hex>` digest of the engine artifact the geometry is read
    /// from.
    pub artifact_digest: String,
    /// The reverse-measurement frame built from the artifact's layer bounding
    /// boxes.
    pub frame: FrameDto,
    #[serde(default)]
    pub token_gates: Vec<TokenGateDto>,
    #[serde(default)]
    pub extra_atoms: Vec<AtomSpecDto>,
    #[serde(default)]
    pub a11y_gates: Vec<A11yGateDto>,
    #[serde(default)]
    pub capability_gates: Vec<CapabilityGateDto>,
    #[serde(default)]
    pub mappings: Vec<MappingClaimDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aesthetic: Option<AestheticDto>,
    /// `token:// -> numeric bound` resolver for metric token gates.
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
}

/// The adjudicated receipt plus its rolled-up disposition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssessResponse {
    pub receipt: AdapterReceipt,
    pub receipt_id: String,
    pub disposition: String,
    pub admissible: bool,
    pub needs_review: bool,
}

/// Reverse-measure + hard/soft-gate an engine artifact into a signed
/// `adapter_receipt/v1` (the Form B arm). The disposition/review flag are
/// computed by [`AdapterReceipt`]'s fold, so a failing artifact can never be
/// laundered into an `Adopt`.
pub fn assess(req: &AssessRequest) -> Result<AssessResponse, ClosedContractError> {
    let frame = req.frame.build()?;
    let digest =
        Sha256Digest::parse(req.artifact_digest.clone()).map_err(|e| invalid(e.to_string()))?;
    let basis = AdapterBasis {
        surface_id: req.surface_id.clone(),
        engine_id: req.engine_id.clone(),
        engine_version: req.engine_version.clone(),
        artifact_digest: digest,
        // shuttle stamps its own evaluator identity — the engine cannot self-certify
        // which reverse-measurement code adjudicated it.
        evaluator_version: EVALUATOR_VERSION.to_string(),
    };

    let mut token_gates = Vec::with_capacity(req.token_gates.len());
    for g in &req.token_gates {
        token_gates.push(TokenGate {
            name: g.name.clone(),
            atom: g.atom.build()?,
            droppable: g.droppable,
        });
    }
    let mut extra_atoms = Vec::with_capacity(req.extra_atoms.len());
    for spec in &req.extra_atoms {
        extra_atoms.push(spec.build()?);
    }
    let a11y_gates = req.a11y_gates.iter().map(A11yGateDto::build).collect();
    let capability_gates = req
        .capability_gates
        .iter()
        .map(|g| CapabilityGate {
            name: g.name.clone(),
            required: g.required,
            advertised: g.advertised,
            droppable: g.droppable,
        })
        .collect();
    let mappings = req
        .mappings
        .iter()
        .map(|m| MappingClaim {
            layer_id: m.layer_id.clone(),
            token_ref: m.token_ref.clone(),
            corroborating_atom: m.corroborating_atom,
        })
        .collect();
    let aesthetic = req.aesthetic.as_ref().map(AestheticDto::build);

    let input = AdapterInput {
        basis,
        frame,
        token_gates,
        extra_atoms,
        a11y_gates,
        capability_gates,
        mappings,
        aesthetic,
        thresholds: req.thresholds.clone(),
    };
    let receipt = adapter::assess(input)?;
    Ok(AssessResponse {
        receipt_id: receipt.receipt_id.to_string(),
        disposition: receipt.disposition.as_str().to_string(),
        admissible: receipt.disposition.is_admissible(),
        needs_review: receipt.needs_review,
        receipt,
    })
}

// ---------------------------------------------------------------------------
// compose (Form C: Figma-class canvas underlay)
// ---------------------------------------------------------------------------

/// Overlay a Form A skeleton (+ optional Form B fidelity layer) into a
/// `canvas_underlay/v1` — the machine-readable 底稿 (赋值 + 命题 + provenance) a
/// host front-end renders into an interactive Figma-class canvas (design §4.3).
///
/// The atoms are first re-verified against the skeleton geometry (the same
/// render=verify loop Form A uses), so the propositions the underlay carries are
/// `reversed`-provenance measurements — never fabricated — and remain the sole
/// truth source (red-line #7). The optional `fidelity` is an already-adjudicated
/// Form B `adapter_receipt/v1`; shuttle does not re-run the engine (red-line #6).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ComposeRequest {
    pub surface_id: String,
    pub scene: SceneDto,
    #[serde(default)]
    pub atoms: Vec<AtomSpecDto>,
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
    /// The already-adjudicated Form B fidelity layer to overlay, if any. It is
    /// re-validated at the boundary so a tampered receipt fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<AdapterReceipt>,
}

/// The composed canvas: the deterministic SVG skeleton the front-end overlays,
/// and the digest-bound underlay it renders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComposeResponse {
    pub svg: String,
    pub skeleton_digest: String,
    pub underlay: CanvasUnderlay,
    pub underlay_id: String,
    pub readiness: CanvasReadiness,
    pub needs_review: bool,
}

/// Compose a Form A skeleton + optional Form B fidelity layer into a signed
/// `canvas_underlay/v1` (the Form C arm). The readiness/review flag are computed
/// by [`CanvasUnderlay`]'s fold, so an inadmissible fidelity layer surfaces as
/// `Blocked` rather than being laundered into an overlay.
pub fn compose(req: &ComposeRequest) -> Result<ComposeResponse, ClosedContractError> {
    let scene = req.scene.build()?;
    let resolver = threshold_resolver(&req.thresholds);

    // Re-verify the declared atoms against the skeleton geometry so the underlay
    // carries reversed-provenance measurements, not raw unevaluated claims.
    let mut declared = Vec::with_capacity(req.atoms.len());
    for spec in &req.atoms {
        declared.push(spec.build()?);
    }
    let report = render_verify(&scene, &declared, &resolver)?;
    let atoms: Vec<CoordAtom> = report.checks.iter().map(|c| c.atom.clone()).collect();

    // Re-validate an embedded fidelity receipt at the boundary; a tampered
    // receipt fails closed before it can influence the composition.
    if let Some(receipt) = &req.fidelity {
        receipt.validate()?;
    }

    let input = CanvasInput {
        surface_id: req.surface_id.clone(),
        scene,
        atoms,
        fidelity: req.fidelity.clone(),
        evaluator_version: EVALUATOR_VERSION.to_string(),
    };
    let composition = canvas::compose(input)?;
    Ok(ComposeResponse {
        skeleton_digest: composition.underlay.basis.skeleton_digest.to_string(),
        underlay_id: composition.underlay.underlay_id.to_string(),
        readiness: composition.underlay.readiness,
        needs_review: composition.underlay.needs_review,
        svg: composition.svg,
        underlay: composition.underlay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{AllenRelation, AxisKind};

    fn axis_x() -> Axis {
        Axis::new("x", AxisKind::Metric)
    }

    fn order_spec() -> AtomSpecDto {
        AtomSpecDto {
            family: RelationFamily::Order,
            relation: "before".to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![axis_x()],
            allen: Some(AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        }
    }

    fn order_frame() -> FrameDto {
        FrameDto {
            tolerance: 0.5,
            boxes: vec![
                BoxDto {
                    element: "a".to_string(),
                    axis: "x".to_string(),
                    start: 0.0,
                    end: 10.0,
                },
                BoxDto {
                    element: "b".to_string(),
                    axis: "x".to_string(),
                    start: 20.0,
                    end: 30.0,
                },
            ],
        }
    }

    #[test]
    fn verify_happy_and_rollup() {
        let req = VerifyRequest {
            frame: order_frame(),
            atoms: vec![order_spec()],
            thresholds: BTreeMap::new(),
        };
        let resp = verify(&req).unwrap();
        assert_eq!(resp.decided_true, 1);
        assert_eq!(resp.decided_false, 0);
        assert_eq!(resp.unknown, 0);
        assert_eq!(resp.outcome, ProjectionOutcome::Satisfied);
        assert_eq!(resp.atoms[0].holds, Holds::True);
    }

    #[test]
    fn verify_missing_box_is_unknown() {
        let mut frame = order_frame();
        frame.boxes.pop(); // drop b -> proxy relation degrades to null
        let req = VerifyRequest {
            frame,
            atoms: vec![order_spec()],
            thresholds: BTreeMap::new(),
        };
        let resp = verify(&req).unwrap();
        assert_eq!(resp.unknown, 1);
        assert_eq!(resp.outcome, ProjectionOutcome::Indeterminate);
    }

    #[test]
    fn verify_rejects_bad_interval() {
        let req = VerifyRequest {
            frame: FrameDto {
                tolerance: 0.0,
                boxes: vec![BoxDto {
                    element: "a".to_string(),
                    axis: "x".to_string(),
                    start: 10.0,
                    end: 0.0, // end < start
                }],
            },
            atoms: vec![],
            thresholds: BTreeMap::new(),
        };
        assert!(verify(&req).is_err());
    }

    #[test]
    fn verify_metric_uses_thresholds() {
        let spec = AtomSpecDto {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![Axis::new("w", AxisKind::Metric)],
            allen: None,
            threshold_ref: Some("token://size/cta-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        };
        let frame = FrameDto {
            tolerance: 0.0,
            boxes: vec![BoxDto {
                element: "cta".to_string(),
                axis: "w".to_string(),
                start: 0.0,
                end: 44.0,
            }],
        };
        let mut thresholds = BTreeMap::new();
        thresholds.insert("token://size/cta-min".to_string(), 44.0);
        let resp = verify(&VerifyRequest {
            frame,
            atoms: vec![spec],
            thresholds,
        })
        .unwrap();
        assert_eq!(resp.atoms[0].holds, Holds::True);
    }

    #[test]
    fn adopt_assembles_signed_projection() {
        let req = AdoptRequest {
            surface_id: "screen:home".to_string(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            frame: order_frame(),
            atoms: vec![order_spec()],
            principles: vec![PrincipleDto {
                name: "Ordered".to_string(),
                formula: FormulaDto::Atom { atom: 0 },
                is_geometric_proxy: false,
            }],
            thresholds: BTreeMap::new(),
        };
        let resp = adopt(&req).unwrap();
        resp.projection.validate().unwrap();
        assert_eq!(resp.outcome, ProjectionOutcome::Satisfied);
        assert!(resp.projection_id.starts_with("sha256:"));
    }

    #[test]
    fn adopt_rejects_bad_digest() {
        let req = AdoptRequest {
            surface_id: "screen:home".to_string(),
            artifact_digest: "not-a-digest".to_string(),
            frame: order_frame(),
            atoms: vec![order_spec()],
            principles: vec![],
            thresholds: BTreeMap::new(),
        };
        assert!(adopt(&req).is_err());
    }

    #[test]
    fn adopt_rejects_dangling_formula_index() {
        let req = AdoptRequest {
            surface_id: "screen:home".to_string(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            frame: order_frame(),
            atoms: vec![order_spec()],
            principles: vec![PrincipleDto {
                name: "Ordered".to_string(),
                formula: FormulaDto::Atom { atom: 9 }, // out of range
                is_geometric_proxy: false,
            }],
            thresholds: BTreeMap::new(),
        };
        assert!(adopt(&req).is_err());
    }

    #[test]
    fn couple_types_and_classifies() {
        let req = CoupleRequest {
            dimensions: vec![
                DimensionDto {
                    name: "gap".to_string(),
                    value: DimValueDto::Metric { value: 8.0 },
                    range: LegalRangeDto::Interval {
                        min: 0.0,
                        max: 64.0,
                    },
                },
                DimensionDto {
                    name: "weight".to_string(),
                    value: DimValueDto::Metric { value: 400.0 }, // metric vs ordinal range
                    range: LegalRangeDto::Ordinal {
                        allowed: vec![100, 400, 700],
                    },
                },
            ],
            couplings: vec![
                CouplingDto {
                    from: "a".to_string(),
                    to: "b".to_string(),
                    kind: CouplingKindDto::Value,
                },
                CouplingDto {
                    from: "b".to_string(),
                    to: "a".to_string(),
                    kind: CouplingKindDto::Value,
                },
            ],
            a11y: vec![A11yDto {
                name: "target-size".to_string(),
                measured: None, // unknown -> blocks
                required_min: 44.0,
            }],
        };
        let resp = couple(&req).unwrap();
        assert_eq!(resp.lexicon.len(), 2);
        assert_eq!(resp.lexicon[0].dimension, "gap");
        assert_eq!(resp.lexicon[0].status, "ok");
        assert_eq!(resp.lexicon[1].status, "type_mismatch");
        assert_eq!(resp.solver_class, "solver"); // a<->b cycle
        assert_eq!(resp.a11y[0].verdict, "blocked");
        assert!(!resp.admissible);
    }

    fn render_scene() -> SceneDto {
        SceneDto {
            width: 320.0,
            height: 200.0,
            tolerance: 0.5,
            elements: vec![
                ElementDto {
                    id: "a".to_string(),
                    x: [0.0, 100.0],
                    y: [0.0, 40.0],
                    text: Some("A".to_string()),
                    role: None,
                },
                ElementDto {
                    id: "b".to_string(),
                    x: [120.0, 220.0],
                    y: [0.0, 40.0],
                    text: Some("B".to_string()),
                    role: None,
                },
            ],
        }
    }

    #[test]
    fn render_reproves_and_is_deterministic() {
        let req = RenderRequest {
            scene: render_scene(),
            atoms: vec![order_spec()],
            thresholds: BTreeMap::new(),
            adopt_surface_id: None,
            principles: vec![],
        };
        let r1 = render(&req).unwrap();
        let r2 = render(&req).unwrap();
        assert_eq!(r1.artifact_digest, r2.artifact_digest);
        assert_eq!(r1.html, r2.html);
        assert!(r1.consistent);
        assert_eq!(r1.verified, 1);
        assert_eq!(r1.regressions, 0);
        assert_eq!(r1.outcome, ProjectionOutcome::Satisfied);
        assert!(r1.artifact_digest.starts_with("sha256:"));
        assert!(r1.projection.is_none());
    }

    #[test]
    fn render_flags_regression() {
        // Declare containment but lay the boxes out disjoint (decidable family).
        let scene = SceneDto {
            width: 320.0,
            height: 200.0,
            tolerance: 0.0,
            elements: vec![
                ElementDto {
                    id: "a".to_string(),
                    x: [200.0, 300.0],
                    y: [0.0, 40.0],
                    text: None,
                    role: None,
                },
                ElementDto {
                    id: "b".to_string(),
                    x: [0.0, 100.0],
                    y: [0.0, 40.0],
                    text: None,
                    role: None,
                },
            ],
        };
        let spec = AtomSpecDto {
            family: RelationFamily::Containment,
            relation: "contains".to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![axis_x()],
            allen: Some(AllenRelation::Contains),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        };
        let resp = render(&RenderRequest {
            scene,
            atoms: vec![spec],
            thresholds: BTreeMap::new(),
            adopt_surface_id: None,
            principles: vec![],
        })
        .unwrap();
        assert!(!resp.consistent);
        assert_eq!(resp.regressions, 1);
        assert_eq!(resp.outcome, ProjectionOutcome::Violated);
    }

    #[test]
    fn render_with_adopt_binds_projection_to_artifact() {
        let req = RenderRequest {
            scene: render_scene(),
            atoms: vec![order_spec()],
            thresholds: BTreeMap::new(),
            adopt_surface_id: Some("screen:home".to_string()),
            principles: vec![PrincipleDto {
                name: "Ordered".to_string(),
                formula: FormulaDto::Atom { atom: 0 },
                is_geometric_proxy: false,
            }],
        };
        let resp = render(&req).unwrap();
        let projection = resp.projection.expect("adopt requested");
        projection.validate().unwrap();
        // The projection basis is bound to the exact rendered artifact digest.
        assert_eq!(
            projection.basis.artifact_digest.as_str(),
            resp.artifact_digest
        );
        assert_eq!(projection.outcome, ProjectionOutcome::Satisfied);
    }

    #[test]
    fn requests_round_trip_json() {
        let req = VerifyRequest {
            frame: order_frame(),
            atoms: vec![order_spec()],
            thresholds: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: VerifyRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn requests_reject_unknown_fields() {
        let mut value = serde_json::to_value(VerifyRequest {
            frame: order_frame(),
            atoms: vec![],
            thresholds: BTreeMap::new(),
        })
        .unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(serde_json::from_slice::<VerifyRequest>(&bytes).is_err());
    }

    // -- assess (Form B) --------------------------------------------------

    fn cta_gate_spec() -> AtomSpecDto {
        AtomSpecDto {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![Axis::new("w", AxisKind::Metric)],
            allen: None,
            threshold_ref: Some("token://size/cta-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        }
    }

    fn cta_frame(width: f64) -> FrameDto {
        FrameDto {
            tolerance: 0.0,
            boxes: vec![BoxDto {
                element: "cta".to_string(),
                axis: "w".to_string(),
                start: 0.0,
                end: width,
            }],
        }
    }

    fn base_assess(width: f64) -> AssessRequest {
        let mut thresholds = BTreeMap::new();
        thresholds.insert("token://size/cta-min".to_string(), 44.0);
        AssessRequest {
            surface_id: "screen:checkout".to_string(),
            engine_id: "v0".to_string(),
            engine_version: "2026.9".to_string(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            frame: cta_frame(width),
            token_gates: vec![],
            extra_atoms: vec![],
            a11y_gates: vec![],
            capability_gates: vec![],
            mappings: vec![],
            aesthetic: None,
            thresholds,
        }
    }

    #[test]
    fn assess_adopts_clean_artifact() {
        let mut req = base_assess(44.0);
        req.token_gates = vec![TokenGateDto {
            name: "token:cta-min-width".to_string(),
            atom: cta_gate_spec(),
            droppable: false,
        }];
        let resp = assess(&req).unwrap();
        assert_eq!(resp.disposition, "adopt");
        assert!(resp.admissible);
        assert!(!resp.needs_review);
        // shuttle stamps its own evaluator version, not the engine's word for it.
        assert_eq!(resp.receipt.basis.evaluator_version, EVALUATOR_VERSION);
        assert!(resp.receipt_id.starts_with("sha256:"));
        resp.receipt.validate().unwrap();
    }

    #[test]
    fn assess_token_below_bound_rejects() {
        let mut req = base_assess(20.0); // below 44 -> false
        req.token_gates = vec![TokenGateDto {
            name: "token:cta-min-width".to_string(),
            atom: cta_gate_spec(),
            droppable: false,
        }];
        let resp = assess(&req).unwrap();
        assert_eq!(resp.disposition, "reject");
        assert!(!resp.admissible);
    }

    #[test]
    fn assess_a11y_unknown_fails_closed() {
        let mut req = base_assess(44.0);
        req.a11y_gates = vec![A11yGateDto {
            name: "contrast".to_string(),
            measured: None, // unknown -> blocked -> reject
            required_min: 4.5,
            droppable: false,
        }];
        let resp = assess(&req).unwrap();
        assert_eq!(resp.disposition, "reject");
    }

    #[test]
    fn assess_capability_unnegotiated_rejects() {
        let mut req = base_assess(44.0);
        req.capability_gates = vec![CapabilityGateDto {
            name: "token-grid-snap".to_string(),
            required: true,
            advertised: None, // could not negotiate -> blocked
            droppable: false,
        }];
        let resp = assess(&req).unwrap();
        assert_eq!(resp.disposition, "reject");
    }

    #[test]
    fn assess_aesthetic_soft_flag_sidecars() {
        let mut req = base_assess(44.0);
        req.aesthetic = Some(AestheticDto {
            score: 0.4,
            floor: 0.7,
            rationale_ref: "engine://aesthetic/weak-hierarchy".to_string(),
        });
        let resp = assess(&req).unwrap();
        assert_eq!(resp.disposition, "sidecar");
        assert!(resp.admissible);
        assert!(resp.needs_review);
    }

    #[test]
    fn assess_mapping_grounds_in_measured_atom() {
        let mut req = base_assess(44.0);
        req.token_gates = vec![TokenGateDto {
            name: "token:cta-min-width".to_string(),
            atom: cta_gate_spec(),
            droppable: false,
        }];
        req.mappings = vec![MappingClaimDto {
            layer_id: "layer:cta".to_string(),
            token_ref: "token://component/button".to_string(),
            corroborating_atom: Some(0),
        }];
        let resp = assess(&req).unwrap();
        let m = resp
            .receipt
            .mappings
            .iter()
            .find(|m| m.layer_id == "layer:cta")
            .unwrap();
        assert_eq!(m.verified, Holds::True);
    }

    #[test]
    fn assess_rejects_bad_digest() {
        let mut req = base_assess(44.0);
        req.artifact_digest = "not-a-digest".to_string();
        assert!(assess(&req).is_err());
    }

    #[test]
    fn assess_request_round_trips_json() {
        let mut req = base_assess(44.0);
        req.token_gates = vec![TokenGateDto {
            name: "token:cta-min-width".to_string(),
            atom: cta_gate_spec(),
            droppable: false,
        }];
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: AssessRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn assess_request_rejects_unknown_fields() {
        let mut value = serde_json::to_value(base_assess(44.0)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(serde_json::from_slice::<AssessRequest>(&bytes).is_err());
    }

    // --- compose (Form C) -------------------------------------------------

    fn compose_scene() -> SceneDto {
        SceneDto {
            width: 320.0,
            height: 200.0,
            tolerance: 0.5,
            elements: vec![
                ElementDto {
                    id: "cta".to_string(),
                    x: [0.0, 120.0],
                    y: [0.0, 44.0],
                    text: Some("Buy".to_string()),
                    role: None,
                },
                ElementDto {
                    id: "hint".to_string(),
                    x: [140.0, 300.0],
                    y: [0.0, 44.0],
                    text: None,
                    role: Some("note".to_string()),
                },
            ],
        }
    }

    /// Build a real Form B receipt (via the assess orchestrator) that maps the
    /// `cta` layer, choosing width relative to the 44px token bound.
    fn compose_fidelity(width: f64, droppable: bool) -> AdapterReceipt {
        let mut req = base_assess(width);
        req.surface_id = "surface:home".to_string();
        req.token_gates = vec![TokenGateDto {
            name: "token:cta-min-width".to_string(),
            atom: cta_gate_spec(),
            droppable,
        }];
        req.mappings = vec![MappingClaimDto {
            layer_id: "cta".to_string(),
            token_ref: "token://component/button".to_string(),
            corroborating_atom: Some(0),
        }];
        assess(&req).unwrap().receipt
    }

    fn compose_order_spec() -> AtomSpecDto {
        AtomSpecDto {
            family: RelationFamily::Order,
            relation: "before".to_string(),
            args: vec!["cta".to_string(), "hint".to_string()],
            axes: vec![axis_x()],
            allen: Some(AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        }
    }

    fn base_compose() -> ComposeRequest {
        ComposeRequest {
            surface_id: "surface:home".to_string(),
            scene: compose_scene(),
            atoms: vec![compose_order_spec()],
            thresholds: BTreeMap::new(),
            fidelity: None,
        }
    }

    #[test]
    fn compose_skeleton_only_without_fidelity() {
        let resp = compose(&base_compose()).unwrap();
        assert_eq!(resp.readiness, CanvasReadiness::SkeletonOnly);
        assert!(!resp.needs_review);
        assert!(resp.svg.contains("<svg"));
        assert!(resp.underlay.basis.fidelity_digest.is_none());
        // The skeleton digest in the response binds the emitted SVG bytes.
        assert_eq!(resp.skeleton_digest, digest_of(resp.svg.as_bytes()));
        resp.underlay.validate().unwrap();
    }

    #[test]
    fn compose_adopts_and_binds_mapped_node() {
        let mut req = base_compose();
        req.fidelity = Some(compose_fidelity(44.0, false));
        let resp = compose(&req).unwrap();
        assert_eq!(resp.readiness, CanvasReadiness::Composited);
        let cta = resp
            .underlay
            .nodes
            .iter()
            .find(|n| n.node_id == "cta")
            .unwrap();
        assert_eq!(cta.token_ref.as_deref(), Some("token://component/button"));
        assert_eq!(cta.fidelity_verified, Some(Holds::True));
        resp.underlay.validate().unwrap();
    }

    #[test]
    fn compose_rejected_fidelity_blocks_and_strips_bindings() {
        let mut req = base_compose();
        // Non-droppable token failure -> Reject -> Blocked, no node binding.
        req.fidelity = Some(compose_fidelity(20.0, false));
        let resp = compose(&req).unwrap();
        assert_eq!(resp.readiness, CanvasReadiness::Blocked);
        assert!(resp.needs_review);
        assert!(resp.underlay.nodes.iter().all(|n| !n.has_fidelity()));
        resp.underlay.validate().unwrap();
    }

    #[test]
    fn compose_request_round_trips_json() {
        let mut req = base_compose();
        req.fidelity = Some(compose_fidelity(44.0, false));
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: ComposeRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn compose_request_rejects_unknown_fields() {
        let mut value = serde_json::to_value(base_compose()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(serde_json::from_slice::<ComposeRequest>(&bytes).is_err());
    }
}
