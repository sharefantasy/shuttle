//! The generation arm (Form A): a deterministic HTML/CSS projector with a
//! `render = verify` loop.
//!
//! A [`Scene`] is a constraint-satisfying *assignment* of bounding boxes to
//! named elements — not pixels invented from a prompt, but the concrete layout
//! that a lexicon/CSP arbitration (see [`crate::constraint`]) has already found
//! admissible. This module projects that assignment to a self-contained,
//! byte-deterministic HTML/CSS document.
//!
//! The discipline that makes this trustworthy is the *render = verify* loop:
//! the same [`Scene`] that produced the HTML is turned into a [`geom::Frame`],
//! and every declared [`CoordAtom`] is re-evaluated against it. If the projected
//! geometry does not re-derive an atom the caller asserted, the atom is reported
//! as a *regression* — the projector never silently ships a layout that violates
//! its own propositions. A geometric proxy that degrades to `null` is reported
//! as unverified (not a regression); only a decided `false` is a regression.
//!
//! The projector is pure: it returns a `String` and never writes a file, owns a
//! path, or holds land rights (red-line #11). Persisting the artifact is the
//! caller's (authority owner's) job.

use crate::geom::{self, Frame, Interval};
use shuttle_protocol::anchor::Sha256Digest;
use shuttle_protocol::{CoordAtom, Holds, ProjectionBasis, ProjectionOutcome, UiProjection};

/// One positioned element in a scene. Its extent is a half-open interval on the
/// `x` (horizontal) and `y` (vertical) axes, matching the axis names the
/// geometry arm reads.
#[derive(Debug, Clone)]
pub struct Element {
    pub id: String,
    pub x: Interval,
    pub y: Interval,
    /// Optional visible text (escaped on projection).
    pub text: Option<String>,
    /// Optional ARIA/semantic role, emitted as `role="..."`.
    pub role: Option<String>,
}

impl Element {
    pub fn new(id: impl Into<String>, x: Interval, y: Interval) -> Self {
        Self {
            id: id.into(),
            x,
            y,
            text: None,
            role: None,
        }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }
}

/// A constraint-satisfying layout assignment ready to project.
#[derive(Debug, Clone)]
pub struct Scene {
    pub width: f64,
    pub height: f64,
    pub elements: Vec<Element>,
    /// Endpoint tolerance carried into the verify frame (sub-pixel rounding).
    pub tolerance: f64,
}

impl Scene {
    pub fn new(width: f64, height: f64, tolerance: f64) -> Self {
        Self {
            width,
            height,
            elements: Vec::new(),
            tolerance,
        }
    }

    pub fn push(&mut self, element: Element) {
        self.elements.push(element);
    }

