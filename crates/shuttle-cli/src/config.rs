//! `shuttle run` config layer (W4).
//!
//! Goal: make `shuttle run "<requirement>"` work zero-config. Precedence, low→high:
//!   built-in defaults  <  shuttle.toml  <  explicit CLI flags.
//!
//! We also auto-inject two things so a bare `shuttle run` can behave predictably
//! without the caller spelling them out:
//!   · `--provider <p>`  when configured via env / shuttle.toml — unless already in passthrough.
//!   · the shuttle tool extension (`shuttle-tools.ts`) — resolved from shuttle.toml or the
//!     conventional `~/.pi/agent/extensions/shuttle-tools.ts` — unless `-e` was given.
//!
//! Credential handling stays hands-off: shuttle NEVER reads provider secrets. We only
//! run a presence check against generic env / keychain locations and log whether
//! credentials look available — never their value. pi resolves the actual secret
//! itself via its provider entry.
//!
//! The TOML reader is a tiny dependency-free subset (flat `key = value`, string /
//! integer / array-of-strings, `#` comments, `[section]` headers ignored) — the
//! `toml` crate isn't vendored and the build runs offline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::run::RunConfig;

/// One parsed TOML scalar/array value (only the shapes shuttle.toml uses).
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Int(i64),
    Arr(Vec<String>),
}

/// Strip a `#` line comment, respecting quoted strings.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_s = false; // '
    let mut in_d = false; // "
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_d => in_s = !in_s,
            b'"' if !in_s => in_d = !in_d,
            b'#' if !in_s && !in_d => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// Parse the minimal TOML subset into a flat key→value map.
fn parse_min_toml(src: &str) -> HashMap<String, Val> {
    let mut map = HashMap::new();
    for raw in src.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        let v = v.trim();
        if key.is_empty() || v.is_empty() {
            continue;
        }
        let val = if v.starts_with('[') {
            let inner = v.trim_start_matches('[').trim_end_matches(']');
            let items: Vec<String> = inner
                .split(',')
                .map(unquote)
                .filter(|s| !s.is_empty())
                .collect();
            Val::Arr(items)
        } else if v.starts_with('"') || v.starts_with('\'') {
            Val::Str(unquote(v))
        } else if let Ok(n) = v.parse::<i64>() {
            Val::Int(n)
        } else {
            Val::Str(v.to_string())
        };
        map.insert(key, val);
    }
    map
}

/// `shuttle.toml` — only the keys `shuttle run` reads. All optional (fill-if-absent).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct EaConfig {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub provider: Option<String>,
    pub extension: Option<String>,
    pub exclude_tools: Option<Vec<String>>,
    pub soft_timeout_ms: Option<u64>,
}

impl EaConfig {
    /// Build from a parsed key→value map (pure; unit-testable).
    fn from_map(m: &HashMap<String, Val>) -> Self {
        let s = |k: &str| match m.get(k) {
            Some(Val::Str(v)) => Some(v.clone()),
            _ => None,
        };
        let arr = |k: &str| match m.get(k) {
            Some(Val::Arr(v)) => Some(v.clone()),
            _ => None,
        };
        let int = |k: &str| match m.get(k) {
            Some(Val::Int(v)) if *v >= 0 => Some(*v as u64),
            _ => None,
        };
        EaConfig {
            model: s("model"),
            thinking: s("thinking"),
            provider: s("provider"),
            extension: s("extension"),
            exclude_tools: arr("exclude_tools"),
            soft_timeout_ms: int("soft_timeout_ms"),
        }
    }

    /// Merge `other` on top of `self` (other wins per-field). Used to layer a
    /// project `shuttle.toml` over a global one.
    fn overlay(self, other: EaConfig) -> EaConfig {
        EaConfig {
            model: other.model.or(self.model),
            thinking: other.thinking.or(self.thinking),
            provider: other.provider.or(self.provider),
            extension: other.extension.or(self.extension),
            exclude_tools: other.exclude_tools.or(self.exclude_tools),
            soft_timeout_ms: other.soft_timeout_ms.or(self.soft_timeout_ms),
        }
    }

    /// Load + layer config: global (`~/.config/shuttle/shuttle.toml`) then project
    /// (`<root>/shuttle.toml`), project winning. Missing files are silently skipped.
    pub fn load(root: &str) -> EaConfig {
        let mut cfg = EaConfig::default();
        if let Some(home) = home_dir() {
            let global = home.join(".config").join("shuttle").join("shuttle.toml");
            if let Some(c) = load_file(&global) {
                cfg = cfg.overlay(c);
            }
        }
        let project = Path::new(root).join("shuttle.toml");
        if let Some(c) = load_file(&project) {
            cfg = cfg.overlay(c);
        }
        cfg
    }
}

