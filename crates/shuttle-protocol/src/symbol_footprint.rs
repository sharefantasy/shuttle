//! Snapshot-bound exact symbol identity and footprint wire contract.
//!
//! Exactness here means unique within the bound repository base and symbol
//! index snapshot. It is evidence, never path-write or execution authority.

use crate::anchor::Sha256Digest;
use crate::canonical::{canonical_digest, canonical_json, ClosedContractError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const SYMBOL_FOOTPRINT_SCHEMA: &str = "shuttle_symbol_footprint/v1";
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// The exact fields committed by `symbol_id`. No path compatibility alias or
/// nested anchor is permitted in this closed shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SymbolIdentity {
    pub symbol_id: Sha256Digest,
    pub repo_id: String,
    pub base_oid: String,
    pub language: String,
    pub normalized_path: String,
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub line: u64,
    pub signature_digest: Sha256Digest,
}

#[derive(Serialize)]
struct SymbolIdentityPayload<'a> {
    repo_id: &'a str,
    base_oid: &'a str,
    language: &'a str,
    normalized_path: &'a str,
    name: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    line: u64,
    signature_digest: &'a Sha256Digest,
}

impl SymbolIdentity {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo_id: impl Into<String>,
        base_oid: impl Into<String>,
        language: impl Into<String>,
        normalized_path: impl Into<String>,
        name: impl Into<String>,
        kind: impl Into<String>,
        scope: Option<String>,
        line: u64,
        signature_digest: Sha256Digest,
    ) -> Result<Self, ClosedContractError> {
        let mut identity = Self {
            symbol_id: Sha256Digest::parse(format!("sha256:{}", "0".repeat(64)))
                .expect("fixed placeholder digest is valid"),
            repo_id: repo_id.into(),
            base_oid: base_oid.into(),
            language: language.into(),
            normalized_path: normalized_path.into(),
            name: name.into(),
            kind: kind.into(),
            scope,
            line,
            signature_digest,
        };
        identity.validate_fields()?;
        identity.symbol_id = identity.expected_symbol_id()?;
        Ok(identity)
    }

    /// Convenience constructor for producers holding the raw definition line.
    #[allow(clippy::too_many_arguments)]
    pub fn from_signature_bytes(
        repo_id: impl Into<String>,
        base_oid: impl Into<String>,
        language: impl Into<String>,
        normalized_path: impl Into<String>,
        name: impl Into<String>,
        kind: impl Into<String>,
        scope: Option<String>,
        line: u64,
        signature: &[u8],
    ) -> Result<Self, ClosedContractError> {
        let signature_digest =
            Sha256Digest::parse(format!("sha256:{}", crate::sha256_hex(signature)))
                .expect("sha256_hex always emits a canonical digest");
        Self::new(
            repo_id,
            base_oid,
            language,
            normalized_path,
            name,
            kind,
            scope,
            line,
            signature_digest,
        )
    }

    /// Compute `sha256(canonical identity without symbol_id)`.
    pub fn expected_symbol_id(&self) -> Result<Sha256Digest, ClosedContractError> {
        canonical_digest(&self.payload())
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        self.validate_fields()?;
        if self.symbol_id != self.expected_symbol_id()? {
            return Err(invalid(
                "symbol_id does not match the canonical identity payload",
            ));
        }
        Ok(())
    }

    fn payload(&self) -> SymbolIdentityPayload<'_> {
        SymbolIdentityPayload {
            repo_id: &self.repo_id,
            base_oid: &self.base_oid,
            language: &self.language,
            normalized_path: &self.normalized_path,
            name: &self.name,
            kind: &self.kind,
            scope: self.scope.as_deref(),
            line: self.line,
            signature_digest: &self.signature_digest,
        }
    }

    fn validate_fields(&self) -> Result<(), ClosedContractError> {
        validate_text_id("repo_id", &self.repo_id)?;
        validate_oid("base_oid", &self.base_oid)?;
        validate_text_id("language", &self.language)?;
        validate_relative_path(&self.normalized_path)?;
        validate_text_id("name", &self.name)?;
        validate_text_id("kind", &self.kind)?;
        if let Some(scope) = &self.scope {
            validate_text_id("scope", scope)?;
        }
        if self.line == 0 {
            return Err(invalid("line must be one-based"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SymbolRepositoryBase {
    pub repo_id: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SymbolCoverage {
    None,
    Partial,
    Exact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SymbolIndexFreshness {
    Fresh,
    Stale,
    Absent,
    Unsupported,
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SymbolFootprintBasis {
    pub repositories: Vec<SymbolRepositoryBase>,
    pub symbol_index_ref: String,
    pub symbol_index_digest: Sha256Digest,
    pub analyzer_version: String,
    pub coverage: SymbolCoverage,
    pub freshness: SymbolIndexFreshness,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedSymbolReason {
    Ambiguous,
    Unsupported,
    Stale,
    Absent,
    Timeout,
    Overflow,
    InvalidDeclaration,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedSymbol {
    pub declaration: String,
    pub reason: UnresolvedSymbolReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlannerEvidenceSource {
    PlannerDeclaredValidated,
    PlannerDeclaredUnresolved,
    PlannerGuess,
}

/// Pre-execution declaration evidence. It cannot contain observed diff facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedSymbolEvidence {
    pub evidence_source: PlannerEvidenceSource,
    #[serde(default)]
    pub declared_read_symbols: Vec<SymbolIdentity>,
    #[serde(default)]
    pub declared_write_symbols: Vec<SymbolIdentity>,
    #[serde(default)]
    pub unresolved: Vec<UnresolvedSymbol>,
}

impl PlannedSymbolEvidence {
    pub fn validated(
        declared_read_symbols: Vec<SymbolIdentity>,
        declared_write_symbols: Vec<SymbolIdentity>,
    ) -> Self {
        Self {
            evidence_source: PlannerEvidenceSource::PlannerDeclaredValidated,
            declared_read_symbols,
            declared_write_symbols,
            unresolved: Vec::new(),
        }
    }

    pub fn unresolved(unresolved: Vec<UnresolvedSymbol>) -> Self {
        Self {
            evidence_source: PlannerEvidenceSource::PlannerDeclaredUnresolved,
            declared_read_symbols: Vec::new(),
            declared_write_symbols: Vec::new(),
            unresolved,
        }
    }

    pub fn planner_guess(unresolved: Vec<UnresolvedSymbol>) -> Self {
        Self {
            evidence_source: PlannerEvidenceSource::PlannerGuess,
            declared_read_symbols: Vec::new(),
            declared_write_symbols: Vec::new(),
            unresolved,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedEvidenceSource {
    #[default]
    NotObserved,
    ObservedDiff,
}

/// Post-execution diff/hunk evidence. Its type and source are disjoint from
/// [`PlannedSymbolEvidence`], preventing a planned declaration being relabeled
/// as an observed write.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservedSymbolEvidence {
    pub evidence_source: ObservedEvidenceSource,
    #[serde(default)]
    pub observed_write_symbols: Vec<SymbolIdentity>,
    #[serde(default)]
    pub unresolved: Vec<UnresolvedSymbol>,
}

impl ObservedSymbolEvidence {
    pub fn not_observed() -> Self {
        Self::default()
    }

    pub fn observed_diff(
        observed_write_symbols: Vec<SymbolIdentity>,
        unresolved: Vec<UnresolvedSymbol>,
    ) -> Self {
        Self {
            evidence_source: ObservedEvidenceSource::ObservedDiff,
            observed_write_symbols,
            unresolved,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskSymbolFootprint {
    pub task_id: String,
    pub planned: PlannedSymbolEvidence,
    pub observed: ObservedSymbolEvidence,
    /// Must remain false. Symbol evidence never grants path write authority.
    pub grants_path_authority: bool,
}

impl TaskSymbolFootprint {
    pub fn new(
        task_id: impl Into<String>,
        planned: PlannedSymbolEvidence,
        observed: ObservedSymbolEvidence,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            planned,
            observed,
            grants_path_authority: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SymbolFootprint {
    pub schema_version: String,
    pub basis: SymbolFootprintBasis,
    pub tasks: Vec<TaskSymbolFootprint>,
}

impl SymbolFootprint {
    /// Construct a canonical, sorted envelope and validate every identity.
    pub fn new(
        mut basis: SymbolFootprintBasis,
        mut tasks: Vec<TaskSymbolFootprint>,
    ) -> Result<Self, ClosedContractError> {
        basis.repositories.sort();
        basis.degraded_reasons.sort();
        basis.degraded_reasons.dedup();
        for task in &mut tasks {
            normalize_task(task);
        }
        tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        let value = Self {
            schema_version: SYMBOL_FOOTPRINT_SCHEMA.to_string(),
            basis,
            tasks,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ClosedContractError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("payload exceeds the 4MiB input limit"));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ClosedContractError> {
        if self.schema_version != SYMBOL_FOOTPRINT_SCHEMA {
            return Err(invalid(format!(
                "unsupported schema_version: {}",
                self.schema_version
            )));
        }
        validate_text_id("symbol_index_ref", &self.basis.symbol_index_ref)?;
        validate_text_id("analyzer_version", &self.basis.analyzer_version)?;
        validate_strictly_sorted("repositories", &self.basis.repositories)?;
        if self.basis.repositories.is_empty() {
            return Err(invalid("repositories must not be empty"));
        }
        let mut repository_ids = BTreeSet::new();
        for repository in &self.basis.repositories {
            validate_text_id("repositories.repo_id", &repository.repo_id)?;
            validate_oid("repositories.base_oid", &repository.base_oid)?;
            if !repository_ids.insert(repository.repo_id.as_str()) {
                return Err(invalid(
                    "repository bindings must have unique repo_id values",
                ));
            }
        }
        validate_sorted_strings("degraded_reasons", &self.basis.degraded_reasons)?;
        if self.basis.coverage == SymbolCoverage::Exact
            && (self.basis.freshness != SymbolIndexFreshness::Fresh || self.basis.truncated)
        {
            return Err(invalid(
                "exact coverage requires a fresh, non-truncated symbol index",
            ));
        }
        if (self.basis.freshness != SymbolIndexFreshness::Fresh || self.basis.truncated)
            && self.basis.degraded_reasons.is_empty()
        {
            return Err(invalid(
                "non-fresh or truncated basis requires degraded_reasons",
            ));
        }
        if !self
            .tasks
            .windows(2)
            .all(|window| window[0].task_id < window[1].task_id)
        {
            return Err(invalid("tasks must be strictly sorted by task_id"));
        }
        for task in &self.tasks {
            validate_task(task, &self.basis.repositories)?;
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ClosedContractError> {
        self.validate()?;
        canonical_json(self)
    }

    pub fn canonical_digest(&self) -> Result<Sha256Digest, ClosedContractError> {
        self.validate()?;
        canonical_digest(self)
    }
}

fn normalize_task(task: &mut TaskSymbolFootprint) {
    task.planned.declared_read_symbols.sort();
    task.planned.declared_read_symbols.dedup();
    task.planned.declared_write_symbols.sort();
    task.planned.declared_write_symbols.dedup();
    task.planned.unresolved.sort();
    task.planned.unresolved.dedup();
    task.observed.observed_write_symbols.sort();
    task.observed.observed_write_symbols.dedup();
    task.observed.unresolved.sort();
    task.observed.unresolved.dedup();
}

fn validate_task(
    task: &TaskSymbolFootprint,
    repositories: &[SymbolRepositoryBase],
) -> Result<(), ClosedContractError> {
    validate_text_id("task_id", &task.task_id)?;
    if task.grants_path_authority {
        return Err(invalid("grants_path_authority must be false"));
    }
    for (name, symbols) in [
        ("declared_read_symbols", &task.planned.declared_read_symbols),
        (
            "declared_write_symbols",
            &task.planned.declared_write_symbols,
        ),
        (
            "observed_write_symbols",
            &task.observed.observed_write_symbols,
        ),
    ] {
        validate_strictly_sorted(name, symbols)?;
        for symbol in symbols {
            symbol.validate()?;
            if !repositories.iter().any(|repository| {
                repository.repo_id == symbol.repo_id && repository.base_oid == symbol.base_oid
            }) {
                return Err(invalid(format!(
                    "symbol {} has no matching repository binding",
                    symbol.symbol_id
                )));
            }
        }
    }
    validate_strictly_sorted("planned.unresolved", &task.planned.unresolved)?;
    validate_strictly_sorted("observed.unresolved", &task.observed.unresolved)?;
    for unresolved in task
        .planned
        .unresolved
        .iter()
        .chain(&task.observed.unresolved)
    {
        validate_text_id("unresolved.declaration", &unresolved.declaration)?;
    }
    match task.planned.evidence_source {
        PlannerEvidenceSource::PlannerDeclaredValidated if !task.planned.unresolved.is_empty() => {
            return Err(invalid(
                "planner_declared_validated must not contain unresolved declarations",
            ));
        }
        PlannerEvidenceSource::PlannerDeclaredUnresolved
            if task.planned.unresolved.is_empty()
                || !task.planned.declared_read_symbols.is_empty()
                || !task.planned.declared_write_symbols.is_empty() =>
        {
            return Err(invalid(
                "planner_declared_unresolved must contain only unresolved declarations",
            ));
        }
        PlannerEvidenceSource::PlannerGuess
            if !task.planned.declared_read_symbols.is_empty()
                || !task.planned.declared_write_symbols.is_empty() =>
        {
            return Err(invalid("planner_guess must not contain exact symbols"));
        }
        _ => {}
    }
    match task.observed.evidence_source {
        ObservedEvidenceSource::NotObserved
            if !task.observed.observed_write_symbols.is_empty()
                || !task.observed.unresolved.is_empty() =>
        {
            return Err(invalid("not_observed must not contain observed evidence"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), ClosedContractError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.as_bytes().get(1) == Some(&b':')
    {
        return Err(invalid(
            "normalized_path must be a normalized repository-relative '/' path",
        ));
    }
    Ok(())
}

pub(crate) fn validate_oid(name: &str, value: &str) -> Result<(), ClosedContractError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid(format!(
            "{name} must be a full 40- or 64-character lowercase hex OID"
        )));
    }
    Ok(())
}

pub(crate) fn validate_text_id(name: &str, value: &str) -> Result<(), ClosedContractError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "{name} must be non-empty, trimmed, control-free, and at most 4096 bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_sorted_strings(
    name: &str,
    values: &[String],
) -> Result<(), ClosedContractError> {
    for value in values {
        validate_text_id(name, value)?;
    }
    validate_strictly_sorted(name, values)
}

pub(crate) fn validate_strictly_sorted<T: Ord>(
    name: &str,
    values: &[T],
) -> Result<(), ClosedContractError> {
    if !values.windows(2).all(|window| window[0] < window[1]) {
        return Err(invalid(format!(
            "{name} must be strictly sorted and unique"
        )));
    }
    Ok(())
}

pub(crate) fn invalid(message: impl Into<String>) -> ClosedContractError {
    ClosedContractError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn digest(byte: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn identity(name: &str, path: &str, line: u64) -> SymbolIdentity {
        SymbolIdentity::new(
            "repo:shuttle",
            BASE,
            "rust",
            path,
            name,
            "function",
            Some("App".into()),
            line,
            digest('b'),
        )
        .unwrap()
    }

    fn basis() -> SymbolFootprintBasis {
        SymbolFootprintBasis {
            repositories: vec![SymbolRepositoryBase {
                repo_id: "repo:shuttle".into(),
                base_oid: BASE.into(),
            }],
            symbol_index_ref: "artifact://index".into(),
            symbol_index_digest: digest('c'),
            analyzer_version: "0.2.0".into(),
            coverage: SymbolCoverage::Exact,
            freshness: SymbolIndexFreshness::Fresh,
            truncated: false,
            degraded_reasons: Vec::new(),
        }
    }

    #[test]
    fn symbol_identity_digest_is_canonical_and_exact() {
        let symbol = identity("render", "src/lib.rs", 7);
        assert_eq!(
            symbol.symbol_id.as_str(),
            "sha256:ccd511f4f3d16c16294f831e30868d20bdc02eda3e4dcc674c898b1761f5ad88"
        );
        symbol.validate().unwrap();

        let mut tampered = symbol;
        tampered.line = 8;
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn symbol_identity_is_flat_closed_and_rejects_legacy_anchor() {
        let value = serde_json::to_value(identity("render", "src/lib.rs", 7)).unwrap();
        assert!(value.get("line").is_some());
        assert!(value.get("signature_digest").is_some());
        assert!(value.get("definition_anchor").is_none());
        assert!(value.get("path").is_none());

        let mut object = value.as_object().unwrap().clone();
        object.insert("unknown".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<SymbolIdentity>(object.into()).is_err());

        let mut legacy = value.as_object().unwrap().clone();
        let line = legacy.remove("line").unwrap();
        let signature_digest = legacy.remove("signature_digest").unwrap();
        legacy.insert(
            "definition_anchor".into(),
            serde_json::json!({"line": line, "signature_digest": signature_digest}),
        );
        assert!(serde_json::from_value::<SymbolIdentity>(legacy.into()).is_err());
    }

    #[test]
    fn footprint_constructor_sorts_for_byte_identical_canonical_digest() {
        let a = identity("alpha", "src/a.rs", 1);
        let z = identity("zeta", "src/z.rs", 2);
        let make = |symbols: Vec<SymbolIdentity>| {
            SymbolFootprint::new(
                basis(),
                vec![TaskSymbolFootprint::new(
                    "task-a",
                    PlannedSymbolEvidence::validated(Vec::new(), symbols),
                    ObservedSymbolEvidence::not_observed(),
                )],
            )
            .unwrap()
        };
        let forward = make(vec![a.clone(), z.clone()]);
        let reverse = make(vec![z, a]);
        assert_eq!(
            forward.canonical_json().unwrap(),
            reverse.canonical_json().unwrap()
        );
        assert_eq!(
            forward.canonical_digest().unwrap(),
            reverse.canonical_digest().unwrap()
        );
        SymbolFootprint::parse(&forward.canonical_json().unwrap()).unwrap();
    }

    #[test]
    fn planned_and_observed_sources_cannot_be_confused() {
        let exact = identity("render", "src/lib.rs", 7);
        let invalid_planned = TaskSymbolFootprint {
            task_id: "task-a".into(),
            planned: PlannedSymbolEvidence {
                evidence_source: PlannerEvidenceSource::PlannerGuess,
                declared_read_symbols: Vec::new(),
                declared_write_symbols: vec![exact.clone()],
                unresolved: Vec::new(),
            },
            observed: ObservedSymbolEvidence::not_observed(),
            grants_path_authority: false,
        };
        assert!(SymbolFootprint::new(basis(), vec![invalid_planned]).is_err());

        let invalid_observed = TaskSymbolFootprint {
            task_id: "task-a".into(),
            planned: PlannedSymbolEvidence::validated(Vec::new(), Vec::new()),
            observed: ObservedSymbolEvidence {
                evidence_source: ObservedEvidenceSource::NotObserved,
                observed_write_symbols: vec![exact],
                unresolved: Vec::new(),
            },
            grants_path_authority: false,
        };
        assert!(SymbolFootprint::new(basis(), vec![invalid_observed]).is_err());
    }

    #[test]
    fn incomplete_basis_and_unresolved_evidence_fail_closed() {
        let mut stale = basis();
        stale.freshness = SymbolIndexFreshness::Stale;
        assert!(SymbolFootprint::new(stale.clone(), Vec::new()).is_err());

        stale.coverage = SymbolCoverage::Partial;
        stale.degraded_reasons = vec!["stale".into()];
        let unresolved = UnresolvedSymbol {
            declaration: "render@src/lib.rs".into(),
            reason: UnresolvedSymbolReason::Stale,
        };
        let footprint = SymbolFootprint::new(
            stale,
            vec![TaskSymbolFootprint::new(
                "task-a",
                PlannedSymbolEvidence::unresolved(vec![unresolved]),
                ObservedSymbolEvidence::not_observed(),
            )],
        )
        .unwrap();
        assert_eq!(footprint.basis.coverage, SymbolCoverage::Partial);
        assert_eq!(footprint.basis.freshness, SymbolIndexFreshness::Stale);
        assert!(!footprint.tasks[0].grants_path_authority);

        let mut value = serde_json::to_value(&footprint).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), serde_json::Value::Null);
        assert!(SymbolFootprint::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
