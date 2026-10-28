//! digest — the `sha256:<64 lowercase hex>` digest form external freezes and shuttle copies.
//!
//! external anchors several fields on this exact shape (`external_attempt/v1`'s
//! `context_manifest_digest`, `external_text_transaction/v1`'s `previous_digest`/
//! `digest`, …). Per the contract note, shuttle **adopts** this form verbatim and
//! **never recomputes** it — a digest handed down by external is copied byte-for-byte
//! into the attempt/receipt chain.
//!
//! > 待与 upstream 正本核对: the authoritative pattern lives in
//! > `upstream/collaboration-fabric/assets/schemas/*.schema.json`, which is not
//! > materialized on this machine. This newtype encodes the ruling's stated
//! > shape (`^sha256:[0-9a-f]{64}$`); reconcile on first upstream sync.

use serde::{Deserialize, Serialize};

/// An external content digest in canonical `sha256:<64 lowercase hex>` form.
///
/// Constructed only through [`Sha256Digest::parse`] so an ill-formed value can
/// never enter the attempt/receipt chain. The inner string is kept private;
/// `as_str` exposes the verbatim value shuttle copies onward.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Digest(String);

/// Why a digest string was rejected (fail-closed: shuttle never fabricates one).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DigestError {
    #[error("digest must start with the `sha256:` prefix: {0}")]
    MissingPrefix(String),
    #[error("digest hex must be exactly 64 lowercase hex chars: {0}")]
    BadHex(String),
}

impl Sha256Digest {
    /// Validate and wrap an external digest string. The value is preserved verbatim so
    /// shuttle can copy it onward without re-canonicalizing (never
    /// recompute).
    pub fn parse(s: impl Into<String>) -> Result<Self, DigestError> {
        let s = s.into();
        let Some(hex) = s.strip_prefix("sha256:") else {
            return Err(DigestError::MissingPrefix(s));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(DigestError::BadHex(s));
        }
        Ok(Sha256Digest(s))
    }

    /// The verbatim `sha256:…` string, for copying into attempt/receipt fields.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Sha256Digest {
    type Error = DigestError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Sha256Digest::parse(s)
    }
}

impl From<Sha256Digest> for String {
    fn from(d: Sha256Digest) -> String {
        d.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> String {
        format!("sha256:{}", "a".repeat(64))
    }

    #[test]
    fn accepts_canonical_form() {
        let d = Sha256Digest::parse(valid()).unwrap();
        assert_eq!(d.as_str(), valid());
        assert_eq!(d.to_string(), valid());
    }

    #[test]
    fn rejects_missing_prefix() {
        assert!(matches!(
            Sha256Digest::parse("a".repeat(64)),
            Err(DigestError::MissingPrefix(_))
        ));
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(matches!(
            Sha256Digest::parse("sha256:abc"),
            Err(DigestError::BadHex(_))
        ));
    }

    #[test]
    fn rejects_uppercase_hex() {
        let upper = format!("sha256:{}", "A".repeat(64));
        assert!(matches!(
            Sha256Digest::parse(upper),
            Err(DigestError::BadHex(_))
        ));
    }

    #[test]
    fn round_trips_through_serde() {
        let d = Sha256Digest::parse(valid()).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{}\"", valid()));
        let back: Sha256Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn serde_rejects_bad_digest() {
        let bad = format!("\"sha256:{}\"", "z".repeat(64));
        assert!(serde_json::from_str::<Sha256Digest>(&bad).is_err());
    }
}
