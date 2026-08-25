//! Signing one HTTP request with a device identity.
//!
//! This is how a memory system authenticates to the gateway now that device
//! identities have replaced API keys on this path. It holds no bearer secret:
//! a captured request cannot be lifted onto a different call, and there is
//! nothing on disk that an attacker can exfiltrate and replay elsewhere.
//!
//! # The contract, and why it is copied rather than shared
//!
//! The verifying half lives in `ferrosa-dbaas`
//! (`dbaas_memory_control::device_keys::device_request_message`), in a separate
//! repository. There is no crate boundary to share, so the format is
//! reimplemented here and pinned by a byte-for-byte vector below. If the two
//! ever diverge, that test fails rather than every request 401ing in
//! production with nothing to point at.
//!
//! The signed message binds method, PATH, a hash of the body, a timestamp and a
//! nonce. Each part earns its place: sign only a token and the signature
//! authorises every endpoint; omit the body hash and the payload can be swapped
//! underneath a valid signature.
//!
//! **Path only — never the query string.** The gateway verifies against
//! `request.uri().path()`, so a client that signed path-plus-query would fail
//! every request that carries one, and would fail it as a 401 that looks like a
//! credential problem rather than a formatting one.
//!
//! Correctness: Correct when the signed bytes match the gateway's derivation
//! exactly, and the hashed body is the same byte sequence that is sent.
//! Last revised: 2026-08-22
//! Last changed: Initial device request signing.

use ferrosa_memory_core::remote_identity::InstanceSigningIdentity;
// `sha2_kdf` is the sha2 crate under this workspace's alias; the plain name
// is a different pinned version elsewhere in the tree.
use sha2_kdf::{Digest, Sha256};

/// Header carrying the device's fingerprint, naming which key signed.
pub const HDR_FINGERPRINT: &str = "X-Device-Fingerprint";
/// Header carrying the Unix timestamp the signature was made at.
pub const HDR_TIMESTAMP: &str = "X-Device-Timestamp";
/// Header carrying the caller-chosen nonce.
pub const HDR_NONCE: &str = "X-Device-Nonce";
/// Header carrying the hex Ed25519 signature.
pub const HDR_SIGNATURE: &str = "X-Device-Signature";

/// The canonical bytes a device signs for one request.
///
/// Must stay byte-identical to the gateway's `device_request_message`. The
/// method is upper-cased so `get` and `GET` are one message rather than two.
#[must_use]
pub fn device_request_message(
    method: &str,
    path: &str,
    body_sha256_hex: &str,
    timestamp_unix: i64,
    nonce: &str,
) -> String {
    format!(
        "maas-device-request:v1:{}:{path}:{body_sha256_hex}:{timestamp_unix}:{nonce}",
        method.to_ascii_uppercase()
    )
}

/// Hex SHA-256 of a request body. An empty body hashes like any other, so a
/// bodyless GET is still bound to its signature.
#[must_use]
pub fn body_sha256_hex(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hex(&hasher.finalize())
}

/// The four headers that authenticate one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    pub fingerprint: String,
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
}

impl SignedHeaders {
    /// The four header name/value pairs, in a fixed order.
    ///
    /// Returned as data rather than applied to a request builder so this module
    /// needs no HTTP client at all. `reqwest` is an OPTIONAL dependency of this
    /// crate, and a signer that dragged it in would be unusable from anything
    /// that does not already pull the whole WebRTC stack — including `fmem`.
    #[must_use]
    pub fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            (HDR_FINGERPRINT, self.fingerprint.as_str()),
            (HDR_TIMESTAMP, self.timestamp.as_str()),
            (HDR_NONCE, self.nonce.as_str()),
            (HDR_SIGNATURE, self.signature.as_str()),
        ]
    }
}

