//! Binary-level CLI integration tests for `shuttle ui` — the U-series formal-UI
//! proposition evidence surface .
//! Spawns the REAL `shuttle` binary and drives all four verbs end-to-end through
//! their closed JSON wire boundary, exercising both the `--file` and stdin
//! input faces:
//!
//!   * `ui verify`  — evidence arm: a proxy-family `order` atom reads `true`
//!     against a well-separated frame (`satisfied`); dropping one box degrades
//!     the geometric false to `null` (`indeterminate`), never a false.
//!   * `ui couple`  — constraint arm: an a<->b value cycle forces the `solver`
//!     class; an a11y hard constraint with no measurement fails closed to
//!     `blocked`, making the arbitration inadmissible.
//!   * `ui render`  — generation arm (Form A): the emitted artifact's geometry
//!     re-proves the atom it was built from (`verified`, zero regressions) and
//!     the response carries a content-addressed `sha256:` artifact digest.
//!   * `ui adopt`   — binds a verified digest + atoms + principles into a signed
//!     `ui_projection/v1`; a malformed digest fails closed (non-zero exit).
//!
//! The iron contract under test (§7/§20「绝不瞎猜」): a geometric blind spot is
//! reported as `null`/`indeterminate` and never laundered into a decided verdict;
//! a non-degradable a11y unknown blocks; a bad digest is rejected at the
//! boundary rather than silently signed.
//!
//! Hermetic: no repo, no toolchain, no network — the verbs are pure functions
//! of their JSON input. Request fixtures live under the OS temp root (honours
//! TMPDIR) for the `--file` cases; the stdin cases pipe bytes directly.

use std::io::Write;
use std::process::{Command, Stdio};

fn shuttle_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shuttle")
}

fn scratch_path(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("shuttle-ui-cli-{tag}-{nanos}.json"))
}

/// Run `shuttle ui <verb> --file <path>` and return (exit_ok, stdout).
fn run_file(verb: &str, body: &str) -> (bool, String) {
    let path = scratch_path(verb);
    std::fs::write(&path, body).unwrap();
    let out = Command::new(shuttle_bin())
        .args(["ui", verb, "--file"])
        .arg(&path)
        .output()
        .expect("spawn shuttle ui");
    let _ = std::fs::remove_file(&path);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Run `shuttle ui <verb>` feeding `body` on stdin; return (exit_ok, stdout).
fn run_stdin(verb: &str, body: &str) -> (bool, String) {
    let mut child = Command::new(shuttle_bin())
        .args(["ui", verb])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shuttle ui");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn field<'a>(json: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    json.get(key)
        .unwrap_or_else(|| panic!("missing key `{key}` in {json}"))
}

const ORDER_ATOM: &str = r#"{"family":"order","relation":"before","args":["a","b"],"axes":[{"name":"x","kind":"metric"}],"allen":"before","frame":"viewport","time_quantifier":"at"}"#;

fn verify_body_full() -> String {
    format!(
        r#"{{"frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}},{{"element":"b","axis":"x","start":20.0,"end":30.0}}]}},"atoms":[{ORDER_ATOM}],"thresholds":{{}}}}"#
    )
}

#[test]
fn ui_verify_decides_true_and_rolls_up_satisfied() {
    let (ok, stdout) = run_file("verify", &verify_body_full());
    assert!(ok, "verify should exit 0");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "outcome"), "satisfied");
    assert_eq!(field(&v, "decided_true"), 1);
    assert_eq!(field(&v, "decided_false"), 0);
    assert_eq!(field(&v, "unknown"), 0);
    // the returned atom carries the decided three-valued verdict
    assert_eq!(field(&v["atoms"][0], "holds"), "true");
}

#[test]
fn ui_verify_missing_box_degrades_to_null_not_false() {
    // Drop element b: the proxy-family (order) geometric false must degrade to
    // a blind-spot null, rolling up to indeterminate — never a decided false.
    let body = format!(
        r#"{{"frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}}]}},"atoms":[{ORDER_ATOM}],"thresholds":{{}}}}"#
    );
    let (ok, stdout) = run_stdin("verify", &body);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "unknown"), 1);
    assert_eq!(field(&v, "decided_false"), 0);
    assert_eq!(field(&v, "outcome"), "indeterminate");
}