const DEFAULT_PROVIDER_ENV: &str = "SHUTTLE_DEFAULT_PROVIDER";
const DEFAULT_MODEL_ENV: &str = "SHUTTLE_DEFAULT_MODEL";
const EXTENSION_PATH_ENV: &str = "SHUTTLE_EXTENSION_PATH";
const SECRET_ENV_NAMES: [&str; 3] = ["SHUTTLE_PROVIDER_KEY", "SHUTTLE_API_KEY", "OPENAI_API_KEY"];
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEYCHAIN_SERVICE_ENV: &str = "SHUTTLE_KEYCHAIN_SERVICE";

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn default_provider() -> Option<String> {
    env_nonempty(DEFAULT_PROVIDER_ENV)
}

fn default_model() -> Option<String> {
    env_nonempty(DEFAULT_MODEL_ENV)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_service() -> Option<String> {
    env_nonempty(KEYCHAIN_SERVICE_ENV)
}

fn load_file(path: &Path) -> Option<EaConfig> {
    let src = std::fs::read_to_string(path).ok()?;
    tracing::debug!(path = %path.display(), "shuttle run: loaded shuttle.toml");
    Some(EaConfig::from_map(&parse_min_toml(&src)))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The conventional location the W3 extension is installed to.
fn default_extension_path() -> Option<PathBuf> {
    default_extension_path_with(
        std::env::var_os(EXTENSION_PATH_ENV).map(PathBuf::from),
        home_dir(),
    )
}

fn default_extension_path_with(
    override_path: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return path.exists().then_some(path);
    }
    let p = home?
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("shuttle-tools.ts");
    p.exists().then_some(p)
}

/// Resolve only the legacy-configured extension path. Digest-bound cell
/// launches use this during preflight to reject conflicting ambient config
/// before replacing it with the descriptor-owned path.
pub(crate) fn configured_extension_path(root: &str) -> Option<String> {
    EaConfig::load(root).extension
}

/// Where the provider key was found (never the value itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Env,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Keychain(String),
    Absent,
}

impl KeySource {
    fn label(&self) -> String {
        match self {
            KeySource::Env => {
                let names = SECRET_ENV_NAMES.join(",");
                format!("env:{names}")
            }
            // 枚举变体全平台存在(见上方 cfg_attr),label 臂不得 cfg 门 macos——
            // 否则 linux 编译 match 非穷尽(E0004,2026-10-26 linux 出包抓到)。
            KeySource::Keychain(service) => format!("keychain:{service}"),
            KeySource::Absent => "absent".to_string(),
        }
    }
}

