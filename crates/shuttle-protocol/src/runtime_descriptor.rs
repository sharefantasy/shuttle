//! Digest-bound identity for the complete runtime used by one SHUTTLE attempt.

use crate::anchor::Sha256Digest;
use crate::hashing::sha256_hex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA: &str = "ea_runtime_descriptor/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    LegacyFull,
    ComposedImage,
    MountedBundle,
    HostBundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCore {
    pub version: String,
    pub runtime_abi: String,
    pub executable_path: String,
    pub digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTools {
    pub source: String,
    pub distribution_digest: Sha256Digest,
    pub manifest_digest: Sha256Digest,
    pub root: String,
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRuntime {
    pub kind: String,
    pub version: String,
    pub binary: String,
    pub binary_digest: Sha256Digest,
    pub extension_path: String,
    pub extension_digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EaRuntimeDescriptor {
    pub schema_version: String,
    pub mode: RuntimeMode,
    pub core: RuntimeCore,
    pub tools: RuntimeTools,
    pub provider: ProviderRuntime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composed_image_digest: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_receipt: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeDescriptorError {
    #[error("unsupported runtime descriptor schema: {0}")]
    UnsupportedSchema(String),
    #[error("invalid runtime descriptor: {0}")]
    Invalid(String),
}

impl EaRuntimeDescriptor {
    pub fn parse(json: &str) -> Result<Self, RuntimeDescriptorError> {
        let descriptor: Self = serde_json::from_str(json)
            .map_err(|error| RuntimeDescriptorError::Invalid(error.to_string()))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), RuntimeDescriptorError> {
        if self.schema_version != SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA {
            return Err(RuntimeDescriptorError::UnsupportedSchema(
                self.schema_version.clone(),
            ));
        }
        for (name, value) in [
            ("core.version", self.core.version.as_str()),
            ("core.runtime_abi", self.core.runtime_abi.as_str()),
            ("tools.source", self.tools.source.as_str()),
            ("provider.kind", self.provider.kind.as_str()),
            ("provider.version", self.provider.version.as_str()),
            ("provider.binary", self.provider.binary.as_str()),
            (
                "provider.extension_path",
                self.provider.extension_path.as_str(),
            ),
        ] {
            if value.trim().is_empty() || value.contains(['\n', '\r', '\0']) {
                return Err(RuntimeDescriptorError::Invalid(format!(
                    "{name} must be non-empty and single-line"
                )));
            }
        }
        for (name, value) in [
            ("core.executable_path", &self.core.executable_path),
            ("tools.root", &self.tools.root),
            ("provider.binary", &self.provider.binary),
            ("provider.extension_path", &self.provider.extension_path),
        ] {
            if !Path::new(value).is_absolute() {
                return Err(RuntimeDescriptorError::Invalid(format!(
                    "{name} must be absolute"
                )));
            }
        }
        if self.tools.profiles.is_empty()
            || self
                .tools
                .profiles
                .iter()
                .any(|profile| profile.trim().is_empty())
        {
            return Err(RuntimeDescriptorError::Invalid(
                "tools.profiles must be non-empty".into(),
            ));
        }
        if !self.tools.profiles.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(RuntimeDescriptorError::Invalid(
                "tools.profiles must be sorted and unique".into(),
            ));
        }
        match self.mode {
            RuntimeMode::ComposedImage if self.composed_image_digest.is_none() => {
                return Err(RuntimeDescriptorError::Invalid(
                    "composed_image requires composed_image_digest".into(),
                ));
            }
            RuntimeMode::MountedBundle
                if self.mount_receipt.as_deref().is_none_or(str::is_empty) =>
            {
                return Err(RuntimeDescriptorError::Invalid(
                    "mounted_bundle requires mount_receipt".into(),
                ));
            }
            RuntimeMode::LegacyFull
            | RuntimeMode::HostBundle
            | RuntimeMode::ComposedImage
            | RuntimeMode::MountedBundle => {}
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<Sha256Digest, RuntimeDescriptorError> {
        self.validate()?;
        let value = serde_json::to_value(self)
            .map_err(|e| RuntimeDescriptorError::Invalid(e.to_string()))?;
        let canonical = canonicalize(value);
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|e| RuntimeDescriptorError::Invalid(e.to_string()))?;
        Sha256Digest::parse(format!("sha256:{}", sha256_hex(&bytes)))
            .map_err(|e| RuntimeDescriptorError::Invalid(e.to_string()))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> EaRuntimeDescriptor {
        let digest = |ch: char| {
            Sha256Digest::parse(format!("sha256:{}", ch.to_string().repeat(64))).unwrap()
        };
        EaRuntimeDescriptor {
            schema_version: SHUTTLE_RUNTIME_DESCRIPTOR_SCHEMA.into(),
            mode: RuntimeMode::ComposedImage,
            core: RuntimeCore {
                version: "0.2.0".into(),
                runtime_abi: "shuttle-runtime/v1".into(),
                executable_path: "/usr/local/bin/shuttle".into(),
                digest: digest('a'),
            },
            tools: RuntimeTools {
                source: "oci://shuttle-tools@sha256:...".into(),
                distribution_digest: digest('b'),
                manifest_digest: digest('c'),
                root: "/opt/shuttle-tools".into(),
                profiles: vec!["cell-supervisor".into(), "source-analysis".into()],
            },
            provider: ProviderRuntime {
                kind: "pi".into(),
                version: "1".into(),
                binary: "/usr/local/bin/pi".into(),
                binary_digest: digest('f'),
                extension_path: "/opt/shuttle-provider/shuttle-tools.ts".into(),
                extension_digest: digest('d'),
            },
            composed_image_digest: Some(digest('e')),
            mount_receipt: None,
        }
    }

    #[test]
    fn digest_is_deterministic_and_unknown_fields_are_rejected() {
        let descriptor = descriptor();
        assert_eq!(descriptor.digest().unwrap(), descriptor.digest().unwrap());
        let mut value = serde_json::to_value(descriptor).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("secret_extra".into(), true.into());
        assert!(EaRuntimeDescriptor::parse(&value.to_string()).is_err());
    }

    #[test]
    fn mode_specific_identity_is_required() {
        let mut descriptor = descriptor();
        descriptor.composed_image_digest = None;
        assert!(descriptor
            .validate()
            .unwrap_err()
            .to_string()
            .contains("composed_image_digest"));
        descriptor.mode = RuntimeMode::MountedBundle;
        assert!(descriptor
            .validate()
            .unwrap_err()
            .to_string()
            .contains("mount_receipt"));
    }

    #[test]
    fn provider_artifact_coordinates_are_required_and_absolute() {
        let mut value = serde_json::to_value(descriptor()).unwrap();
        let provider = value["provider"].as_object_mut().unwrap();
        provider.remove("binary_digest");
        provider.remove("extension_path");
        assert!(EaRuntimeDescriptor::parse(&value.to_string()).is_err());

        let mut relative_binary = descriptor();
        relative_binary.provider.binary = "pi".into();
        assert!(relative_binary
            .validate()
            .unwrap_err()
            .to_string()
            .contains("provider.binary"));

        let mut descriptor = descriptor();
        descriptor.provider.extension_path = "shuttle-tools.ts".into();
        assert!(descriptor
            .validate()
            .unwrap_err()
            .to_string()
            .contains("provider.extension_path"));
    }
}