#[test]
fn ui_verify_rejects_degenerate_interval() {
    // end < start is rejected at the wire boundary (fail-closed), non-zero exit.
    let body = r#"{"frame":{"tolerance":0.0,"boxes":[{"element":"a","axis":"x","start":10.0,"end":0.0}]},"atoms":[],"thresholds":{}}"#;
    let (ok, _stdout) = run_stdin("verify", body);
    assert!(!ok, "degenerate interval must fail closed");
}

#[test]
fn ui_couple_cycle_forces_solver_and_a11y_blocks() {
    let body = r#"{"dimensions":[{"name":"gap","value":{"kind":"metric","value":8.0},"range":{"kind":"interval","min":0.0,"max":64.0}}],"couplings":[{"from":"a","to":"b","kind":"value"},{"from":"b","to":"a","kind":"value"}],"a11y":[{"name":"target-size","required_min":44.0}]}"#;
    let (ok, stdout) = run_file("couple", body);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "solver_class"), "solver");
    assert_eq!(field(&v, "admissible"), false);
    assert_eq!(field(&v["lexicon"][0], "status"), "ok");
    assert_eq!(field(&v["a11y"][0], "verdict"), "blocked");
}

#[test]
fn ui_render_reproves_atom_and_signs_artifact() {
    let body = format!(
        r#"{{"scene":{{"width":320.0,"height":200.0,"tolerance":0.5,"elements":[{{"id":"a","x":[10.0,60.0],"y":[10.0,40.0],"text":"A"}},{{"id":"b","x":[100.0,160.0],"y":[10.0,40.0],"text":"B"}}]}},"atoms":[{ORDER_ATOM}],"thresholds":{{}},"principles":[]}}"#
    );
    let (ok, stdout) = run_file("render", &body);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "outcome"), "satisfied");
    assert_eq!(field(&v, "verified"), 1);
    assert_eq!(field(&v, "regressions"), 0);
    assert_eq!(field(&v, "consistent"), true);
    let digest = field(&v, "artifact_digest").as_str().unwrap();
    assert!(digest.starts_with("sha256:"), "artifact must be signed");
    assert!(field(&v, "html").as_str().unwrap().contains("<"));
}

#[test]
fn ui_render_is_deterministic() {
    let body = format!(
        r#"{{"scene":{{"width":320.0,"height":200.0,"tolerance":0.5,"elements":[{{"id":"a","x":[10.0,60.0],"y":[10.0,40.0],"text":"A"}},{{"id":"b","x":[100.0,160.0],"y":[10.0,40.0],"text":"B"}}]}},"atoms":[{ORDER_ATOM}],"thresholds":{{}},"principles":[]}}"#
    );
    let (_ok1, a) = run_file("render", &body);
    let (_ok2, b) = run_stdin("render", &body);
    let va: serde_json::Value = serde_json::from_str(&a).unwrap();
    let vb: serde_json::Value = serde_json::from_str(&b).unwrap();
    // Same scene → byte-identical artifact digest, regardless of input face.
    assert_eq!(field(&va, "artifact_digest"), field(&vb, "artifact_digest"));
    assert_eq!(field(&va, "html"), field(&vb, "html"));
}

