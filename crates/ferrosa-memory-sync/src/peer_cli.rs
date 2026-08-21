//! CLI support for the P2P peer session (MAAS-T-36): device key files, the
//! durable directory apply store, and the share/receive command runners.
//!
//! # Device key file
//!
//! `p2p-keygen` writes a JSON file holding the device's instance id and the
//! 32-byte Ed25519 secret (hex), mode 0600. The printed **public key hex** is
//! what gets registered at the gateway (`POST /v1/devices`); the printed
//! fingerprint must then appear in the registry. The secret never leaves the
//! file.
//!
//! # Directory apply store
//!
//! [`DirPackApplyStore`] is the CLI's durable [`PackApplyStore`]: staged packs
//! land as `staged-<pack>.json` (tmp + rename), the flip renames them to
//! `applied-<pack>-v<version>.json` and updates `versions.json` in the same
//! way. It is explicit about being a file-backed landing zone — the
//! storage-backed apply store (imported packs visible to live recall) is a
//! separate, board-tracked packet and is NOT faked here.

// Fail-loud on untrusted input; production paths never unwrap. Tests assert
// on known-good fixtures and are exempt.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ferrosa_memory_core::remote_identity::{InstanceId, InstanceSigningIdentity};

use crate::learner_ingest::{PackApplyStore, StagedPack};

/// The on-disk device key file (mode 0600).
#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceKeyFile {
    /// The device's instance id.
    pub instance_id: Uuid,
    /// The 32-byte Ed25519 secret, hex. NEVER logged or transmitted.
    pub secret_hex: String,
}

/// Errors from CLI support paths. All fail-closed.
#[derive(Debug, thiserror::Error)]
pub enum PeerCliError {
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("device key file {0}: {1}")]
    KeyFile(PathBuf, String),
    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> PeerCliError + '_ {
    move |source| PeerCliError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Generate a fresh device identity and write its key file (0600).
/// Returns the identity so the caller can print the public key + fingerprint
/// for gateway registration.
pub fn keygen(out: &Path) -> Result<InstanceSigningIdentity, PeerCliError> {
    let identity = InstanceSigningIdentity::generate(InstanceId(Uuid::new_v4()));
    let file = DeviceKeyFile {
        instance_id: identity.instance_id.0,
        secret_hex: hex_encode(&identity.secret_bytes()),
    };
    let json = serde_json::to_vec_pretty(&file)?;
    // Write-then-rename so a crash never leaves a partial secret file.
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, &json).map_err(io_err(&tmp))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(io_err(&tmp))?;
    }
    std::fs::rename(&tmp, out).map_err(io_err(out))?;
    Ok(identity)
}

