//! `shuttle ui` subcommand dispatcher — the validator for formal UI propositions.
//!
//! Maps `shuttle ui <verb>` to the corresponding orchestrator in
//! [`shuttle_ui::request`] and formats output as JSON on stdout. Every verb reads a
//! single closed-schema request document as JSON — from `--file <path>` or, when
//! omitted, from stdin — and prints the corresponding response document.
//!
//! * `verify`  — the **evidence arm**: re-measure a frame's bounding boxes and
//!   read every declared `coord_atom/v1` to a three-valued verdict.
//! * `couple`  — the **constraint arm**: L1 lexicon type-check + L2 coupling
//!   arbitration + non-degradable a11y gate.
//! * `render`  — the **generation arm (Form A)**: emit deterministic HTML/CSS
//!   whose geometry re-derives the atoms it was built from (render = verify),
//!   optionally binding the artifact into a `ui_projection/v1`.
//! * `adopt`   — bind an already-verified artifact digest, its atoms, and its
//!   principles into a signed `ui_projection/v1`.
//! * `assess`  — the **Form B arm** (design §4.2): reverse-measure an external
//!   pixel engine's produced artifact and gate it with the §3.3 hard/soft
//!   dichotomy (token 硬门 + a11y/可区分性 + capability negotiation fail closed;
//!   the engine's aesthetic self-report only flags) into an `adapter_receipt/v1`
//!   with a component-mapping — never silently failing. shuttle never generates the
//!   pixels itself (red-line #6).
//! * `canvas`  — the **Form C arm** (design §4.3): overlay a Form A SVG skeleton
//!   with an optional already-adjudicated Form B fidelity layer into one
//!   machine-readable digest-bound `canvas_underlay/v1` (赋值 + 命题 +
//!   provenance) that a host front-end renders into a Figma-class interactive
//!   canvas. The canvas is a **render product, not a truth source** (truth stays
//!   in the coord_atom propositions, red-line #7); shuttle builds neither the canvas
//!   UI nor the pixel engine (red-lines #6 / #11) and never silently fails
//!   (§4.2) — an inadmissible fidelity layer surfaces as `Blocked` with no
//!   laundered bindings.
//! * `receipt` — the **evidence-owner emit path** (design §6.3): seal a verified
//!   `ui_projection/v1` 底稿 into a digest-bound `pde_campaign_phase_receipt/v1`
//!   whose `owner_artifact` references the projection and whose `receipt_digest`
//!   is byte-compatible with pde-campaign's own `bind_digest`. This is a
//!   **read-only mirror** of pde-campaign's FROZEN schema (N4 discipline): shuttle
//!   emits a cross-verifiable owner receipt but NEVER becomes a second truth
//!   source for the campaign (red-line #7 / #11).
//!
//! Like every `shuttle` verb this only reasons and reports: it never mutates a
//! workspace, owns a physical path, or holds land rights (red-line #11). The
//! `receipt` verb's only side effect is writing shuttle's own sealed receipt sidecar
//! (under `--out` or `<root>/.shuttle/receipts/`), never the campaign store.

use std::io::Read;
use std::path::Path;

use shuttle_protocol::UiPhaseReceiptRequest;
use shuttle_ui::request::{adopt, assess, compose, couple, render, verify};