#[test]
fn ui_adopt_signs_projection_bound_to_digest() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let body = format!(
        r#"{{"surface_id":"screen:home","artifact_digest":"{digest}","frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}},{{"element":"b","axis":"x","start":20.0,"end":30.0}}]}},"atoms":[{ORDER_ATOM}],"principles":[{{"name":"Ordered","formula":{{"op":"atom","atom":0}},"is_geometric_proxy":false}}],"thresholds":{{}}}}"#
    );
    let (ok, stdout) = run_file("adopt", &body);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "outcome"), "satisfied");
    assert_eq!(field(&v, "needs_review"), false);
    assert!(field(&v, "projection_id")
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[test]
fn ui_adopt_rejects_bad_digest() {
    let body = format!(
        r#"{{"surface_id":"screen:home","artifact_digest":"not-a-digest","frame":{{"boxes":[]}},"atoms":[{ORDER_ATOM}],"principles":[],"thresholds":{{}}}}"#
    );
    let (ok, _stdout) = run_stdin("adopt", &body);
    assert!(!ok, "malformed digest must fail closed");
}

#[test]
fn ui_adopt_rejects_dangling_formula_index() {
    let digest = format!("sha256:{}", "b".repeat(64));
    let body = format!(
        r#"{{"surface_id":"screen:home","artifact_digest":"{digest}","frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}},{{"element":"b","axis":"x","start":20.0,"end":30.0}}]}},"atoms":[{ORDER_ATOM}],"principles":[{{"name":"Ordered","formula":{{"op":"atom","atom":9}},"is_geometric_proxy":false}}],"thresholds":{{}}}}"#
    );
    let (ok, _stdout) = run_stdin("adopt", &body);
    assert!(!ok, "out-of-range atom index must fail closed");
}

#[test]
fn ui_rejects_unknown_fields() {
    // The closed schema (deny_unknown_fields) rejects extra keys at the wire.
    let body = r#"{"frame":{"boxes":[]},"atoms":[],"thresholds":{},"bogus":true}"#;
    let (ok, _stdout) = run_stdin("verify", body);
    assert!(!ok, "unknown field must be rejected");
}

// ---------------------------------------------------------------------------
// assess — the Form B external visual-engine adapter (design §4.2, task U7)
//
// shuttle never generates pixels (red-line #6). An external engine generates INSIDE
// shuttle's token hard constraints; `shuttle ui assess` feeds shuttle the engine's produced
// artifact geometry (layer bounding boxes) plus the gates to adjudicate, then
// reverse-measures the atom holds and gates the artifact with the §3.3
// hard/soft dichotomy into an `adapter_receipt/v1`:
//   * a token 硬门 met by the geometry → adopt; unmet & non-droppable → reject;
//     unmet & droppable → graceful drop; a blind-spot (no threshold) → blocked
//     → reject (「绝不瞎猜」: never a silent pass).
//   * a11y / capability negotiation are hard, fail closed on unknown/denied.
//   * the engine's aesthetic self-report is soft: below-floor only sidecars.
// The disposition is computed by the receipt fold, so a failing artifact can
// never be laundered into an adopt at the wire.
// ---------------------------------------------------------------------------

const CTA_GATE_ATOM: &str = r#"{"family":"metric","relation":"min_width","args":["cta"],"axes":[{"name":"w","kind":"metric"}],"threshold_ref":"token://size/cta-min","frame":"viewport","time_quantifier":"at"}"#;

/// Build an `AssessRequest` JSON body: one reverse-measured cta layer of the
/// given `width`, one token 硬门 (`>= 44`), and the caller-supplied extra JSON
/// fragments spliced into the request object.
fn assess_body(width: f64, droppable: bool, extra: &str) -> String {
    format!(
        r#"{{"surface_id":"screen:checkout","engine_id":"v0","engine_version":"2026.9","artifact_digest":"sha256:{d}","frame":{{"tolerance":0.0,"boxes":[{{"element":"cta","axis":"w","start":0.0,"end":{width}}}]}},"token_gates":[{{"name":"token:cta-min-width","atom":{CTA_GATE_ATOM},"droppable":{droppable}}}],"thresholds":{{"token://size/cta-min":44.0}}{extra}}}"#,
        d = "a".repeat(64),
    )
}

#[test]
fn ui_assess_adopts_clean_artifact_with_reversed_provenance() {
    let (ok, stdout) = run_file("assess", &assess_body(44.0, false, ""));
    assert!(ok, "a clean artifact should assess 0");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "adopt");
    assert_eq!(field(&v, "admissible"), true);
    assert_eq!(field(&v, "needs_review"), false);
    assert!(field(&v, "receipt_id")
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    // shuttle stamps its OWN evaluator identity into the basis — the engine cannot
    // self-certify which reverse-measurement code adjudicated it.
    let ev = field(&v["receipt"]["basis"], "evaluator_version")
        .as_str()
        .unwrap();
    assert!(
        ev.starts_with("shuttle-ui/"),
        "shuttle stamps its evaluator id"
    );
    // The token gate atom carries `reversed` provenance, never fabricated.
    assert_eq!(
        field(&v["receipt"]["atoms"][0]["provenance"], "class"),
        "reversed"
    );
    let gate = &v["receipt"]["hard_gates"][0];
    assert_eq!(field(gate, "kind"), "token");
    assert_eq!(field(gate, "verdict"), "pass");
}

#[test]
fn ui_assess_token_below_bound_rejects() {
    let (ok, stdout) = run_stdin("assess", &assess_body(20.0, false, ""));
    assert!(ok, "assess itself exits 0 — the verdict is in the receipt");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "reject");
    assert_eq!(field(&v, "admissible"), false);
}

