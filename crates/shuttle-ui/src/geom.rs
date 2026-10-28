//! Bounding-box geometry to three-valued `coord_atom` truth.
//!
//! This is the evidence arm: given the rendered/declared bounding boxes of UI
//! elements in a frame, it derives, on each named axis, the Allen interval
//! relation between element pairs and reads it through the six UI families to
//! *evaluate* a [`CoordAtom`] proposition to a [`Holds`].
//!
//! Discipline (§7/§20 — never guess):
//! * Geometry is a **sufficient-condition proxy**. A geometric `true` may be
//!   trusted; a geometric `false` is a *blind-spot* — it is returned as `null`
//!   for proxy families unless the caller has corroborating evidence, because
//!   "the boxes don't look aligned" does not prove "not aligned" (sub-pixel
//!   rounding, transforms, off-frame content).
//! * A relation over a box that is *absent* from the frame is `null`, never
//!   `false`.
//! * The evaluator only ever emits `reversed` provenance (it reverse-engineers
//!   truth from a rendered artifact). It never emits `fabricated` and never
//!   invents a `design_system` grounding.

use shuttle_protocol::{
    AllenRelation, Axis, AxisKind, CoordAtom, Holds, Provenance, ProvenanceClass, RelationFamily,
    VerificationTier,
};
use std::collections::BTreeMap;

/// A half-open interval `[start, end)` in one axis's coordinate space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub start: f64,
    pub end: f64,
}

impl Interval {
    pub fn new(start: f64, end: f64) -> Option<Self> {
        if start.is_finite() && end.is_finite() && end >= start {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub fn length(&self) -> f64 {
        self.end - self.start
    }
}

/// A named element's extent along one axis. The frame maps element -> axis ->
/// interval, so an atom on axis `x` reads the `x` interval of each arg.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    boxes: BTreeMap<String, BTreeMap<String, Interval>>,
    /// The comparison tolerance (in coordinate units) below which two endpoints
    /// are treated as equal. Sub-tolerance differences are *alignment*, not a
    /// strict order.
    tolerance: f64,
}

impl Frame {
    pub fn new(tolerance: f64) -> Self {
        Self {
            boxes: BTreeMap::new(),
            tolerance: if tolerance.is_finite() && tolerance >= 0.0 {
                tolerance
            } else {
                0.0
            },
        }
    }

    /// Register an element's interval on a given axis.
    pub fn set(&mut self, element: impl Into<String>, axis: impl Into<String>, interval: Interval) {
        self.boxes
            .entry(element.into())
            .or_default()
            .insert(axis.into(), interval);
    }

    fn interval(&self, element: &str, axis: &str) -> Option<Interval> {
        self.boxes.get(element)?.get(axis).copied()
    }
}

/// Compare two endpoints with the frame tolerance. Returns Ordering-like -1/0/1.
fn cmp_eps(a: f64, b: f64, eps: f64) -> i8 {
    if (a - b).abs() <= eps {
        0
    } else if a < b {
        -1
    } else {
        1
    }
}

/// Derive the Allen relation of interval `p` against interval `q` on one axis,
/// under an endpoint tolerance. Shared endpoints (within tolerance) collapse to
/// the meeting/aligned relations exactly as Allen's algebra prescribes.
pub fn allen_of(p: Interval, q: Interval, eps: f64) -> AllenRelation {
    let ps_qs = cmp_eps(p.start, q.start, eps);
    let ps_qe = cmp_eps(p.start, q.end, eps);
    let pe_qs = cmp_eps(p.end, q.start, eps);
    let pe_qe = cmp_eps(p.end, q.end, eps);

    if pe_qs < 0 {
        return AllenRelation::Before;
    }
    if pe_qs == 0 {
        return AllenRelation::Meets;
    }
    if ps_qe > 0 {
        return AllenRelation::After;
    }
    if ps_qe == 0 {
        return AllenRelation::MetBy;
    }
    // Now the intervals genuinely overlap. Disambiguate by endpoint equality.
    match (ps_qs, pe_qe) {
        (0, 0) => AllenRelation::Equals,
        (0, -1) => AllenRelation::Starts,
        (0, 1) => AllenRelation::StartedBy,
        (1, 0) => AllenRelation::Finishes,
        (-1, 0) => AllenRelation::FinishedBy,
        (-1, 1) => AllenRelation::Contains,
        (1, -1) => AllenRelation::During,
        (-1, -1) => AllenRelation::Overlaps,
        (1, 1) => AllenRelation::OverlappedBy,
        // (0,0) handled; remaining combos are unreachable given the guards.
        _ => AllenRelation::Equals,
    }
}

/// The result of evaluating one atom against a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeomVerdict {
    /// The geometry decides the truth and it may be trusted.
    Decided(Holds),
    /// The geometry could not decide (missing box, or a proxy blind-spot). The
    /// atom degrades to `null`.
    BlindSpot,
}

