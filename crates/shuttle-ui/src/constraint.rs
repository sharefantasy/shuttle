//! The constraint arm: lexicon typing, a dimension coupling graph, CSP-class
//! arbitration, and a non-degradable a11y hard constraint.
//!
//! Layering (§14):
//! * **L1 lexicon** — every dimension declares `[value, legal_range]`, the
//!   type-system analogue. A value outside its range is a *type error*, decided
//!   mechanically (no proxy blindness — the range is a contract).
//! * **L2 grammar/arbitration** — cross-dimension constraints form a graph. The
//!   graph *topology* sets the L2 complexity floor: an acyclic, sparse graph is
//!   lintable in one pass; a cyclic, dense graph needs a Cassowary-class
//!   solver. This module classifies the topology and runs the cheap path when
//!   it can.
//! * **a11y** — accessibility constraints are hard, cross-dimension, and
//!   *non-degradable* (the fifth provenance class). They fail closed: an
//!   undecided a11y constraint blocks, it never degrades to "probably fine".
//!
//! This module reasons and reports; it never mutates a workspace (red-line #11).

use std::collections::{BTreeMap, BTreeSet};

/// A numeric legal range `[min, max]` (inclusive), or an enumerated ordinal set.
#[derive(Debug, Clone, PartialEq)]
pub enum LegalRange {
    Interval { min: f64, max: f64 },
    Ordinal { allowed: BTreeSet<i64> },
}

impl LegalRange {
    pub fn interval(min: f64, max: f64) -> Self {
        LegalRange::Interval { min, max }
    }

    pub fn ordinal(values: impl IntoIterator<Item = i64>) -> Self {
        LegalRange::Ordinal {
            allowed: values.into_iter().collect(),
        }
    }

    fn admits(&self, value: &DimValue) -> bool {
        match (self, value) {
            (LegalRange::Interval { min, max }, DimValue::Metric(v)) => v >= min && v <= max,
            (LegalRange::Ordinal { allowed }, DimValue::Ordinal(v)) => allowed.contains(v),
            // A metric value against an ordinal range (or vice versa) is a type
            // mismatch, not merely out of range.
            _ => false,
        }
    }
}

/// A dimension's concrete value: a continuous magnitude or an ordinal index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DimValue {
    Metric(f64),
    Ordinal(i64),
}

/// One dimension in the lexicon.
#[derive(Debug, Clone)]
pub struct Dimension {
    pub name: String,
    pub value: DimValue,
    pub range: LegalRange,
}

/// The outcome of L1 type-checking one dimension.
#[derive(Debug, Clone, PartialEq)]
pub enum LexiconVerdict {
    Ok,
    OutOfRange { name: String },
    TypeMismatch { name: String },
}

impl Dimension {
    pub fn check(&self) -> LexiconVerdict {
        let type_ok = matches!(
            (&self.value, &self.range),
            (DimValue::Metric(_), LegalRange::Interval { .. })
                | (DimValue::Ordinal(_), LegalRange::Ordinal { .. })
        );
        if !type_ok {
            return LexiconVerdict::TypeMismatch {
                name: self.name.clone(),
            };
        }
        if self.range.admits(&self.value) {
            LexiconVerdict::Ok
        } else {
            LexiconVerdict::OutOfRange {
                name: self.name.clone(),
            }
        }
    }
}

/// How two dimensions are coupled (§16). Orthogonal dimensions have *no* edge:
/// A ⊥ B ⟺ changing A never changes B's legal domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CouplingKind {
    /// A's value directly determines B's value.
    Value,
    /// A's value reshapes B's legal *range* (domain coupling).
    Constraint,
    /// The coupling only manifests across time (animation/transition).
    Time,
}

/// A directed coupling edge `from -> to` of a given kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Coupling {
    pub from: String,
    pub to: String,
    pub kind: CouplingKind,
}

/// The complexity floor the coupling topology imposes on L2 arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolverClass {
    /// Acyclic and sparse: a single propagation pass (a lint) suffices.
    Lint,
    /// Cyclic or dense: a Cassowary-class constraint solver is required.
    Solver,
}

/// The dimension coupling graph.
#[derive(Debug, Clone, Default)]
pub struct CouplingGraph {
    dims: BTreeSet<String>,
    edges: Vec<Coupling>,
}