#[test]
fn ui_assess_droppable_token_degrades_gracefully() {
    let (ok, stdout) = run_file("assess", &assess_body(20.0, true, ""));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "drop");
    assert_eq!(field(&v, "admissible"), true);
    assert_eq!(field(&v, "needs_review"), true);
}

#[test]
fn ui_assess_a11y_unknown_fails_closed() {
    // measured omitted -> A11yConstraint blocks -> reject (non-degradable).
    let extra = r#","a11y_gates":[{"name":"contrast","required_min":4.5,"droppable":false}]"#;
    let (ok, stdout) = run_file("assess", &assess_body(44.0, false, extra));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "reject");
    let a11y = v["receipt"]["hard_gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["kind"] == "a11y")
        .expect("an a11y gate row");
    assert_eq!(field(a11y, "verdict"), "blocked");
}

#[test]
fn ui_assess_capability_unnegotiated_rejects() {
    // advertised omitted -> None -> could-not-negotiate -> blocked -> reject.
    let extra =
        r#","capability_gates":[{"name":"token-grid-snap","required":true,"droppable":false}]"#;
    let (ok, stdout) = run_file("assess", &assess_body(44.0, false, extra));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "reject");
}

#[test]
fn ui_assess_aesthetic_soft_flag_only_sidecars() {
    // A below-floor aesthetic self-report is soft: it sidecars (admissible) and
    // flags review, but never blocks an otherwise-clean artifact.
    let extra = r#","aesthetic":{"score":0.4,"floor":0.7,"rationale_ref":"engine://aesthetic/weak-hierarchy"}"#;
    let (ok, stdout) = run_file("assess", &assess_body(44.0, false, extra));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "disposition"), "sidecar");
    assert_eq!(field(&v, "admissible"), true);
    assert_eq!(field(&v, "needs_review"), true);
}

#[test]
fn ui_assess_mapping_grounds_in_reverse_measured_atom() {
    // A mapping corroborated by the (true) token-gate atom reads verified=true.
    let extra = r#","mappings":[{"layer_id":"layer:cta","token_ref":"token://component/button","corroborating_atom":0}]"#;
    let (ok, stdout) = run_file("assess", &assess_body(44.0, false, extra));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let m = &v["receipt"]["mappings"][0];
    assert_eq!(field(m, "layer_id"), "layer:cta");
    assert_eq!(field(m, "verified"), "true");
}

#[test]
fn ui_assess_rejects_bad_digest() {
    let body = assess_body(44.0, false, "").replace(&"a".repeat(64), "not-a-digest");
    let (ok, _stdout) = run_stdin("assess", &body);
    assert!(
        !ok,
        "a malformed artifact digest must fail closed at the wire"
    );
}