impl GeomVerdict {
    pub fn holds(self) -> Holds {
        match self {
            GeomVerdict::Decided(h) => h,
            GeomVerdict::BlindSpot => Holds::Null,
        }
    }
}

/// Whether a family is a geometric proxy whose *false* must degrade to a
/// blind-spot rather than assert a refutation.
fn is_proxy_family(family: RelationFamily) -> bool {
    // Order/Adjacency/Alignment/Overlap are read purely from box geometry and a
    // "not-holding" can be a rounding/transform artifact. Containment and Metric
    // are decidable both ways from the boxes themselves.
    matches!(
        family,
        RelationFamily::Order
            | RelationFamily::Adjacency
            | RelationFamily::Alignment
            | RelationFamily::Overlap
    )
}

/// Does the observed Allen relation satisfy the family the atom asserts?
fn family_satisfied(observed: AllenRelation, asserted: RelationFamily) -> bool {
    observed.family() == asserted
}

/// Evaluate a single [`CoordAtom`] against a [`Frame`], returning a new atom
/// carrying the geometry verdict as its `holds` with `reversed` provenance
/// (when decided). The proposition identity is preserved.
///
/// `threshold_of` resolves a metric atom's `token://` threshold to a numeric
/// bound; if it returns `None` for a metric atom, the metric cannot be decided
/// and degrades to a blind-spot.
pub fn evaluate_atom(
    atom: &CoordAtom,
    frame: &Frame,
    threshold_of: &dyn Fn(&str) -> Option<f64>,
) -> Result<CoordAtom, shuttle_protocol::ClosedContractError> {
    let verdict = verdict_for(atom, frame, threshold_of);
    let prop = shuttle_protocol::CoordAtomProposition {
        family: atom.family,
        relation: atom.relation.clone(),
        args: atom.args.clone(),
        axes: atom.axes.clone(),
        allen: atom.allen,
        threshold_ref: atom.threshold_ref.clone(),
        frame: atom.frame.clone(),
        time_quantifier: atom.time_quantifier,
    };
    match verdict.holds() {
        Holds::Null => CoordAtom::unevaluated(prop),
        decided => CoordAtom::evaluated(
            prop,
            decided,
            Provenance::new(ProvenanceClass::Reversed, "reversed://frame-geometry"),
            VerificationTier::Fsm,
        ),
    }
}

fn verdict_for(
    atom: &CoordAtom,
    frame: &Frame,
    threshold_of: &dyn Fn(&str) -> Option<f64>,
) -> GeomVerdict {
    // Metric atoms compare a magnitude against a threshold rather than an
    // interval relation.
    if atom.family == RelationFamily::Metric {
        return metric_verdict(atom, frame, threshold_of);
    }

    // Relational atoms: fold the per-axis Allen reading across every axis.
    // A relation must hold on *all* declared axes (∧). The first pair is the
    // subject/object; multi-arg relations require the relation to hold for each
    // consecutive pair (a left_of b left_of c).
    let mut overall = Holds::True;
    for axis in &atom.axes {
        let axis_holds = axis_relation_holds(atom, frame, &axis.name);
        overall = overall.and(axis_holds);
    }

    match overall {
        Holds::True => GeomVerdict::Decided(Holds::True),
        Holds::False => {
            if is_proxy_family(atom.family) {
                // Sufficient-condition proxy: a false is a blind-spot.
                GeomVerdict::BlindSpot
            } else {
                GeomVerdict::Decided(Holds::False)
            }
        }
        Holds::Null => GeomVerdict::BlindSpot,
    }
}