impl CouplingGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_dimension(&mut self, name: impl Into<String>) {
        self.dims.insert(name.into());
    }

    pub fn couple(&mut self, from: impl Into<String>, to: impl Into<String>, kind: CouplingKind) {
        let from = from.into();
        let to = to.into();
        self.dims.insert(from.clone());
        self.dims.insert(to.clone());
        let edge = Coupling { from, to, kind };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    /// Two dimensions are orthogonal iff no edge connects them in either
    /// direction (precise §16 definition, restricted to declared couplings).
    pub fn orthogonal(&self, a: &str, b: &str) -> bool {
        !self
            .edges
            .iter()
            .any(|e| (e.from == a && e.to == b) || (e.from == b && e.to == a))
    }

    /// Detect a directed cycle over the non-time edges. Time couplings do not
    /// participate in the static (instantaneous) arbitration graph.
    pub fn has_static_cycle(&self) -> bool {
        let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &self.edges {
            if e.kind != CouplingKind::Time {
                adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
            }
        }
        // Iterative DFS with color marking.
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color: BTreeMap<&str, Color> = self
            .dims
            .iter()
            .map(|d| (d.as_str(), Color::White))
            .collect();

        fn visit<'a>(
            node: &'a str,
            adj: &BTreeMap<&'a str, Vec<&'a str>>,
            color: &mut BTreeMap<&'a str, Color>,
        ) -> bool {
            color.insert(node, Color::Gray);
            if let Some(neigh) = adj.get(node) {
                for &n in neigh {
                    match color.get(n).copied().unwrap_or(Color::White) {
                        Color::Gray => return true,
                        Color::White => {
                            if visit(n, adj, color) {
                                return true;
                            }
                        }
                        Color::Black => {}
                    }
                }
            }
            color.insert(node, Color::Black);
            false
        }

        let nodes: Vec<&str> = self.dims.iter().map(String::as_str).collect();
        for node in nodes {
            if color.get(node).copied() == Some(Color::White) && visit(node, &adj, &mut color) {
                return true;
            }
        }
        false
    }

    /// Edge density = |E| / |V| (average out-degree). Above `1.5` a graph is
    /// "dense" enough that single-pass propagation is unsafe.
    fn density(&self) -> f64 {
        if self.dims.is_empty() {
            0.0
        } else {
            self.edges.len() as f64 / self.dims.len() as f64
        }
    }

    /// Classify the L2 solver complexity floor from the topology.
    pub fn solver_class(&self) -> SolverClass {
        if self.has_static_cycle() || self.density() > 1.5 {
            SolverClass::Solver
        } else {
            SolverClass::Lint
        }
    }
}

/// An accessibility hard constraint. It is non-degradable: it must be positively
/// satisfied, and an unknown blocks.
#[derive(Debug, Clone)]
pub struct A11yConstraint {
    pub name: String,
    /// The measured value, if known. `None` means "could not measure".
    pub measured: Option<f64>,
    /// The minimum required value (e.g. a 4.5:1 contrast ratio, a 44px target).
    pub required_min: f64,
}

/// The three-valued outcome of an a11y constraint. Unlike geometric proxies,
/// there is no "trust the false" degradation — an unknown *blocks*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A11yVerdict {
    Pass,
    Fail,
    /// Could not measure: fail closed (blocks the gate).
    Blocked,
}

impl A11yConstraint {
    pub fn check(&self) -> A11yVerdict {
        match self.measured {
            Some(v) if v >= self.required_min => A11yVerdict::Pass,
            Some(_) => A11yVerdict::Fail,
            None => A11yVerdict::Blocked,
        }
    }
}

/// The rolled-up arbitration report over a lexicon + coupling graph + a11y set.
#[derive(Debug, Clone, PartialEq)]
pub struct ArbitrationReport {
    pub lexicon: Vec<LexiconVerdict>,
    pub solver_class: SolverClass,
    pub a11y: Vec<(String, A11yVerdict)>,
    /// True iff every lexicon check is Ok and no a11y constraint fails or blocks.
    pub admissible: bool,
}

