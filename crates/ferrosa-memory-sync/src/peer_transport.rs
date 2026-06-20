//! MAAS-T-25 — WebRTC peer transport: bounded, backpressured, cancellation-safe.
//!
//! This module holds the **security-critical transport logic** of the P2P pack
//! path, kept deliberately independent of any concrete WebRTC stack. It speaks
//! to a peer through the small [`DataChannel`] trait — the exact surface a
//! `webrtc-rs` `RTCDataChannel` implements (ready-state, `buffered_amount`,
//! `on_buffered_amount_low`, ordered reliable `send`). Wiring that concrete
//! channel is a thin follow-up (tracked separately); everything that can corrupt
//! memory or exhaust it lives here and is unit-testable without ICE/DTLS.
//!
//! # Requirements implemented
//!
//! - **MR-P2P-03** — a [`PeerTransport`] refuses to send until the channel has
//!   reached [`TransportState::Open`]. A send-before-open returns
//!   [`TransportError::SendBeforeOpen`] — it is **never** a silent drop.
//! - **MR-P2P-04** — in-flight is bounded. Per-frame payloads above
//!   [`SendLimits::max_frame_payload`] and chunk indices/totals above
//!   [`SendLimits::max_chunks`] are rejected before any send, and the receive
//!   side ([`ChunkAssembler`]) caps total reassembled bytes so an oversized peer
//!   yields a typed error, not OOM.
//! - **MR-P2P-05** — before each send the transport waits for the channel's
//!   buffered amount to fall below the high-water mark
//!   ([`DataChannel::wait_buffered_below`]), honoring SCTP backpressure.
//! - **MR-P2P-06** — cancellation-safe. Each chunk is one atomic
//!   [`DataChannel::send`] await; accounting advances **only after** that await
//!   returns `Ok`. Dropping a `send_chunk`/`send_pack` future mid-flight leaves
//!   no partial frame and no half-updated counters — the transfer is resumable.
//!
//! # Actor model
//!
//! [`PeerTransport`] is a single-owner state machine: every mutator takes
//! `&mut self`, so there is exactly one owning task and **no lock is ever held
//! across an `.await`** (enforced by `deny(clippy::await_holding_lock)`).

// Fail-loud on untrusted peer bytes: no panics, no unwrap/expect, no indexing,
// and never hold a lock across await. Mirrors `pack.rs` / `pack_crypto.rs`.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]
#![deny(clippy::await_holding_lock)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use std::collections::BTreeMap;
use std::future::Future;

// ─────────────────────────────────────────────────────────────────────────
// Wire framing — bounded, never-panicking encode/decode
// ─────────────────────────────────────────────────────────────────────────

/// Magic prefix identifying a Ferrosa MaaS pack frame (`"FMP1"`).
pub const FRAME_MAGIC: [u8; 4] = *b"FMP1";

/// Frame wire version.
pub const FRAME_VERSION: u8 = 1;

/// Header layout: magic(4) ‖ version(1) ‖ index(4) ‖ total(4) ‖ payload_len(4).
pub const FRAME_HEADER_LEN: usize = 4 + 1 + 4 + 4 + 4;

/// One decoded transport frame carrying a single sealed chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkFrame {
    /// Zero-based chunk index within the pack.
    pub index: u32,
    /// Total number of chunks the pack was framed into.
    pub total: u32,
    /// The sealed (ciphertext) chunk bytes. Opaque to the transport.
    pub payload: Vec<u8>,
}

/// Errors from framing/deframing peer bytes. Every variant is a typed rejection;
/// the decoder never panics on arbitrary, truncated, or oversized input.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame is shorter than the {FRAME_HEADER_LEN}-byte header")]
    TooShort,
    #[error("bad frame magic")]
    BadMagic,
    #[error("unsupported frame version {found}; this build speaks {supported}")]
    BadVersion { found: u8, supported: u8 },
    #[error("declared payload length {declared} exceeds maximum {max}")]
    PayloadTooLarge { declared: u64, max: usize },
    #[error(
        "frame length mismatch: header declares {declared} payload bytes, frame body has {actual}"
    )]
    LengthMismatch { declared: u64, actual: usize },
    #[error("chunk index {index} is not below declared total {total}")]
    IndexOutOfRange { index: u32, total: u32 },
}

