//! upstream — the `UpstreamOrchestrator` seam that decouples
//! `shuttle cell-run` from any external orchestrator.
//!
//! `shuttle cell-run` always does the same three things: (1) lower a raw input
//! envelope into a fail-closed [`CellBinding`], (2) run the identical caged pi
//! loop confined to that binding's write boundary, and (3) report the outcome
//! back to whoever launched it. Only steps (1) and (3) differ by launcher:
//!
//!   * **direct agent launch** ([`LocalUpstream`]) — a hand-written
//!     `cell_binding/v1` file. No external contracts, no provider receipt;
//!     the run's telemetry (one JSON line on stderr) is the only observation.
//!     This is the zero-upstream path an OSS build ships with.
//!   * **orchestrated launch** ([`ExternalUpstream`]) — a launch bundle that carries
//!     the protocol anchors, lowered via [`ExternalLaunchBundle::into_binding`],
//!     with a provider receipt emitted for every terminal / refusal state.
//!
//! The caged run *between* (1) and (3) never names any upstream:
//! [`crate::cell_run`] drives it through this trait, so building `shuttle`
//! without any upstream plumbing is a matter of only ever constructing a
//! [`LocalUpstream`]. The upstream-specific types stay confined to
//! [`ExternalUpstream`] and [`crate::provider_receipt`].

use shuttle_protocol::anchor::{CodingResultExport, ExternalLaunchBundle};
use shuttle_protocol::{CellBinding, ScopedPathResolver};

use crate::provider_receipt;
use crate::run::RunTelemetry;

/// The launcher-specific half of `shuttle cell-run`: how a raw envelope lowers to a
/// binding, and what observation the launcher expects back. Everything in
/// between (resolver construction, preflight, the caged pi loop) is
/// upstream-agnostic and lives in [`crate::cell_run`].
///
/// Fail-closed contract: an orchestrated implementation MUST emit a legal
/// observation (a refusal / non-terminal receipt) on every early-exit path so
/// the upstream never sees a silent crash; a direct implementation has no
/// observation to emit and returns `Ok(())` for those hooks.
pub trait UpstreamOrchestrator {
    /// Lower the raw input envelope into a validated, fail-closed
    /// [`CellBinding`]. On failure the implementation may emit a refusal
    /// observation (orchestrated) before the error propagates, or simply
    /// surface the error (direct). A successful ingest may also stash
    /// launcher-local state (e.g. a provider run id and the bundle's own
    /// prompt) for later hooks.
    fn ingest(&mut self, raw: &str) -> anyhow::Result<CellBinding>;

    /// Stable provider run identity used by the shuttle-local durable registry.
    fn provider_run_id(&self, binding: &CellBinding) -> String;

    /// Upstream attempt coordinate, when one exists.
    fn attempt_id(&self, binding: &CellBinding) -> Option<String>;

    /// Emit a previously persisted receipt through this upstream's configured
    /// destination without repeating the launch effect.
    fn emit_existing(
        &self,
        receipt: &shuttle_protocol::anchor::ProviderReceipt,
    ) -> anyhow::Result<()>;

    /// Resolve the effective prompt from the CLI `--prompt` (`external`). An
    /// orchestrated implementation falls back to the bundle's carried prompt
    /// when no external prompt is supplied (queue injection beats the bundle);
    /// the
    /// direct implementation uses only the external one. The caller applies the
    /// non-empty filter, so returning `Some("  ")` is still treated as "no
    /// prompt".
    fn resolve_prompt(&self, external: Option<String>) -> Option<String>;

    /// Hook for a setup failure that occurs AFTER a successful [`Self::ingest`]
    /// — e.g. the mandatory resolver could not be constructed. external emits a
    /// refusal receipt (so the upstream re-queries rather than treating a live
    /// run as landed); the direct path is a no-op.
    fn note_failure(&mut self);