#[test]
fn ui_assess_rejects_unknown_field() {
    let body = assess_body(44.0, false, r#","surprise":true"#);
    let (ok, _stdout) = run_stdin("assess", &body);
    assert!(!ok, "unknown request field must be rejected");
}

// ---------------------------------------------------------------------------
// receipt — the evidence-owner emit path (design §6.3, task U5)
//
// End-to-end across TWO real `shuttle` binaries in series: `ui adopt` signs a
// verified `ui_projection/v1` 底稿, then `ui receipt` seals that projection into
// a digest-bound `pde_campaign_phase_receipt/v1`. The sealed line is re-parsed
// through the FROZEN mirror gate (`PdeCampaignPhaseReceipt::parse`) — the same
// gate pde-campaign's kernel enforces — proving the receipt shuttle emits is
// cross-verifiable byte-for-byte without shuttle ever writing the campaign store
// (red-line #7 / #11). The 「绝不瞎猜」invariant is exercised too: a decided
// effect asserted on a non-satisfied projection is refused at the seal.
// ---------------------------------------------------------------------------

use shuttle_protocol::{PdeCampaignPhaseReceipt, UI_PROJECTION_ARTIFACT_TYPE};

/// Drive `shuttle ui adopt` on a well-separated two-box order scene and return the
/// signed projection object (the `projection` field of the adopt response).
fn adopt_satisfied_projection() -> serde_json::Value {
    let digest = format!("sha256:{}", "a".repeat(64));
    let body = format!(
        r#"{{"surface_id":"screen:checkout","artifact_digest":"{digest}","frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}},{{"element":"b","axis":"x","start":20.0,"end":30.0}}]}},"atoms":[{ORDER_ATOM}],"principles":[{{"name":"Ordered","formula":{{"op":"atom","atom":0}},"is_geometric_proxy":false}}],"thresholds":{{}}}}"#
    );
    let (ok, stdout) = run_file("adopt", &body);
    assert!(ok, "adopt should sign a satisfied projection");
    let resp: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&resp, "outcome"), "satisfied");
    resp.get("projection")
        .expect("adopt response carries the signed projection")
        .clone()
}

/// Build a `UiPhaseReceiptRequest` JSON document embedding `projection` with the
/// given `effect` block.
fn receipt_request(projection: &serde_json::Value, effect: serde_json::Value) -> String {
    let request = serde_json::json!({
        "campaign_id": "campaign-checkout-01",
        "expected_campaign_revision": 3,
        "idempotency_key": "idem-ui-receipt-e2e-1",
        "action": "projection.ready",
        "from_phase": "WAITING_EVIDENCE",
        "to_phase": "EVIDENCE_COMMITTED",
        "owner": "shuttle-ui",
        "authority_artifact": null,
        "effect": effect,
        "decision": null,
        "observed_at": "2026-09-15T08:30:00Z",
        "projection": projection,
    });
    serde_json::to_string(&request).unwrap()
}

#[test]
fn ui_receipt_seals_owner_receipt_that_binds_and_round_trips() {
    let projection = adopt_satisfied_projection();
    // A no-op effect: this transition has no side effect to reconcile.
    let effect = serde_json::json!({
        "state": "none",
        "receipt_ref": null,
        "receipt_digest": null,
    });
    let (ok, stdout) = run_file("receipt", &receipt_request(&projection, effect));
    assert!(
        ok,
        "receipt should seal a satisfied-projection owner receipt"
    );

    // The emitted line is a single compact JSON receipt; it MUST re-parse
    // through the frozen mirror gate that pde-campaign's kernel also enforces.
    let line = stdout.trim();
    let bytes = line.as_bytes();
    let receipt = PdeCampaignPhaseReceipt::parse(bytes)
        .expect("emitted receipt re-parses through the frozen gate");

    // owner_artifact points at the projection 底稿 by its content-addressed id.
    let projection_id = projection.get("projection_id").unwrap().as_str().unwrap();
    assert_eq!(
        receipt.owner_artifact.artifact_type,
        UI_PROJECTION_ARTIFACT_TYPE
    );
    assert_eq!(receipt.owner_artifact.artifact_ref, projection_id);
    assert_eq!(receipt.owner_artifact.digest.to_string(), projection_id);

    // The stored digest binds the exact canonical body (pde-byte-compatible).
    assert_eq!(
        receipt.receipt_digest,
        receipt.expected_receipt_digest().unwrap(),
        "receipt_digest must bind the canonical receipt body"
    );

    // Round-trip: re-serialize and re-parse yields an identical receipt.
    let reserialized = serde_json::to_vec(&receipt).unwrap();
    let reparsed = PdeCampaignPhaseReceipt::parse(&reserialized).unwrap();
    assert_eq!(reparsed, receipt);
}

