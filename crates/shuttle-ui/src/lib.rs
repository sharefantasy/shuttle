//! Formal UI proposition evaluation for shuttle.
//!
//! `shuttle-protocol` defines the *data model* for formal UI propositions — the
//! atomic `coord_atom/v1`, the composite `design_principle/v1`, and the bundled
//! `ui_projection/v1`. This crate is the *evaluation and generation engine* that
//! turns those schemas into verdicts and back into a renderable artifact:
//!
//! * [`geom`] — the **evidence arm**. Given the bounding boxes of elements in a
//!   frame, it reverse-engineers each atom's Allen interval relation and reads
//!   it through the six UI families to a three-valued [`shuttle_protocol::Holds`],
//!   emitting `reversed` provenance and honouring the sufficient-condition proxy
//!   discipline (a geometric *false* on a proxy family degrades to `null`).
//! * [`constraint`] — the **constraint arm**. An L1 lexicon type-checks each
//!   dimension against its legal range; a dimension coupling graph classifies
//!   the L2 arbitration complexity floor (lint vs Cassowary-class solver); a
//!   non-degradable a11y hard constraint fails closed.
//! * [`render`] — the **generation arm (Form A)**. A deterministic HTML/CSS
//!   projector emits an artifact whose geometry, when re-measured, re-derives
//!   the very atoms it was built from — the *render = verify* loop.
//! * [`adapter`] — the **Form B arm**. shuttle never generates pixels (red-line #6);
//!   an external pixel engine generates *inside* shuttle's token hard constraints,
//!   and this arm reverse-measures the engine's produced artifact, gates it with
//!   the §3.3 hard/soft dichotomy (token 硬门 + a11y/可区分性 + capability
//!   negotiation fail closed; the engine's aesthetic self-report only flags),
//!   and emits a component-mapping receipt — never silently failing.
//!
//! * [`canvas`] — the **Form C arm**. It overlays the Form A SVG skeleton
//!   with an optional Form B fidelity layer into a single digest-bound
//!   `canvas_underlay/v1` — the machine-readable 底稿 (赋值 + 命题 +
//!   provenance) a host front-end renders into an interactive Figma-class
//!   canvas. The canvas is a render product, never a truth source
//!   (red-line #7); shuttle neither builds the canvas UI nor owns its pixels
//!   (red-lines #6 / #11), and an inadmissible fidelity layer surfaces as
//!   `Blocked` rather than being laundered into an overlay.
//!
//! Every arm reasons and reports; none of them mutate a workspace, own a
//! physical path, or hold land rights (red-line #11).

pub mod adapter;
pub mod canvas;
pub mod constraint;
pub mod geom;
pub mod render;

/// The serde wire boundary and orchestration facade over the arms:
/// `verify` (evidence), `couple` (constraint), `render` (generation),
/// `adopt` (bind a verified artifact into a `ui_projection/v1`), and
/// `assess` (Form B: gate an external engine's artifact into an
/// `adapter_receipt/v1`), and `compose` (Form C: overlay a Form A
/// skeleton + optional Form B layer into a `canvas_underlay/v1`).
pub mod request;
