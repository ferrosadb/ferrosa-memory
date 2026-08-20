//! Module: Remote memory instance identity and signed payload envelopes.
//! Correctness: Correct when content hashes are deterministic, signatures verify only
//! for the original payload, and no private key material is serialized by envelope types.
//! Last revised: 2026-05-12
//! Last changed: Added initial Ed25519 signing primitives for remote teaching packets.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use uuid::Uuid;

/// Stable identifier for a ferrosa-memory instance participating as teacher or learner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(pub Uuid);

impl InstanceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Hex SHA-256 digest over canonical JSON bytes or explicit byte payloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn sha256_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hex_encode(&hasher.finalize()))
    }

    pub fn sha256_json<T: Serialize>(payload: &T) -> Result<Self, RemoteIdentityError> {
        let value = serde_json::to_value(payload)?;
        let canonical = canonicalize_json(value);
        let bytes = serde_json::to_vec(&canonical)?;
        Ok(Self::sha256_bytes(&bytes))
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable lowercase SHA-256 fingerprint for a device public signing key.
///
/// The full 64-character digest deliberately matches the gateway device-key
/// registry. Truncating it would make a locally generated identity impossible
/// to vouch through the live signaling service.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicKeyFingerprint(pub String);

impl PublicKeyFingerprint {
    pub fn from_verifying_key(key: &VerifyingKey) -> Self {
        let digest = ContentHash::sha256_bytes(key.as_bytes());
        Self(digest.0)
    }
}

/// Ed25519 signature bytes. Serialized as an integer byte array, never as key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SignatureBytes(pub Vec<u8>);

/// Public signing identity for a ferrosa-memory instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstancePublicIdentity {
    pub instance_id: InstanceId,
    pub public_key: Vec<u8>,
    pub public_key_fingerprint: PublicKeyFingerprint,
}

impl InstancePublicIdentity {
    pub fn verifying_key(&self) -> Result<VerifyingKey, RemoteIdentityError> {
        let bytes: [u8; 32] = self
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| RemoteIdentityError::InvalidPublicKeyLength(self.public_key.len()))?;
        VerifyingKey::from_bytes(&bytes).map_err(RemoteIdentityError::PublicKey)
    }
}

/// In-memory signing identity. This type intentionally does not implement Serialize.
pub struct InstanceSigningIdentity {
    pub instance_id: InstanceId,
    signing_key: SigningKey,
}

impl InstanceSigningIdentity {
    pub fn generate(instance_id: InstanceId) -> Self {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        let signing_key = SigningKey::from_bytes(&secret);
        Self {
            instance_id,
            signing_key,
        }
    }

    /// Rebuild an identity from its 32-byte Ed25519 secret (MAAS-T-36 device
    /// key files). The caller owns secret-material hygiene; this type still
    /// never serializes the secret itself.
    pub fn from_secret_bytes(instance_id: InstanceId, secret: [u8; 32]) -> Self {
        Self {
            instance_id,
            signing_key: SigningKey::from_bytes(&secret),
        }
    }

    /// Export the 32-byte Ed25519 secret (for writing a device key file at
    /// keygen time ONLY — never log or transmit this).
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn public_identity(&self) -> InstancePublicIdentity {
        let verifying_key = self.signing_key.verifying_key();
        InstancePublicIdentity {
            instance_id: self.instance_id,
            public_key: verifying_key.as_bytes().to_vec(),
            public_key_fingerprint: PublicKeyFingerprint::from_verifying_key(&verifying_key),
        }
    }

    pub fn sign<T>(&self, payload: T) -> Result<SignedEnvelope<T>, RemoteIdentityError>
    where
        T: Serialize,
    {
        let canonical_payload = canonical_payload_bytes(&payload)?;
        let content_hash = ContentHash::sha256_bytes(&canonical_payload);
        let signature = self.signing_key.sign(&canonical_payload);
        let public_identity = self.public_identity();

        Ok(SignedEnvelope {
            payload,
            content_hash,
            signer: self.instance_id,
            public_key_fingerprint: public_identity.public_key_fingerprint,
            signature: SignatureBytes(signature.to_bytes().to_vec()),
        })
    }

    /// Sign an externally specified byte contract without serializing it.
    ///
    /// Enrollment approval uses a gateway-owned, versioned UTF-8 message
    /// rather than the canonical-JSON envelope used by memory payloads.
    pub fn sign_bytes(&self, payload: &[u8]) -> SignatureBytes {
        SignatureBytes(self.signing_key.sign(payload).to_bytes().to_vec())
    }
}

/// Signed payload envelope used for teacher packets and learner import decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEnvelope<T> {
    pub payload: T,
    pub content_hash: ContentHash,
    pub signer: InstanceId,
    pub public_key_fingerprint: PublicKeyFingerprint,
    pub signature: SignatureBytes,
}

