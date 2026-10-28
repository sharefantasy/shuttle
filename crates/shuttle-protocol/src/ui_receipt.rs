//! pde-campaign phase-receipt mirror — `pde_campaign_phase_receipt/v1`.
//!
//! This module is a **read-only projection** of pde-campaign's FROZEN
//! `pde_campaign_phase_receipt/v1` JSON schema into `shuttle-protocol` (the N4
//! discipline): shuttle mirrors the peer's contract by shape + digest so a receipt
//! shuttle emits is cross-verifiable against pde's own kernel, and shuttle **never
//! becomes a second truth source** for the campaign (design §6.3, red-line #11
//! and #7). The campaign store owns phase truth; shuttle only emits digest-bound
//! *owner* receipts as the geometric-evidence + judgment owner.
//!
//! ### Digest byte-compatibility
//!
//! pde binds a receipt with `bind_digest(value, "receipt_digest")` =
//! `canonical_digest({all fields except receipt_digest})`, where
//! `canonical_digest(v) = "sha256:" + sha256(canonical_bytes(v).rstrip("\n"))`
//! and `canonical_bytes` is `json.dumps(v, ensure_ascii=False, sort_keys=True,
//! separators=(",",":"), allow_nan=False) + "\n"` (utf-8). shuttle's
//! [`crate::canonical::canonical_digest`] recursively sorts object keys and
//! emits compact JSON with no trailing newline, then `sha256:` + hex — the
//! exact same bytes. So [`PdeCampaignPhaseReceipt::expected_receipt_digest`] is
//! byte-identical to pde's `bind_digest`, and a receipt shuttle seals verifies under
//! pde's kernel unchanged.
//!
//! ### Fail-closed, never fabricate (§7/§20「绝不瞎猜」)
//!
//! Every enum rejects an unknown variant, every object is
//! `deny_unknown_fields`, every nullable-but-required field is presence-checked
//! ([`PdeCampaignPhaseReceipt::from_json_value`]), and the evidence-owner
//! [`UiPhaseReceiptRequest::seal`] refuses to assert a *decided* effect
//! (`applied`/`not_applied`) on a projection whose outcome is indeterminate: a
//! geometric blind-spot leaves the effect `unknown`/`none`, it is never guessed.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, ClosedContractError};
use crate::symbol_footprint::invalid;
use crate::ui_projection::UiProjection;
use serde::{Deserialize, Serialize};

/// The frozen pde-campaign phase-receipt schema id shuttle mirrors verbatim.
pub const PDE_CAMPAIGN_PHASE_RECEIPT_SCHEMA: &str = "pde_campaign_phase_receipt/v1";
/// The frozen `protocol_revision` const (schema pins it to exactly `1`).
pub const PDE_CAMPAIGN_PROTOCOL_REVISION: i64 = 1;
/// The artifact `type` shuttle stamps into `owner_artifact` when the owner evidence
/// is a `ui_projection/v1` 底稿. Equal to [`crate::UI_PROJECTION_SCHEMA`].
pub const UI_PROJECTION_ARTIFACT_TYPE: &str = "ui_projection/v1";
/// Hard input ceiling for [`PdeCampaignPhaseReceipt::parse`] (1 MiB).
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// The 15 frozen top-level keys (order matches the schema `required` list).
const TOP_LEVEL_KEYS: [&str; 15] = [
    "schema_version",
    "protocol_revision",
    "campaign_id",
    "expected_campaign_revision",
    "idempotency_key",
    "action",
    "from_phase",
    "to_phase",
    "owner",
    "owner_artifact",
    "authority_artifact",
    "effect",
    "decision",
    "observed_at",
    "receipt_digest",
];

/// The 3 frozen `effect` keys (all required, two nullable).
const EFFECT_KEYS: [&str; 3] = ["state", "receipt_ref", "receipt_digest"];

/// The 4 frozen `artifact_ref` keys (all required).
const ARTIFACT_KEYS: [&str; 4] = ["type", "ref", "digest", "revision"];