    /// Elements in a stable, id-sorted order so projection is byte-deterministic
    /// regardless of insertion order.
    fn ordered(&self) -> Vec<&Element> {
        let mut v: Vec<&Element> = self.elements.iter().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Build the geometry frame this scene encodes, for the verify half of the
    /// loop.
    pub fn to_frame(&self) -> Frame {
        let mut frame = Frame::new(self.tolerance);
        for e in &self.elements {
            frame.set(e.id.clone(), "x", e.x);
            frame.set(e.id.clone(), "y", e.y);
        }
        frame
    }

    /// Project the scene to a self-contained HTML/CSS document. Output is
    /// deterministic: elements are emitted in id order and every number is
    /// formatted with a fixed precision.
    pub fn to_html(&self) -> String {
        let mut out = String::new();
        out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
        out.push_str("<meta charset=\"utf-8\">\n");
        out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
        out.push_str("<title>shuttle ui projection</title>\n<style>\n");
        out.push_str("  * { margin: 0; box-sizing: border-box; }\n");
        out.push_str(&format!(
            "  .shuttle-frame {{ position: relative; width: {}px; height: {}px; }}\n",
            fmt_num(self.width),
            fmt_num(self.height)
        ));
        out.push_str("  .shuttle-el { position: absolute; overflow: hidden; }\n");
        // Per-element rules, id-ordered.
        for e in self.ordered() {
            out.push_str(&format!(
                "  #{sel} {{ left: {l}px; top: {t}px; width: {w}px; height: {h}px; }}\n",
                sel = css_ident(&e.id),
                l = fmt_num(e.x.start),
                t = fmt_num(e.y.start),
                w = fmt_num(e.x.length()),
                h = fmt_num(e.y.length()),
            ));
        }
        out.push_str("</style>\n</head>\n<body>\n");
        out.push_str("<div class=\"shuttle-frame\">\n");
        for e in self.ordered() {
            let role = e
                .role
                .as_ref()
                .map(|r| format!(" role=\"{}\"", escape_attr(r)))
                .unwrap_or_default();
            let text = e.text.as_deref().map(escape_text).unwrap_or_default();
            out.push_str(&format!(
                "  <div class=\"shuttle-el\" id=\"{id}\"{role}>{text}</div>\n",
                id = escape_attr(&e.id),
                role = role,
                text = text,
            ));
        }
        out.push_str("</div>\n</body>\n</html>\n");
        out
    }

    /// Project the scene to a deterministic SVG *skeleton* — the Form A layer a
    /// Figma-class canvas (Form C, design §4.3) overlays with Form B's
    /// high-fidelity layers. Every element becomes an id-tagged group holding a
    /// `<rect>` for its bounding box and, when present, a centred `<text>`
    /// label. Output is byte-deterministic: groups are emitted in id order and
    /// every number is formatted with a fixed precision. The SVG is a *render
    /// product*, never a truth source (red-line #7): the propositions it depicts
    /// still live in the [`CoordAtom`]s the caller carries alongside it.
    pub fn to_svg(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n",
            w = fmt_num(self.width),
            h = fmt_num(self.height),
        ));
        out.push_str("  <g class=\"shuttle-skeleton\">\n");
        for e in self.ordered() {
            let role = e
                .role
                .as_ref()
                .map(|r| format!(" role=\"{}\"", escape_attr(r)))
                .unwrap_or_default();
            out.push_str(&format!(
                "    <g id=\"{id}\"{role}>\n",
                id = escape_attr(&e.id),
                role = role,
            ));
            out.push_str(&format!(
                "      <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"none\" />\n",
                x = fmt_num(e.x.start),
                y = fmt_num(e.y.start),
                w = fmt_num(e.x.length()),
                h = fmt_num(e.y.length()),
            ));
            if let Some(text) = e.text.as_deref() {
                out.push_str(&format!(
                    "      <text x=\"{cx}\" y=\"{cy}\" text-anchor=\"middle\" dominant-baseline=\"central\">{t}</text>\n",
                    cx = fmt_num(e.x.start + e.x.length() / 2.0),
                    cy = fmt_num(e.y.start + e.y.length() / 2.0),
                    t = escape_text(text),
                ));
            }
            out.push_str("    </g>\n");
        }
        out.push_str("  </g>\n");
        out.push_str("</svg>\n");
        out
    }
}

/// The per-atom result of the verify half of the render=verify loop.
#[derive(Debug, Clone, PartialEq)]
pub struct AtomCheck {
    /// The re-evaluated atom (identity preserved from the input proposition).
    pub atom: CoordAtom,
    /// The re-derived truth from the projected geometry.
    pub holds: Holds,
    /// True iff the projected geometry decides the atom *false* — the layout
    /// contradicts a proposition it was supposed to satisfy.
    pub regression: bool,
    /// True iff the atom could not be decided (proxy blind-spot / missing box):
    /// the projection did not prove it, but did not break it either.
    pub unverified: bool,
}

/// The rolled-up report of projecting a scene and re-verifying its atoms.
#[derive(Debug, Clone)]
pub struct RenderVerifyReport {
    pub html: String,
    pub checks: Vec<AtomCheck>,
    /// True iff no atom regressed (every declared atom is either re-proved or,
    /// at worst, left unverified — none is contradicted).
    pub consistent: bool,
    /// Count of atoms the projection positively re-proved.
    pub verified: usize,
    /// Count of atoms left unverified (blind-spot / missing box).
    pub unverified: usize,
    /// Count of atoms the projection contradicted.
    pub regressions: usize,
}