impl<T> SignedEnvelope<T>
where
    T: Serialize,
{
    pub fn verify(
        &self,
        public_identity: &InstancePublicIdentity,
    ) -> Result<(), RemoteIdentityError> {
        if self.signer != public_identity.instance_id {
            return Err(RemoteIdentityError::SignerMismatch {
                envelope_signer: self.signer,
                public_identity: public_identity.instance_id,
            });
        }

        if self.public_key_fingerprint != public_identity.public_key_fingerprint {
            return Err(RemoteIdentityError::FingerprintMismatch);
        }

        let canonical_payload = canonical_payload_bytes(&self.payload)?;
        let actual_hash = ContentHash::sha256_bytes(&canonical_payload);
        if actual_hash != self.content_hash {
            return Err(RemoteIdentityError::ContentHashMismatch {
                expected: self.content_hash.clone(),
                actual: actual_hash,
            });
        }

        let signature = Signature::try_from(self.signature.0.as_slice())
            .map_err(|_| RemoteIdentityError::InvalidSignatureLength(self.signature.0.len()))?;
        public_identity
            .verifying_key()?
            .verify(&canonical_payload, &signature)
            .map_err(RemoteIdentityError::Signature)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteIdentityError {
    #[error("failed to serialize signed payload: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid public key length {0}; expected 32 bytes")]
    InvalidPublicKeyLength(usize),
    #[error("invalid Ed25519 public key: {0}")]
    PublicKey(ed25519_dalek::SignatureError),
    #[error("invalid signature length {0}; expected 64 bytes")]
    InvalidSignatureLength(usize),
    #[error("signature verification failed: {0}")]
    Signature(ed25519_dalek::SignatureError),
    #[error("envelope signer {envelope_signer} does not match public identity {public_identity}")]
    SignerMismatch {
        envelope_signer: InstanceId,
        public_identity: InstanceId,
    },
    #[error("public key fingerprint does not match envelope")]
    FingerprintMismatch,
    #[error("content hash mismatch: expected {expected}, actual {actual}")]
    ContentHashMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
}

fn canonical_payload_bytes<T: Serialize>(payload: &T) -> Result<Vec<u8>, RemoteIdentityError> {
    let value = serde_json::to_value(payload)?;
    let canonical = canonicalize_json(value);
    Ok(serde_json::to_vec(&canonical)?)
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonicalize_json(value));
            }
            Value::Object(sorted)
        }
        scalar => scalar,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Verify an envelope after deserializing it from JSON.
pub fn verify_json_envelope<T>(
    json: &[u8],
    public_identity: &InstancePublicIdentity,
) -> Result<SignedEnvelope<T>, RemoteIdentityError>
where
    T: Serialize + DeserializeOwned,
{
    let envelope: SignedEnvelope<T> = serde_json::from_slice(json)?;
    envelope.verify(public_identity)?;
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_byte_signature_verifies_against_public_identity() {
        let identity = InstanceSigningIdentity::generate(InstanceId::new());
        let message = b"maas-device-approval:v1:account:fingerprint";
        let signature = identity.sign_bytes(message);
        let public = identity.public_identity();
        let verifying_key = VerifyingKey::from_bytes(
            public
                .public_key
                .as_slice()
                .try_into()
                .expect("32-byte key"),
        )
        .expect("valid key");
        let signature = Signature::from_slice(&signature.0).expect("64-byte signature");
        assert!(verifying_key.verify(message, &signature).is_ok());
    }

    #[test]
    fn instance_id_serializes_and_deserializes() {
        let id = InstanceId::new();
        let encoded = serde_json::to_string(&id).unwrap();
        let decoded: InstanceId = serde_json::from_str(&encoded).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn content_hash_is_stable_for_same_json_payload() {
        let left = json!({"b": [2, {"d": 4, "c": 3}], "a": 1});
        let right = json!({"a": 1, "b": [2, {"c": 3, "d": 4}]});

        assert_eq!(
            ContentHash::sha256_json(&left).unwrap(),
            ContentHash::sha256_json(&right).unwrap()
        );
    }

    #[test]
    fn device_fingerprint_matches_gateway_registry_shape() {
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let public = signer.public_identity();
        let expected = ContentHash::sha256_bytes(&public.public_key);

        assert_eq!(public.public_key_fingerprint.0.len(), 64);
        assert_eq!(public.public_key_fingerprint.0, expected.0);
    }

    #[test]
    fn signature_verification_fails_after_payload_mutation() {
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let public_identity = signer.public_identity();
        let mut envelope = signer
            .sign(json!({"kind": "teaching_packet", "query": "CUDA OOM"}))
            .unwrap();

        envelope.verify(&public_identity).unwrap();
        envelope.payload = json!({"kind": "teaching_packet", "query": "mutated"});

        let err = envelope.verify(&public_identity).unwrap_err();
        assert!(matches!(
            err,
            RemoteIdentityError::ContentHashMismatch { .. }
        ));
    }

    #[test]
    fn signature_verification_fails_with_wrong_identity() {
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let wrong_signer = InstanceSigningIdentity::generate(InstanceId::new());
        let envelope = signer.sign(json!({"packet_id": "p1"})).unwrap();

        let err = envelope
            .verify(&wrong_signer.public_identity())
            .unwrap_err();
        assert!(matches!(err, RemoteIdentityError::SignerMismatch { .. }));
    }
}