/// Run the whole arbitration: L1 type-check, L2 topology classification, and
/// the non-degradable a11y gate.
pub fn arbitrate(
    dims: &[Dimension],
    graph: &CouplingGraph,
    a11y: &[A11yConstraint],
) -> ArbitrationReport {
    let lexicon: Vec<LexiconVerdict> = dims.iter().map(Dimension::check).collect();
    let a11y_verdicts: Vec<(String, A11yVerdict)> =
        a11y.iter().map(|c| (c.name.clone(), c.check())).collect();
    let lexicon_ok = lexicon.iter().all(|v| *v == LexiconVerdict::Ok);
    let a11y_ok = a11y_verdicts.iter().all(|(_, v)| *v == A11yVerdict::Pass);
    ArbitrationReport {
        lexicon,
        solver_class: graph.solver_class(),
        a11y: a11y_verdicts,
        admissible: lexicon_ok && a11y_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexicon_ok_out_of_range_and_mismatch() {
        let ok = Dimension {
            name: "gap".to_string(),
            value: DimValue::Metric(8.0),
            range: LegalRange::interval(0.0, 64.0),
        };
        assert_eq!(ok.check(), LexiconVerdict::Ok);

        let oor = Dimension {
            name: "gap".to_string(),
            value: DimValue::Metric(128.0),
            range: LegalRange::interval(0.0, 64.0),
        };
        assert!(matches!(oor.check(), LexiconVerdict::OutOfRange { .. }));

        let mism = Dimension {
            name: "weight".to_string(),
            value: DimValue::Metric(400.0),
            range: LegalRange::ordinal([100, 400, 700]),
        };
        assert!(matches!(mism.check(), LexiconVerdict::TypeMismatch { .. }));
    }

    #[test]
    fn ordinal_admission() {
        let d = Dimension {
            name: "weight".to_string(),
            value: DimValue::Ordinal(700),
            range: LegalRange::ordinal([100, 400, 700]),
        };
        assert_eq!(d.check(), LexiconVerdict::Ok);
    }

    #[test]
    fn orthogonality_and_lint_topology() {
        let mut g = CouplingGraph::new();
        g.add_dimension("color");
        g.add_dimension("radius");
        g.couple("width", "columns", CouplingKind::Constraint);
        assert!(g.orthogonal("color", "radius"));
        assert!(!g.orthogonal("width", "columns"));
        assert_eq!(g.solver_class(), SolverClass::Lint);
    }

    #[test]
    fn cycle_forces_solver() {
        let mut g = CouplingGraph::new();
        g.couple("a", "b", CouplingKind::Value);
        g.couple("b", "c", CouplingKind::Value);
        g.couple("c", "a", CouplingKind::Value);
        assert!(g.has_static_cycle());
        assert_eq!(g.solver_class(), SolverClass::Solver);
    }

    #[test]
    fn time_edges_excluded_from_static_cycle() {
        let mut g = CouplingGraph::new();
        g.couple("a", "b", CouplingKind::Value);
        g.couple("b", "a", CouplingKind::Time); // only a time coupling closes it
        assert!(!g.has_static_cycle());
    }

    #[test]
    fn density_forces_solver() {
        // A pure DAG whose edge/vertex ratio exceeds 1.5 must route to the
        // solver even without a cycle. A hub fanning out to leaves plus a few
        // cross edges gives |E|/|V| > 1.5 while staying acyclic.
        let mut g = CouplingGraph::new();
        g.couple("h", "a", CouplingKind::Value);
        g.couple("h", "b", CouplingKind::Value);
        g.couple("h", "c", CouplingKind::Value);
        g.couple("h", "d", CouplingKind::Value);
        g.couple("a", "b", CouplingKind::Constraint);
        g.couple("a", "c", CouplingKind::Constraint);
        g.couple("b", "d", CouplingKind::Constraint);
        g.couple("c", "d", CouplingKind::Constraint);
        // vertices h,a,b,c,d = 5; edges = 8 -> density 1.6 > 1.5.
        assert!(!g.has_static_cycle());
        assert_eq!(g.solver_class(), SolverClass::Solver);
    }

    #[test]
    fn sparse_acyclic_is_lint() {
        let mut g = CouplingGraph::new();
        g.couple("a", "b", CouplingKind::Value);
        g.couple("b", "c", CouplingKind::Constraint);
        assert!(!g.has_static_cycle());
        assert_eq!(g.solver_class(), SolverClass::Lint);
    }

    #[test]
    fn a11y_pass_fail_block() {
        let pass = A11yConstraint {
            name: "contrast".to_string(),
            measured: Some(4.6),
            required_min: 4.5,
        };
        assert_eq!(pass.check(), A11yVerdict::Pass);
        let fail = A11yConstraint {
            name: "contrast".to_string(),
            measured: Some(3.0),
            required_min: 4.5,
        };
        assert_eq!(fail.check(), A11yVerdict::Fail);
        let blocked = A11yConstraint {
            name: "contrast".to_string(),
            measured: None,
            required_min: 4.5,
        };
        assert_eq!(blocked.check(), A11yVerdict::Blocked);
    }

    #[test]
    fn arbitrate_blocks_on_a11y_unknown() {
        let dims = vec![Dimension {
            name: "gap".to_string(),
            value: DimValue::Metric(8.0),
            range: LegalRange::interval(0.0, 64.0),
        }];
        let graph = CouplingGraph::new();
        let a11y = vec![A11yConstraint {
            name: "target-size".to_string(),
            measured: None,
            required_min: 44.0,
        }];
        let report = arbitrate(&dims, &graph, &a11y);
        assert!(!report.admissible); // unknown a11y fails closed
    }
}
