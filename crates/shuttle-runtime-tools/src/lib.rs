//! Runtime resolution for the fixed subprocesses owned by Shuttle.
//!
//! Strict mode is deliberately expensive and fail-closed: the whole bundle is
//! authenticated before any executable path is returned. Caller-selected
//! verifier programs, providers, language servers and host infrastructure are
//! outside this crate.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MANIFEST_SCHEMA: &str = "ea_external_tools_bundle/v1";
pub const RUNTIME_ABI: &str = "shuttle-runtime/v1";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a resolved external tool with bounded wall time and captured output.
/// This avoids `Command::output()` turning an unexpectedly broad search or
/// history scan into an unbounded memory allocation.
pub fn output_bounded(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: u64,
    stderr_limit: u64,
) -> Result<std::process::Output, String> {
    use std::io::Read;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("subprocess stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("subprocess stderr unavailable")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(stdout_limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > stdout_limit {
            return Err(format!("stdout exceeds {stdout_limit} byte limit"));
        }
        Ok(bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(stderr_limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > stderr_limit {
            return Err(format!("stderr exceeds {stderr_limit} byte limit"));
        }
        Ok(bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            kill_process_group(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "subprocess timed out after {} ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked".to_string())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked".to_string())??;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolId {
    Git,
    Rg,
    Ctags,
    Tar,
    Ps,
    AstGrep,
    Mergiraf,
}

impl ToolId {
    pub const ALL: [Self; 7] = [
        Self::Git,
        Self::Rg,
        Self::Ctags,
        Self::Tar,
        Self::Ps,
        Self::AstGrep,
        Self::Mergiraf,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Rg => "rg",
            Self::Ctags => "ctags",
            Self::Tar => "tar",
            Self::Ps => "ps",
            Self::AstGrep => "ast-grep",
            Self::Mergiraf => "mergiraf",
        }
    }

    fn override_env(self) -> &'static str {
        match self {
            Self::Git => "SHUTTLE_GIT_BIN",
            Self::Rg => "SHUTTLE_RG_BIN",
            Self::Ctags => "SHUTTLE_CTAGS_BIN",
            Self::Tar => "SHUTTLE_TAR_BIN",
            Self::Ps => "SHUTTLE_PS_BIN",
            Self::AstGrep => "SHUTTLE_AST_GREP_BIN",
            Self::Mergiraf => "SHUTTLE_MERGIRAF_BIN",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    Legacy,
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema_version: String,
    pub bundle_id: String,
    pub bundle_version: String,
    pub platform: Platform,
    pub runtime_abis: Vec<String>,
    pub profiles: BTreeMap<String, ToolProfile>,
    pub tools: BTreeMap<ToolId, ToolEntry>,
    pub payload_manifest: PayloadManifest,
    #[serde(default)]
    pub source_archives: Vec<SourceArchive>,
    #[serde(default)]
    pub binary_archives: Vec<BinaryArchive>,
    pub license_notices: String,
    pub sbom_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub libc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProfile {
    pub required_tools: Vec<ToolId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    pub path: String,
    pub version: String,
    pub sha256: String,
    pub probes: Vec<CapabilityProbe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProbe {
    pub kind: ProbeKind,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: String,
    pub expected_exit: i32,
    #[serde(default)]
    pub stdout_contains: Option<String>,
    #[serde(default)]
    pub stderr_contains: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    Version,
    Behavior,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadManifest {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceArchive {
    pub name: String,
    pub packages: Vec<String>,
    pub source_package: String,
    pub source_version: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryArchive {
    pub name: String,
    pub package: String,
    pub version: String,
    pub sha256: String,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct ResolverConfig {
    pub mode: ToolMode,
    pub bundle_root: Option<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub profiles: Vec<String>,
    pub bundle_digest: Option<String>,
}

impl ResolverConfig {
    pub fn from_env() -> Result<Self, ToolError> {
        let mode = match std::env::var("SHUTTLE_TOOL_MODE")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None | Some("") | Some("legacy") => ToolMode::Legacy,
            Some("strict") => ToolMode::Strict,
            Some(other) => {
                return Err(ToolError::Configuration(format!(
                    "unsupported SHUTTLE_TOOL_MODE {other:?}"
                )))
            }
        };
        let root = std::env::var_os("SHUTTLE_TOOL_BUNDLE_ROOT").map(PathBuf::from);
        let manifest = std::env::var_os("SHUTTLE_TOOL_MANIFEST").map(PathBuf::from);
        let mut profiles: Vec<String> = std::env::var("SHUTTLE_TOOL_PROFILE")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        profiles.sort();
        if profiles.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ToolError::Configuration(
                "SHUTTLE_TOOL_PROFILE must not contain duplicates".into(),
            ));
        }
        let bundle_digest = std::env::var("SHUTTLE_TOOL_BUNDLE_DIGEST")
            .ok()
            .filter(|v| !v.trim().is_empty());
        if mode == ToolMode::Strict {
            if root.is_none()
                || manifest.is_none()
                || profiles.is_empty()
                || bundle_digest.is_none()
            {
                return Err(ToolError::Configuration("strict mode requires SHUTTLE_TOOL_BUNDLE_ROOT, SHUTTLE_TOOL_MANIFEST, SHUTTLE_TOOL_PROFILE and SHUTTLE_TOOL_BUNDLE_DIGEST".into()));
            }
            validate_digest(bundle_digest.as_deref().unwrap())?;
        }
        Ok(Self {
            mode,
            bundle_root: root,
            manifest_path: manifest,
            profiles,
            bundle_digest,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("runtime tool configuration rejected: {0}")]
    Configuration(String),
    #[error("runtime tool manifest rejected: {0}")]
    Manifest(String),
    #[error("runtime tool bundle integrity failure: {0}")]
    Integrity(String),
    #[error("runtime tool `{tool}` unavailable: {reason}")]
    Unavailable { tool: String, reason: String },
    #[error("runtime tool `{tool}` incompatible: {reason}")]
    Incompatible { tool: String, reason: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ToolResolver {
    config: ResolverConfig,
    verified: Option<VerifiedBundle>,
}

#[derive(Debug, Clone)]
struct VerifiedBundle {
    root: PathBuf,
    manifest_digest: String,
    manifest: BundleManifest,
    selected_tools: BTreeSet<ToolId>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: &'static str,
    pub mode: ToolMode,
    pub status: &'static str,
    pub bundle_root: Option<String>,
    pub manifest_path: Option<String>,
    pub bundle_digest: Option<String>,
    pub manifest_digest: Option<String>,
    pub profiles: Vec<String>,
    pub tools: BTreeMap<String, DoctorTool>,
    pub issues: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorTool {
    pub status: &'static str,
    pub path: Option<String>,
    pub version: Option<String>,
}

impl ToolResolver {
    pub fn from_env() -> Result<Self, ToolError> {
        Self::new(ResolverConfig::from_env()?)
    }

    pub fn new(config: ResolverConfig) -> Result<Self, ToolError> {
        let verified = if config.mode == ToolMode::Strict {
            Some(verify_bundle(&config)?)
        } else {
            None
        };
        Ok(Self { config, verified })
    }

    pub fn mode(&self) -> ToolMode {
        self.config.mode
    }

    pub fn manifest_digest(&self) -> Option<&str> {
        self.verified.as_ref().map(|v| v.manifest_digest.as_str())
    }

    pub fn bundle_digest(&self) -> Option<&str> {
        self.config.bundle_digest.as_deref()
    }

    pub fn bundle_root(&self) -> Option<&Path> {
        self.verified.as_ref().map(|v| v.root.as_path())
    }

    pub fn resolve(&self, tool: ToolId) -> Result<PathBuf, ToolError> {
        match &self.verified {
            Some(bundle) => {
                if !bundle.selected_tools.contains(&tool) {
                    return Err(ToolError::Unavailable {
                        tool: tool.as_str().into(),
                        reason: "not enabled by the selected profiles".into(),
                    });
                }
                let entry =
                    bundle
                        .manifest
                        .tools
                        .get(&tool)
                        .ok_or_else(|| ToolError::Unavailable {
                            tool: tool.as_str().into(),
                            reason: "not declared by the verified bundle".into(),
                        })?;
                secure_path(&bundle.root, &entry.path, false)
            }
            None => Ok(resolve_legacy_with(tool, |name| std::env::var_os(name))),
        }
    }

    pub fn assert_identity(
        &self,
        manifest_digest: &str,
        bundle_digest: &str,
        root: &Path,
        profiles: &[String],
    ) -> Result<(), ToolError> {
        if self.config.mode != ToolMode::Strict {
            return Err(ToolError::Configuration(
                "a digest-bound runtime descriptor requires strict tool mode".into(),
            ));
        }
        if self.manifest_digest() != Some(manifest_digest) {
            return Err(ToolError::Integrity(
                "runtime descriptor manifest digest does not match the verified manifest".into(),
            ));
        }
        if self.bundle_digest() != Some(bundle_digest) {
            return Err(ToolError::Integrity(
                "runtime descriptor distribution digest does not match SHUTTLE_TOOL_BUNDLE_DIGEST"
                    .into(),
            ));
        }
        let expected_root = canonical_root(root)?;
        if self.bundle_root() != Some(expected_root.as_path()) {
            return Err(ToolError::Integrity(
                "runtime descriptor tools root does not match SHUTTLE_TOOL_BUNDLE_ROOT".into(),
            ));
        }
        if self.config.profiles != profiles {
            return Err(ToolError::Integrity(
                "runtime descriptor profiles do not match SHUTTLE_TOOL_PROFILE".into(),
            ));
        }
        Ok(())
    }

    pub fn doctor(&self) -> DoctorReport {
        let mut tools = BTreeMap::new();
        let mut issues = Vec::new();
        let ids: BTreeSet<ToolId> = match &self.verified {
            Some(v) => self
                .config
                .profiles
                .iter()
                .filter_map(|p| v.manifest.profiles.get(p))
                .flat_map(|p| p.required_tools.iter().copied())
                .collect(),
            None => ToolId::ALL.into_iter().collect(),
        };
        for id in ids {
            match self.resolve(id) {
                Ok(path) => {
                    let version = self
                        .verified
                        .as_ref()
                        .and_then(|v| v.manifest.tools.get(&id))
                        .map(|e| e.version.clone());
                    tools.insert(
                        id.as_str().into(),
                        DoctorTool {
                            status: "verified",
                            path: Some(path.display().to_string()),
                            version,
                        },
                    );
                }
                Err(error) => {
                    issues.push(error.to_string());
                    tools.insert(
                        id.as_str().into(),
                        DoctorTool {
                            status: "unavailable",
                            path: None,
                            version: None,
                        },
                    );
                }
            }
        }
        issues.sort();
        DoctorReport {
            schema_version: "ea_tools_doctor/v1",
            mode: self.config.mode,
            status: if issues.is_empty() { "ok" } else { "error" },
            bundle_root: self
                .config
                .bundle_root
                .as_ref()
                .map(|p| p.display().to_string()),
            manifest_path: self
                .config
                .manifest_path
                .as_ref()
                .map(|p| p.display().to_string()),
            bundle_digest: self.config.bundle_digest.clone(),
            manifest_digest: self.manifest_digest().map(str::to_owned),
            profiles: self.config.profiles.clone(),
            tools,
            issues,
        }
    }
}

pub fn resolve(tool: ToolId) -> Result<PathBuf, ToolError> {
    ToolResolver::from_env()?.resolve(tool)
}

fn resolve_legacy_with(
    tool: ToolId,
    get_env: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> PathBuf {
    get_env(tool.override_env())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(tool.as_str()))
}

fn verify_bundle(config: &ResolverConfig) -> Result<VerifiedBundle, ToolError> {
    let root = canonical_root(
        config
            .bundle_root
            .as_deref()
            .expect("strict config checked"),
    )?;
    let requested_manifest = config
        .manifest_path
        .as_deref()
        .expect("strict config checked");
    reject_symlink(requested_manifest)?;
    let manifest_path = std::fs::canonicalize(requested_manifest)?;
    if !manifest_path.starts_with(&root) {
        return Err(ToolError::Integrity("manifest escapes bundle root".into()));
    }
    let manifest_bytes = std::fs::read(&manifest_path)?;
    let manifest_digest = digest(&manifest_bytes);
    let manifest: BundleManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| ToolError::Manifest(e.to_string()))?;
    validate_manifest(&manifest)?;
    let current = current_platform();
    if manifest.platform.os != current.os
        || manifest.platform.arch != current.arch
        || manifest.platform.libc != current.libc
    {
        return Err(ToolError::Manifest(format!(
            "platform mismatch: bundle={}/{}/{} runtime={}/{}/{}",
            manifest.platform.os,
            manifest.platform.arch,
            manifest.platform.libc,
            current.os,
            current.arch,
            current.libc
        )));
    }
    if !manifest.runtime_abis.iter().any(|abi| abi == RUNTIME_ABI) {
        return Err(ToolError::Manifest(format!(
            "runtime ABI {RUNTIME_ABI} is not supported"
        )));
    }
    let selected = selected_tools(&manifest, &config.profiles)?;
    let checksum_path = secure_path(&root, &manifest.payload_manifest.path, false)?;
    let checksum_bytes = std::fs::read(&checksum_path)?;
    if digest(&checksum_bytes) != manifest.payload_manifest.sha256 {
        return Err(ToolError::Integrity(
            "payload manifest digest mismatch".into(),
        ));
    }
    let checksums = parse_checksums(&checksum_bytes)?;
    verify_payload_closure(&root, &manifest_path, &checksum_path, &checksums)?;
    for (id, entry) in &manifest.tools {
        let path = secure_path(&root, &entry.path, false)?;
        let actual = digest(&std::fs::read(&path)?);
        if actual != entry.sha256 {
            return Err(ToolError::Integrity(format!(
                "{} digest mismatch",
                id.as_str()
            )));
        }
        if checksums.get(&entry.path) != Some(&entry.sha256) {
            return Err(ToolError::Integrity(format!(
                "{} is not identically bound by the payload manifest",
                id.as_str()
            )));
        }
        if selected.contains(id) {
            verify_probes(*id, &path, entry)?;
        }
    }
    Ok(VerifiedBundle {
        root,
        manifest_digest,
        manifest,
        selected_tools: selected,
    })
}

fn validate_manifest(manifest: &BundleManifest) -> Result<(), ToolError> {
    if manifest.schema_version != MANIFEST_SCHEMA {
        return Err(ToolError::Manifest(format!(
            "unsupported schema {}",
            manifest.schema_version
        )));
    }
    for (name, value) in [
        ("bundle_id", &manifest.bundle_id),
        ("bundle_version", &manifest.bundle_version),
        ("license_notices", &manifest.license_notices),
        ("sbom_ref", &manifest.sbom_ref),
    ] {
        if value.trim().is_empty() {
            return Err(ToolError::Manifest(format!("{name} must be non-empty")));
        }
    }
    validate_digest(&manifest.payload_manifest.sha256)?;
    for (id, entry) in &manifest.tools {
        validate_relative(&entry.path)?;
        validate_digest(&entry.sha256)?;
        if entry.version.trim().is_empty() {
            return Err(ToolError::Manifest(format!(
                "{} version must be non-empty",
                id.as_str()
            )));
        }
        if !entry.probes.iter().any(|p| p.kind == ProbeKind::Version)
            || !entry.probes.iter().any(|p| p.kind == ProbeKind::Behavior)
        {
            return Err(ToolError::Manifest(format!(
                "{} requires version and behavior probes",
                id.as_str()
            )));
        }
    }
    validate_relative(&manifest.payload_manifest.path)?;
    Ok(())
}

fn selected_tools(
    manifest: &BundleManifest,
    profiles: &[String],
) -> Result<BTreeSet<ToolId>, ToolError> {
    let mut selected = BTreeSet::new();
    for name in profiles {
        let profile = manifest
            .profiles
            .get(name)
            .ok_or_else(|| ToolError::Manifest(format!("requested profile {name:?} is absent")))?;
        for id in &profile.required_tools {
            if !manifest.tools.contains_key(id) {
                return Err(ToolError::Manifest(format!(
                    "profile {name:?} requires undeclared tool {}",
                    id.as_str()
                )));
            }
            selected.insert(*id);
        }
    }
    Ok(selected)
}

fn verify_probes(id: ToolId, path: &Path, entry: &ToolEntry) -> Result<(), ToolError> {
    for probe in &entry.probes {
        let output = run_probe(path, probe).map_err(|e| ToolError::Incompatible {
            tool: id.as_str().into(),
            reason: e,
        })?;
        if output.status.code() != Some(probe.expected_exit)
            || probe
                .stdout_contains
                .as_ref()
                .is_some_and(|v| !String::from_utf8_lossy(&output.stdout).contains(v))
            || probe
                .stderr_contains
                .as_ref()
                .is_some_and(|v| !String::from_utf8_lossy(&output.stderr).contains(v))
        {
            return Err(ToolError::Incompatible {
                tool: id.as_str().into(),
                reason: format!("{:?} probe did not match its declared result", probe.kind),
            });
        }
    }
    Ok(())
}

fn run_probe(path: &Path, probe: &CapabilityProbe) -> Result<std::process::Output, String> {
    use std::io::Write;
    let mut child = Command::new(path)
        .args(&probe.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if !probe.stdin.is_empty() {
        child
            .stdin
            .as_mut()
            .ok_or("probe stdin unavailable")?
            .write_all(probe.stdin.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    drop(child.stdin.take());
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|e| e.to_string()),
            None if start.elapsed() >= PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "probe timed out after {} seconds",
                    PROBE_TIMEOUT.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn parse_checksums(bytes: &[u8]) -> Result<BTreeMap<String, String>, ToolError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ToolError::Manifest(format!("payload manifest is not UTF-8: {e}")))?;
    let mut out = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (hex, path) = line
            .split_once("  ")
            .ok_or_else(|| ToolError::Manifest(format!("invalid checksum line {}", index + 1)))?;
        let value = format!("sha256:{hex}");
        validate_digest(&value)?;
        validate_relative(path)?;
        if out.insert(path.to_owned(), value).is_some() {
            return Err(ToolError::Manifest(format!(
                "duplicate checksum path {path}"
            )));
        }
    }
    Ok(out)
}

fn verify_payload_closure(
    root: &Path,
    manifest: &Path,
    checksums_file: &Path,
    checksums: &BTreeMap<String, String>,
) -> Result<(), ToolError> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        let mut entries = std::fs::read_dir(&dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                return Err(ToolError::Integrity(format!(
                    "symlink is forbidden in bundle: {}",
                    path.display()
                )));
            }
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            if path == manifest || path == checksums_file {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|_| ToolError::Integrity("payload escaped root".into()))?
                .to_string_lossy()
                .replace('\\', "/");
            let expected = checksums.get(&rel).ok_or_else(|| {
                ToolError::Integrity(format!("payload file is not checksum-bound: {rel}"))
            })?;
            if digest(&std::fs::read(&path)?) != *expected {
                return Err(ToolError::Integrity(format!(
                    "payload digest mismatch: {rel}"
                )));
            }
            seen.insert(rel);
        }
    }
    if seen.len() != checksums.len() {
        let missing: Vec<_> = checksums.keys().filter(|p| !seen.contains(*p)).collect();
        return Err(ToolError::Integrity(format!(
            "checksum entries do not name regular payload files: {missing:?}"
        )));
    }
    Ok(())
}

fn canonical_root(path: &Path) -> Result<PathBuf, ToolError> {
    reject_symlink(path)?;
    let root = std::fs::canonicalize(path)?;
    if !root.is_dir() {
        return Err(ToolError::Integrity(
            "bundle root is not a directory".into(),
        ));
    }
    Ok(root)
}
fn reject_symlink(path: &Path) -> Result<(), ToolError> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(ToolError::Integrity(format!(
            "symlink path is forbidden: {}",
            path.display()
        )));
    }
    Ok(())
}
fn secure_path(root: &Path, relative: &str, allow_directory: bool) -> Result<PathBuf, ToolError> {
    validate_relative(relative)?;
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        if let Component::Normal(part) = component {
            current.push(part);
            reject_symlink(&current)?;
        }
    }
    let canonical = std::fs::canonicalize(&current)?;
    if !canonical.starts_with(root) || (!allow_directory && !canonical.is_file()) {
        return Err(ToolError::Integrity(format!(
            "path escapes root or is not a file: {relative}"
        )));
    }
    Ok(canonical)
}
fn validate_relative(path: &str) -> Result<(), ToolError> {
    let p = Path::new(path);
    if path.is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ToolError::Manifest(format!(
            "path must be a normalized relative path: {path:?}"
        )));
    }
    Ok(())
}
fn validate_digest(value: &str) -> Result<(), ToolError> {
    let hex = value
        .strip_prefix("sha256:")
        .ok_or_else(|| ToolError::Manifest(format!("invalid SHA-256 digest: {value}")))?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(ToolError::Manifest(format!(
            "invalid SHA-256 digest: {value}"
        )));
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn current_platform() -> Platform {
    let libc = if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "glibc"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "unknown"
    };
    Platform {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        libc: libc.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Duration;

    struct Fixture {
        dir: tempfile::TempDir,
        config: ResolverConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("bin")).unwrap();
            let git = dir.path().join("bin/git");
            std::fs::write(&git, "#!/bin/sh\ncase \"$1\" in --version) echo fake-git-1;; status) exit 0;; *) exit 9;; esac\n").unwrap();
            let mut permissions = std::fs::metadata(&git).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&git, permissions).unwrap();
            let git_digest = digest(&std::fs::read(&git).unwrap());
            let checksums = format!("{}  bin/git\n", git_digest.trim_start_matches("sha256:"));
            std::fs::write(dir.path().join("checksums.sha256"), &checksums).unwrap();
            let checksum_digest = digest(checksums.as_bytes());
            let platform = current_platform();
            let manifest = serde_json::json!({
                "schema_version": MANIFEST_SCHEMA,
                "bundle_id": "test-tools",
                "bundle_version": "1.0.0",
                "platform": platform,
                "runtime_abis": [RUNTIME_ABI],
                "profiles": {"cell-supervisor": {"required_tools": ["git"]}},
                "tools": {"git": {
                    "path": "bin/git", "version": "fake-git-1", "sha256": git_digest,
                    "probes": [
                        {"kind":"version","args":["--version"],"expected_exit":0,"stdout_contains":"fake-git-1"},
                        {"kind":"behavior","args":["status"],"expected_exit":0}
                    ]
                }},
                "payload_manifest": {"path": "checksums.sha256", "sha256": checksum_digest},
                "source_archives": [],
                "binary_archives": [],
                "license_notices": "licenses/NOTICE",
                "sbom_ref": "sbom.cdx.json"
            });
            let manifest_path = dir.path().join("manifest.json");
            std::fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            let config = ResolverConfig {
                mode: ToolMode::Strict,
                bundle_root: Some(dir.path().to_path_buf()),
                manifest_path: Some(manifest_path),
                profiles: vec!["cell-supervisor".into()],
                bundle_digest: Some(format!("sha256:{}", "f".repeat(64))),
            };
            Self { dir, config }
        }
    }

    #[test]
    fn strict_manifest_resolves_verified_absolute_path() {
        let fixture = Fixture::new();
        let resolver = ToolResolver::new(fixture.config).unwrap();
        assert_eq!(
            resolver.resolve(ToolId::Git).unwrap(),
            std::fs::canonicalize(fixture.dir.path().join("bin/git")).unwrap()
        );
        assert_eq!(resolver.doctor().status, "ok");
    }

    #[test]
    fn legacy_mode_preserves_override_then_path_resolution() {
        assert_eq!(
            resolve_legacy_with(ToolId::Git, |_| None),
            PathBuf::from("git")
        );
        assert_eq!(
            resolve_legacy_with(ToolId::Git, |name| {
                assert_eq!(name, "SHUTTLE_GIT_BIN");
                Some(std::ffi::OsString::from("/verified/legacy/git"))
            }),
            PathBuf::from("/verified/legacy/git")
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        let fixture = Fixture::new();
        let path = fixture.config.manifest_path.as_ref().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".into(), true.into());
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            ToolResolver::new(fixture.config),
            Err(ToolError::Manifest(_))
        ));
    }

    #[test]
    fn strict_mode_never_falls_back_to_path_for_missing_or_invalid_tools() {
        let fixture = Fixture::new();
        let resolver = ToolResolver::new(fixture.config.clone()).unwrap();
        let error = resolver.resolve(ToolId::AstGrep).unwrap_err();
        assert!(matches!(error, ToolError::Unavailable { .. }));

        // A required executable disappearing after manifest creation must also
        // fail bundle construction. Neither case returns a bare program name
        // that Command could resolve through ambient PATH.
        std::fs::remove_file(fixture.dir.path().join("bin/git")).unwrap();
        assert!(ToolResolver::new(fixture.config).is_err());
    }

    #[test]
    fn strict_resolution_rejects_tools_outside_selected_profiles() {
        let fixture = Fixture::new();
        let path = fixture.config.manifest_path.as_ref().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value["tools"]["ast-grep"] = value["tools"]["git"].clone();
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let resolver = ToolResolver::new(fixture.config).unwrap();
        let error = resolver.resolve(ToolId::AstGrep).unwrap_err();
        assert!(matches!(error, ToolError::Unavailable { .. }));
        assert!(error.to_string().contains("selected profiles"));
    }

    #[test]
    fn symlink_and_root_escape_are_rejected() {
        let fixture = Fixture::new();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(fixture.dir.path().join("bin/git")).unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.dir.path().join("bin/git")).unwrap();
        assert!(ToolResolver::new(fixture.config)
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(validate_relative("../git").is_err());
        assert!(validate_relative("/usr/bin/git").is_err());
    }

    #[test]
    fn payload_and_platform_mismatch_fail_closed() {
        let fixture = Fixture::new();
        std::fs::write(fixture.dir.path().join("bin/git"), "tampered").unwrap();
        assert!(ToolResolver::new(fixture.config).is_err());

        let fixture = Fixture::new();
        let path = fixture.config.manifest_path.as_ref().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value["platform"]["arch"] = "definitely-not-this-architecture".into();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(ToolResolver::new(fixture.config)
            .unwrap_err()
            .to_string()
            .contains("platform mismatch"));
    }

    #[test]
    fn bounded_output_rejects_stdout_over_limit() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 123456789"]);
        let error = output_bounded(&mut command, Duration::from_secs(1), 8, 1024).unwrap_err();
        assert!(error.contains("stdout exceeds 8 byte limit"), "{error}");
    }

    #[test]
    fn bounded_output_times_out_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2"]);
        let started = Instant::now();
        let error =
            output_bounded(&mut command, Duration::from_millis(50), 1024, 1024).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