#[test]
fn ui_receipt_writes_atomically_to_out_path() {
    let projection = adopt_satisfied_projection();
    let effect = serde_json::json!({
        "state": "applied",
        "receipt_ref": "file:///campaign/effect",
        "receipt_digest": null,
    });
    let req_path = scratch_path("receipt-req");
    std::fs::write(&req_path, receipt_request(&projection, effect)).unwrap();
    let out_path = scratch_path("receipt-out");

    let status = Command::new(shuttle_bin())
        .args(["ui", "receipt", "--file"])
        .arg(&req_path)
        .arg("--out")
        .arg(&out_path)
        .status()
        .expect("spawn shuttle ui receipt --out");
    assert!(status.success(), "receipt --out should seal to disk");

    // Applied effect on a SATISFIED projection is admissible; the file binds.
    let written = std::fs::read(&out_path).expect("receipt file exists");
    let receipt = PdeCampaignPhaseReceipt::parse(&written)
        .expect("on-disk receipt re-parses through the frozen gate");
    assert_eq!(
        receipt.receipt_digest,
        receipt.expected_receipt_digest().unwrap()
    );
    let _ = std::fs::remove_file(&req_path);
    let _ = std::fs::remove_file(&out_path);
}

#[test]
fn ui_receipt_refuses_decided_effect_on_blind_spot() {
    // A projection that cannot decide (drop element b) rolls up indeterminate.
    // Asserting a DECIDED effect (applied) on that blind spot is 「绝不瞎猜」
    // and must fail closed at the seal — never a fabricated owner receipt.
    let digest = format!("sha256:{}", "c".repeat(64));
    let adopt_body = format!(
        r#"{{"surface_id":"screen:checkout","artifact_digest":"{digest}","frame":{{"tolerance":0.5,"boxes":[{{"element":"a","axis":"x","start":0.0,"end":10.0}}]}},"atoms":[{ORDER_ATOM}],"principles":[{{"name":"Ordered","formula":{{"op":"atom","atom":0}},"is_geometric_proxy":false}}],"thresholds":{{}}}}"#
    );
    let (ok, adopt_out) = run_file("adopt", &adopt_body);
    assert!(ok);
    let resp: serde_json::Value = serde_json::from_str(&adopt_out).unwrap();
    assert_eq!(field(&resp, "outcome"), "indeterminate");
    let projection = resp.get("projection").unwrap().clone();

    let effect = serde_json::json!({
        "state": "applied",
        "receipt_ref": "file:///campaign/effect",
        "receipt_digest": null,
    });
    let (ok, _stdout) = run_file("receipt", &receipt_request(&projection, effect));
    assert!(
        !ok,
        "decided effect on a non-satisfied projection must fail closed"
    );
}

#[test]
fn ui_receipt_rejects_unknown_request_field() {
    // The closed request schema (deny_unknown_fields) rejects a stray key at
    // the wire, before any sealing work.
    let projection = adopt_satisfied_projection();
    let mut request: serde_json::Value = serde_json::from_str(&receipt_request(
        &projection,
        serde_json::json!({"state":"none","receipt_ref":null,"receipt_digest":null}),
    ))
    .unwrap();
    request
        .as_object_mut()
        .unwrap()
        .insert("surprise".to_string(), serde_json::json!(1));
    let (ok, _stdout) = run_stdin("receipt", &serde_json::to_string(&request).unwrap());
    assert!(!ok, "unknown request field must be rejected");
}

// ---------------------------------------------------------------------------
// canvas — the Form C Figma-class canvas underlay (design §4.3, task U8)
//
// `shuttle ui canvas` overlays a Form A SVG skeleton with an optional
// already-adjudicated Form B fidelity layer (an `adapter_receipt/v1` emitted by
// `shuttle ui assess`) into ONE digest-bound `canvas_underlay/v1` — the machine-
// readable 底稿 (赋值 + 命题 + provenance) a host front-end renders into an
// interactive canvas. The canvas is a RENDER PRODUCT, not a truth source: truth
// stays in the coord_atom propositions (red-line #7); shuttle builds neither the
// canvas UI nor the pixel engine (red-lines #6 / #11) and never silently fails
// (§4.2) — an inadmissible fidelity layer surfaces as `blocked` with no
// laundered node bindings. The readiness/review flags are computed by the
// underlay fold, so a rejected layer can never be laundered into an overlay.
// End-to-end this chains TWO real `shuttle` binaries: `ui assess` adjudicates the
// engine artifact, then `ui canvas` overlays that receipt onto the skeleton.
// ---------------------------------------------------------------------------

