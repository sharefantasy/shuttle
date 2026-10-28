//! cf_receipt — build & emit the `external_provider_receipt/v1` for a external-driven
//! `shuttle cell-run`.
//!
//! The receipt is the **only** legal external evidence shuttle produces for a external
//! attempt: exactly the five-field inner observation (`provider_status`,
//! `reconciliation_hint`, `result_ref`, `provider_run_id`, `terminal`). The external
//! consumer adapter owns any lift into its canonical nine-field receipt. Gate
//! detail / telemetry / inline artifacts NEVER go in the SHUTTLE inner receipt —
//! they land as a referenced artifact and only the `result_ref` points at them.
//!
//! ## Gate mapping
//!
//! shuttle's internal quality gate for a synchronous single-shot cell run is the
//! child's exit code plus a landed result artifact:
//!   * **all-green** (exit 0 AND a non-empty `result_ref` AND a non-empty
//!     `provider_run_id`) ⇒ `completed` + `terminal = true`.
//!   * **gate fail** (non-zero exit) ⇒ NOT completed ⇒ `failed` + `terminal =
//!     true` (the run finished, it just failed the gate) + a
//!     `reconciliation_hint` of `restart`.
//!   * **unknown** (we never obtained telemetry / a run id) ⇒ `unknown` +
//!     `terminal = false` + `reconciliation_hint = query_provider`.
//!
//! > 待与 upstream 正本核对: a production external deployment returns an
//! > `artifact://…` ref from its artifact store. This machine has no such
//! > store, so shuttle lands the gate evidence (the run telemetry) as a local file
//! > and emits a `file://<abs>` `result_ref`. When the upstream artifact API is
//! > materialized, swap the sink (see [`land_result_artifact`]) — the receipt
//! > shape and gate mapping do not change (§7「绝不瞎猜」).

use std::collections::BTreeMap;
use std::path::Path;

use shuttle_protocol::anchor::{
    AttemptCommit, AttemptStatus, CodingAttemptResult, CodingFlowContract, CodingFlowPath,
    CodingResultExport, CommitOid, ProviderReceipt, ProviderStatus, ProviderTerminalReceipt,
    ProviderTerminalStatus, ReconciliationHint, Sha256Digest,
};
use shuttle_protocol::CellBinding;

use crate::run::RunTelemetry;
use crate::sha256;

const SNAPSHOT_MAX_FILES: usize = 10_000;
const SNAPSHOT_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    files: BTreeMap<String, String>,
}

pub fn snapshot_workspace(root: &Path) -> anyhow::Result<WorkspaceSnapshot> {
    let root = std::fs::canonicalize(root)?;
    let ignore_patterns = read_ignore_patterns(&root)?;
    let mut files = BTreeMap::new();
    let mut pending = vec![root.clone()];
    let mut total_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        let mut entries = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/");
            if ignored_snapshot_path(&relative, &ignore_patterns) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            let bytes = if metadata.file_type().is_symlink() {
                std::fs::read_link(&path)?
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec()
            } else {
                std::fs::read(&path)?
            };
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            if files.len() >= SNAPSHOT_MAX_FILES || total_bytes > SNAPSHOT_MAX_BYTES {
                anyhow::bail!("workspace snapshot exceeds file/byte budget");
            }
            files.insert(relative, sha256::sha256_hex_prefixed(&bytes));
        }
    }
    Ok(WorkspaceSnapshot { files })
}