/// A mirrored, digest-verifiable pde-campaign contract. Structurally parallel
/// to [`crate::anchor::CodingFlowContract`]: a `schema_version` const plus a
/// fail-closed `validate` that re-checks every frozen invariant.
pub trait PdeCampaignContract {
    /// The frozen `schema_version` this type mirrors.
    const SCHEMA_VERSION: &'static str;
    /// Re-check every frozen field/cross-field rule; fail closed.
    fn validate(&self) -> Result<(), ClosedContractError>;
}

/// The 13 frozen campaign phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PdePhase {
    WaitingGeneration,
    WaitingCandidateAuthority,
    CandidateCommitted,
    WaitingEvidence,
    WaitingEvidenceAuthority,
    EvidenceCommitted,
    WaitingAdjudicationAuthority,
    Adjudicated,
    ConvergenceChecked,
    Completed,
    NeedsRevision,
    ReconcileRequired,
    Blocked,
}

/// The 10 frozen transition actions. Renamed explicitly to the dotted wire form
/// so an unknown action is rejected at the serde boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PdePhaseAction {
    #[serde(rename = "generation.completed")]
    GenerationCompleted,
    #[serde(rename = "candidate.commit")]
    CandidateCommit,
    #[serde(rename = "projection.ready")]
    ProjectionReady,
    #[serde(rename = "evidence.collected")]
    EvidenceCollected,
    #[serde(rename = "evidence.commit")]
    EvidenceCommit,
    #[serde(rename = "adjudication.prepared")]
    AdjudicationPrepared,
    #[serde(rename = "adjudication.commit")]
    AdjudicationCommit,
    #[serde(rename = "convergence.checked")]
    ConvergenceChecked,
    #[serde(rename = "effect.reconciled")]
    EffectReconciled,
    #[serde(rename = "campaign.blocked")]
    CampaignBlocked,
}

/// The 4 frozen effect states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdeEffectState {
    None,
    Applied,
    NotApplied,
    Unknown,
}

/// The 5 frozen non-null decisions (a `null` decision is modelled as
/// `Option::None`, matching the schema's `enum: [null, ...]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdeDecision {
    Halt,
    NeedsRevision,
    MoreEvidence,
    StaleHead,
    OwnerRejected,
}

/// The frozen 4-field `artifact_ref` (distinct from the 2-field
/// `ArtifactRef`).
/// `revision` is an open scalar constrained to `integer | string | null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdeArtifactRef {
    #[serde(rename = "type")]
    pub artifact_type: String,
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub digest: Sha256Digest,
    pub revision: serde_json::Value,
}

impl PdeArtifactRef {
    /// Build an `owner_artifact` that points at a `ui_projection/v1` 底稿 by its
    /// content-addressed `projection_id` (the digest doubles as the ref).
    pub fn from_ui_projection(projection: &UiProjection) -> Self {
        Self {
            artifact_type: UI_PROJECTION_ARTIFACT_TYPE.to_string(),
            artifact_ref: projection.projection_id.as_str().to_string(),
            digest: projection.projection_id.clone(),
            revision: serde_json::Value::Null,
        }
    }

    fn validate(&self, field: &str) -> Result<(), ClosedContractError> {
        require_nonempty(&format!("{field}.type"), &self.artifact_type)?;
        require_nonempty(&format!("{field}.ref"), &self.artifact_ref)?;
        // digest is validated by the Sha256Digest newtype at parse time.
        match &self.revision {
            serde_json::Value::Null | serde_json::Value::String(_) => Ok(()),
            serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => Ok(()),
            _ => Err(invalid(format!(
                "{field}.revision must be an integer, string, or null"
            ))),
        }
    }
}

/// The frozen `effect` block. Both nullable fields are *required keys*, so they
/// always serialize (as `null` when absent) and are therefore part of the
/// receipt-digest binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdeEffect {
    pub state: PdeEffectState,
    pub receipt_ref: Option<String>,
    pub receipt_digest: Option<Sha256Digest>,
}

impl PdeEffect {
    /// A blind-spot effect: shuttle has no geometric ground to decide, so it neither
    /// claims an application nor its negation (§7/§20「绝不瞎猜」).
    pub fn unknown() -> Self {
        Self {
            state: PdeEffectState::Unknown,
            receipt_ref: None,
            receipt_digest: None,
        }
    }

