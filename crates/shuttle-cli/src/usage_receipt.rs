//! Immutable provider-usage evidence for one durable cell attempt.
//!
//! This sidecar is deliberately separate from the frozen external provider receipt.
//! It records observed usage for every terminal outcome without granting land,
//! retry, budget, or quality authority. Missing observations stay `null`.

use serde::Serialize;
use std::path::Path;

use crate::cell_registry::{CellRunRecord, CellState};
use crate::resource::RunResources;
use crate::run::{ProviderCostObservation, RunTelemetry, RunTermination, UsageBreakdown};

pub const USAGE_RECEIPT_SCHEMA: &str = "shuttle_usage_receipt/v1";

#[derive(Debug, Clone, Serialize)]
pub struct UsageTerminal {
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageTiming {
    pub wall_ms: Option<u128>,
    pub ttft_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageWork {
    pub tool_calls: Option<u32>,
    pub apply_total: Option<u32>,
    pub apply_ok: Option<u32>,
    pub accept_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageCompleteness {
    pub status: &'static str,
    pub missing_fields: Vec<String>,
    pub reason_codes: Vec<String>,
    pub consistency_checks: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageProvenance {
    pub source_kind: &'static str,
    pub event_count: u64,
    pub evidence_ref: Option<String>,
    pub evidence_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EaUsageReceipt {
    pub schema_version: &'static str,
    pub receipt_id: String,
    pub run_id: String,
    pub provider_run_id: String,
    pub attempt_id: Option<String>,
    pub cohort_id: Option<String>,
    pub parent_attempt_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub terminal: UsageTerminal,
    pub observed_at_ms: u128,
    pub usage: UsageBreakdown,
    pub provider_cost_observation: Option<ProviderCostObservation>,
    pub timing: UsageTiming,
    pub work: UsageWork,
    /// Best-effort host/process resource peaks sampled during the run.
    /// `null` when the supervisor gathered nothing (honest absence, §7):
    /// this receipt never fabricates a resource figure the sampler could
    /// not observe. Advisory only — carries no land/retry/budget authority.
    pub resources: Option<RunResources>,
    pub completeness: UsageCompleteness,
    pub provenance: UsageProvenance,
}

impl EaUsageReceipt {
    pub fn from_terminal(record: &CellRunRecord, telemetry: Option<&RunTelemetry>) -> Self {
        let terminal = terminal(record, telemetry);
        let usage = telemetry
            .map(|value| value.usage.clone())
            .unwrap_or_default();
        let mut missing_fields = Vec::new();
        for (name, value) in [
            ("usage.input_tokens", usage.input_tokens),
            ("usage.output_tokens", usage.output_tokens),
            ("usage.cache_read_tokens", usage.cache_read_tokens),
            ("usage.cache_write_tokens", usage.cache_write_tokens),
            ("usage.total_tokens", usage.total_tokens),
        ] {
            if value.is_none() {
                missing_fields.push(name.to_string());
            }
        }
        let mut reason_codes = Vec::new();
        if telemetry.is_none() {
            reason_codes.push("terminal_without_telemetry".to_string());
        }
        if !missing_fields.is_empty() {
            reason_codes.push("provider_usage_fields_missing".to_string());
        }
        let resources = telemetry.and_then(|value| value.resources.clone());
        if telemetry.is_some() && resources.is_none() {
            // Telemetry existed but the sampler never landed a reading
            // (probes degraded to None, or sampling was disabled). Record
            // the honest gap without downgrading token-usage completeness.
            reason_codes.push("resource_telemetry_unavailable".to_string());
        }
        let status = if telemetry.is_none() || missing_fields.len() == 5 {
            "unavailable"
        } else if missing_fields.is_empty() {
            "complete"
        } else {
            "partial"
        };
        let consistency_checks = token_consistency_checks(&usage);
        Self {
            schema_version: USAGE_RECEIPT_SCHEMA,
            receipt_id: format!("shuttle-usage:{}", record.provider_run_id),
            run_id: record.provider_run_id.clone(),
            provider_run_id: record.provider_run_id.clone(),
            attempt_id: record.attempt_id.clone(),
            cohort_id: record.cohort_id.clone(),
            parent_attempt_id: record.parent_attempt_id.clone(),
            provider: telemetry.and_then(|value| value.provider.clone()),
            model: telemetry.and_then(|value| value.model.clone()),
            terminal,
            observed_at_ms: record.updated_at_ms,
            usage,
            provider_cost_observation: telemetry.and_then(|value| value.provider_cost.clone()),
            timing: UsageTiming {
                wall_ms: telemetry.map(|value| value.wall_ms),
                ttft_ms: telemetry.and_then(|value| value.ttft_ms),
            },
            work: UsageWork {
                tool_calls: telemetry.map(|value| value.tool_calls),
                apply_total: telemetry.map(|value| value.apply_total),
                apply_ok: telemetry.map(|value| value.apply_ok),
                accept_rate: telemetry.and_then(|value| value.accept_rate),
            },
            resources,
            completeness: UsageCompleteness {
                status,
                missing_fields,
                reason_codes,
                consistency_checks,
            },
            provenance: UsageProvenance {
                source_kind: if telemetry.is_some() {
                    "pi_json_events"
                } else {
                    "recovery_journal"
                },
                event_count: telemetry.map(|value| value.event_count).unwrap_or(0),
                evidence_ref: None,
                evidence_sha256: None,
            },
        }
    }

    pub fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut body = serde_json::to_vec_pretty(self)?;
        body.push(b'\n');
        Ok(body)
    }
}

fn terminal(record: &CellRunRecord, telemetry: Option<&RunTelemetry>) -> UsageTerminal {
    let exit_code = telemetry.map(|value| value.exit_code);
    let status = match record.state {
        CellState::Done => "completed",
        CellState::Cancelled => "cancelled",
        CellState::Failed
            if telemetry.is_some_and(|value| {
                matches!(
                    value.termination,
                    RunTermination::SoftTimeout | RunTermination::StallTimeout
                )
            }) =>
        {
            "timed_out"
        }
        CellState::Failed => "failed",
        _ => "unknown",
    };
    UsageTerminal {
        status,
        exit_code,
        reason_code: telemetry
            .map(|value| {
                value
                    .limit_reason
                    .clone()
                    .unwrap_or_else(|| value.termination.reason_code().to_string())
            })
            .or_else(|| record.error_class.clone()),
    }
}

fn token_consistency_checks(usage: &UsageBreakdown) -> Vec<serde_json::Value> {
    let mut checks = Vec::new();
    if let (Some(input), Some(output), Some(cache_read), Some(cache_write), Some(total)) = (
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.total_tokens,
    ) {
        let component_sum = input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_write);
        checks.push(serde_json::json!({
            "check": "component_sum_equals_total",
            "status": if component_sum == total { "passed" } else { "observed_mismatch" },
            "component_sum": component_sum,
            "reported_total": total
        }));
    }
    checks
}

pub fn write_immutable(path: &Path, receipt: &EaUsageReceipt) -> anyhow::Result<String> {
    let body = receipt.canonical_bytes()?;
    if path.is_file() {
        let existing = std::fs::read(path)?;
        if existing != body {
            anyhow::bail!("usage receipt conflicts with immutable existing artifact");
        }
        return Ok(crate::sha256::sha256_hex_prefixed(&body));
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("usage receipt path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    match std::fs::hard_link(&tmp, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(path)?;
            let _ = std::fs::remove_file(&tmp);
            if existing != body {
                anyhow::bail!("usage receipt conflicts with immutable existing artifact");
            }
            return Ok(crate::sha256::sha256_hex_prefixed(&body));
        }
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
    }
    std::fs::remove_file(&tmp)?;
    Ok(crate::sha256::sha256_hex_prefixed(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: CellState) -> CellRunRecord {
        CellRunRecord {
            schema_version: crate::cell_registry::RUN_RECORD_SCHEMA.into(),
            provider_run_id: "run-1".into(),
            attempt_id: Some("attempt-1".into()),
            cohort_id: Some("cohort-1".into()),
            parent_attempt_id: None,
            state,
            revision: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            binding_ref: "binding".into(),
            binding_digest: "sha256:binding".into(),
            assignment_digest: "sha256:assignment".into(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            workspace_root: "/workspace".into(),
            environment_id: "env".into(),
            base_oid: "oid".into(),
            session_ref: "session".into(),
            pid: None,
            pgid: None,
            result_ref: None,
            receipt: None,
            error_class: None,
            usage_ref: None,
            usage_digest: None,
        }
    }

    #[test]
    fn unavailable_terminal_never_forges_zero_usage() {
        let receipt = EaUsageReceipt::from_terminal(&record(CellState::Cancelled), None);
        assert_eq!(receipt.terminal.status, "cancelled");
        assert_eq!(receipt.completeness.status, "unavailable");
        assert_eq!(receipt.usage.total_tokens, None);
        assert_eq!(receipt.work.tool_calls, None);
    }

    #[test]
    fn immutable_write_is_idempotent_and_rejects_drift() {
        let root = std::env::temp_dir().join(format!(
            "shuttle-usage-receipt-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("receipt.json");
        let receipt = EaUsageReceipt::from_terminal(&record(CellState::Cancelled), None);
        let first = write_immutable(&path, &receipt).unwrap();
        let second = write_immutable(&path, &receipt).unwrap();
        assert_eq!(first, second);
        let mut changed = record(CellState::Failed);
        changed.error_class = Some("different".into());
        let changed = EaUsageReceipt::from_terminal(&changed, None);
        assert!(write_immutable(&path, &changed).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_telemetry_flows_into_receipt() {
        use crate::resource::{HostMemLevel, RunResources};
        let mut tel = RunTelemetry::errored(42);
        tel.resources = Some(RunResources {
            peak_managed_rss_bytes: Some(123 * 1024),
            peak_host_mem_used_frac: Some(0.5),
            peak_host_mem_level: Some(HostMemLevel::Normal),
            peak_host_swap_used_bytes: Some(2 * 1024 * 1024),
            samples: 3,
            admission_control: None,
        });
        let receipt = EaUsageReceipt::from_terminal(&record(CellState::Done), Some(&tel));
        let res = receipt.resources.expect("resources surfaced");
        assert_eq!(res.peak_managed_rss_bytes, Some(123 * 1024));
        assert_eq!(res.samples, 3);
        // Present resources must not raise the unavailability reason.
        assert!(!receipt
            .completeness
            .reason_codes
            .iter()
            .any(|c| c == "resource_telemetry_unavailable"));
    }

    #[test]
    fn missing_resource_telemetry_is_honest_null_with_reason() {
        // Telemetry exists (errored) but its resources field is None: the
        // sampler degraded (§7). The receipt must keep resources null and
        // flag the gap without fabricating a figure.
        let tel = RunTelemetry::errored(7);
        assert!(tel.resources.is_none());
        let receipt = EaUsageReceipt::from_terminal(&record(CellState::Failed), Some(&tel));
        assert!(receipt.resources.is_none());
        assert!(receipt
            .completeness
            .reason_codes
            .iter()
            .any(|c| c == "resource_telemetry_unavailable"));
    }
}