/// Land the gate evidence (run telemetry) as a referenced artifact and return
/// the `result_ref` that points at it. The receipt carries only this ref, never
/// the telemetry itself.
///
/// The artifact is written under `<workspace_root>/.shuttle/receipts/<run_id>.json`.
/// This is shuttle's own sidecar written by the shuttle process (not the caged child), so
/// it is outside the child's scope-enforced write boundary by construction.
pub fn land_result_artifact(
    workspace_root: &Path,
    run_id: &str,
    telemetry: &RunTelemetry,
    binding: Option<&CellBinding>,
) -> anyhow::Result<String> {
    let dir = workspace_root.join(".shuttle").join("receipts");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create receipt artifact dir {dir:?}: {e}"))?;
    // A filesystem-safe artifact name from the (opaque) run id.
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.telemetry.json"));
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "schema_version": "shuttle_cell_result_evidence/v1",
        "provider_run_id": run_id,
        "runtime_descriptor_ref": binding.and_then(|b| b.runtime_descriptor_ref.as_deref()),
        "runtime_descriptor_digest": binding.and_then(|b| b.runtime_descriptor_digest.as_ref()).map(ToString::to_string),
        "telemetry": telemetry,
    }))?;
    std::fs::write(&path, body)
        .map_err(|e| anyhow::anyhow!("failed to write result artifact {path:?}: {e}"))?;
    let abs = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(format!("file://{}", abs.display()))
}