    /// A no-op effect: this transition has no side effect to reconcile.
    pub fn none() -> Self {
        Self {
            state: PdeEffectState::None,
            receipt_ref: None,
            receipt_digest: None,
        }
    }

    fn validate(&self) -> Result<(), ClosedContractError> {
        if let Some(reference) = &self.receipt_ref {
            require_nonempty("effect.receipt_ref", reference)?;
        }
        Ok(())
    }

    /// True iff this effect asserts a *decided* outcome (applied or not).
    fn is_decided(&self) -> bool {
        matches!(
            self.state,
            PdeEffectState::Applied | PdeEffectState::NotApplied
        )
    }
}

/// The mirrored, digest-bound `pde_campaign_phase_receipt/v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdeCampaignPhaseReceipt {
    pub schema_version: String,
    pub protocol_revision: i64,
    pub campaign_id: String,
    pub expected_campaign_revision: i64,
    pub idempotency_key: String,
    pub action: PdePhaseAction,
    pub from_phase: PdePhase,
    pub to_phase: PdePhase,
    pub owner: String,
    pub owner_artifact: PdeArtifactRef,
    pub authority_artifact: Option<PdeArtifactRef>,
    pub effect: PdeEffect,
    pub decision: Option<PdeDecision>,
    pub observed_at: String,
    pub receipt_digest: Sha256Digest,
}

impl PdeCampaignContract for PdeCampaignPhaseReceipt {
    const SCHEMA_VERSION: &'static str = PDE_CAMPAIGN_PHASE_RECEIPT_SCHEMA;

    fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != PDE_CAMPAIGN_PHASE_RECEIPT_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        if self.protocol_revision != PDE_CAMPAIGN_PROTOCOL_REVISION {
            return Err(invalid(format!(
                "unsupported protocol_revision: {}",
                self.protocol_revision
            )));
        }
        self.validate_fields()?;
        // Fail closed if the stored digest does not bind the exact body — a
        // tampered field or a tampered digest is rejected.
        if self.receipt_digest != self.expected_receipt_digest()? {
            return Err(invalid(
                "receipt_digest does not bind the canonical receipt body",
            ));
        }
        Ok(())
    }
}

impl PdeCampaignPhaseReceipt {
    /// Parse from raw bytes through the full frozen gate (presence of every
    /// required key, structural typing, semantic invariants, digest binding).
    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 1MiB input limit"));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        Self::from_json_value(&value)
    }

    /// The production gate. serde's `Option` would silently accept a *missing*
    /// nullable-required key (`authority_artifact`, `decision`,
    /// `effect.receipt_ref`, `effect.receipt_digest`); the frozen schema marks
    /// them `required`, so we presence-check them before typed deserialization.
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self, ClosedContractError> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("receipt must be a JSON object"))?;
        require_keys("receipt", object, &TOP_LEVEL_KEYS)?;
        let effect = object
            .get("effect")
            .and_then(|value| value.as_object())
            .ok_or_else(|| invalid("effect must be a JSON object"))?;
        require_keys("effect", effect, &EFFECT_KEYS)?;
        // `owner_artifact`/`authority_artifact` required keys are enforced by
        // serde (their fields are non-`Option`), but presence-check here too so
        // a malformed object yields a contract error rather than a serde error.
        if let Some(owner_artifact) = object.get("owner_artifact").and_then(|v| v.as_object()) {
            require_keys("owner_artifact", owner_artifact, &ARTIFACT_KEYS)?;
        }
        if let Some(authority) = object.get("authority_artifact") {
            if let Some(authority) = authority.as_object() {
                require_keys("authority_artifact", authority, &ARTIFACT_KEYS)?;
            }
        }
        let receipt: Self = serde_json::from_value(value.clone())?;
        receipt.validate()?;
        Ok(receipt)
    }

    /// `bind_digest`-compatible: `canonical_digest` over every field *except*
    /// the top-level `receipt_digest`. The nested `effect.receipt_digest` is
    /// intentionally *not* excluded — only the outermost binding field is.
    pub fn expected_receipt_digest(&self) -> Result<Sha256Digest, ClosedContractError> {
        let mut value = serde_json::to_value(self)?;
        value
            .as_object_mut()
            .ok_or_else(|| invalid("receipt did not serialize to an object"))?
            .remove("receipt_digest");
        canonical_digest(&value)
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        validate_id("campaign_id", &self.campaign_id)?;
        if self.expected_campaign_revision < 1 {
            return Err(invalid("expected_campaign_revision must be >= 1"));
        }
        require_nonempty("idempotency_key", &self.idempotency_key)?;
        require_nonempty("owner", &self.owner)?;
        self.owner_artifact.validate("owner_artifact")?;
        if let Some(authority) = &self.authority_artifact {
            authority.validate("authority_artifact")?;
        }
        self.effect.validate()?;
        validate_datetime("observed_at", &self.observed_at)?;
        Ok(())
    }
}