/// Encode one chunk into a framed wire message.
///
/// Rejects payloads larger than `max_payload` and indices not below `total`
/// before allocating the frame (bounded — MR-P2P-04).
pub fn encode_frame(
    index: u32,
    total: u32,
    payload: &[u8],
    max_payload: usize,
) -> Result<Vec<u8>, FrameError> {
    if payload.len() > max_payload {
        return Err(FrameError::PayloadTooLarge {
            declared: payload.len() as u64,
            max: max_payload,
        });
    }
    if index >= total {
        return Err(FrameError::IndexOutOfRange { index, total });
    }
    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    buf.extend_from_slice(&FRAME_MAGIC);
    buf.push(FRAME_VERSION);
    buf.extend_from_slice(&index.to_be_bytes());
    buf.extend_from_slice(&total.to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Read a big-endian `u32` at `offset` without indexing or panicking.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(u32::from_be_bytes(arr))
}

/// Decode a framed wire message, bounding the declared payload by `max_payload`.
///
/// Never panics: malformed/truncated/oversized input yields a typed
/// [`FrameError`]. The declared length is validated against `max_payload`
/// **before** the body is read, so a hostile header cannot drive allocation.
pub fn decode_frame(bytes: &[u8], max_payload: usize) -> Result<ChunkFrame, FrameError> {
    if bytes.len() < FRAME_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes.get(0..4) != Some(&FRAME_MAGIC) {
        return Err(FrameError::BadMagic);
    }
    match bytes.get(4) {
        Some(&v) if v == FRAME_VERSION => {}
        Some(&found) => {
            return Err(FrameError::BadVersion {
                found,
                supported: FRAME_VERSION,
            });
        }
        None => return Err(FrameError::TooShort),
    }
    let index = read_u32(bytes, 5).ok_or(FrameError::TooShort)?;
    let total = read_u32(bytes, 9).ok_or(FrameError::TooShort)?;
    let declared = read_u32(bytes, 13).ok_or(FrameError::TooShort)? as u64;

    if declared > max_payload as u64 {
        return Err(FrameError::PayloadTooLarge {
            declared,
            max: max_payload,
        });
    }
    if index >= total {
        return Err(FrameError::IndexOutOfRange { index, total });
    }
    let body = bytes.get(FRAME_HEADER_LEN..).unwrap_or(&[]);
    if body.len() as u64 != declared {
        return Err(FrameError::LengthMismatch {
            declared,
            actual: body.len(),
        });
    }
    Ok(ChunkFrame {
        index,
        total,
        payload: body.to_vec(),
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Bounded reassembly — oversized peer ⇒ typed error, not OOM (MR-P2P-04)
// ─────────────────────────────────────────────────────────────────────────

/// Caps that bound a [`ChunkAssembler`]'s memory regardless of peer behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssemblerLimits {
    /// Maximum number of chunks the pack may declare.
    pub max_chunks: u32,
    /// Maximum total reassembled bytes across all chunks.
    pub max_total_bytes: usize,
    /// Maximum bytes in any single frame payload.
    pub max_frame_payload: usize,
}

/// Errors raised while reassembling frames into a pack body.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssembleError {
    #[error("declared chunk total {total} exceeds maximum {max}")]
    TotalTooLarge { total: u32, max: u32 },
    #[error("frame declares total {found} but assembler already fixed total {expected}")]
    InconsistentTotal { expected: u32, found: u32 },
    #[error("chunk index {index} is not below total {total}")]
    IndexOutOfRange { index: u32, total: u32 },
    #[error("duplicate chunk index {index}")]
    DuplicateChunk { index: u32 },
    #[error("frame payload {len} exceeds per-frame maximum {max}")]
    FramePayloadTooLarge { len: usize, max: usize },
    #[error("reassembled size would reach {would_be} bytes, exceeding maximum {max}")]
    TotalBytesExceeded { would_be: usize, max: usize },
    #[error("assembly incomplete: have {have} of {total} chunks")]
    Incomplete { have: usize, total: u32 },
}

/// Reassembles framed chunks into the original sealed-chunk byte stream with
/// hard memory bounds. A peer that declares or sends too much is rejected with a
/// typed error long before memory is exhausted.
#[derive(Debug)]
pub struct ChunkAssembler {
    total: Option<u32>,
    received: BTreeMap<u32, Vec<u8>>,
    bytes: usize,
    limits: AssemblerLimits,
}

impl ChunkAssembler {
    /// Create a new assembler bounded by `limits`.
    pub fn new(limits: AssemblerLimits) -> Self {
        Self {
            total: None,
            received: BTreeMap::new(),
            bytes: 0,
            limits,
        }
    }

    /// Accept one frame, enforcing all bounds. Idempotent rejection of
    /// duplicates and out-of-range indices; never allocates beyond the caps.
    pub fn accept(&mut self, frame: ChunkFrame) -> Result<(), AssembleError> {
        if frame.total > self.limits.max_chunks {
            return Err(AssembleError::TotalTooLarge {
                total: frame.total,
                max: self.limits.max_chunks,
            });
        }
        match self.total {
            None => self.total = Some(frame.total),
            Some(t) if t != frame.total => {
                return Err(AssembleError::InconsistentTotal {
                    expected: t,
                    found: frame.total,
                });
            }
            Some(_) => {}
        }
        if frame.index >= frame.total {
            return Err(AssembleError::IndexOutOfRange {
                index: frame.index,
                total: frame.total,
            });
        }
        if frame.payload.len() > self.limits.max_frame_payload {
            return Err(AssembleError::FramePayloadTooLarge {
                len: frame.payload.len(),
                max: self.limits.max_frame_payload,
            });
        }
        if self.received.contains_key(&frame.index) {
            return Err(AssembleError::DuplicateChunk { index: frame.index });
        }
        let would_be = self.bytes.saturating_add(frame.payload.len());
        if would_be > self.limits.max_total_bytes {
            return Err(AssembleError::TotalBytesExceeded {
                would_be,
                max: self.limits.max_total_bytes,
            });
        }
        self.bytes = would_be;
        self.received.insert(frame.index, frame.payload);
        Ok(())
    }

    /// Whether every declared chunk has been received.
    pub fn is_complete(&self) -> bool {
        matches!(self.total, Some(t) if self.received.len() == t as usize)
    }

    /// Number of distinct chunks accepted so far.
    pub fn received_count(&self) -> usize {
        self.received.len()
    }

    /// Concatenate accepted chunks in index order. Errors if incomplete.
    pub fn into_assembled(self) -> Result<Vec<u8>, AssembleError> {
        let total = self.total.unwrap_or(0);
        if !matches!(self.total, Some(t) if self.received.len() == t as usize) {
            return Err(AssembleError::Incomplete {
                have: self.received.len(),
                total,
            });
        }
        // BTreeMap iterates in ascending key order, so chunks concatenate in
        // index order without an explicit sort.
        let mut out = Vec::with_capacity(self.bytes);
        for (_idx, payload) in self.received {
            out.extend_from_slice(&payload);
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// DataChannel seam — what a webrtc-rs RTCDataChannel provides
// ─────────────────────────────────────────────────────────────────────────

/// The minimal peer-channel surface the transport needs. A concrete
/// `webrtc-rs` `RTCDataChannel` satisfies this: `ready_state()` → [`is_open`],
/// `buffered_amount()`, `set_buffered_amount_low_threshold` +
/// `on_buffered_amount_low` → [`wait_buffered_below`], and an ordered reliable
/// `send`.
///
/// [`is_open`]: DataChannel::is_open
/// [`wait_buffered_below`]: DataChannel::wait_buffered_below
pub trait DataChannel {
    /// Whether the underlying channel has reached the open ready-state.
    fn is_open(&self) -> bool;

    /// Resolve once the channel's buffered amount is at or below `threshold`.
    /// Backs SCTP send backpressure (MR-P2P-05).
    fn wait_buffered_below(&self, threshold: usize) -> impl Future<Output = ()> + Send;

    /// Send one framed message as a single ordered, reliable SCTP message.
    /// Atomic at the message level: it either delivers the whole frame or errors.
    fn send(&self, frame: &[u8]) -> impl Future<Output = Result<(), TransportError>> + Send;
}

// ─────────────────────────────────────────────────────────────────────────
// Transport state machine
// ─────────────────────────────────────────────────────────────────────────

/// Explicit transport lifecycle. Sends are only legal from [`Open`] onward.
///
/// [`Open`]: TransportState::Open
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// Peer identity verified (T-29 DTLS vouch) but the data channel is not open.
    Verified,
    /// Channel open; no chunk sent yet.
    Open,
    /// At least one chunk has been sent.
    DataFlow,
    /// Channel closed; no further sends.
    Closed,
}

/// Send-side bounds derived from the pack manifest (MR-P2P-04/05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendLimits {
    /// High-water mark: wait for buffered amount ≤ this before each send.
    pub max_buffered_bytes: usize,
    /// Maximum bytes in any single frame payload.
    pub max_frame_payload: usize,
    /// Maximum chunk count (and thus the exclusive upper bound on any index).
    pub max_chunks: u32,
}

/// Errors raised by the send path.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("send attempted before the channel reached Open (state: {state:?})")]
    SendBeforeOpen { state: TransportState },
    #[error("send attempted on a closed channel")]
    Closed,
    #[error("frame error: {0}")]
    Frame(#[from] FrameError),
    #[error("channel reported not-open at send time")]
    ChannelNotOpen,
    #[error("underlying channel send failed: {0}")]
    ChannelSend(String),
}

/// Single-owner WebRTC pack transport: enforces ordering, bounds, backpressure,
/// and cancellation-safety on top of a [`DataChannel`].
#[derive(Debug)]
pub struct PeerTransport<C: DataChannel> {
    channel: C,
    state: TransportState,
    limits: SendLimits,
    sent_chunks: u32,
    sent_bytes: u64,
}

impl<C: DataChannel> PeerTransport<C> {
    /// Create a transport in the [`TransportState::Verified`] state. The peer
    /// identity is assumed already vouched (T-29); the channel may not be open.
    pub fn new(channel: C, limits: SendLimits) -> Self {
        Self {
            channel,
            state: TransportState::Verified,
            limits,
            sent_chunks: 0,
            sent_bytes: 0,
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> TransportState {
        self.state
    }

    /// Chunks successfully sent so far (advances only after an `Ok` send).
    pub fn sent_chunks(&self) -> u32 {
        self.sent_chunks
    }

    /// Bytes successfully sent so far (payload only; excludes framing).
    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// Transition `Verified → Open` once the channel's `on_open` has fired.
    /// Idempotent from `Open`/`DataFlow`; rejected from `Closed`.
    pub fn mark_open(&mut self) -> Result<(), TransportError> {
        match self.state {
            TransportState::Verified | TransportState::Open | TransportState::DataFlow => {
                if self.state == TransportState::Verified {
                    self.state = TransportState::Open;
                }
                Ok(())
            }
            TransportState::Closed => Err(TransportError::Closed),
        }
    }

    /// Mark the channel closed; subsequent sends fail loud.
    pub fn mark_closed(&mut self) {
        self.state = TransportState::Closed;
    }

    /// Frame and send one chunk.
    ///
    /// Order of operations matters for cancellation-safety (MR-P2P-06): all
    /// bounds are checked first, then we await backpressure, then we perform the
    /// single atomic channel send, and **only after it returns `Ok`** do we
    /// advance state and counters. Dropping this future at any await leaves the
    /// transport exactly as it was — no partial frame, no double-counting.
    pub async fn send_chunk(
        &mut self,
        index: u32,
        total: u32,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        // (1) State gate — never a silent drop (MR-P2P-03).
        match self.state {
            TransportState::Verified => {
                return Err(TransportError::SendBeforeOpen { state: self.state });
            }
            TransportState::Closed => return Err(TransportError::Closed),
            TransportState::Open | TransportState::DataFlow => {}
        }

        // (2) Bounds, before any allocation or await (MR-P2P-04).
        let frame = encode_frame(index, total, payload, self.limits.max_frame_payload)?;
        if total > self.limits.max_chunks {
            return Err(TransportError::Frame(FrameError::IndexOutOfRange {
                index,
                total,
            }));
        }

        // (3) Backpressure — honor buffered_amount (MR-P2P-05). Cancellation
        // here means nothing has been sent yet: fully safe.
        self.channel
            .wait_buffered_below(self.limits.max_buffered_bytes)
            .await;

        // Re-check liveness after the await; the channel may have closed while
        // we waited. Fail loud rather than send into a dead channel.
        if !self.channel.is_open() {
            return Err(TransportError::ChannelNotOpen);
        }

        // (4) Atomic send. Cancellation before completion = no partial state;
        // the SCTP message is all-or-nothing and counters are untouched.
        self.channel.send(&frame).await?;

        // (5) Commit accounting only after a successful send.
        self.state = TransportState::DataFlow;
        self.sent_chunks = self.sent_chunks.saturating_add(1);
        self.sent_bytes = self.sent_bytes.saturating_add(payload.len() as u64);
        Ok(())
    }

    /// Send a whole pack as ordered frames. Stops at the first error, having
    /// sent a clean prefix (every delivered chunk is whole). Resumable from
    /// [`sent_chunks`].
    ///
    /// [`sent_chunks`]: PeerTransport::sent_chunks
    pub async fn send_pack(&mut self, chunks: &[Vec<u8>]) -> Result<(), TransportError> {
        let total = chunks.len() as u32;
        for (index, payload) in chunks.iter().enumerate() {
            self.send_chunk(index as u32, total, payload).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    /// Test channel recording sends, with a gate to suspend a send so we can
    /// exercise cancellation, and a settable buffered amount for backpressure.
    #[derive(Clone, Default)]
    struct MockChannel {
        open: Arc<AtomicBool>,
        sends: Arc<Mutex<Vec<Vec<u8>>>>,
        block: Arc<AtomicBool>,
        gate: Arc<Notify>,
        drain_waits: Arc<AtomicUsize>,
    }

    impl MockChannel {
        fn open() -> Self {
            let c = Self::default();
            c.open.store(true, Ordering::SeqCst);
            c
        }
        fn frames(&self) -> Vec<Vec<u8>> {
            self.sends.lock().unwrap().clone()
        }
    }

    impl DataChannel for MockChannel {
        fn is_open(&self) -> bool {
            self.open.load(Ordering::SeqCst)
        }
        async fn wait_buffered_below(&self, _threshold: usize) {
            self.drain_waits.fetch_add(1, Ordering::SeqCst);
        }
        async fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
            if self.block.load(Ordering::SeqCst) {
                // Suspend forever (until the test drops the future).
                self.gate.notified().await;
            }
            self.sends.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
    }

    fn limits() -> SendLimits {
        SendLimits {
            max_buffered_bytes: 1024,
            max_frame_payload: 1024,
            max_chunks: 16,
        }
    }

    // ── MT-P2P-03 — no send before Open; never a silent drop ──────────────────

    #[tokio::test]
    async fn mt_p2p_03_send_before_open_errors_not_dropped() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch.clone(), limits());
        // State is Verified — a send must error, and nothing reaches the channel.
        let err = t.send_chunk(0, 1, b"x").await.unwrap_err();
        assert!(matches!(err, TransportError::SendBeforeOpen { .. }));
        assert!(
            ch.frames().is_empty(),
            "no frame may be dropped onto the wire"
        );
        assert_eq!(t.sent_chunks(), 0);
    }

    #[tokio::test]
    async fn mt_p2p_03_send_after_open_transitions_to_dataflow() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch.clone(), limits());
        t.mark_open().unwrap();
        assert_eq!(t.state(), TransportState::Open);
        t.send_chunk(0, 1, b"hello").await.unwrap();
        assert_eq!(t.state(), TransportState::DataFlow);
        assert_eq!(ch.frames().len(), 1);
        // The wire bytes decode back to the original chunk.
        let decoded = decode_frame(&ch.frames()[0], 1024).unwrap();
        assert_eq!(decoded.payload, b"hello");
        assert_eq!(decoded.index, 0);
    }

    #[tokio::test]
    async fn send_on_closed_errors() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch, limits());
        t.mark_open().unwrap();
        t.mark_closed();
        assert_eq!(
            t.send_chunk(0, 1, b"x").await.unwrap_err(),
            TransportError::Closed
        );
    }

    // ── MT-P2P-04 — bounds: oversized frame rejected, assembler bounds memory ──

    #[tokio::test]
    async fn mt_p2p_04_oversized_frame_rejected_before_send() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch.clone(), limits());
        t.mark_open().unwrap();
        let big = vec![0u8; 2048]; // > max_frame_payload (1024)
        let err = t.send_chunk(0, 1, &big).await.unwrap_err();
        assert!(matches!(
            err,
            TransportError::Frame(FrameError::PayloadTooLarge { .. })
        ));
        assert!(ch.frames().is_empty());
    }

    #[test]
    fn mt_p2p_04_assembler_rejects_oversized_peer_without_oom() {
        let lim = AssemblerLimits {
            max_chunks: 4,
            max_total_bytes: 8,
            max_frame_payload: 8,
        };
        let mut asm = ChunkAssembler::new(lim);
        asm.accept(ChunkFrame {
            index: 0,
            total: 2,
            payload: vec![1, 2, 3, 4, 5],
        })
        .unwrap();
        // Second chunk would push total bytes to 11 > 8 → typed error, not OOM.
        let err = asm
            .accept(ChunkFrame {
                index: 1,
                total: 2,
                payload: vec![6, 7, 8, 9, 10, 11],
            })
            .unwrap_err();
        assert!(matches!(err, AssembleError::TotalBytesExceeded { .. }));
    }

    #[test]
    fn assembler_rejects_too_many_chunks_and_dupes_and_reorders() {
        let lim = AssemblerLimits {
            max_chunks: 3,
            max_total_bytes: 1024,
            max_frame_payload: 1024,
        };
        // Declared total above the cap.
        let mut asm = ChunkAssembler::new(lim);
        assert!(matches!(
            asm.accept(ChunkFrame {
                index: 0,
                total: 9,
                payload: vec![1]
            }),
            Err(AssembleError::TotalTooLarge { .. })
        ));

        // Happy path with out-of-order arrival reassembles in index order.
        let mut asm = ChunkAssembler::new(lim);
        asm.accept(ChunkFrame {
            index: 1,
            total: 2,
            payload: vec![3, 4],
        })
        .unwrap();
        asm.accept(ChunkFrame {
            index: 0,
            total: 2,
            payload: vec![1, 2],
        })
        .unwrap();
        // Duplicate index rejected.
        assert!(matches!(
            asm.accept(ChunkFrame {
                index: 0,
                total: 2,
                payload: vec![9]
            }),
            Err(AssembleError::DuplicateChunk { index: 0 })
        ));
        assert!(asm.is_complete());
        assert_eq!(asm.into_assembled().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn assembler_incomplete_is_error() {
        let lim = AssemblerLimits {
            max_chunks: 3,
            max_total_bytes: 1024,
            max_frame_payload: 1024,
        };
        let mut asm = ChunkAssembler::new(lim);
        asm.accept(ChunkFrame {
            index: 0,
            total: 2,
            payload: vec![1],
        })
        .unwrap();
        assert!(!asm.is_complete());
        assert!(matches!(
            asm.into_assembled(),
            Err(AssembleError::Incomplete { have: 1, total: 2 })
        ));
    }

    // ── MT-P2P-05 — backpressure consulted before every send ──────────────────

    #[tokio::test]
    async fn mt_p2p_05_backpressure_consulted_each_send() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch.clone(), limits());
        t.mark_open().unwrap();
        t.send_pack(&[vec![1], vec![2], vec![3]]).await.unwrap();
        assert_eq!(ch.drain_waits.load(Ordering::SeqCst), 3);
        assert_eq!(t.sent_chunks(), 3);
    }

    // ── MT-P2P-06 — cancellation mid-send leaves no partial state ──────────────

    #[tokio::test]
    async fn mt_p2p_06_drop_mid_send_leaves_no_partial_state() {
        let ch = MockChannel::open();
        let mut t = PeerTransport::new(ch.clone(), limits());
        t.mark_open().unwrap();
        // Two clean sends.
        t.send_chunk(0, 3, b"a").await.unwrap();
        t.send_chunk(1, 3, b"b").await.unwrap();
        assert_eq!(t.sent_chunks(), 2);

        // Make the channel suspend the next send, then cancel via timeout.
        ch.block.store(true, Ordering::SeqCst);
        {
            let fut = t.send_chunk(2, 3, b"c");
            let cancelled = tokio::time::timeout(std::time::Duration::from_millis(20), fut).await;
            assert!(
                cancelled.is_err(),
                "send must still be suspended → timed out"
            );
        } // fut dropped here, mid-send

        // No partial frame reached the wire, and no counters advanced.
        assert_eq!(
            ch.frames().len(),
            2,
            "the suspended frame must not be recorded"
        );
        assert_eq!(t.sent_chunks(), 2);
        assert_eq!(t.state(), TransportState::DataFlow);

        // Resumable: unblock and re-send the same chunk cleanly.
        ch.block.store(false, Ordering::SeqCst);
        t.send_chunk(2, 3, b"c").await.unwrap();
        assert_eq!(ch.frames().len(), 3);
        assert_eq!(t.sent_chunks(), 3);
    }

    // ── Frame round-trip + truncation rejection ───────────────────────────────

    #[test]
    fn frame_round_trips_and_rejects_truncation_and_magic() {
        let bytes = encode_frame(2, 5, b"payload", 1024).unwrap();
        let f = decode_frame(&bytes, 1024).unwrap();
        assert_eq!(
            (f.index, f.total, f.payload.as_slice()),
            (2, 5, b"payload".as_slice())
        );

        // Truncated body → LengthMismatch, never a panic.
        let truncated = &bytes[..bytes.len() - 2];
        assert!(matches!(
            decode_frame(truncated, 1024),
            Err(FrameError::LengthMismatch { .. }) | Err(FrameError::TooShort)
        ));

        // Bad magic.
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert_eq!(decode_frame(&bad, 1024), Err(FrameError::BadMagic));

        // Header claims more than max_payload → rejected before reading body.
        assert!(matches!(
            decode_frame(&bytes, 2),
            Err(FrameError::PayloadTooLarge { .. })
        ));
    }
}