#[derive(Debug, clap::Subcommand)]
pub enum UiCmd {
    /// Evidence arm: evaluate declared `coord_atom/v1` propositions against a
    /// measured frame, rolling up a three-valued outcome. Reads a
    /// `VerifyRequest` JSON document.
    Verify {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Constraint arm: L1 lexicon type-check + L2 coupling arbitration + a11y
    /// hard gate. Reads a `CoupleRequest` JSON document.
    Couple {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Generation arm (Form A): render deterministic HTML/CSS and re-prove the
    /// declared atoms against the rendered geometry. Reads a `RenderRequest`
    /// JSON document; with `adopt_surface_id` set it also emits a signed
    /// projection bound to the artifact digest.
    Render {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Bind a verified artifact digest, its atoms, and its principles into a
    /// signed `ui_projection/v1`. Reads an `AdoptRequest` JSON document.
    Adopt {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Form B arm (design §4.2): reverse-measure an external engine's produced
    /// artifact and gate it (token 硬门 + a11y + capability negotiation, with an
    /// engine aesthetic soft score) into an `adapter_receipt/v1`. Reads an
    /// `AssessRequest` JSON document. shuttle never generates the pixels (red-line #6).
    Assess {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Form C arm (design §4.3): overlay a Form A SVG skeleton with an optional
    /// already-adjudicated Form B fidelity layer into a digest-bound
    /// `canvas_underlay/v1` a host front-end renders into a Figma-class canvas.
    /// Reads a `ComposeRequest` JSON document. The canvas is a render product,
    /// not a truth source (red-line #7); an inadmissible layer surfaces as
    /// `Blocked` (§4.2), never a silent overlay.
    Canvas {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
    },
    /// Evidence-owner emit path (design §6.3): seal a verified `ui_projection/v1`
    /// into a digest-bound `pde_campaign_phase_receipt/v1`. Reads a
    /// `UiPhaseReceiptRequest` JSON document, runs the fail-closed
    /// [`shuttle_protocol::UiPhaseReceiptRequest::seal`] gate (projection self-verify,
    /// 「绝不瞎猜」refusal of a decided effect on a non-satisfied projection,
    /// digest binding byte-compatible with pde's kernel), and emits the sealed
    /// receipt as a single compact JSON line — to `--out <path>` (atomically) or
    /// stdout. shuttle mirrors pde-campaign's FROZEN schema read-only; it never writes
    /// the campaign store (red-line #7 / #11).
    Receipt {
        /// Path to the request JSON; reads stdin when omitted.
        #[arg(long)]
        file: Option<String>,
        /// Destination for the sealed receipt line. Writes atomically to this
        /// path when given (`-` or omitted ⇒ stdout).
        #[arg(long)]
        out: Option<String>,
    },
}

pub fn run(cmd: UiCmd) -> anyhow::Result<()> {
    match cmd {
        UiCmd::Verify { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = verify(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Couple { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = couple(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Render { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = render(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Adopt { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = adopt(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Assess { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = assess(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Canvas { file } => {
            let raw = read_input(file)?;
            let req = serde_json::from_str(&raw)?;
            let resp = compose(&req)?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        UiCmd::Receipt { file, out } => {
            let raw = read_input(file)?;
            // Parse through the closed-schema request type (`deny_unknown_fields`)
            // so a stray key is rejected before any sealing work.
            let request: UiPhaseReceiptRequest = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("ui receipt request rejected: {e}"))?;
            // The fail-closed evidence-owner gate: validates the projection,
            // refuses to fabricate a decided effect on a blind-spot projection
            // (「绝不瞎猜」), and binds the receipt digest exactly like pde's
            // kernel so pde can cross-verify without shuttle ever owning the campaign.
            let receipt = request
                .seal()
                .map_err(|e| anyhow::anyhow!("ui receipt seal failed (fail-closed): {e}"))?;
            emit_receipt(&receipt, out.as_deref())?;
        }
    }
    Ok(())
}

/// Read the request document from `--file <path>`, or from stdin when omitted.
fn read_input(file: Option<String>) -> anyhow::Result<String> {
    match file {
        Some(path) => Ok(std::fs::read_to_string(path)?),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

/// Emit the sealed receipt as a single compact JSON line. With a concrete
/// `--out` path the line is written atomically (create-new temp + fsync +
/// rename) so a consumer never observes a half-written receipt; `-` or `None`
/// prints to stdout on its own line. The bytes on the wire are exactly the
/// canonical receipt body — the same bytes the `receipt_digest` binds.
fn emit_receipt(
    receipt: &shuttle_protocol::PdeCampaignPhaseReceipt,
    dest: Option<&str>,
) -> anyhow::Result<()> {
    let line = serde_json::to_string(receipt)?;
    match dest {
        Some(path) if path != "-" => {
            atomic_write(Path::new(path), line.as_bytes())?;
        }
        _ => {
            println!("{line}");
        }
    }
    Ok(())
}

/// Atomically write `bytes` (plus a trailing newline) to `path` via a
/// create-new temp file, fsync, and rename — the same emit discipline as
/// `provider_receipt::atomic_write`.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("failed to create receipt dir {parent:?}: {e}"))?;
        }
    }
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| anyhow::anyhow!("failed to open temp receipt {temporary:?}: {e}"))?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
        .map_err(|e| anyhow::anyhow!("failed to publish receipt {path:?}: {e}"))?;
    Ok(())
}