/// The shuttle-owned request that seals a digest-bound owner receipt from a verified
/// [`UiProjection`] 底稿. This is the *evidence-owner emit path* (design §6.3):
/// shuttle binds the projection it owns into a receipt whose `owner_artifact` points
/// at that projection and whose `receipt_digest` is computed exactly like pde's
/// kernel — so pde can verify it without shuttle ever writing to the campaign store.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPhaseReceiptRequest {
    pub campaign_id: String,
    pub expected_campaign_revision: i64,
    pub idempotency_key: String,
    pub action: PdePhaseAction,
    pub from_phase: PdePhase,
    pub to_phase: PdePhase,
    pub owner: String,
    #[serde(default)]
    pub authority_artifact: Option<PdeArtifactRef>,
    pub effect: PdeEffect,
    #[serde(default)]
    pub decision: Option<PdeDecision>,
    pub observed_at: String,
    /// The verified 底稿 whose `projection_id` becomes `owner_artifact`.
    pub projection: UiProjection,
}

impl UiPhaseReceiptRequest {
    /// Assemble and digest-bind the owner receipt.
    ///
    /// Fails closed if: the projection does not self-verify; the caller tries to
    /// assert a *decided* effect on a non-satisfied (blind-spot) projection
    /// (「绝不瞎猜」); or any frozen field is invalid. On success the returned
    /// receipt round-trips through [`PdeCampaignPhaseReceipt::validate`] and its
    /// digest is byte-compatible with pde's `bind_digest`.
    pub fn seal(&self) -> Result<PdeCampaignPhaseReceipt, ClosedContractError> {
        self.projection.validate()?;

        // Red-line #7/#11 + 「绝不瞎猜」: shuttle is the evidence/judgment owner, not
        // a truth fabricator. A decided effect must rest on a satisfied 底稿;
        // an indeterminate projection may only carry `unknown`/`none`.
        if self.projection.outcome != crate::ui_projection::ProjectionOutcome::Satisfied
            && self.effect.is_decided()
        {
            return Err(invalid(
                "cannot assert a decided effect on a non-satisfied projection (绝不瞎猜)",
            ));
        }

        let owner_artifact = PdeArtifactRef::from_ui_projection(&self.projection);
        let mut receipt = PdeCampaignPhaseReceipt {
            schema_version: PDE_CAMPAIGN_PHASE_RECEIPT_SCHEMA.to_string(),
            protocol_revision: PDE_CAMPAIGN_PROTOCOL_REVISION,
            campaign_id: self.campaign_id.clone(),
            expected_campaign_revision: self.expected_campaign_revision,
            idempotency_key: self.idempotency_key.clone(),
            action: self.action,
            from_phase: self.from_phase,
            to_phase: self.to_phase,
            owner: self.owner.clone(),
            owner_artifact,
            authority_artifact: self.authority_artifact.clone(),
            effect: self.effect.clone(),
            decision: self.decision,
            observed_at: self.observed_at.clone(),
            receipt_digest: placeholder_digest(),
        };
        // Validate the body before binding so the digest covers a sound receipt.
        receipt.validate_fields()?;
        receipt.receipt_digest = receipt.expected_receipt_digest()?;
        // Full self-check (schema/revision/fields/digest) before emit.
        receipt.validate()?;
        Ok(receipt)
    }
}