/// Export a host-readable `coding_attempt_result/v1` from Git truth. The nested
/// provider receipt is a first-phase immutable snapshot with `result_ref=null`;
/// the final five-field receipt points to this artifact, avoiding self-reference.
pub fn land_coding_result_artifact(
    workspace_root: &Path,
    provider_run_id: &str,
    attempt_id: &str,
    binding: &CellBinding,
    export: &CodingResultExport,
    baseline: Option<&WorkspaceSnapshot>,
) -> anyhow::Result<String> {
    for (name, value) in [
        ("delivery_id", export.delivery_id.as_str()),
        ("workspace_ref", export.workspace_ref.as_str()),
        ("repo_id", export.repo_id.as_str()),
        ("change_id", export.change_id.as_str()),
        ("attempt_id", attempt_id),
        ("provider_run_id", provider_run_id),
    ] {
        if value.trim().is_empty() || value.len() > 256 {
            anyhow::bail!("coding result {name} must be non-empty and bounded");
        }
    }
    let root = std::fs::canonicalize(workspace_root)?;
    let head = git_worktree_head(&root)?;
    let mut changed_files = if let Some(head) = head.as_deref() {
        let mut files = git_changed_files(&root, &binding.base_oid, head)?;
        files.extend(git_dirty_files(&root)?);
        files
    } else {
        let before = baseline.ok_or_else(|| {
            anyhow::anyhow!(
                "coding result export requires Git truth or a pre-run workspace snapshot"
            )
        })?;
        snapshot_changed_files(before, &snapshot_workspace(&root)?)
    };
    changed_files.sort();
    changed_files.dedup();
    let scope_violations: Vec<String> = changed_files
        .iter()
        .filter(|path| !path_allowed(path, &binding.allowed_paths))
        .cloned()
        .collect();
    if !scope_violations.is_empty() {
        anyhow::bail!(
            "coding result export found write-scope violations: {}",
            scope_violations.join(",")
        );
    }
    let terminal_projection = serde_json::json!({
        "provider_status": "completed",
        "provider_run_id": provider_run_id,
        "terminal": true,
    });
    let projection_bytes = canonical_json(&terminal_projection)?;
    let artifacts = workspace_root.join(".shuttle").join("receipts");
    std::fs::create_dir_all(&artifacts)?;
    let safe = safe_run_id(provider_run_id);
    let provider_path = artifacts.join(format!("{safe}.provider-terminal.json"));
    atomic_write(&provider_path, &projection_bytes)?;
    let provider_digest = sha256::sha256_hex_prefixed(&projection_bytes);
    let artifact_digest = sha256::sha256_hex_prefixed(&canonical_json(&serde_json::json!({
        "base_oid": binding.base_oid,
        "head_oid": head,
        "changed_files": changed_files,
    }))?);
    // Build the frozen `coding_attempt_result/v1` through its typed anchor so
    // the emitted artifact is schema-exact by construction: `deny_unknown_fields`
    // forbids any stray key (no `runtime_descriptor_*` — those are shuttle-local, not
    // part of the frozen provider result) and the newtypes reject an absolute
    // path or a malformed commit oid before the artifact is ever written.
    let typed_changed_files = changed_files
        .iter()
        .map(|p| CodingFlowPath::parse(p.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("coding result changed_files: {e}"))?;
    let typed_generated_tests = export
        .generated_tests
        .iter()
        .map(|p| CodingFlowPath::parse(p.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("coding result generated_tests: {e}"))?;
    let commits = if head.as_deref().is_none_or(|oid| oid == binding.base_oid) {
        Vec::new()
    } else {
        let oid = head.as_deref().unwrap();
        vec![AttemptCommit {
            repo_id: export.repo_id.clone(),
            change_id: export.change_id.clone(),
            commit_oid: CommitOid::parse(oid.to_string())
                .map_err(|e| anyhow::anyhow!("coding result commit_oid: {e}"))?,
        }]
    };
    let result = CodingAttemptResult {
        schema_version: "coding_attempt_result/v1".to_string(),
        result_id: format!("shuttle-{safe}"),
        delivery_id: export.delivery_id.clone(),
        attempt_id: attempt_id.to_string(),
        provider_run_id: provider_run_id.to_string(),
        workspace_ref: export.workspace_ref.clone(),
        status: AttemptStatus::Completed,
        changed_files: typed_changed_files,
        commits,
        generated_tests: typed_generated_tests,
        unresolved_items: export.unresolved_items.clone(),
        // Reaching this point means no scope violation was found above (else we
        // bailed); completed ⟹ empty scope_violations is enforced by validate().
        scope_violations: Vec::new(),
        artifact_digest: Sha256Digest::parse(artifact_digest)
            .map_err(|e| anyhow::anyhow!("coding result artifact_digest: {e}"))?,
        provider_terminal_receipt: ProviderTerminalReceipt {
            receipt_ref: format!("file://{}", provider_path.display()),
            digest: Sha256Digest::parse(provider_digest)
                .map_err(|e| anyhow::anyhow!("coding result provider digest: {e}"))?,
            provider_status: ProviderTerminalStatus::Completed,
        },
    };
    // Fail-closed self-check against the frozen semantic rules before emit.
    result
        .validate()
        .map_err(|e| anyhow::anyhow!("coding_attempt_result/v1 self-validation failed: {e}"))?;
    let result_path = artifacts.join(format!("{safe}.coding-attempt-result.json"));
    atomic_write(&result_path, &serde_json::to_vec_pretty(&result)?)?;
    Ok(format!(
        "file://{}",
        std::fs::canonicalize(result_path)?.display()
    ))
}

fn canonical_json(value: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod runtime_evidence_tests {
    use super::*;
    use shuttle_protocol::anchor::{CodingResultExport, Sha256Digest};

    fn digest(ch: char) -> Sha256Digest {
        Sha256Digest::parse(format!("sha256:{}", ch.to_string().repeat(64))).unwrap()
    }

    fn binding(root: &Path) -> CellBinding {
        let descriptor = shuttle_protocol::EaRuntimeDescriptor {
            schema_version: shuttle_protocol::SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA.into(),
            mode: shuttle_protocol::RuntimeMode::HostBundle,
            core: shuttle_protocol::RuntimeCore {
                version: "1".into(),
                runtime_abi: "shuttle-runtime/v1".into(),
                executable_path: "/bin/shuttle".into(),
                digest: digest('a'),
            },
            tools: shuttle_protocol::RuntimeTools {
                source: "file:///tools".into(),
                distribution_digest: digest('b'),
                manifest_digest: digest('c'),
                root: "/opt/shuttle-tools".into(),
                profiles: vec!["source-analysis".into()],
            },
            provider: shuttle_protocol::ProviderRuntime {
                kind: "pi".into(),
                version: "1".into(),
                binary: "/bin/pi".into(),
                binary_digest: digest('f'),
                extension_path: "/opt/shuttle-provider/shuttle-tools.ts".into(),
                extension_digest: digest('d'),
            },
            composed_image_digest: None,
            mount_receipt: None,
        };
        let descriptor_digest = descriptor.digest().unwrap();
        let binding: CellBinding = serde_json::from_value(serde_json::json!({
            "version": 1, "workspace_root": root, "allowed_paths": ["src/**"],
            "allowed_commands": [], "base_oid": "oid", "assignment_digest": "sha256:assignment",
            "runtime_descriptor_ref": "artifact://runtime/one",
            "runtime_descriptor_digest": descriptor_digest, "runtime_descriptor": descriptor,
            "environment_id": "env"
        }))
        .unwrap();
        binding.validate().unwrap();
        binding
    }

    #[test]
    fn result_artifact_preserves_runtime_descriptor_identity() {
        let root = tempfile::tempdir().unwrap();
        let binding = binding(root.path());
        let telemetry = RunTelemetry::errored(7);
        let reference =
            land_result_artifact(root.path(), "run-1", &telemetry, Some(&binding)).unwrap();
        let path = reference.strip_prefix("file://").unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["runtime_descriptor_ref"], "artifact://runtime/one");
        assert_eq!(
            value["runtime_descriptor_digest"],
            binding.runtime_descriptor_digest.unwrap().to_string()
        );
        assert_eq!(value["telemetry"]["wall_ms"], 7);
        assert_eq!(value["telemetry"]["exit_code"], -1);
    }

    #[test]
    fn coding_result_artifact_is_schema_exact_and_omits_runtime_descriptor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let baseline = snapshot_workspace(root.path()).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let binding = binding(root.path());
        let export = CodingResultExport {
            delivery_id: "delivery-1".into(),
            workspace_ref: "workspace://one".into(),
            repo_id: "repo-1".into(),
            change_id: "change-1".into(),
            generated_tests: vec![],
            unresolved_items: vec![],
        };
        let reference = land_coding_result_artifact(
            root.path(),
            "run-1",
            "attempt-1",
            &binding,
            &export,
            Some(&baseline),
        )
        .unwrap();
        let path = reference.strip_prefix("file://").unwrap();
        let bytes = std::fs::read(path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["schema_version"], "coding_attempt_result/v1");
        assert_eq!(value["changed_files"], serde_json::json!(["src/lib.rs"]));
        assert_eq!(value["status"], "completed");
        assert_eq!(value["scope_violations"], serde_json::json!([]));
        assert_eq!(
            value["provider_terminal_receipt"]["provider_status"],
            "completed"
        );

        // The frozen `coding_attempt_result/v1` has additionalProperties:false —
        // the shuttle-local runtime-descriptor identity MUST NOT leak into it.
        let obj = value.as_object().unwrap();
        assert!(
            !obj.contains_key("runtime_descriptor_ref"),
            "frozen result must not carry shuttle-local runtime_descriptor_ref"
        );
        assert!(
            !obj.contains_key("runtime_descriptor_digest"),
            "frozen result must not carry shuttle-local runtime_descriptor_digest"
        );

        // The emitted artifact must round-trip through the frozen typed anchor
        // (deny_unknown_fields) and pass its semantic validate(), proving the
        // bytes on disk are schema-exact — not merely a superset.
        let typed: CodingAttemptResult = serde_json::from_slice(&bytes).unwrap();
        typed.validate().unwrap();
        assert_eq!(typed.attempt_id, "attempt-1");
        assert_eq!(typed.provider_run_id, "run-1");
    }
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
    let output = std::process::Command::new(git)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "Git truth query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_optional(root: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
    let output = std::process::Command::new(git)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if output.status.success() {
        return Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not a git repository") {
        return Ok(None);
    }
    anyhow::bail!("Git truth query failed: {}", stderr.trim())
}

fn git_worktree_head(root: &Path) -> anyhow::Result<Option<String>> {
    let Some(top_level) = git_optional(root, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    if std::fs::canonicalize(top_level)? != root {
        return Ok(None);
    }
    git_optional(root, &["rev-parse", "--verify", "HEAD^{commit}"])
}

fn read_ignore_patterns(root: &Path) -> anyhow::Result<Vec<String>> {
    let path = root.join(".gitignore");
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(str::to_string)
        .collect())
}

fn ignored_snapshot_path(path: &str, patterns: &[String]) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".shuttle"
        || path.starts_with(".shuttle/")
        || patterns.iter().any(|pattern| {
            let pattern = pattern.trim_start_matches('/');
            let base = pattern
                .strip_suffix("/**")
                .or_else(|| pattern.strip_suffix('/'))
                .unwrap_or(pattern);
            path == base
                || path.starts_with(&format!("{base}/"))
                || (!base.contains('/') && path.split('/').any(|part| part == base))
        })
}