    /// The run was preflight-only: the binding validated and the boundary was
    /// described, but no prompt was supplied so nothing landed. external emits a
    /// non-terminal `unknown` receipt (`query_provider`); the direct path is a
    /// no-op.
    fn on_preflight_only(
        &mut self,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>>;

    /// Report a settled caged run. external lands the gate evidence (telemetry) as a
    /// referenced artifact under `<root>/.shuttle/receipts/` and emits the 9-field
    /// `external_provider_receipt/v1` (ruling §2 gate mapping); the direct path
    /// is a no-op because the telemetry line the caller already printed is the
    /// only observation it owes.
    fn on_settled(
        &mut self,
        resolver: &ScopedPathResolver,
        tel: &RunTelemetry,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>>;
}

/// Direct-agent-launch upstream: a hand-written `cell_binding/v1`. Carries no
/// external state and emits no provider receipt — the run telemetry is the
/// sole observation. This is the upstream an OSS / zero-upstream build uses.
#[derive(Debug, Default)]
pub struct LocalUpstream;

impl UpstreamOrchestrator for LocalUpstream {
    fn ingest(&mut self, raw: &str) -> anyhow::Result<CellBinding> {
        CellBinding::parse(raw).map_err(|e| anyhow::anyhow!("cell_binding rejected: {e}"))
    }

    fn provider_run_id(&self, binding: &CellBinding) -> String {
        binding
            .provider_run_id
            .clone()
            .unwrap_or_else(|| binding.environment_id.clone())
    }

    fn attempt_id(&self, binding: &CellBinding) -> Option<String> {
        binding.attempt_id.clone()
    }

    fn emit_existing(
        &self,
        _receipt: &shuttle_protocol::anchor::ProviderReceipt,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn resolve_prompt(&self, external: Option<String>) -> Option<String> {
        external
    }

    fn note_failure(&mut self) {}

    fn on_preflight_only(
        &mut self,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>> {
        Ok(None)
    }

    fn on_settled(
        &mut self,
        _resolver: &ScopedPathResolver,
        _tel: &RunTelemetry,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>> {
        Ok(None)
    }
}

/// Orchestrated upstream: a launch bundle lowered to a shuttle-local binding,
/// with a provider receipt emitted for every terminal / refusal state. All
/// upstream knowledge — the launch-bundle transport and the receipt shape — is
/// confined here and in [`crate::provider_receipt`], so the caged run stays
/// upstream-free.
#[derive(Debug)]
pub struct ExternalUpstream {
    /// Where the receipt is written: a file path, or `-`/`None` for stdout.
    receipt_out: Option<String>,
    /// The idempotency key carried verbatim from the upstream bundle.
    /// Populated by [`Self::ingest`]; empty until then (an ingest that fails
    /// before parse emits a refusal with an empty run id, matching the
    /// upstream's "unknown attempt" semantics).
    provider_run_id: String,
    /// The bundle's own prompt, used as the fallback when the CLI supplies no
    /// `--prompt` override. Populated by [`Self::ingest`].
    bundle_prompt: Option<String>,
    attempt_id: Option<String>,
    coding_result_export: Option<CodingResultExport>,
    binding: Option<CellBinding>,
    workspace_snapshot: Option<provider_receipt::WorkspaceSnapshot>,
}

impl ExternalUpstream {
    /// Construct an external upstream that writes its receipt to `receipt_out`
    /// (`None`/`-` = stdout).
    pub fn new(receipt_out: Option<String>) -> Self {
        Self {
            receipt_out,
            provider_run_id: String::new(),
            bundle_prompt: None,
            attempt_id: None,
            coding_result_export: None,
            binding: None,
            workspace_snapshot: None,
        }
    }

    /// Emit a fail-closed refusal receipt for the current `provider_run_id`.
    fn emit_refusal(&self) {
        let _ = provider_receipt::emit_receipt(
            &provider_receipt::refusal_receipt(&self.provider_run_id),
            self.receipt_out.as_deref(),
        );
    }
}

impl UpstreamOrchestrator for ExternalUpstream {
    fn ingest(&mut self, raw: &str) -> anyhow::Result<CellBinding> {
        // Parse the transport. A rejected bundle still hands external a refusal
        // (best-effort: run id unknown) before the error propagates.
        let bundle = match ExternalLaunchBundle::parse(raw) {
            Ok(b) => b,
            Err(e) => {
                // provider_run_id is still empty here — external sees an "unknown
                // attempt" refusal, which is the correct fail-closed signal.
                self.emit_refusal();
                return Err(anyhow::anyhow!("external_launch_bundle rejected: {e}"));
            }
        };
        // The idempotency key is the receipt's provider_run_id (carried
        // verbatim, never re-minted). Stash it + the bundle prompt before any
        // further step can fail, so later refusals carry the right id.
        self.provider_run_id = bundle.provider_run_id().to_string();
        self.attempt_id = Some(bundle.launch_intent.attempt_id.clone());
        self.bundle_prompt = bundle.prompt.clone();
        self.coding_result_export = bundle.coding_result_export.clone();

        // Lower the external orchestrator contracts into a shuttle-local, validated binding.
        match bundle.into_binding() {
            Ok(b) => {
                if self.coding_result_export.is_some() {
                    self.workspace_snapshot = Some(provider_receipt::snapshot_workspace(
                        std::path::Path::new(&b.workspace_root),
                    )?);
                }
                self.binding = Some(b.clone());
                Ok(b)
            }
            Err(e) => {
                self.emit_refusal();
                Err(anyhow::anyhow!(
                    "external_launch_bundle lowering rejected: {e}"
                ))
            }
        }
    }

    fn provider_run_id(&self, _binding: &CellBinding) -> String {
        self.provider_run_id.clone()
    }

    fn attempt_id(&self, _binding: &CellBinding) -> Option<String> {
        self.attempt_id.clone()
    }

    fn emit_existing(
        &self,
        receipt: &shuttle_protocol::anchor::ProviderReceipt,
    ) -> anyhow::Result<()> {
        provider_receipt::emit_receipt(receipt, self.receipt_out.as_deref())
    }

    fn resolve_prompt(&self, external: Option<String>) -> Option<String> {
        // A --prompt override beats the bundle's own prompt (queue injection);
        // absent an override, fall back to the bundle prompt.
        external.or_else(|| self.bundle_prompt.clone())
    }

    fn note_failure(&mut self) {
        self.emit_refusal();
    }

    fn on_preflight_only(
        &mut self,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>> {
        // Nothing landed: a non-terminal unknown telling external to re-query once a
        // prompt is committed.
        let receipt = provider_receipt::refusal_receipt(&self.provider_run_id);
        provider_receipt::emit_receipt(&receipt, self.receipt_out.as_deref())?;
        Ok(Some(receipt))
    }

    fn on_settled(
        &mut self,
        resolver: &ScopedPathResolver,
        tel: &RunTelemetry,
    ) -> anyhow::Result<Option<shuttle_protocol::anchor::ProviderReceipt>> {
        // Land the gate evidence and build the receipt. The caller first commits
        // it through the durable registry's active-state CAS, then emits it; this
        // prevents a concurrent cancellation from being overwritten on stdout.
        let result_ref = match (&self.coding_result_export, &self.binding, &self.attempt_id) {
            (Some(export), Some(binding), Some(attempt_id)) if tel.exit_code == 0 => {
                match provider_receipt::land_coding_result_artifact(
                    resolver.root(),
                    &self.provider_run_id,
                    attempt_id,
                    binding,
                    export,
                    self.workspace_snapshot.as_ref(),
                ) {
                    Ok(result_ref) => Some(result_ref),
                    Err(error) => {
                        eprintln!("coding_result_export_failed: {error:#}");
                        None
                    }
                }
            }
            _ => provider_receipt::land_result_artifact(
                resolver.root(),
                &self.provider_run_id,
                tel,
                self.binding.as_ref(),
            )
            .ok(),
        };
        let receipt = provider_receipt::build_receipt(tel, &self.provider_run_id, result_ref);
        Ok(Some(receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::anchor::{ProviderStatus, ReconciliationHint};

    /// A canonicalizable, unique scratch dir (never the literal blocked temp
    /// paths). Serves both as a resolver root and as a receipt sink.
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "shuttle_upstream_{tag}_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tel(exit: i32) -> RunTelemetry {
        RunTelemetry {
            termination: crate::run::RunTermination::ChildExit,
            limit_reason: None,
            wall_ms: 42,
            tokens: None,
            usage: Default::default(),
            provider_cost: None,
            provider: None,
            model: None,
            event_count: 0,
            stdout_bytes: 0,
            tool_calls: 1,
            accept_rate: Some(1.0),
            exit_code: exit,
            ttft_ms: Some(7),
            resources: None,
            apply_ok: 0,
            apply_total: 0,
        }
    }

    fn read_receipt(path: &std::path::Path) -> serde_json::Value {
        let body = std::fs::read_to_string(path).expect("receipt file must exist");
        serde_json::from_str(body.trim()).expect("receipt is a single JSON line")
    }

    // ---- shared fixtures -----------------------------------------------------

    const LOCAL_BINDING: &str = r#"{
        "version": 1,
        "workspace_root": "/abs/cell/ws",
        "allowed_paths": ["src/**"],
        "allowed_commands": [{"program": "git", "argv_prefix": ["status"]}],
        "base_oid": "abc123",
        "assignment_digest": "sha256:deadbeef",
        "environment_id": "env-1"
    }"#;

    /// A known-valid `external_launch_bundle/v1` (the sub-contracts mirror the shapes
    /// the shuttle-protocol ingest tests prove deserialize). `workspace_root` is
    /// absolute but need not exist — `into_binding` validates absoluteness, not
    /// filesystem presence (canonicalization happens later in `prepare_cell`).
    fn valid_bundle_json(prompt: Option<&str>) -> String {
        let digest = format!("sha256:{}", "a".repeat(64));
        let prompt_field = match prompt {
            Some(p) => format!(",\n            \"prompt\": \"{p}\""),
            None => String::new(),
        };
        format!(
            r#"{{
            "version": 1,
            "workspace_root": "/abs/ws",
            "environment_id": "env-1",
            "launch_intent": {{
                "launch_intent_id": "li-1",
                "attempt_id": "att-1",
                "runtime_id": "rt-1",
                "package_id": "pkg-1",
                "idempotency_key": "idem-xyz",
                "agent_type": "shuttle",
                "execution_profile_ref": "profile://shuttle/default",
                "context_manifest_ref": "cm-1",
                "resource_claim_refs": ["rc-1"],
                "deadline": "2026-01-01T00:00:00Z",
                "requested_by": "dispatch"
            }},
            "context_manifest": {{
                "context_manifest_id": "cm-1",
                "context_manifest_digest": "{digest}",
                "tool_allowlist": ["git", "cargo"],
                "source_versions": [{{"digest": "oid-abc"}}]
            }}{prompt_field}
        }}"#
        )
    }

    // ---- LocalUpstream -------------------------------------------------------

    #[test]
    fn local_ingest_parses_valid_binding() {
        let mut orch = LocalUpstream;
        let b = orch
            .ingest(LOCAL_BINDING)
            .expect("valid binding must parse");
        assert_eq!(b.workspace_root, "/abs/cell/ws");
        assert_eq!(b.allowed_paths, vec!["src/**".to_string()]);
    }

    #[test]
    fn local_ingest_rejects_garbage() {
        let mut orch = LocalUpstream;
        assert!(orch.ingest("not json").is_err());
        // Wrong version is fail-closed too.
        assert!(orch
            .ingest(r#"{"version": 999, "workspace_root": "/abs/ws"}"#)
            .is_err());
    }

    #[test]
    fn local_resolve_prompt_is_external_verbatim() {
        let orch = LocalUpstream;
        assert_eq!(
            orch.resolve_prompt(Some("do it".into())),
            Some("do it".into())
        );
        assert_eq!(orch.resolve_prompt(None), None);
    }

    #[test]
    fn local_hooks_emit_no_receipt() {
        // A direct launch never writes a receipt file, even on settle/preflight.
        let dir = unique_dir("local_noreceipt");
        let receipt = dir.join("should_not_exist.json");
        let mut orch = LocalUpstream;
        orch.on_preflight_only().unwrap();
        orch.note_failure();

        let resolver = ScopedPathResolver::new(&dir, vec!["**".to_string()])
            .expect("scratch dir must canonicalize");
        assert!(orch.on_settled(&resolver, &tel(0)).unwrap().is_none());

        assert!(
            !receipt.exists(),
            "LocalUpstream must never emit a provider receipt"
        );
    }

    // ---- ExternalUpstream: ingest --------------------------------------------------

    #[test]
    fn cf_ingest_lowers_bundle_and_stashes_run_id_and_prompt() {
        let mut orch = ExternalUpstream::new(None);
        let b = orch
            .ingest(&valid_bundle_json(Some("bundle prompt")))
            .expect("valid bundle must lower");
        assert_eq!(b.workspace_root, "/abs/ws");
        assert_eq!(orch.provider_run_id, "idem-xyz");
        assert_eq!(orch.bundle_prompt.as_deref(), Some("bundle prompt"));
    }

    #[test]
    fn cf_ingest_bad_bundle_emits_unknown_run_id_refusal() {
        let dir = unique_dir("cf_badbundle");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));

        let err = orch.ingest("not a bundle").unwrap_err();
        assert!(err.to_string().contains("external_launch_bundle rejected"));

        let v = read_receipt(&receipt);
        // Fail-closed: non-terminal unknown, and the run id is null (unknown
        // attempt) because parse failed before we could read the idempotency key.
        assert_eq!(v["provider_status"], "unknown");
        assert_eq!(v["terminal"], false);
        assert!(v["provider_run_id"].is_null());
    }

    #[test]
    fn cf_ingest_lowering_failure_emits_refusal_with_run_id() {
        // A relative workspace_root passes parse but fails into_binding's
        // absoluteness gate — the refusal must now carry the real run id.
        let dir = unique_dir("cf_lowerfail");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));

        let raw = valid_bundle_json(None).replace("/abs/ws", "relative/ws");
        let err = orch.ingest(&raw).unwrap_err();
        assert!(err.to_string().contains("lowering rejected"));

        let v = read_receipt(&receipt);
        assert_eq!(v["provider_status"], "unknown");
        assert_eq!(v["terminal"], false);
        assert_eq!(v["provider_run_id"], "idem-xyz");
    }

    // ---- ExternalUpstream: prompt resolution --------------------------------------

    #[test]
    fn cf_resolve_prompt_override_beats_bundle_then_falls_back() {
        let mut orch = ExternalUpstream::new(None);
        orch.ingest(&valid_bundle_json(Some("bundle prompt")))
            .unwrap();
        // External override wins.
        assert_eq!(
            orch.resolve_prompt(Some("queue injected".into())),
            Some("queue injected".into())
        );
        // No override => the bundle's own prompt.
        assert_eq!(orch.resolve_prompt(None), Some("bundle prompt".into()));
    }

    #[test]
    fn cf_resolve_prompt_none_when_neither_present() {
        let mut orch = ExternalUpstream::new(None);
        orch.ingest(&valid_bundle_json(None)).unwrap();
        assert_eq!(orch.resolve_prompt(None), None);
    }

    // ---- ExternalUpstream: observation hooks --------------------------------------

    #[test]
    fn cf_preflight_only_emits_nonterminal_unknown() {
        let dir = unique_dir("cf_preflight");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));
        orch.ingest(&valid_bundle_json(None)).unwrap();

        orch.on_preflight_only().unwrap();

        let v = read_receipt(&receipt);
        assert_eq!(v["provider_status"], "unknown");
        assert_eq!(v["terminal"], false);
        assert_eq!(v["reconciliation_hint"], "query_provider");
        assert_eq!(v["provider_run_id"], "idem-xyz");
    }