fn placeholder_digest() -> Sha256Digest {
    Sha256Digest::parse(format!("sha256:{}", "0".repeat(64)))
        .expect("fixed placeholder digest is valid")
}

fn require_nonempty(field: &str, value: &str) -> Result<(), ClosedContractError> {
    if value.is_empty() {
        return Err(invalid(format!("{field} must be a non-empty string")));
    }
    Ok(())
}

/// The frozen `$defs/id` pattern: `^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`.
fn validate_id(field: &str, value: &str) -> Result<(), ClosedContractError> {
    let bytes = value.as_bytes();
    let ok = (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'));
    if !ok {
        return Err(invalid(format!(
            "{field} must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,127}}$"
        )));
    }
    Ok(())
}

/// The frozen `format: date-time` gate. Matches pde's `_timestamp`: an RFC3339
/// instant that *must* carry a timezone designator (`Z` or `±HH:MM`).
fn validate_datetime(field: &str, value: &str) -> Result<(), ClosedContractError> {
    if !is_rfc3339(value) {
        return Err(invalid(format!(
            "{field} must be an RFC3339 date-time with a timezone"
        )));
    }
    Ok(())
}

fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    // Minimum: `1970-01-01T00:00:00Z` = 20 bytes.
    if b.len() < 20 {
        return false;
    }
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let two = |i: usize| digit(i) && digit(i + 1);
    let val2 = |i: usize| ((b[i] - b'0') as u32) * 10 + (b[i + 1] - b'0') as u32;
    if !(digit(0) && digit(1) && digit(2) && digit(3)) {
        return false;
    }
    if b[4] != b'-' || !two(5) || b[7] != b'-' || !two(8) {
        return false;
    }
    let (month, day) = (val2(5), val2(8));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    if b[10] != b'T' && b[10] != b't' {
        return false;
    }
    if !two(11) || b[13] != b':' || !two(14) || b[16] != b':' || !two(17) {
        return false;
    }
    let (hour, minute, second) = (val2(11), val2(14), val2(17));
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    let mut i = 19;
    // Optional fractional seconds.
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match b.get(i) {
        Some(&b'Z') | Some(&b'z') => i + 1 == b.len(),
        Some(&b'+') | Some(&b'-') => {
            i += 1;
            if i + 5 != b.len() {
                return false;
            }
            let shape = b[i].is_ascii_digit()
                && b[i + 1].is_ascii_digit()
                && b[i + 2] == b':'
                && b[i + 3].is_ascii_digit()
                && b[i + 4].is_ascii_digit();
            if !shape {
                return false;
            }
            let offset_hour = ((b[i] - b'0') as u32) * 10 + (b[i + 1] - b'0') as u32;
            let offset_minute = ((b[i + 3] - b'0') as u32) * 10 + (b[i + 4] - b'0') as u32;
            offset_hour <= 23 && offset_minute <= 59
        }
        _ => false,
    }
}