/// Run the full render=verify loop: project `scene` to HTML, then re-evaluate
/// every atom in `atoms` against the geometry that HTML encodes.
///
/// `threshold_of` resolves metric atoms' `token://` thresholds, exactly as in
/// [`geom::evaluate_atom`].
pub fn render_verify(
    scene: &Scene,
    atoms: &[CoordAtom],
    threshold_of: &dyn Fn(&str) -> Option<f64>,
) -> Result<RenderVerifyReport, shuttle_protocol::ClosedContractError> {
    let html = scene.to_html();
    let frame = scene.to_frame();
    let mut checks = Vec::with_capacity(atoms.len());
    let (mut verified, mut unverified, mut regressions) = (0usize, 0usize, 0usize);
    for atom in atoms {
        let evaluated = geom::evaluate_atom(atom, &frame, threshold_of)?;
        let holds = evaluated.holds;
        let regression = holds == Holds::False;
        let is_unverified = holds == Holds::Null;
        match holds {
            Holds::True => verified += 1,
            Holds::Null => unverified += 1,
            Holds::False => regressions += 1,
        }
        checks.push(AtomCheck {
            atom: evaluated,
            holds,
            regression,
            unverified: is_unverified,
        });
    }
    Ok(RenderVerifyReport {
        html,
        checks,
        consistent: regressions == 0,
        verified,
        unverified,
        regressions,
    })
}

/// Assemble a signed [`UiProjection`] from a render=verify pass. The projection
/// is content-addressed over the re-evaluated atoms so it is bound to the exact
/// geometry the artifact encodes; `artifact_digest` should be the digest of the
/// projected HTML the caller persists.
pub fn projection_from_report(
    surface_id: &str,
    artifact_digest: Sha256Digest,
    evaluator_version: &str,
    report: &RenderVerifyReport,
    principles: Vec<shuttle_protocol::DesignPrinciple>,
) -> Result<UiProjection, shuttle_protocol::ClosedContractError> {
    let basis = ProjectionBasis {
        surface_id: surface_id.to_string(),
        artifact_digest,
        evaluator_version: evaluator_version.to_string(),
    };
    let atoms: Vec<CoordAtom> = report.checks.iter().map(|c| c.atom.clone()).collect();
    UiProjection::assemble(basis, atoms, principles)
}

/// The projection outcome implied by a report, for callers that only need the
/// three-valued roll-up rather than a full signed bundle.
pub fn outcome_of(report: &RenderVerifyReport) -> ProjectionOutcome {
    if report.regressions > 0 {
        ProjectionOutcome::Violated
    } else if report.unverified > 0 {
        ProjectionOutcome::Indeterminate
    } else {
        ProjectionOutcome::Satisfied
    }
}

/// Format a number deterministically: integers without a fractional part, else
/// three decimal places, with a normalised `-0` -> `0`.
fn fmt_num(n: f64) -> String {
    let n = if n == 0.0 { 0.0 } else { n };
    if (n.round() - n).abs() < f64::EPSILON {
        format!("{}", n.round() as i64)
    } else {
        format!("{n:.3}")
    }
}