fn snapshot_changed_files(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<String> {
    before
        .files
        .keys()
        .chain(after.files.keys())
        .filter(|path| before.files.get(*path) != after.files.get(*path))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn git_dirty_files(root: &Path) -> anyhow::Result<Vec<String>> {
    let tracked = git(root, &["diff", "--name-only", "HEAD", "--"])?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(tracked
        .lines()
        .chain(untracked.lines())
        .filter(|path| *path != ".shuttle" && !path.starts_with(".shuttle/"))
        .map(str::to_string)
        .collect())
}

fn git_changed_files(root: &Path, base: &str, head: &str) -> anyhow::Result<Vec<String>> {
    let text = git(root, &["diff", "--name-only", base, head, "--"])?;
    Ok(text
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn path_allowed(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|pattern| {
        pattern == "**"
            || pattern == path
            || pattern
                .strip_suffix("/**")
                .is_some_and(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
    })
}

fn safe_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the exact five-field SHUTTLE inner receipt from a completed synchronous run, applying the
/// ruling §2 gate mapping. `provider_run_id` is the external orchestrator idempotency key carried
/// verbatim from the launch bundle (never re-minted). `result_ref` is the
/// landed artifact ref (non-empty on a green gate).
pub fn build_receipt(
    telemetry: &RunTelemetry,
    provider_run_id: &str,
    result_ref: Option<String>,
) -> ProviderReceipt {
    let green = telemetry.exit_code == 0
        && result_ref
            .as_deref()
            .map(|r| !r.is_empty())
            .unwrap_or(false)
        && !provider_run_id.is_empty();

    let (provider_status, reconciliation_hint, terminal) = if green {
        // All-green: the internal gate passed and the result landed.
        (
            ProviderStatus::Completed,
            ReconciliationHint::QueryProvider,
            true,
        )
    } else {
        // Gate fail: the run finished but did not pass the gate. Terminal
        // failure — external should restart rather than reconnect to a live run.
        (ProviderStatus::Failed, ReconciliationHint::Restart, true)
    };

    ProviderReceipt {
        provider_status,
        reconciliation_hint,
        // On a failed gate the result did not land, so no ref is emitted.
        result_ref: if green { result_ref } else { None },
        provider_run_id: Some(provider_run_id.to_string()),
        terminal,
        extra: Default::default(),
    }
}

/// A pre-run refusal receipt (fail-closed): the attempt never reached a run —
/// e.g. the binding failed validation or the bundle was rejected. Non-terminal
/// `unknown` so external re-queries rather than treating it as a landed failure.
pub fn refusal_receipt(provider_run_id: &str) -> ProviderReceipt {
    ProviderReceipt {
        provider_status: ProviderStatus::Unknown,
        reconciliation_hint: ReconciliationHint::QueryProvider,
        result_ref: None,
        provider_run_id: if provider_run_id.is_empty() {
            None
        } else {
            Some(provider_run_id.to_string())
        },
        terminal: false,
        extra: Default::default(),
    }
}

pub fn cancelled_receipt(provider_run_id: &str) -> ProviderReceipt {
    ProviderReceipt {
        provider_status: ProviderStatus::Cancelled,
        reconciliation_hint: ReconciliationHint::Block,
        result_ref: None,
        provider_run_id: Some(provider_run_id.to_string()),
        terminal: true,
        extra: Default::default(),
    }
}

/// Emit a receipt to `dest` — a file path, or `-` / `None`-equivalent for
/// stdout. Always a single compact JSON line so a queue consumer can read it
/// straight off the stream.
pub fn emit_receipt(receipt: &ProviderReceipt, dest: Option<&str>) -> anyhow::Result<()> {
    let line = serde_json::to_string(receipt)?;
    match dest {
        Some(path) if path != "-" => {
            std::fs::write(path, format!("{line}\n"))
                .map_err(|e| anyhow::anyhow!("failed to write receipt {path}: {e}"))?;
        }
        _ => {
            // stdout, on its own line, distinct from the preflight (pretty) blob.
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("shuttle_cf_receipt_{tag}_{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn tel(exit: i32) -> RunTelemetry {
        RunTelemetry {
            termination: crate::run::RunTermination::ChildExit,
            limit_reason: None,
            wall_ms: 100,
            tokens: None,
            usage: Default::default(),
            provider_cost: None,
            provider: None,
            model: None,
            event_count: 0,
            stdout_bytes: 0,
            tool_calls: 3,
            accept_rate: Some(1.0),
            exit_code: exit,
            ttft_ms: Some(30),
            resources: None,
            apply_ok: 0,
            apply_total: 0,
        }
    }

    #[test]
    fn workspace_snapshot_tracks_bounded_changes_and_ignores_runtime_state() {
        let root = scratch_dir("snapshot");
        std::fs::write(root.join(".gitignore"), "state/\n").unwrap();
        std::fs::write(root.join("kept.txt"), "before\n").unwrap();
        std::fs::write(root.join("removed.txt"), "before\n").unwrap();
        let before = snapshot_workspace(&root).unwrap();

        std::fs::write(root.join("kept.txt"), "after\n").unwrap();
        std::fs::remove_file(root.join("removed.txt")).unwrap();
        std::fs::write(root.join("added.txt"), "after\n").unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("state/runtime.json"), "ignored\n").unwrap();
        std::fs::create_dir_all(root.join(".shuttle")).unwrap();
        std::fs::write(root.join(".shuttle/receipt.json"), "ignored\n").unwrap();
        let after = snapshot_workspace(&root).unwrap();

        assert_eq!(
            snapshot_changed_files(&before, &after),
            vec!["added.txt", "kept.txt", "removed.txt"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn green_gate_yields_completed_terminal() {
        let r = build_receipt(&tel(0), "idem-1", Some("file:///art".into()));
        assert_eq!(r.provider_status, ProviderStatus::Completed);
        assert!(r.terminal);
        assert_eq!(r.result_ref.as_deref(), Some("file:///art"));
        assert_eq!(r.provider_run_id.as_deref(), Some("idem-1"));
    }

    #[test]
    fn nonzero_exit_yields_failed_restart() {
        let r = build_receipt(&tel(1), "idem-1", Some("file:///art".into()));
        assert_eq!(r.provider_status, ProviderStatus::Failed);
        assert!(r.terminal);
        assert_eq!(r.reconciliation_hint, ReconciliationHint::Restart);
        // A failed gate lands no result.
        assert!(r.result_ref.is_none());
    }

    #[test]
    fn missing_result_ref_is_not_green() {
        let r = build_receipt(&tel(0), "idem-1", None);
        assert_eq!(r.provider_status, ProviderStatus::Failed);
    }

    #[test]
    fn empty_run_id_is_not_green() {
        let r = build_receipt(&tel(0), "", Some("file:///art".into()));
        assert_eq!(r.provider_status, ProviderStatus::Failed);
    }

    #[test]
    fn refusal_is_nonterminal_unknown() {
        let r = refusal_receipt("idem-1");
        assert_eq!(r.provider_status, ProviderStatus::Unknown);
        assert!(!r.terminal);
        assert_eq!(r.reconciliation_hint, ReconciliationHint::QueryProvider);
    }

    #[test]
    fn receipt_serializes_to_exact_five_field_ea_contract() {
        let r = build_receipt(&tel(0), "idem-1", Some("file:///art".into()));
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        let obj = v.as_object().unwrap();
        // SHUTTLE emits this exact five-field inner receipt. The external-side adapter
        // lifts it into the canonical nine-field provider receipt without
        // putting runtime identity or telemetry inline.
        let keys: std::collections::BTreeSet<_> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "provider_run_id",
                "provider_status",
                "reconciliation_hint",
                "result_ref",
                "terminal",
            ]
            .into_iter()
            .collect()
        );
    }
}