/// A two-element scene (`cta` before `hint`) plus an order atom that the
/// skeleton geometry decides `true` — so a skeleton-only compose is satisfied
/// and needs no review (the propositions are decided, not blind spots).
const CANVAS_ORDER_ATOM: &str = r#"{"family":"order","relation":"before","args":["cta","hint"],"axes":[{"name":"x","kind":"metric"}],"allen":"before","frame":"viewport","time_quantifier":"at"}"#;

fn canvas_scene() -> String {
    r#"{"width":320.0,"height":200.0,"tolerance":0.5,"elements":[{"id":"cta","x":[0.0,120.0],"y":[0.0,44.0],"text":"Buy"},{"id":"hint","x":[140.0,300.0],"y":[0.0,44.0],"role":"note"}]}"#.to_string()
}

/// Build a `ComposeRequest` JSON body from the shared scene + order atom, with
/// the caller-supplied extra fragments (e.g. a `fidelity` receipt) spliced in.
fn canvas_body(extra: &str) -> String {
    format!(
        r#"{{"surface_id":"surface:home","scene":{scene},"atoms":[{CANVAS_ORDER_ATOM}],"thresholds":{{}}{extra}}}"#,
        scene = canvas_scene(),
    )
}

/// Drive `shuttle ui assess` to adjudicate an engine artifact of the given cta
/// `width`, mapping the `cta` layer to a component token, and return the sealed
/// `adapter_receipt/v1` object (the `receipt` field of the assess response).
fn assess_receipt_for_canvas(width: f64, droppable: bool) -> serde_json::Value {
    let extra = r#","mappings":[{"layer_id":"cta","token_ref":"token://component/button","corroborating_atom":0}]"#;
    let (ok, stdout) = run_file("assess", &assess_body(width, droppable, extra));
    assert!(ok, "assess should adjudicate the engine artifact");
    let resp: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    resp.get("receipt")
        .expect("assess response carries the adapter receipt")
        .clone()
}

/// Splice a fidelity receipt object into a compose request `extra` fragment.
fn fidelity_extra(receipt: &serde_json::Value) -> String {
    format!(",\"fidelity\":{}", serde_json::to_string(receipt).unwrap())
}