/// Sanitise an id into a CSS identifier usable after `#`. Non `[A-Za-z0-9_-]`
/// characters become `_`, and a leading digit is prefixed to keep it valid.
fn css_ident(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for (i, c) in id.chars().enumerate() {
        let ok = c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if i == 0 && c.is_ascii_digit() {
            out.push('_');
        }
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{
        AllenRelation, Axis, AxisKind, CoordAtom, CoordAtomProposition, RelationFamily,
        TimeQuantifier,
    };

    fn iv(s: f64, e: f64) -> Interval {
        Interval::new(s, e).unwrap()
    }

    fn scene_two() -> Scene {
        let mut s = Scene::new(320.0, 200.0, 0.5);
        s.push(Element::new("a", iv(0.0, 100.0), iv(0.0, 40.0)).with_text("A"));
        s.push(Element::new("b", iv(120.0, 220.0), iv(0.0, 40.0)).with_text("B"));
        s
    }

    fn order_atom() -> CoordAtom {
        CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Order,
            relation: "before".to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![Axis::new("x", AxisKind::Metric)],
            allen: Some(AllenRelation::Before),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap()
    }

    #[test]
    fn html_is_deterministic_and_id_ordered() {
        let s = scene_two();
        let h1 = s.to_html();
        let h2 = s.to_html();
        assert_eq!(h1, h2);
        // "a" rule appears before "b" rule regardless of push order.
        let ia = h1.find("#a {").unwrap();
        let ib = h1.find("#b {").unwrap();
        assert!(ia < ib);
        assert!(h1.contains("width: 100px"));
        assert!(h1.contains("left: 120px"));
    }

    #[test]
    fn render_verify_reproves_declared_atom() {
        let s = scene_two();
        let report = render_verify(&s, &[order_atom()], &|_| None).unwrap();
        assert!(report.consistent);
        assert_eq!(report.verified, 1);
        assert_eq!(report.regressions, 0);
        assert_eq!(outcome_of(&report), ProjectionOutcome::Satisfied);
    }

    #[test]
    fn render_verify_flags_regression() {
        // Declare "a before b" but lay them out b-then-a on a decidable family.
        let mut s = Scene::new(320.0, 200.0, 0.0);
        s.push(Element::new("a", iv(200.0, 300.0), iv(0.0, 40.0)));
        s.push(Element::new("b", iv(0.0, 100.0), iv(0.0, 40.0)));
        // Containment is decidable both ways, so a false is a real regression.
        let atom = CoordAtom::unevaluated(CoordAtomProposition {
            family: RelationFamily::Containment,
            relation: "contains".to_string(),
            args: vec!["a".to_string(), "b".to_string()],
            axes: vec![Axis::new("x", AxisKind::Metric)],
            allen: Some(AllenRelation::Contains),
            threshold_ref: None,
            frame: "viewport".to_string(),
            time_quantifier: TimeQuantifier::At,
        })
        .unwrap();
        let report = render_verify(&s, &[atom], &|_| None).unwrap();
        assert!(!report.consistent);
        assert_eq!(report.regressions, 1);
        assert_eq!(outcome_of(&report), ProjectionOutcome::Violated);
    }

    #[test]
    fn render_verify_unverified_is_not_regression() {
        // Missing box -> null -> unverified, not a regression.
        let mut s = Scene::new(320.0, 200.0, 0.5);
        s.push(Element::new("a", iv(0.0, 100.0), iv(0.0, 40.0)));
        let report = render_verify(&s, &[order_atom()], &|_| None).unwrap();
        assert!(report.consistent);
        assert_eq!(report.unverified, 1);
        assert_eq!(report.verified, 0);
        assert_eq!(outcome_of(&report), ProjectionOutcome::Indeterminate);
    }

    #[test]
    fn css_ident_sanitises() {
        assert_eq!(css_ident("a.b c"), "a_b_c");
        assert_eq!(css_ident("1x"), "_1x");
        assert_eq!(css_ident(""), "_");
    }

    #[test]
    fn text_is_escaped() {
        let mut s = Scene::new(10.0, 10.0, 0.0);
        s.push(Element::new("a", iv(0.0, 5.0), iv(0.0, 5.0)).with_text("<b>&"));
        let h = s.to_html();
        assert!(h.contains("&lt;b&gt;&amp;"));
        assert!(!h.contains("<b>&<"));
    }

    #[test]
    fn projection_from_report_is_signed() {
        let s = scene_two();
        let report = render_verify(&s, &[order_atom()], &|_| None).unwrap();
        let digest = Sha256Digest::parse(format!("sha256:{}", "0".repeat(64))).unwrap();
        let proj =
            projection_from_report("surface/home", digest, "shuttle-ui/0.2.0", &report, vec![])
                .unwrap();
        proj.validate().unwrap();
        assert_eq!(proj.outcome, ProjectionOutcome::Satisfied);
    }
}
