//! What a memory system records about being enrolled.
//!
//! Kept BESIDE the device key rather than inside it, so the key file keeps the
//! exact shape `memory-sync p2p-keygen` writes and `peer_cli::load_identity`
//! reads. `control-listen` and the peer commands go on working against the same
//! file; widening that struct would fork the format for no gain.
//!
//! This file holds no secret. The key next to it does.
//!
//! Correctness: Correct when a recorded enrolment matches the key it sits
//! beside, and a mismatch is refused rather than reported as enrolled.
//! Last revised: 2026-08-22
//! Last changed: Initial enrolment record.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The non-secret record of an enrolment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrolment {
    /// Schema marker, so a future format change is detectable rather than
    /// silently misread.
    pub contract: String,
    /// MCP port of the memory system this belongs to.
    pub system_port: u16,
    /// Device id the gateway assigned.
    pub device_id: String,
    /// Fingerprint the gateway derived. Checked against the local key on read.
    pub fingerprint: String,
    /// What it enrolled as. Immutable server-side (trust D15).
    pub kind: String,
    /// Label it enrolled under.
    pub label: String,
    /// Account it bound to, for display.
    pub email: Option<String>,
    /// Console origin it enrolled against, so an operator can see which
    /// control plane a system belongs to without guessing.
    pub console_url: String,
}

/// The current schema marker.
pub const CONTRACT: &str = "ferrosa-memory.enrolment.v1";

#[derive(Debug, thiserror::Error)]
pub enum EnrolmentError {
    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a readable enrolment record: {reason}")]
    Malformed { path: PathBuf, reason: String },
    #[error(
        "{path} records fingerprint {recorded} but the key beside it is {actual}. \
         The record and the key have diverged; delete the record and enrol again."
    )]
    Mismatch {
        path: PathBuf,
        recorded: String,
        actual: String,
    },
}

/// Write the record, atomically.
///
/// Write-then-rename for the same reason the key file uses it: a crash
/// mid-write must not leave a half-parsed record that reads as a corrupt
/// enrolment for a device that is actually fine.
pub fn save(path: &Path, record: &Enrolment) -> Result<(), EnrolmentError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| EnrolmentError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json = serde_json::to_vec_pretty(record).map_err(|e| EnrolmentError::Malformed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &json).map_err(|source| EnrolmentError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| EnrolmentError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Read the record, verifying it belongs to the key beside it.
///
/// `actual_fingerprint` is derived from the key file this record sits next to.
/// A mismatch means the key was regenerated without the record being cleared,
/// which would otherwise report a system as enrolled under an identity it can
/// no longer sign for — the failure would surface much later as a gateway
/// refusing every request.
pub fn load(path: &Path, actual_fingerprint: &str) -> Result<Option<Enrolment>, EnrolmentError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(EnrolmentError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let record: Enrolment =
        serde_json::from_slice(&bytes).map_err(|e| EnrolmentError::Malformed {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    if record.contract != CONTRACT {
        return Err(EnrolmentError::Malformed {
            path: path.to_path_buf(),
            reason: format!(
                "unknown contract {:?}, expected {CONTRACT}",
                record.contract
            ),
        });
    }
    if !record.fingerprint.eq_ignore_ascii_case(actual_fingerprint) {
        return Err(EnrolmentError::Mismatch {
            path: path.to_path_buf(),
            recorded: record.fingerprint,
            actual: actual_fingerprint.to_string(),
        });
    }
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fingerprint: &str) -> Enrolment {
        Enrolment {
            contract: CONTRACT.to_string(),
            system_port: 43971,
            device_id: "11111111-1111-4111-8111-111111111111".to_string(),
            fingerprint: fingerprint.to_string(),
            kind: "memory".to_string(),
            label: "studio:43971".to_string(),
            email: Some("ben@example.com".to_string()),
            console_url: "https://example.test".to_string(),
        }
    }

    #[test]
    fn a_record_round_trips() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("devices").join("43971.enrolment.json");
        let written = record("aa");

        save(&path, &written).expect("save");
        let read = load(&path, "aa").expect("load").expect("present");
        assert_eq!(read, written);
    }

    /// Absent is a normal answer, not an error: a system that has never
    /// enrolled is the starting state.
    #[test]
    fn a_missing_record_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let got = load(&tmp.path().join("nope.json"), "aa").expect("no error");
        assert!(got.is_none());
    }

    /// The check that matters. A regenerated key with a stale record would
    /// report the system as enrolled under an identity it can no longer sign
    /// for, and the symptom would appear much later as the gateway refusing
    /// every request.
    #[test]
    fn a_record_that_does_not_match_its_key_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("43971.enrolment.json");
        save(&path, &record("aa")).expect("save");

        match load(&path, "bb") {
            Err(EnrolmentError::Mismatch {
                recorded, actual, ..
            }) => {
                assert_eq!(recorded, "aa");
                assert_eq!(actual, "bb");
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    /// Fingerprints are hex; case must not make a record look foreign.
    #[test]
    fn the_fingerprint_check_ignores_case() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("43971.enrolment.json");
        save(&path, &record("abcdef")).expect("save");

        assert!(load(&path, "ABCDEF").expect("load").is_some());
    }

    /// An unknown contract is refused, not best-effort parsed. A future format
    /// read as this one would produce a confidently wrong record.
    #[test]
    fn an_unknown_contract_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("43971.enrolment.json");
        let mut future = record("aa");
        future.contract = "ferrosa-memory.enrolment.v99".to_string();
        save(&path, &future).expect("save");

        match load(&path, "aa") {
            Err(EnrolmentError::Malformed { reason, .. }) => {
                assert!(reason.contains("v99"), "{reason}");
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    /// The error must tell the operator how to recover, because the fix
    /// (delete the record) is not guessable.
    #[test]
    fn the_mismatch_error_says_how_to_recover() {
        let err = EnrolmentError::Mismatch {
            path: PathBuf::from("/x/43971.enrolment.json"),
            recorded: "aa".into(),
            actual: "bb".into(),
        };
        let message = err.to_string();
        assert!(message.contains("delete the record"), "{message}");
        assert!(message.contains("enrol again"), "{message}");
    }

    /// Garbage on disk is named as such, with its path.
    #[test]
    fn a_corrupt_record_names_the_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("43971.enrolment.json");
        std::fs::write(&path, b"{not json").expect("write");

        match load(&path, "aa") {
            Err(EnrolmentError::Malformed { path: p, .. }) => assert_eq!(p, path),
            other => panic!("expected malformed, got {other:?}"),
        }
    }
}