/// The Kleene truth of the atom's family on one axis, across all consecutive
/// arg pairs. Missing boxes make the reading `null`.
fn axis_relation_holds(atom: &CoordAtom, frame: &Frame, axis: &str) -> Holds {
    let mut result = Holds::True;
    for pair in atom.args.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let (pi, qi) = match (frame.interval(a, axis), frame.interval(b, axis)) {
            (Some(pi), Some(qi)) => (pi, qi),
            _ => return Holds::Null, // absent box -> unknown, never false
        };
        let observed = allen_of(pi, qi, frame.tolerance);
        // If the atom pins a specific Allen relation, require an exact match;
        // otherwise require the family to match.
        let holds = match atom.allen {
            Some(pinned) => bool_to_holds(observed == pinned),
            None => bool_to_holds(family_satisfied(observed, atom.family)),
        };
        result = result.and(holds);
    }
    result
}

fn metric_verdict(
    atom: &CoordAtom,
    frame: &Frame,
    threshold_of: &dyn Fn(&str) -> Option<f64>,
) -> GeomVerdict {
    let token = match &atom.threshold_ref {
        Some(t) => t,
        None => return GeomVerdict::BlindSpot,
    };
    let bound = match threshold_of(token) {
        Some(b) => b,
        None => return GeomVerdict::BlindSpot,
    };
    // A unary metric measures the first arg's length on the (single) axis.
    let element = match atom.args.first() {
        Some(e) => e,
        None => return GeomVerdict::BlindSpot,
    };
    let axis = match atom.axes.first() {
        Some(a) => a,
        None => return GeomVerdict::BlindSpot,
    };
    // An ordinal axis carries no magnitude; a metric comparison on it is
    // undecidable (§15 static-font ordinal vs variable-font metric).
    if axis.kind == AxisKind::Ordinal {
        return GeomVerdict::BlindSpot;
    }
    let interval = match frame.interval(element, &axis.name) {
        Some(i) => i,
        None => return GeomVerdict::BlindSpot,
    };
    let measured = interval.length();
    let holds = match atom.relation.as_str() {
        "min_width" | "min_size" | "min_length" | "at_least" => measured + frame.tolerance >= bound,
        "max_width" | "max_size" | "max_length" | "at_most" => measured <= bound + frame.tolerance,
        "exact_width" | "exact_size" | "equals" => (measured - bound).abs() <= frame.tolerance,
        // Metric is decidable both ways from the box, so an unknown relation
        // name is a genuine blind-spot rather than a guessed truth.
        _ => return GeomVerdict::BlindSpot,
    };
    GeomVerdict::Decided(bool_to_holds(holds))
}

fn bool_to_holds(b: bool) -> Holds {
    if b {
        Holds::True
    } else {
        Holds::False
    }
}