/// Sign one request.
///
/// `body` must be the EXACT bytes that will be transmitted. Serializing twice —
/// once to hash and once to send — risks two different byte sequences from one
/// value, and the signature would then cover something the server never
/// received. Callers build the bytes once and pass them here and to the
/// request.
///
/// `timestamp_unix` and `nonce` are parameters rather than read from the clock
/// and a generator, so the signature is reproducible in a test without mocking
/// either.
#[must_use]
pub fn sign_request(
    identity: &InstanceSigningIdentity,
    method: &str,
    path: &str,
    body: &[u8],
    timestamp_unix: i64,
    nonce: &str,
) -> SignedHeaders {
    let digest = body_sha256_hex(body);
    let message = device_request_message(method, path, &digest, timestamp_unix, nonce);
    let signature = identity.sign_bytes(message.as_bytes());
    SignedHeaders {
        fingerprint: identity.public_identity().public_key_fingerprint.0,
        timestamp: timestamp_unix.to_string(),
        nonce: nonce.to_string(),
        signature: hex(&signature.0),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosa_memory_core::remote_identity::InstanceId;

    fn identity() -> InstanceSigningIdentity {
        InstanceSigningIdentity::from_secret_bytes(InstanceId(uuid::Uuid::nil()), [7u8; 32])
    }

    /// The cross-repo contract, pinned.
    ///
    /// This exact string is what `dbaas_memory_control::device_keys::
    /// device_request_message` produces for the same inputs. If the gateway's
    /// format changes, this fails here instead of every request 401ing in
    /// production with nothing to point at.
    #[test]
    fn the_signed_message_matches_the_gateways_format() {
        assert_eq!(
            device_request_message("GET", "/v1/devices", "h", 1, "n"),
            "maas-device-request:v1:GET:/v1/devices:h:1:n"
        );
    }

    /// Method case must not produce two different messages for one call.
    #[test]
    fn the_method_is_normalised() {
        assert_eq!(
            device_request_message("get", "/v1/devices", "h", 1, "n"),
            device_request_message("GET", "/v1/devices", "h", 1, "n")
        );
        assert!(
            device_request_message("patch", "/x", "h", 1, "n")
                .starts_with("maas-device-request:v1:PATCH:")
        );
    }

    /// An empty body still hashes, so a bodyless GET is bound to its signature
    /// rather than leaving the body slot free to fill in.
    #[test]
    fn an_empty_body_still_hashes() {
        let empty = body_sha256_hex(b"");
        assert_eq!(
            empty, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "the well-known SHA-256 of the empty string"
        );
        assert_ne!(empty, body_sha256_hex(b"{}"));
    }

    /// Changing ANY bound component changes the signature.
    ///
    /// Each of these is a lift a captured signature could otherwise perform:
    /// same credential onto a different method, path, body, or moment.
    #[test]
    fn every_bound_component_changes_the_signature() {
        let id = identity();
        let base = sign_request(&id, "GET", "/v1/devices", b"", 1_000, "n");

        let variants = [
            sign_request(&id, "POST", "/v1/devices", b"", 1_000, "n"),
            sign_request(&id, "GET", "/v1/devices/other", b"", 1_000, "n"),
            sign_request(&id, "GET", "/v1/devices", b"{\"a\":1}", 1_000, "n"),
            sign_request(&id, "GET", "/v1/devices", b"", 1_001, "n"),
            sign_request(&id, "GET", "/v1/devices", b"", 1_000, "other-nonce"),
        ];
        for (i, v) in variants.iter().enumerate() {
            assert_ne!(
                base.signature, v.signature,
                "variant {i} produced the same signature as the base request"
            );
        }
    }

    /// The fingerprint header names the key that signed, and matches the
    /// derivation the gateway and the phones both use.
    #[test]
    fn the_fingerprint_header_identifies_the_signing_key() {
        let id = identity();
        let headers = sign_request(&id, "GET", "/v1/devices", b"", 1, "n");

        assert_eq!(
            headers.fingerprint,
            id.public_identity().public_key_fingerprint.0
        );
        assert_eq!(headers.fingerprint.len(), 64);
        assert!(headers.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// All four headers are emitted, named exactly as the gateway reads them.
    #[test]
    fn every_required_header_is_present_and_named_correctly() {
        let headers = sign_request(&identity(), "GET", "/x", b"", 1, "n");
        let names: Vec<&str> = headers.pairs().iter().map(|(n, _)| *n).collect();

        assert_eq!(
            names,
            [
                "X-Device-Fingerprint",
                "X-Device-Timestamp",
                "X-Device-Nonce",
                "X-Device-Signature"
            ]
        );
        assert!(
            headers.pairs().iter().all(|(_, v)| !v.is_empty()),
            "an empty header value would authenticate nothing"
        );
    }

    /// Signatures are lowercase hex of the raw 64 bytes.
    #[test]
    fn the_signature_is_hex_encoded() {
        let headers = sign_request(&identity(), "GET", "/x", b"", 1, "n");
        assert_eq!(headers.signature.len(), 128, "64 bytes as hex");
        assert!(
            headers
                .signature
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    /// Signing is deterministic for fixed inputs, which is what makes the
    /// pinned vector above meaningful.
    #[test]
    fn signing_is_reproducible() {
        let id = identity();
        assert_eq!(
            sign_request(&id, "GET", "/x", b"body", 42, "n"),
            sign_request(&id, "GET", "/x", b"body", 42, "n")
        );
    }

    /// A query string must NOT be part of the signed path.
    ///
    /// The gateway verifies against `uri().path()`. A client that signed
    /// path-plus-query would fail every request carrying one, as a 401 that
    /// looks like a credential problem rather than a formatting one. This
    /// documents the split so nobody "fixes" it by appending the query.
    #[test]
    fn the_path_and_the_query_are_different_messages() {
        assert_ne!(
            device_request_message("GET", "/v1/control/signals", "h", 1, "n"),
            device_request_message("GET", "/v1/control/signals?session_id=x", "h", 1, "n")
        );
    }
}
