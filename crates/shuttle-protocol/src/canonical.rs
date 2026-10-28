//! Deterministic JSON encoding for SHUTTLE-owned closed wire contracts.

use crate::anchor::Sha256Digest;
use serde::Serialize;
use std::collections::BTreeMap;

/// Errors shared by closed SHUTTLE wire contracts.
#[derive(Debug, thiserror::Error)]
pub enum ClosedContractError {
    #[error("invalid closed contract: {0}")]
    Invalid(String),
    #[error("invalid closed contract JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Recursively sort object keys and emit compact JSON. Array order remains a
/// contract-level semantic and is validated by each closed envelope.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ClosedContractError> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_vec(&canonicalize(value))?)
}

/// Digest the deterministic JSON representation of an SHUTTLE-owned value.
pub fn canonical_digest<T: Serialize>(value: &T) -> Result<Sha256Digest, ClosedContractError> {
    let bytes = canonical_json(value)?;
    Sha256Digest::parse(format!("sha256:{}", crate::sha256_hex(&bytes)))
        .map_err(|error| ClosedContractError::Invalid(error.to_string()))
}

fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        serde_json::Value::Object(items) => {
            let sorted: BTreeMap<_, _> = items
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}
