use crate::GoalError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub r#ref: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, GoalError> {
        let requested = root.as_ref();
        fs::create_dir_all(requested)?;
        let root = fs::canonicalize(requested)?;
        fs::create_dir_all(root.join("objects/sha256"))?;
        Ok(Self { root })
    }

    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, GoalError> {
        let root = fs::canonicalize(root.as_ref())?;
        let objects = root.join("objects/sha256");
        let meta = fs::symlink_metadata(&objects)?;
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            return Err(GoalError::Integrity(format!(
                "artifact object root is not a real directory: {}",
                objects.display()
            )));
        }
        Ok(Self { root })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ArtifactRef, GoalError> {
        let hex = hex_digest(bytes);
        let dir = self.root.join("objects/sha256").join(&hex[..2]);
        if dir.exists() && fs::symlink_metadata(&dir)?.file_type().is_symlink() {
            return Err(GoalError::Integrity(format!(
                "artifact prefix is a symlink: {}",
                dir.display()
            )));
        }
        fs::create_dir_all(&dir)?;
        let canonical_dir = fs::canonicalize(&dir)?;
        if !canonical_dir.starts_with(&self.root) {
            return Err(GoalError::Integrity(format!(
                "artifact prefix escapes store: {}",
                dir.display()
            )));
        }
        let target = dir.join(&hex);
        if target.exists() && fs::symlink_metadata(&target)?.file_type().is_symlink() {
            return Err(GoalError::Integrity(format!(
                "artifact target is a symlink: {}",
                target.display()
            )));
        }
        if !target.exists() {
            let tmp = dir.join(format!(".{hex}.tmp-{}", std::process::id()));
            let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            match fs::rename(&tmp, &target) {
                Ok(()) => {}
                Err(_e) if target.exists() => {
                    let _ = fs::remove_file(&tmp);
                }
                Err(e) => return Err(e.into()),
            }
        }
        let stored = fs::read(&target)?;
        if hex_digest(&stored) != hex {
            return Err(GoalError::Integrity(format!(
                "artifact digest mismatch: {hex}"
            )));
        }
        Ok(ArtifactRef {
            r#ref: format!("artifact://sha256/{hex}"),
            digest: format!("sha256:{hex}"),
            size: bytes.len() as u64,
        })
    }

    pub fn get(&self, reference: &str) -> Result<Vec<u8>, GoalError> {
        let Some(hex) = reference.strip_prefix("artifact://sha256/") else {
            return Err(GoalError::InvalidInput(format!(
                "unsupported artifact ref: {reference}"
            )));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(GoalError::InvalidInput(format!(
                "invalid artifact digest: {reference}"
            )));
        }
        let path = self.root.join("objects/sha256").join(&hex[..2]).join(hex);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() || meta.file_type().is_symlink() {
            return Err(GoalError::Integrity(format!(
                "artifact is not a regular file: {reference}"
            )));
        }
        let bytes = fs::read(path)?;
        if hex_digest(&bytes) != hex {
            return Err(GoalError::Integrity(format!(
                "artifact digest mismatch: {reference}"
            )));
        }
        Ok(bytes)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_addressed_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(dir.path()).unwrap();
        let a = s.put(b"acceptance").unwrap();
        let b = s.put(b"acceptance").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.get(&a.r#ref).unwrap(), b"acceptance");
    }

    #[test]
    #[cfg(unix)]
    fn symlink_prefix_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(dir.path()).unwrap();
        let hex = hex_digest(b"payload");
        let prefix = dir.path().join("objects/sha256").join(&hex[..2]);
        symlink(outside.path(), &prefix).unwrap();
        assert!(matches!(s.put(b"payload"), Err(GoalError::Integrity(_))));
    }
}
