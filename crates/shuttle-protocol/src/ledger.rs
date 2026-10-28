//! ledger — the G9·D2 **provenance ledger**: every adjudicated edge/verdict
//! carries a replayable evidence chain, so «绝不瞎猜» becomes an *auditable*
//! institution rather than a mere runtime discipline
//! ().
//!
//! # The record
//! One [`ProvenanceEntry`] per adjudicated edge: the edge endpoints, the
//! machine's verdict (AGREE/ADD/DROP/CONFLICT/UNKNOWN), the edge's provenance
//! and confidence, a **content-addressed digest of the evidence text**, and the
//! `snapshot_oid` (the commit the adjudication ran against). A whole
//! adjudication is a [`ProvenanceLedger`].
//!
//! # Minimal replayable seed — never the full text (§2 坑 #14 防爆盘)
//! The ledger stores the evidence's [`Sha256Digest`], **not** the evidence
//! string. Auditing replays it: re-derive the evidence at `snapshot_oid`,
//! re-hash it with [`crate::hashing::sha256_hex`], and the digest must match
//! bit-for-bit. So the ledger stays small and tamper-evident — a fabricated or
//! drifted evidence text cannot reproduce the recorded digest. An edge whose
//! `SplitEdge::evidence` was empty (a bare name-match the corrector could not
//! attribute) records **no** digest — an honest "no evidence", never a hash of
//! the empty string dressed up as proof (§7).
//!
//! # Same source as the frozen receipt
//! The digest is the canonical `sha256:<64 hex>` [`Sha256Digest`] from
//! the digest module, so a ledger entry drops straight into the audit
//! chain next to the 9-field receipt. shuttle mints the digest for evidence
//! it *itself* produced (via [`crate::hashing`]); a digest handed down by an
//! upstream is still adopted verbatim elsewhere.

use serde::{Deserialize, Serialize};

use crate::anchor::Sha256Digest;
use crate::hashing::sha256_hex;
use crate::tool_outcome::{EdgeSource, EdgeVerdict, OutcomeConfidence, SplitDetail, SplitEdge};

/// Schema version stamped into a persisted ledger so a later auditor can reject
/// or migrate an incompatible file.
pub const LEDGER_VERSION: u32 = 1;

/// Mint the canonical `sha256:<64 hex>` digest of a piece of evidence text
/// shuttle produced. Returns a validated [`Sha256Digest`] in the canonical
/// form.
///
/// This is the one place shuttle *computes* a digest (as opposed to adopting a
/// upstream-handed one verbatim): the seed the D2 ledger stores in place of
/// the full evidence text.
pub fn evidence_digest(evidence: &str) -> Sha256Digest {
    let hex = sha256_hex(evidence.as_bytes());
    // `sha256_hex` is 64 lowercase hex by construction, so `parse` cannot fail;
    // fall back to a re-parse rather than `unwrap` to keep the mint total.
    Sha256Digest::parse(format!("sha256:{hex}"))
        .expect("sha256_hex always yields 64 lowercase hex chars")
}

/// One replayable provenance record for a single adjudicated edge.
///
/// Deliberately carries the *seed*, not the evidence: `evidence_digest` is
/// `Some` iff the edge carried non-empty evidence. Everything here is copied
/// straight off the [`SplitEdge`] the corrector already decided — this struct
/// never re-adjudicates (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceEntry {
    /// Prerequisite task id (the `from` a `to` depends on).
    pub from: String,
    /// Dependent task id.
    pub to: String,
    /// The machine's three-state-plus verdict for the edge.
    pub verdict: EdgeVerdict,
    /// Which source(s) attested the edge (intent / symbolgraph / both).
    pub source: EdgeSource,
    /// The confidence grade the corrector assigned.
    pub confidence: OutcomeConfidence,
    /// Content-addressed digest of the evidence text. `None` when the edge had
    /// no attributable evidence (an honest "no proof", never a hash of "").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<Sha256Digest>,
}

impl ProvenanceEntry {
    /// Distill one [`SplitEdge`] into a replayable seed. Empty evidence → no
    /// digest (§7: we never manufacture a proof for an unattributed edge).
    pub fn from_edge(edge: &SplitEdge) -> Self {
        let evidence_digest = if edge.evidence.is_empty() {
            None
        } else {
            Some(evidence_digest(&edge.evidence))
        };
        ProvenanceEntry {
            from: edge.from.clone(),
            to: edge.to.clone(),
            verdict: edge.verdict,
            source: edge.source,
            confidence: edge.confidence,
            evidence_digest,
        }
    }

    /// Verify a candidate evidence text against the recorded digest — the
    /// audit-time replay primitive. `true` iff this entry recorded a digest and
    /// re-hashing `evidence` reproduces it bit-for-bit. An entry with no
    /// recorded digest can never be "confirmed" by any text (there was nothing
    /// to prove), so this returns `false`.
    pub fn replay_matches(&self, evidence: &str) -> bool {
        match &self.evidence_digest {
            Some(d) => evidence_digest(evidence).as_str() == d.as_str(),
            None => false,
        }
    }
}