/// Load a device identity from its key file.
pub fn load_identity(path: &Path) -> Result<InstanceSigningIdentity, PeerCliError> {
    let bytes = std::fs::read(path).map_err(io_err(path))?;
    let file: DeviceKeyFile = serde_json::from_slice(&bytes)?;
    let secret = parse_hex32(&file.secret_hex).ok_or_else(|| {
        PeerCliError::KeyFile(path.to_path_buf(), "secret must be 64 hex chars".into())
    })?;
    Ok(InstanceSigningIdentity::from_secret_bytes(
        InstanceId(file.instance_id),
        secret,
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    // The length check above guarantees 32 whole pairs and an empty remainder;
    // `as_chunks` yields `&[u8; 2]`, so the pair length is known at compile time.
    let (pairs, _remainder) = hex.as_bytes().as_chunks::<2>();
    for (i, chunk) in pairs.iter().enumerate() {
        let hi = hex_digit(*chunk.first()?)?;
        let lo = hex_digit(*chunk.get(1)?)?;
        *out.get_mut(i)? = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory apply store
// ---------------------------------------------------------------------------

/// Versions ledger: pack id → last applied version.
#[derive(Debug, Default, Serialize, Deserialize)]
struct VersionsFile {
    applied: std::collections::HashMap<Uuid, u64>,
}

/// Durable, file-backed [`PackApplyStore`] for the CLI receive path.
///
/// Stage-then-flip maps to write-tmp-then-rename: a crash mid-apply leaves a
/// `staged-*.json` (never a half-`applied-*`), and re-running the receive is
/// idempotent by pack version.
pub struct DirPackApplyStore {
    dir: PathBuf,
}

impl DirPackApplyStore {
    /// Open (creating if needed) the landing directory.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, PeerCliError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        Ok(Self { dir })
    }

    fn versions_path(&self) -> PathBuf {
        self.dir.join("versions.json")
    }

    fn read_versions(&self) -> Result<VersionsFile, PeerCliError> {
        let path = self.versions_path();
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VersionsFile::default()),
            Err(e) => Err(PeerCliError::Io { path, source: e }),
        }
    }

    fn write_versions(&self, versions: &VersionsFile) -> Result<(), PeerCliError> {
        let path = self.versions_path();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(versions)?).map_err(io_err(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io_err(&path))?;
        Ok(())
    }

    fn staged_path(&self, pack_id: Uuid) -> PathBuf {
        self.dir.join(format!("staged-{pack_id}.json"))
    }

    fn applied_path(&self, pack_id: Uuid, version: u64) -> PathBuf {
        self.dir.join(format!("applied-{pack_id}-v{version}.json"))
    }
}

impl PackApplyStore for DirPackApplyStore {
    async fn last_applied_version(&self, pack_id: Uuid) -> anyhow::Result<Option<u64>> {
        Ok(self.read_versions()?.applied.get(&pack_id).copied())
    }

    async fn stage(&self, staged: &StagedPack) -> anyhow::Result<()> {
        let path = self.staged_path(staged.pack_id);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(staged)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    async fn flip(&self, staged: &StagedPack) -> anyhow::Result<()> {
        let from = self.staged_path(staged.pack_id);
        let to = self.applied_path(staged.pack_id, staged.pack_version);
        std::fs::rename(&from, &to)?;
        let mut versions = self.read_versions()?;
        versions.applied.insert(staged.pack_id, staged.pack_version);
        self.write_versions(&versions)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// `parse_hex32` decodes every byte value, and rejects malformed input.
    ///
    /// Cover for the `as_chunks::<2>()` rewrite: the decoder indexes into
    /// fixed-size pairs now, so a mistake here would silently mis-decode key
    /// material rather than fail to compile.
    #[test]
    fn parse_hex32_roundtrips_every_byte_value_and_rejects_garbage() {
        let mut key = [0u8; 32];
        for (i, slot) in key.iter_mut().enumerate() {
            // 0, 8, 16 ... 248 — spans the low and high nibble ranges.
            *slot = (i as u8).wrapping_mul(8);
        }
        let encoded = hex_encode(&key);
        assert_eq!(encoded.len(), 64);
        assert_eq!(parse_hex32(&encoded), Some(key));

        assert_eq!(parse_hex32(""), None, "empty input");
        assert_eq!(parse_hex32(&encoded[..62]), None, "too short");
        assert_eq!(parse_hex32(&format!("{encoded}00")), None, "too long");
        assert_eq!(parse_hex32(&"zz".repeat(32)), None, "non-hex digits");
    }

    #[test]
    fn keygen_roundtrips_through_the_key_file() {
        let dir = std::env::temp_dir().join(format!("t36-keygen-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("device.key.json");
        let generated = keygen(&path).expect("keygen");
        let loaded = load_identity(&path).expect("load");
        assert_eq!(loaded.instance_id, generated.instance_id);
        assert_eq!(
            loaded.public_identity().public_key_fingerprint,
            generated.public_identity().public_key_fingerprint
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be 0600");
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn dir_store_stage_flip_and_replay_ledger() {
        let dir = std::env::temp_dir().join(format!("t36-dirstore-{}", Uuid::new_v4()));
        let store = DirPackApplyStore::open(&dir).expect("open");
        let staged = StagedPack {
            pack_id: Uuid::new_v4(),
            pack_version: 3,
            entities: vec![],
            folds: vec![],
            temporal: vec![],
            edges: vec![],
            provenance: vec![],
            ttl_expires_at: Some(Utc::now()),
            remote_id: Uuid::new_v4(),
        };
        assert_eq!(
            store.last_applied_version(staged.pack_id).await.expect("v"),
            None
        );
        store.stage(&staged).await.expect("stage");
        assert!(dir.join(format!("staged-{}.json", staged.pack_id)).exists());
        store.flip(&staged).await.expect("flip");
        assert!(
            dir.join(format!("applied-{}-v3.json", staged.pack_id))
                .exists()
        );
        assert!(!dir.join(format!("staged-{}.json", staged.pack_id)).exists());
        assert_eq!(
            store.last_applied_version(staged.pack_id).await.expect("v"),
            Some(3)
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