/// Convenience: build an axis with a metric kind.
pub fn metric_axis(name: &str) -> Axis {
    Axis::new(name, AxisKind::Metric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{CoordAtomProposition, TimeQuantifier};

    fn iv(s: f64, e: f64) -> Interval {
        Interval::new(s, e).unwrap()
    }

    #[test]
    fn allen_before_and_meets() {
        assert_eq!(
            allen_of(iv(0.0, 10.0), iv(20.0, 30.0), 0.0),
            AllenRelation::Before
        );
        assert_eq!(
            allen_of(iv(0.0, 20.0), iv(20.0, 30.0), 0.0),
            AllenRelation::Meets
        );
        assert_eq!(
            allen_of(iv(0.0, 10.0), iv(0.0, 10.0), 0.0),
            AllenRelation::Equals
        );
        assert_eq!(
            allen_of(iv(5.0, 8.0), iv(0.0, 20.0), 0.0),
            AllenRelation::During
        );
        assert_eq!(
            allen_of(iv(0.0, 20.0), iv(5.0, 8.0), 0.0),
            AllenRelation::Contains
        );
    }

    #[test]
    fn tolerance_collapses_to_meets() {
        // 20.3 vs 20.0 within tolerance 0.5 -> Meets, not Overlaps.
        assert_eq!(
            allen_of(iv(0.0, 20.3), iv(20.0, 30.0), 0.5),
            AllenRelation::Meets
        );
    }

    fn order_atom() -> CoordAtom {
        CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Order,
            relation: "before".to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![metric_axis("x")],
            allen: Some(AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap()
    }

    #[test]
    fn order_true_is_trusted() {
        let mut frame = Frame::new(0.5);
        frame.set("a", "x", iv(0.0, 10.0));
        frame.set("b", "x", iv(20.0, 30.0));
        let out = evaluate_atom(&order_atom(), &frame, &|_| None).unwrap();
        assert_eq!(out.holds, Holds::True);
        assert_eq!(out.atom_id, order_atom().atom_id); // identity preserved
    }

    #[test]
    fn order_false_is_blind_spot_null() {
        let mut frame = Frame::new(0.5);
        // b is before a — the asserted "a before b" does not hold geometrically.
        frame.set("a", "x", iv(20.0, 30.0));
        frame.set("b", "x", iv(0.0, 10.0));
        let out = evaluate_atom(&order_atom(), &frame, &|_| None).unwrap();
        // Proxy family: a geometric false degrades to null, never asserts false.
        assert_eq!(out.holds, Holds::Null);
    }

    #[test]
    fn missing_box_is_null() {
        let mut frame = Frame::new(0.5);
        frame.set("a", "x", iv(0.0, 10.0)); // b missing
        let out = evaluate_atom(&order_atom(), &frame, &|_| None).unwrap();
        assert_eq!(out.holds, Holds::Null);
    }

    #[test]
    fn containment_false_is_decidable() {
        let atom = CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Containment,
            relation: "contains".to_string(),
            args: vec!["outer".to_string(), "inner".to_string()],
            axes: vec![metric_axis("x")],
            allen: Some(AllenRelation::Contains),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap();
        let mut frame = Frame::new(0.0);
        // inner is NOT contained; containment is decidable both ways.
        frame.set("outer", "x", iv(0.0, 10.0));
        frame.set("inner", "x", iv(50.0, 60.0));
        let out = evaluate_atom(&atom, &frame, &|_| None).unwrap();
        assert_eq!(out.holds, Holds::False);
    }

    #[test]
    fn metric_min_width_against_threshold() {
        let atom = CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![metric_axis("w")],
            allen: None,
            threshold_ref: Some("token://size/cta-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap();
        let mut frame = Frame::new(0.0);
        frame.set("cta", "w", iv(0.0, 44.0));
        let out = evaluate_atom(&atom, &frame, &|t| {
            if t == "token://size/cta-min" {
                Some(44.0)
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(out.holds, Holds::True);

        // Below the bound -> false (metric is decidable both ways).
        let mut small = Frame::new(0.0);
        small.set("cta", "w", iv(0.0, 20.0));
        let out2 = evaluate_atom(&atom, &small, &|_| Some(44.0)).unwrap();
        assert_eq!(out2.holds, Holds::False);
    }

    #[test]
    fn metric_without_threshold_is_blind_spot() {
        let atom = CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["cta".to_string()],
            axes: vec![metric_axis("w")],
            allen: None,
            threshold_ref: Some("token://size/cta-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap();
        let mut frame = Frame::new(0.0);
        frame.set("cta", "w", iv(0.0, 44.0));
        let out = evaluate_atom(&atom, &frame, &|_| None).unwrap(); // resolver misses
        assert_eq!(out.holds, Holds::Null);
    }

    #[test]
    fn ordinal_axis_metric_is_blind_spot() {
        let atom = CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Metric,
            relation: "min_width".to_string(),
            args: vec!["glyph".to_string()],
            axes: vec![Axis::new("weight", AxisKind::Ordinal)],
            allen: None,
            threshold_ref: Some("token://type/weight-min".to_string()),
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap();
        let mut frame = Frame::new(0.0);
        frame.set("glyph", "weight", iv(0.0, 700.0));
        let out = evaluate_atom(&atom, &frame, &|_| Some(400.0)).unwrap();
        assert_eq!(out.holds, Holds::Null);
    }

    #[test]
    fn reversed_provenance_only() {
        let mut frame = Frame::new(0.5);
        frame.set("a", "x", iv(0.0, 10.0));
        frame.set("b", "x", iv(20.0, 30.0));
        let out = evaluate_atom(&order_atom(), &frame, &|_| None).unwrap();
        assert_eq!(
            out.provenance.as_ref().unwrap().class,
            ProvenanceClass::Reversed
        );
    }
}
