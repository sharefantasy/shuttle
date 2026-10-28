//! capability — the fail-closed effective-capability intersection.
//!
//! Effective authority = `admission (external ceiling) ∩ execution_profile (shuttle-local)
//! ∩ resource_claim/context_manifest (task narrowing)`. Any term that fails to
//! grant → the capability is denied (fail-closed). shuttle NEVER widens beyond the
//! external `admission` ceiling; over-scope requests fail closed rather than clamp
//! silently, so a mis-scoped task is refused, not quietly downgraded.

use super::contracts::{Admission, ContextManifest, ResourceClaim, TargetKindsAuthorized};

/// The shuttle-local execution profile's read/write grant. The profile
/// document itself is NOT frozen — `execution_profile_ref` points at an
/// shuttle-local doc — so this is a minimal projection of the two switches the
/// intersection needs.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionProfileGrant {
    pub allow_readonly: bool,
    pub allow_product_source_write: bool,
}

impl Default for ExecutionProfileGrant {
    /// Fail-closed default: a profile that grants nothing.
    fn default() -> Self {
        ExecutionProfileGrant {
            allow_readonly: false,
            allow_product_source_write: false,
        }
    }
}

/// The computed effective capability set after intersecting all three terms.
/// Every field is a floor-of-ceilings AND — a capability survives only if every
/// term granted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCapabilities {
    /// May shuttle read the product source at all?
    pub readonly: bool,
    /// May shuttle write product source? Requires admission + profile to BOTH grant
    /// it AND a concrete file_write_scope claim to exist (ruling §1, §3).
    pub product_source_write: bool,
    /// The write-domain globs (from the file_write_scope claim). Empty ⇒ no
    /// write domain ⇒ fail-closed even if the write bit is otherwise granted.
    pub write_scope_keys: Vec<String>,
    /// The command allowlist from the context manifest.
    pub tool_allowlist: Vec<String>,
}

/// Why an effective-capability computation refused a request (fail-closed).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapabilityError {
    /// A capability was requested that the external orchestrator admission ceiling does not grant.
    #[error(
        "over-scope: requested {capability} exceeds the external orchestrator admission ceiling"
    )]
    OverScope { capability: String },
}

/// Compute the effective capability set, fail-closed.
///
/// `admission` is the external orchestrator ceiling; `profile` the shuttle-local grant; `manifest` and
/// the optional file_write_scope `claim` narrow to the task. A capability is
/// effective iff **every** applicable term grants it.
pub fn effective_capabilities(
    admission: &Admission,
    profile: ExecutionProfileGrant,
    manifest: &ContextManifest,
    write_claim: Option<&ResourceClaim>,
) -> EffectiveCapabilities {
    let TargetKindsAuthorized {
        readonly: adm_ro,
        product_source_write: adm_write,
    } = admission.target_kinds_authorized;

    // Read: admission ∩ profile.
    let readonly = adm_ro && profile.allow_readonly;

    // Write domain: only from an exclusive_write file_write_scope claim.
    let write_scope_keys: Vec<String> = write_claim
        .and_then(|c| c.file_write_scope_keys())
        .map(|k| k.to_vec())
        .unwrap_or_default();

    // Write: admission ∩ profile ∩ (a non-empty write domain exists). Absent
    // any term, writing is denied (fail-closed).
    let product_source_write =
        adm_write && profile.allow_product_source_write && !write_scope_keys.is_empty();

    EffectiveCapabilities {
        readonly,
        product_source_write,
        write_scope_keys,
        tool_allowlist: manifest.tool_allowlist.clone(),
    }
}