    #[test]
    fn cf_note_failure_emits_refusal() {
        let dir = unique_dir("cf_notefail");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));
        orch.ingest(&valid_bundle_json(None)).unwrap();

        orch.note_failure();

        let v = read_receipt(&receipt);
        assert_eq!(v["provider_status"], "unknown");
        assert_eq!(v["provider_run_id"], "idem-xyz");
    }

    #[test]
    fn cf_settled_green_lands_artifact_and_completes() {
        let dir = unique_dir("cf_green");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));
        orch.ingest(&valid_bundle_json(Some("go"))).unwrap();

        // The settled root is a real dir so the artifact can land under it.
        let resolver = ScopedPathResolver::new(&dir, vec!["**".to_string()])
            .expect("scratch dir must canonicalize");
        let built = orch.on_settled(&resolver, &tel(0)).unwrap().unwrap();
        orch.emit_existing(&built).unwrap();

        let v = read_receipt(&receipt);
        // Green gate: completed + terminal, with a landed result_ref.
        assert_eq!(v["provider_status"], "completed");
        assert_eq!(v["terminal"], true);
        assert_eq!(v["provider_run_id"], "idem-xyz");
        assert!(v["result_ref"].as_str().unwrap().starts_with("file://"));

        // The gate evidence (telemetry) landed as a referenced artifact and
        // NEVER leaked into the 9-field receipt.
        let artifact = dir.join(".shuttle/receipts/idem-xyz.telemetry.json");
        assert!(artifact.exists(), "telemetry artifact must land");
        assert!(
            v.get("wall_ms").is_none(),
            "telemetry must not leak into the receipt"
        );
    }

    #[test]
    fn cf_settled_nonzero_exit_fails_terminal_restart() {
        let dir = unique_dir("cf_fail");
        let receipt = dir.join("receipt.json");
        let mut orch = ExternalUpstream::new(Some(receipt.display().to_string()));
        orch.ingest(&valid_bundle_json(Some("go"))).unwrap();

        let resolver = ScopedPathResolver::new(&dir, vec!["**".to_string()])
            .expect("scratch dir must canonicalize");
        let built = orch.on_settled(&resolver, &tel(1)).unwrap().unwrap();
        orch.emit_existing(&built).unwrap();

        let v = read_receipt(&receipt);
        // Gate fail: the run finished but did not pass — terminal failure with a
        // restart hint and no landed result.
        assert_eq!(v["provider_status"], "failed");
        assert_eq!(v["terminal"], true);
        assert_eq!(v["reconciliation_hint"], "restart");
        assert!(v["result_ref"].is_null());
    }

    #[test]
    fn provider_status_enum_names_match_receipt_wire() {
        // Guard: the wire strings this module's tests assert on are exactly the
        // ProviderStatus / ReconciliationHint serde forms (so a rename upstream
        // trips a test rather than silently drifting).
        assert_eq!(
            serde_json::to_value(ProviderStatus::Completed).unwrap(),
            serde_json::json!("completed")
        );
        assert_eq!(
            serde_json::to_value(ProviderStatus::Failed).unwrap(),
            serde_json::json!("failed")
        );
        assert_eq!(
            serde_json::to_value(ProviderStatus::Unknown).unwrap(),
            serde_json::json!("unknown")
        );
        assert_eq!(
            serde_json::to_value(ReconciliationHint::Restart).unwrap(),
            serde_json::json!("restart")
        );
        assert_eq!(
            serde_json::to_value(ReconciliationHint::QueryProvider).unwrap(),
            serde_json::json!("query_provider")
        );
    }
}