/// A whole adjudication's provenance: the goal, the commit it ran against, and
/// one [`ProvenanceEntry`] per edge. Serialize this to `--ledger <path>` for
/// offline audit / replay; it is deliberately separate from the stdout
/// `ToolOutcome` envelope (the machine contract stays unchanged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceLedger {
    /// Schema version ([`LEDGER_VERSION`]).
    pub version: u32,
    /// The goal label carried through the split (may be empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    /// The commit oid the adjudication ran against — the replay coordinate.
    /// Empty outside git (no HEAD); replay then keys on content alone.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_oid: String,
    /// One seed per adjudicated edge, in the corrector's edge order.
    pub entries: Vec<ProvenanceEntry>,
}

impl ProvenanceLedger {
    /// Build the ledger from an adjudicated [`SplitDetail`] and the commit it
    /// ran against. Pure projection: one entry per edge, evidence reduced to a
    /// digest seed, no full text retained.
    pub fn from_split(detail: &SplitDetail, snapshot_oid: impl Into<String>) -> Self {
        ProvenanceLedger {
            version: LEDGER_VERSION,
            goal: detail.goal.clone(),
            snapshot_oid: snapshot_oid.into(),
            entries: detail
                .edges
                .iter()
                .map(ProvenanceEntry::from_edge)
                .collect(),
        }
    }

    /// The count of entries that carry a replayable digest (i.e. had evidence).
    /// The rest are honest "no proof" records.
    pub fn attributed(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.evidence_digest.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_outcome::{SplitDetail, SplitEdge, SplitNode};

    fn edge(from: &str, to: &str, verdict: EdgeVerdict, source: EdgeSource, ev: &str) -> SplitEdge {
        SplitEdge {
            from: from.into(),
            to: to.into(),
            verdict,
            confidence: OutcomeConfidence::High,
            source,
            evidence: ev.into(),
        }
    }

    fn detail() -> SplitDetail {
        SplitDetail {
            goal: "g".into(),
            nodes: vec![SplitNode {
                id: "a".into(),
                intent: String::new(),
                touched_files: vec![],
            }],
            edges: vec![
                edge(
                    "a",
                    "b",
                    EdgeVerdict::Add,
                    EdgeSource::Symbolgraph,
                    "import a from b at src/b.py:3",
                ),
                // Unknown edge with no evidence — the honest "no proof" case.
                edge("c", "d", EdgeVerdict::Unknown, EdgeSource::Intent, ""),
            ],
            added: 1,
            dropped: 0,
            review_surface: 1,
            review_required: vec![1],
        }
    }

    #[test]
    fn entry_records_digest_only_when_evidence_present() {
        let l = ProvenanceLedger::from_split(&detail(), "deadbeef");
        assert_eq!(l.version, LEDGER_VERSION);
        assert_eq!(l.snapshot_oid, "deadbeef");
        assert_eq!(l.entries.len(), 2);
        assert!(l.entries[0].evidence_digest.is_some());
        assert!(l.entries[1].evidence_digest.is_none());
        assert_eq!(l.attributed(), 1);
    }

    #[test]
    fn digest_is_canonical_cf_form() {
        let d = evidence_digest("some evidence");
        assert!(d.as_str().starts_with("sha256:"));
        assert_eq!(d.as_str().len(), "sha256:".len() + 64);
    }

    #[test]
    fn replay_matches_the_original_evidence() {
        let ev = "import a from b at src/b.py:3";
        let l = ProvenanceLedger::from_split(&detail(), "oid1");
        let entry = &l.entries[0];
        assert!(entry.replay_matches(ev), "original evidence must replay");
        assert!(
            !entry.replay_matches("tampered evidence"),
            "a different text must not reproduce the digest"
        );
    }

    #[test]
    fn no_digest_entry_never_confirmed() {
        let l = ProvenanceLedger::from_split(&detail(), "oid1");
        let unknown = &l.entries[1];
        // There was nothing to prove — no text can "confirm" it.
        assert!(!unknown.replay_matches(""));
        assert!(!unknown.replay_matches("anything"));
    }

    #[test]
    fn ledger_holds_no_full_evidence_text() {
        // The serialized ledger must contain only digests, never the evidence.
        let l = ProvenanceLedger::from_split(&detail(), "oid1");
        let json = serde_json::to_string(&l).unwrap();
        assert!(
            !json.contains("import a from b"),
            "ledger must not carry full evidence text: {json}"
        );
        assert!(
            json.contains("sha256:"),
            "ledger must carry the digest seed"
        );
    }

    #[test]
    fn ledger_roundtrips_through_json() {
        let l = ProvenanceLedger::from_split(&detail(), "oid1");
        let s = serde_json::to_string(&l).unwrap();
        let back: ProvenanceLedger = serde_json::from_str(&s).unwrap();
        assert_eq!(l, back);
    }

    #[test]
    fn empty_split_yields_empty_ledger() {
        let d = SplitDetail {
            goal: String::new(),
            nodes: vec![],
            edges: vec![],
            added: 0,
            dropped: 0,
            review_surface: 0,
            review_required: vec![],
        };
        let l = ProvenanceLedger::from_split(&d, "");
        assert!(l.entries.is_empty());
        assert_eq!(l.attributed(), 0);
        // snapshot_oid empty is elided; goal empty is elided.
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("snapshot_oid"));
        assert!(!json.contains("\"goal\""));
    }
}