/// Verify a requested capability set does not exceed the external orchestrator admission ceiling.
/// Returns the first over-scope violation (fail-closed): shuttle refuses rather than
/// clamping a task that asks for more than admission grants.
pub fn assert_within_ceiling(
    admission: &Admission,
    request_readonly: bool,
    request_write: bool,
) -> Result<(), CapabilityError> {
    let ceiling = admission.target_kinds_authorized;
    if request_readonly && !ceiling.readonly {
        return Err(CapabilityError::OverScope {
            capability: "readonly".into(),
        });
    }
    if request_write && !ceiling.product_source_write {
        return Err(CapabilityError::OverScope {
            capability: "product_source_write".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::contracts::{ClaimMode, ResourceType};
    use crate::anchor::digest::Sha256Digest;

    fn admission(readonly: bool, write: bool) -> Admission {
        Admission {
            authority: None,
            target_kinds_authorized: TargetKindsAuthorized {
                readonly,
                product_source_write: write,
            },
            max_parallelism: None,
            recovery_rto_ms: None,
            canary_status: None,
            canary_base_oid: None,
            cross_session_visibility: None,
            extra: Default::default(),
        }
    }

    fn manifest() -> ContextManifest {
        ContextManifest {
            context_manifest_id: "cm-1".into(),
            context_manifest_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
            tool_allowlist: vec!["git".into()],
            scope: None,
            write_claim_refs: vec![],
            source_versions: vec![],
            extra: Default::default(),
        }
    }

    fn write_claim(keys: Vec<String>) -> ResourceClaim {
        ResourceClaim {
            resource_type: ResourceType::FileWriteScope,
            resource_keys: keys,
            claim_mode: ClaimMode::ExclusiveWrite,
            lease_epoch: Some(1),
            extra: Default::default(),
        }
    }

    #[test]
    fn write_requires_admission_profile_and_claim() {
        let adm = admission(true, true);
        let prof = ExecutionProfileGrant {
            allow_readonly: true,
            allow_product_source_write: true,
        };
        let claim = write_claim(vec!["src/**".into()]);
        let eff = effective_capabilities(&adm, prof, &manifest(), Some(&claim));
        assert!(eff.readonly);
        assert!(eff.product_source_write);
        assert_eq!(eff.write_scope_keys, vec!["src/**".to_string()]);
    }

    #[test]
    fn no_claim_means_no_write_even_if_admission_and_profile_allow() {
        let adm = admission(true, true);
        let prof = ExecutionProfileGrant {
            allow_readonly: true,
            allow_product_source_write: true,
        };
        let eff = effective_capabilities(&adm, prof, &manifest(), None);
        assert!(!eff.product_source_write, "no write domain ⇒ fail-closed");
        assert!(eff.write_scope_keys.is_empty());
    }

    #[test]
    fn admission_denies_overrides_profile_grant() {
        let adm = admission(true, false); // ceiling forbids writes
        let prof = ExecutionProfileGrant {
            allow_readonly: true,
            allow_product_source_write: true, // profile would allow
        };
        let claim = write_claim(vec!["src/**".into()]);
        let eff = effective_capabilities(&adm, prof, &manifest(), Some(&claim));
        assert!(!eff.product_source_write, "admission ceiling wins");
    }

    #[test]
    fn default_profile_grants_nothing() {
        let adm = admission(true, true);
        let claim = write_claim(vec!["src/**".into()]);
        let eff = effective_capabilities(
            &adm,
            ExecutionProfileGrant::default(),
            &manifest(),
            Some(&claim),
        );
        assert!(!eff.readonly);
        assert!(!eff.product_source_write);
    }

    #[test]
    fn assert_within_ceiling_rejects_over_scope_write() {
        let adm = admission(true, false);
        assert_eq!(
            assert_within_ceiling(&adm, true, true),
            Err(CapabilityError::OverScope {
                capability: "product_source_write".into()
            })
        );
    }

    #[test]
    fn assert_within_ceiling_rejects_over_scope_read() {
        let adm = admission(false, false);
        assert_eq!(
            assert_within_ceiling(&adm, true, false),
            Err(CapabilityError::OverScope {
                capability: "readonly".into()
            })
        );
    }

    #[test]
    fn assert_within_ceiling_accepts_in_bounds() {
        let adm = admission(true, true);
        assert!(assert_within_ceiling(&adm, true, true).is_ok());
        assert!(assert_within_ceiling(&adm, false, false).is_ok());
    }
}