fn require_keys(
    context: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Result<(), ClosedContractError> {
    for key in keys {
        if !object.contains_key(*key) {
            return Err(invalid(format!(
                "{context} is missing required key `{key}`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_atom::{
        Axis, AxisKind, CoordAtom, CoordAtomProposition, Holds, Provenance, ProvenanceClass,
        RelationFamily, TimeQuantifier, VerificationTier,
    };
    use crate::ui_principle::{DesignPrinciple, Formula};
    use crate::ui_projection::{ProjectionBasis, ProjectionOutcome};

    fn atom(relation: &str, holds: Holds) -> CoordAtom {
        let proposition = CoordAtomProposition {
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
                proposition,
                holds,
                Provenance::new(ProvenanceClass::DesignSystem, "token://align/baseline"),
                VerificationTier::Fsm,
            )
            .unwrap()
        } else {
            CoordAtom::unevaluated(proposition).unwrap()
        }
    }

    fn basis() -> ProjectionBasis {
        ProjectionBasis {
            surface_id: "screen:checkout".to_string(),
            artifact_digest: Sha256Digest::parse(format!("sha256:{}", "c".repeat(64))).unwrap(),
            evaluator_version: "shuttle-ui/0.1".to_string(),
        }
    }

    fn projection(holds: Holds) -> UiProjection {
        let a = atom("aligned_top", holds);
        let formula = Formula::Atom {
            atom_ref: a.atom_id.clone(),
        };
        let principle = DesignPrinciple::new("Aligned", formula, holds == Holds::Null).unwrap();
        UiProjection::assemble(basis(), vec![a], vec![principle]).unwrap()
    }

    fn request(effect: PdeEffect, holds: Holds) -> UiPhaseReceiptRequest {
        UiPhaseReceiptRequest {
            campaign_id: "campaign-01".to_string(),
            expected_campaign_revision: 3,
            idempotency_key: "idem-key-1".to_string(),
            action: PdePhaseAction::ProjectionReady,
            from_phase: PdePhase::WaitingEvidence,
            to_phase: PdePhase::EvidenceCommitted,
            owner: "shuttle-ui".to_string(),
            authority_artifact: None,
            effect,
            decision: None,
            observed_at: "2026-09-15T08:30:00Z".to_string(),
            projection: projection(holds),
        }
    }

    #[test]
    fn sealed_receipt_binds_to_projection_and_round_trips() {
        let request = request(PdeEffect::none(), Holds::True);
        let receipt = request.seal().unwrap();

        // owner_artifact references the projection 底稿 by its projection_id.
        assert_eq!(
            receipt.owner_artifact.artifact_type,
            UI_PROJECTION_ARTIFACT_TYPE
        );
        assert_eq!(
            receipt.owner_artifact.artifact_ref,
            request.projection.projection_id.as_str()
        );
        assert_eq!(
            receipt.owner_artifact.digest,
            request.projection.projection_id
        );
        assert_eq!(receipt.owner_artifact.revision, serde_json::Value::Null);

        // Full round-trip through the frozen gate.
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let parsed = PdeCampaignPhaseReceipt::parse(&bytes).unwrap();
        assert_eq!(parsed, receipt);
    }

    #[test]
    fn bind_digest_excludes_only_the_top_level_receipt_digest() {
        let request = request(PdeEffect::none(), Holds::True);
        let receipt = request.seal().unwrap();

        // Recompute the binding the pde way: canonical_digest over the whole
        // body with only the outermost receipt_digest removed.
        let mut value = serde_json::to_value(&receipt).unwrap();
        value.as_object_mut().unwrap().remove("receipt_digest");
        // The nested effect.receipt_digest key must still be present in the body.
        assert!(value["effect"]
            .as_object()
            .unwrap()
            .contains_key("receipt_digest"));
        let expected = canonical_digest(&value).unwrap();
        assert_eq!(receipt.receipt_digest, expected);
        assert_eq!(
            receipt.receipt_digest,
            receipt.expected_receipt_digest().unwrap()
        );
    }

    #[test]
    fn blind_spot_effect_is_unknown_not_fabricated() {
        // An indeterminate projection may carry only unknown/none.
        let ok = request(PdeEffect::unknown(), Holds::Null);
        assert_eq!(ok.projection.outcome, ProjectionOutcome::Indeterminate);
        ok.seal().unwrap();

        // Asserting a decided effect on that blind spot is refused (绝不瞎猜).
        let decided = request(
            PdeEffect {
                state: PdeEffectState::Applied,
                receipt_ref: Some("file:///effect".to_string()),
                receipt_digest: None,
            },
            Holds::Null,
        );
        assert!(decided.seal().is_err());
    }

    #[test]
    fn tampered_receipt_digest_is_rejected() {
        let receipt = request(PdeEffect::none(), Holds::True).seal().unwrap();
        let mut value = serde_json::to_value(&receipt).unwrap();
        value.as_object_mut().unwrap().insert(
            "receipt_digest".to_string(),
            serde_json::json!(format!("sha256:{}", "a".repeat(64))),
        );
        assert!(PdeCampaignPhaseReceipt::from_json_value(&value).is_err());
    }

    #[test]
    fn tampered_body_breaks_the_binding() {
        let receipt = request(PdeEffect::none(), Holds::True).seal().unwrap();
        let mut value = serde_json::to_value(&receipt).unwrap();
        // Flip a bound field but keep the old digest — binding must fail.
        value
            .as_object_mut()
            .unwrap()
            .insert("owner".to_string(), serde_json::json!("someone-else"));
        assert!(PdeCampaignPhaseReceipt::from_json_value(&value).is_err());
    }

    #[test]
    fn missing_required_key_is_rejected() {
        let receipt = request(PdeEffect::none(), Holds::True).seal().unwrap();
        for key in TOP_LEVEL_KEYS {
            let mut value = serde_json::to_value(&receipt).unwrap();
            value.as_object_mut().unwrap().remove(key);
            assert!(
                PdeCampaignPhaseReceipt::from_json_value(&value).is_err(),
                "removing `{key}` must be rejected"
            );
        }
        // Nested effect required keys are enforced too.
        for key in ["receipt_ref", "receipt_digest"] {
            let mut value = serde_json::to_value(&receipt).unwrap();
            value["effect"].as_object_mut().unwrap().remove(key);
            assert!(
                PdeCampaignPhaseReceipt::from_json_value(&value).is_err(),
                "removing `effect.{key}` must be rejected"
            );
        }
    }

    #[test]
    fn unknown_field_is_rejected() {
        let receipt = request(PdeEffect::none(), Holds::True).seal().unwrap();
        let mut value = serde_json::to_value(&receipt).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(1));
        assert!(PdeCampaignPhaseReceipt::from_json_value(&value).is_err());
    }

    #[test]
    fn bad_campaign_id_and_datetime_and_revision_are_rejected() {
        // A campaign_id that violates the frozen id pattern.
        let mut bad_id = request(PdeEffect::none(), Holds::True);
        bad_id.campaign_id = "has space".to_string();
        assert!(bad_id.seal().is_err());

        // A naive timestamp (no timezone) is rejected.
        let mut naive = request(PdeEffect::none(), Holds::True);
        naive.observed_at = "2026-09-15T08:30:00".to_string();
        assert!(naive.seal().is_err());

        // expected_campaign_revision below the frozen minimum.
        let mut low = request(PdeEffect::none(), Holds::True);
        low.expected_campaign_revision = 0;
        assert!(low.seal().is_err());
    }

    #[test]
    fn revision_accepts_integer_string_and_null() {
        for revision in [
            serde_json::json!(7),
            serde_json::json!("r7"),
            serde_json::Value::Null,
        ] {
            let artifact = PdeArtifactRef {
                artifact_type: "authority/v1".to_string(),
                artifact_ref: "authority-ref".to_string(),
                digest: Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap(),
                revision,
            };
            assert!(artifact.validate("authority_artifact").is_ok());
        }
        // A float revision is not an integer and must be rejected.
        let bad = PdeArtifactRef {
            artifact_type: "authority/v1".to_string(),
            artifact_ref: "authority-ref".to_string(),
            digest: Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap(),
            revision: serde_json::json!(1.5),
        };
        assert!(bad.validate("authority_artifact").is_err());
    }

    #[test]
    fn schema_version_const_matches_frozen() {
        assert_eq!(
            PdeCampaignPhaseReceipt::SCHEMA_VERSION,
            "pde_campaign_phase_receipt/v1"
        );
        assert_eq!(PDE_CAMPAIGN_PROTOCOL_REVISION, 1);
        assert_eq!(UI_PROJECTION_ARTIFACT_TYPE, crate::UI_PROJECTION_SCHEMA);
    }

    #[test]
    fn rfc3339_accepts_offsets_and_fractions() {
        assert!(is_rfc3339("2026-09-15T08:30:00Z"));
        assert!(is_rfc3339("2026-09-15T08:30:00.123456Z"));
        assert!(is_rfc3339("2026-09-15T08:30:00+08:00"));
        assert!(is_rfc3339("2026-09-15T08:30:00-05:30"));
        assert!(!is_rfc3339("2026-09-15T08:30:00"));
        assert!(!is_rfc3339("2026-09-15 08:30:00Z"));
        assert!(!is_rfc3339("2026-13-15T08:30:00Z"));
        assert!(!is_rfc3339("not-a-time"));
    }
}
