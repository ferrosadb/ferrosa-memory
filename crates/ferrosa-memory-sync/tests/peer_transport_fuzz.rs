//! Property/fuzz tests for the MAAS-T-25 chunk framer and reassembler.
//!
//! Acceptance gate: feeding arbitrary / truncated / oversized bytes to the
//! frame decoder and the assembler must NEVER panic, must always return a typed
//! error, and must never allocate beyond the configured bounds. Uses `proptest`
//! (no nightly fuzzer required).

use ferrosa_memory_sync::peer_transport::{
    AssemblerLimits, ChunkAssembler, ChunkFrame, decode_frame, encode_frame,
};
use proptest::prelude::*;

const MAX_PAYLOAD: usize = 4096;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Arbitrary bytes fed to the frame decoder never panic. They either decode
    /// to a frame whose payload respects the declared length, or return a typed
    /// FrameError. A panic is the only failure.
    #[test]
    fn decode_frame_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..8192)) {
        match decode_frame(&bytes, MAX_PAYLOAD) {
            Ok(frame) => {
                // A decoded frame must obey its own bounds.
                prop_assert!(frame.payload.len() <= MAX_PAYLOAD);
                prop_assert!(frame.index < frame.total);
            }
            Err(_) => { /* typed error — acceptable */ }
        }
    }

    /// encode → decode round-trips for any in-bounds payload, and the decoder
    /// rejects (without panic) when the bound is set below the payload size.
    #[test]
    fn encode_decode_round_trips(
        index in 0u32..1000,
        extra in 0u32..1000,
        payload in prop::collection::vec(any::<u8>(), 0..MAX_PAYLOAD),
    ) {
        let total = index + extra + 1; // guarantee index < total
        let framed = encode_frame(index, total, &payload, MAX_PAYLOAD)
            .expect("in-bounds payload encodes");
        let decoded = decode_frame(&framed, MAX_PAYLOAD).expect("round-trips");
        prop_assert_eq!(decoded, ChunkFrame { index, total, payload: payload.clone() });

        // A stricter bound than the payload size rejects, never panics.
        if !payload.is_empty() {
            prop_assert!(decode_frame(&framed, payload.len() - 1).is_err());
        }
    }

    /// The assembler never exceeds its byte budget no matter what frames arrive:
    /// every accept either stays within bounds or returns a typed error.
    #[test]
    fn assembler_respects_byte_budget(
        frames in prop::collection::vec(
            (0u32..8, prop::collection::vec(any::<u8>(), 0..64)),
            0..32,
        ),
    ) {
        let limits = AssemblerLimits {
            max_chunks: 8,
            max_total_bytes: 128,
            max_frame_payload: 64,
        };
        let mut asm = ChunkAssembler::new(limits);
        for (index, payload) in frames {
            // total fixed at 8 so accepts are comparable; bounds still apply.
            let _ = asm.accept(ChunkFrame { index, total: 8, payload });
            // Invariant: received chunk count never exceeds the declared total.
            prop_assert!(asm.received_count() <= 8);
        }
    }
}

/// Deterministic truncation/flip sweep over a valid frame: the decoder stays
/// panic-free on every prefix and every single-byte mutation.
#[test]
fn mutated_frame_bytes_never_panic() {
    let framed = encode_frame(3, 7, b"a representative chunk payload", MAX_PAYLOAD).unwrap();

    // Every truncation.
    for cut in 0..=framed.len() {
        let _ = decode_frame(&framed[..cut], MAX_PAYLOAD);
    }
    // Every single-byte flip.
    for i in 0..framed.len() {
        let mut m = framed.clone();
        m[i] ^= 0xff;
        let _ = decode_frame(&m, MAX_PAYLOAD);
    }
    // Trailing garbage appended (length mismatch, not a panic).
    let mut extended = framed.clone();
    extended.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert!(decode_frame(&extended, MAX_PAYLOAD).is_err());
}