/// Presence-only credential preflight. NEVER reads or logs the secret value —
/// only reports whether credentials look resolvable, so a bare `shuttle run` can
/// warn early instead of failing deep inside pi. Env wins; else probe the
/// macOS Keychain service configured by `SHUTTLE_KEYCHAIN_SERVICE` (no `-w`, so no
/// password is emitted). If no generic source is configured, skip the keychain probe.
pub fn provider_key_source() -> KeySource {
    if SECRET_ENV_NAMES
        .iter()
        .any(|name| env_nonempty(name).is_some())
    {
        return KeySource::Env;
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(service) = keychain_service() {
            // `security find-generic-password -s <svc>` exits 0 if the item exists
            // and prints only non-secret attributes (we discard them anyway).
            let ok = std::process::Command::new("security")
                .args(["find-generic-password", "-s", service.as_str()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|st| st.success())
                .unwrap_or(false);
            if ok {
                return KeySource::Keychain(service);
            }
        }
    }
    KeySource::Absent
}

/// True if the passthrough already carries a `--provider` flag (user override).
fn passthrough_has_provider(passthrough: &[String]) -> bool {
    passthrough
        .iter()
        .any(|a| a == "--provider" || a.starts_with("--provider="))
}

/// Resolve the final `RunConfig` from CLI-provided optionals + shuttle.toml + defaults.
///
/// Precedence (low→high): built-in default < shuttle.toml < explicit CLI flag.
/// Also auto-injects the default provider and the W3 extension, and runs the
/// presence-only key preflight (logging source, never the secret).
#[allow(clippy::too_many_arguments)]
pub fn resolve(
    prompt: String,
    cli_model: Option<String>,
    cli_thinking: Option<String>,
    cli_provider: Option<String>,
    cli_exclude_tools: Option<Vec<String>>,
    cli_extension: Option<String>,
    root: String,
    deep: bool,
    no_session: bool,
    passthrough: Vec<String>,
) -> RunConfig {
    let file = EaConfig::load(&root);
    let mut cfg = RunConfig {
        prompt,
        root,
        no_session,
        mode: if deep {
            crate::run::WrapMode::RpcDeep
        } else {
            crate::run::WrapMode::JsonSupervisor
        },
        ..Default::default()
    };

    if let Some(m) = cli_model.or(file.model).or_else(default_model) {
        cfg.model = Some(m);
    }
    if let Some(t) = cli_thinking.or(file.thinking) {
        cfg.thinking = t;
    }
    if let Some(x) = cli_exclude_tools.or(file.exclude_tools) {
        cfg.exclude_tools = x;
    }

    // Provider: CLI > shuttle.toml > env default. Skip if the user already
    // pinned one via passthrough (`-- --provider …`) to avoid a duplicate flag.
    let provider = cli_provider.or(file.provider).or_else(default_provider);
    if !passthrough_has_provider(&passthrough) {
        cfg.provider = provider;
    }
    cfg.passthrough = passthrough;

    // Extension: CLI > shuttle.toml > conventional install path.
    cfg.extension = cli_extension
        .or(file.extension)
        .or_else(|| default_extension_path().map(|p| p.to_string_lossy().into_owned()));

    // Soft timeout from config → env (only if the caller hasn't set it).
    if let Some(ms) = file.soft_timeout_ms {
        if std::env::var_os("SHUTTLE_SOFT_TIMEOUT_MS").is_none() {
            std::env::set_var("SHUTTLE_SOFT_TIMEOUT_MS", ms.to_string());
        }
    }

    // Presence-only key preflight (never logs the secret).
    let src = provider_key_source();
    if src == KeySource::Absent {
        tracing::warn!(
            "shuttle run: no provider credential source found ({}); pi may fail to authenticate. \
             Set one of {:?} or configure SHUTTLE_KEYCHAIN_SERVICE.",
            src.label(),
            SECRET_ENV_NAMES
        );
    } else {
        tracing::info!(
            source = src.label(),
            "shuttle run: provider credential source present"
        );
    }

    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_scalars_arrays_and_comments() {
        let src = r#"
            # a comment
            model = "vendor/x"   # trailing comment
            thinking = 'high'
            soft_timeout_ms = 90000
            exclude_tools = ["edit", "write", "bash"]
            [ignored_section]
            provider = "vendor"
        "#;
        let m = parse_min_toml(src);
        let c = EaConfig::from_map(&m);
        assert_eq!(c.model.as_deref(), Some("vendor/x"));
        assert_eq!(c.thinking.as_deref(), Some("high"));
        assert_eq!(c.soft_timeout_ms, Some(90000));
        assert_eq!(
            c.exclude_tools,
            Some(vec!["edit".into(), "write".into(), "bash".into()])
        );
        assert_eq!(c.provider.as_deref(), Some("vendor"));
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let m = parse_min_toml(r#"model = "a#b""#);
        assert_eq!(m.get("model"), Some(&Val::Str("a#b".into())));
    }

    #[test]
    fn overlay_project_wins() {
        let global = EaConfig {
            model: Some("g".into()),
            thinking: Some("off".into()),
            ..Default::default()
        };
        let project = EaConfig {
            model: Some("p".into()),
            ..Default::default()
        };
        let merged = global.overlay(project);
        assert_eq!(merged.model.as_deref(), Some("p")); // project overrides
        assert_eq!(merged.thinking.as_deref(), Some("off")); // global fills gap
    }

    #[test]
    fn cli_overrides_file_and_default() {
        // No shuttle.toml in a tmp root → file layer empty; CLI must still win.
        let cfg = resolve(
            "do it".into(),
            Some("cli-model".into()),
            None,
            Some("cli-provider".into()),
            None,
            None,
            "/nonexistent-root-xyz".into(),
            false,
            true,
            vec![],
        );
        assert_eq!(cfg.model.as_deref(), Some("cli-model"));
        assert_eq!(cfg.provider.as_deref(), Some("cli-provider"));
        assert_eq!(cfg.thinking, "off"); // built-in default preserved
        assert!(cfg.no_session);
    }

    #[test]
    fn env_defaults_fill_missing_provider_and_model() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(DEFAULT_PROVIDER_ENV, "env-provider");
        std::env::set_var(DEFAULT_MODEL_ENV, "env/model");
        let cfg = resolve(
            "do it".into(),
            None,
            None,
            None,
            None,
            None,
            "/nonexistent-root-xyz".into(),
            false,
            true,
            vec![],
        );
        assert_eq!(cfg.model.as_deref(), Some("env/model"));
        assert_eq!(cfg.provider.as_deref(), Some("env-provider"));
        std::env::remove_var(DEFAULT_PROVIDER_ENV);
        std::env::remove_var(DEFAULT_MODEL_ENV);
    }

    #[test]
    fn passthrough_provider_suppresses_injection() {
        let cfg = resolve(
            "x".into(),
            None,
            None,
            None,
            None,
            None,
            "/nonexistent-root-xyz".into(),
            false,
            false,
            vec!["--provider".into(), "custom".into()],
        );
        // We must NOT inject our own provider when the user pinned one.
        assert_eq!(cfg.provider, None);
    }

    #[test]
    fn extension_path_env_overrides_the_conventional_install() {
        let dir =
            std::env::temp_dir().join(format!("shuttle_extension_override_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let extension = dir.join("shuttle-tools.ts");
        std::fs::write(&extension, "// test\n").unwrap();
        assert_eq!(
            default_extension_path_with(Some(extension.clone()), None),
            Some(extension)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