#[test]
fn ui_canvas_skeleton_only_is_satisfied_and_binds_svg_digest() {
    // No fidelity layer: the underlay is skeleton-only, the propositions are
    // decided (order true), so it needs no review; the emitted SVG's bytes are
    // bound into the basis skeleton digest.
    let (ok, stdout) = run_file("canvas", &canvas_body(""));
    assert!(ok, "skeleton-only canvas should exit 0");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "readiness"), "skeleton_only");
    assert_eq!(field(&v, "needs_review"), false);
    let svg = field(&v, "svg").as_str().unwrap();
    assert!(svg.contains("<svg"), "the skeleton is emitted as SVG");
    // No fidelity ⇒ no fidelity digest in the basis, and no node is bound.
    assert!(v["underlay"]["basis"]["fidelity_digest"].is_null());
    assert!(v["underlay"]["fidelity"].is_null());
    // The skeleton digest binds the exact emitted SVG bytes.
    let skeleton_digest = field(&v, "skeleton_digest").as_str().unwrap();
    assert!(skeleton_digest.starts_with("sha256:"));
    assert_eq!(
        field(&v["underlay"]["basis"], "skeleton_digest")
            .as_str()
            .unwrap(),
        skeleton_digest
    );
    // The underlay id is content-addressed.
    assert!(field(&v, "underlay_id")
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[test]
fn ui_canvas_is_deterministic_across_input_faces() {
    // Same request → byte-identical underlay id and skeleton digest regardless
    // of the --file vs stdin input face.
    let (_a_ok, a) = run_file("canvas", &canvas_body(""));
    let (_b_ok, b) = run_stdin("canvas", &canvas_body(""));
    let va: serde_json::Value = serde_json::from_str(&a).unwrap();
    let vb: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(field(&va, "underlay_id"), field(&vb, "underlay_id"));
    assert_eq!(field(&va, "skeleton_digest"), field(&vb, "skeleton_digest"));
    assert_eq!(field(&va, "svg"), field(&vb, "svg"));
}

#[test]
fn ui_canvas_adopts_fidelity_and_binds_mapped_node() {
    // A clean (adopted) Form B receipt composites into the underlay: the mapped
    // `cta` node carries its token_ref + a verified holds, the basis records a
    // fidelity digest, and the readiness rolls up to composited.
    let receipt = assess_receipt_for_canvas(44.0, false);
    let (ok, stdout) = run_file("canvas", &canvas_body(&fidelity_extra(&receipt)));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "readiness"), "composited");
    assert!(!v["underlay"]["basis"]["fidelity_digest"].is_null());
    assert!(!v["underlay"]["fidelity"].is_null());
    let cta = v["underlay"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node_id"] == "cta")
        .expect("a cta node");
    assert_eq!(field(cta, "token_ref"), "token://component/button");
    assert_eq!(field(cta, "fidelity_verified"), "true");
    // An unmapped node is never falsely bound.
    let hint = v["underlay"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node_id"] == "hint")
        .expect("a hint node");
    assert!(hint["token_ref"].is_null());
    assert!(hint["fidelity_verified"].is_null());
}

#[test]
fn ui_canvas_rejected_fidelity_blocks_and_strips_all_bindings() {
    // A non-droppable token failure adjudicates to reject; the underlay must
    // surface as `blocked`, flag review, and carry NO node bindings — never a
    // silent laundered overlay (§4.2, red-line #7).
    let receipt = assess_receipt_for_canvas(20.0, false);
    assert_eq!(receipt["disposition"], "reject");
    let (ok, stdout) = run_file("canvas", &canvas_body(&fidelity_extra(&receipt)));
    assert!(ok, "canvas itself exits 0 — the verdict is in the underlay");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "readiness"), "blocked");
    assert_eq!(field(&v, "needs_review"), true);
    assert!(v["underlay"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["token_ref"].is_null() && n["fidelity_verified"].is_null()));
}

#[test]
fn ui_canvas_droppable_fidelity_degrades_but_still_composes() {
    // A droppable token failure adjudicates to drop: the layer is admissible
    // (graceful degrade), so the underlay still composes but flags review.
    let receipt = assess_receipt_for_canvas(20.0, true);
    assert_eq!(receipt["disposition"], "drop");
    let (ok, stdout) = run_file("canvas", &canvas_body(&fidelity_extra(&receipt)));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(field(&v, "readiness"), "degraded");
    assert_eq!(field(&v, "needs_review"), true);
    // The basis still records the fidelity digest — the layer was adjudicated,
    // not dropped on the floor.
    assert!(!v["underlay"]["basis"]["fidelity_digest"].is_null());
}

#[test]
fn ui_canvas_rejects_unknown_field() {
    // The closed request schema (deny_unknown_fields) rejects a stray key.
    let (ok, _stdout) = run_stdin("canvas", &canvas_body(r#","surprise":true"#));
    assert!(!ok, "unknown request field must be rejected");
}

#[test]
fn ui_canvas_rejects_tampered_fidelity_receipt() {
    // A tampered embedded receipt (its id no longer binds its body) must fail
    // closed at the wire boundary before it can influence the composition.
    let mut receipt = assess_receipt_for_canvas(44.0, false);
    receipt["receipt_id"] = serde_json::json!(format!("sha256:{}", "0".repeat(64)));
    let (ok, _stdout) = run_stdin("canvas", &canvas_body(&fidelity_extra(&receipt)));
    assert!(!ok, "a tampered fidelity receipt must fail closed");
}
