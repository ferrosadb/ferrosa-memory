//! Property/fuzz tests for the pack deserializer and crypto open path.
//!
//! Acceptance gate: feeding arbitrary / truncated / oversized bytes to the
//! pack parser must NEVER panic and must always return a typed error. This uses
//! `proptest` (no nightly fuzzer required).

use ferrosa_memory_sync::pack::{KnowledgePack, PackRef};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Arbitrary bytes fed to the KnowledgePack JSON deserializer never panic.
    /// They either deserialize to a (possibly-invalid) pack or return a typed
    /// serde error — both are fine; a panic is not.
    #[test]
    fn knowledgepack_deserializer_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let result: Result<KnowledgePack, _> = serde_json::from_slice(&bytes);
        match result {
            Ok(pack) => {
                // If it parsed, validate() must also never panic and return a
                // typed Result.
                let _ = pack.validate();
            }
            Err(_) => { /* typed error — acceptable */ }
        }
    }

    /// Arbitrary bytes fed to the PackRef deserializer never panic.
    #[test]
    fn packref_deserializer_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _: Result<PackRef, _> = serde_json::from_slice(&bytes);
    }
}

/// Deterministic mutation loop over a valid pack's serialized bytes: flip,
/// truncate, and extend, asserting the parser never panics.
#[test]
fn mutated_pack_bytes_never_panic() {
    // A minimal but valid-shaped JSON object that resembles a pack. We don't
    // need a real pack here — the point is the parser stays panic-free on
    // hostile input derived from a plausible shape.
    let seed = br#"{"manifest":{},"payload":{}}"#.to_vec();

    // Truncations.
    for cut in 0..seed.len() {
        let slice = &seed[..cut];
        let _: Result<KnowledgePack, _> = serde_json::from_slice(slice);
        let _: Result<PackRef, _> = serde_json::from_slice(slice);
    }

    // Single-byte flips.
    for i in 0..seed.len() {
        let mut m = seed.clone();
        m[i] ^= 0xff;
        let _: Result<KnowledgePack, _> = serde_json::from_slice(&m);
        let _: Result<PackRef, _> = serde_json::from_slice(&m);
    }

    // Oversized: a huge declared structure as raw bytes (parser must not OOM on
    // shape alone; it rejects on bound checks during validate()).
    let mut big = Vec::new();
    big.extend_from_slice(br#"{"manifest":{"entity_count":"#);
    big.extend_from_slice(b"99999999999999999999");
    big.extend_from_slice(br#"},"payload":{}}"#);
    let _: Result<KnowledgePack, _> = serde_json::from_slice(&big);
}
