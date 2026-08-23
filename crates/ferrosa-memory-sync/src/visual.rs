//! Module: The public seam between this runtime and visual-streaming plugins.
//! Correctness: Correct when the runtime can host a visual session with no
//! plugin crate present, when the null implementation exercises every path a
//! real one would, and when no plugin can make an authorization decision.
//! Last revised: 2026-08-22
//! Last changed: Initial plugin seam.
//!
//! # Why the traits live here and the implementations do not
//!
//! Transport, crypto, identity and the control protocol stay in this repository
//! because they should be auditable — obscurity in key handling or a wire format
//! is a liability rather than a control, and the parts a reviewer most needs to
//! read are the ones deciding who may connect and what protects the bytes.
//!
//! Capture, encode, provisioning and policy carry no security-relevant secrecy.
//! Keeping them out of tree makes the system no harder to attack and publishing
//! them would make it no safer, so they are the parts that move.
//!
//! The test for whether this line is in the right place: **no plugin may make an
//! authorization decision.** If one needs to, it belongs here instead.
//!
//! # The null implementation is not a stub
//!
//! [`NullCapture`] produces a real, deterministic test pattern. That is
//! deliberate: it lets this repository exercise track setup, renegotiation,
//! congestion response, the input round trip and telemetry with no plugin crate
//! at all — which makes interaction latency, the number that decides whether
//! this product is any good, measurable in CI that anyone can run.
//!
//! A trait with no in-tree implementation is a contract nobody compiles
//! against, and it drifts until the day someone tries to swap it.

use std::fmt;

/// What kind of surface a session is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    /// A single browser, sandboxed. Smaller capture area, fixed resolution,
    /// predictable input coordinates.
    Browser,
    /// A whole desktop: window manager, terminals, arbitrary applications.
    Desktop,
}

/// What a host can offer, as reported during capability negotiation.
///
/// Absent capabilities are how a client knows not to offer the control at all.
/// Advertising something the host cannot do converts a clear "not available"
/// into a session that negotiates and then fails.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SurfaceCapabilities {
    pub browser: bool,
    pub desktop: bool,
    /// Largest surface this host will provision, in pixels.
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

impl SurfaceCapabilities {
    /// Whether this host can serve the requested kind at all.
    pub const fn supports(&self, kind: SurfaceKind) -> bool {
        match kind {
            SurfaceKind::Browser => self.browser,
            SurfaceKind::Desktop => self.desktop,
        }
    }

    /// Whether anything at all is on offer.
    pub const fn any(&self) -> bool {
        self.browser || self.desktop
    }
}

/// A session's request for a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRequest {
    pub kind: SurfaceKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// An opaque handle to a provisioned surface.
///
/// Opaque on purpose: the runtime must not be able to reason about what backs a
/// session, or it would grow assumptions that only hold for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceHandle(pub String);

/// One encoded frame, ready for a media track.
///
/// Encoded rather than raw. The encoder is as platform-specific as the capture
/// — VideoToolbox, VAAPI, x264 — and a trait handing back raw frames would force
/// a copy through a common pixel format that no platform natively wants, once
/// per frame, on the hot path.
#[derive(Debug, Clone)]
pub struct EncodedSample {
    pub data: Vec<u8>,
    /// Presentation timestamp, microseconds since session start.
    pub pts_micros: u64,
    pub keyframe: bool,
    /// The surface size this sample was captured at.
    ///
    /// Travels WITH the sample so input can be resolved against the surface the
    /// client actually rendered rather than the newest one. Without it, a resize
    /// in flight lands clicks in the wrong place.
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum SurfaceError {
    Unsupported(SurfaceKind),
    Unavailable(String),
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(k) => write!(f, "this host does not provide a {k:?} surface"),
            Self::Unavailable(why) => write!(f, "surface unavailable: {why}"),
        }
    }
}

impl std::error::Error for SurfaceError {}

#[derive(Debug)]
pub enum CaptureError {
    NotStarted,
    Failed(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => write!(f, "capture has not been started"),
            Self::Failed(why) => write!(f, "capture failed: {why}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Supplies the thing that will be captured.
///
/// Separate from capture because provisioning changes with infrastructure —
/// which container, which display, which browser profile — while capture changes
/// with the platform's graphics stack. Different reasons, different rates,
/// different traits.
pub trait SurfaceProvider: Send + Sync {
    /// What this host can offer. Drives capability negotiation, and therefore
    /// whether a client shows the control at all.
    fn capabilities(&self) -> SurfaceCapabilities;

    /// Provision a surface.
    ///
    /// Failure must be reported BEFORE a media track is added. A session that
    /// negotiates media and then has nothing to send renders black on the
    /// client, and black is indistinguishable from a working desktop that
    /// happens to be dark.
    fn acquire(&self, request: &SurfaceRequest) -> Result<SurfaceHandle, SurfaceError>;

    /// Release it. Called on teardown and after the reconnect grace period.
    fn release(&self, handle: &SurfaceHandle);
}

/// Turns a provisioned surface into encoded samples.
pub trait VisualCapture: Send {
    fn start(
        &mut self,
        surface: &SurfaceHandle,
        request: &SurfaceRequest,
    ) -> Result<(), CaptureError>;

    /// Next encoded sample, or `None` when the source has ended.
    ///
    /// Ending is an outcome, not an error: a browser that exited or a display
    /// that went away must stop the stream rather than emit frames of nothing.
    fn next_sample(&mut self) -> Option<EncodedSample>;

    /// Produce a frame now, regardless of cadence.
    ///
    /// Called when input arrives. Responsiveness to a keystroke is what a user
    /// actually feels, and it must not wait for a change detector to notice.
    fn request_frame_now(&mut self);

    fn set_viewport(&mut self, width: u32, height: u32) -> Result<(), CaptureError>;

    fn stop(&mut self);
}

/// A host that provides nothing.
///
/// The default. Reports no capabilities, so negotiation correctly concludes this
/// host cannot stream and the client hides the control — rather than offering
/// something that will fail on connect.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSurfaceProvider;

impl SurfaceProvider for NullSurfaceProvider {
    fn capabilities(&self) -> SurfaceCapabilities {
        SurfaceCapabilities::default()
    }

    fn acquire(&self, request: &SurfaceRequest) -> Result<SurfaceHandle, SurfaceError> {
        Err(SurfaceError::Unsupported(request.kind))
    }

    fn release(&self, _handle: &SurfaceHandle) {}
}

/// A capture source that emits a deterministic test pattern.
///
/// Not a stub. This is what lets the repository test the whole visual path —
/// track setup, renegotiation, pacing, the input round trip, telemetry — with no
/// plugin crate present. The pattern is a counter rather than noise so a test
/// can assert on exact bytes and on frame ordering.
#[derive(Debug, Default)]
pub struct NullCapture {
    started: bool,
    frame: u64,
    width: u32,
    height: u32,
    fps: u32,
    forced: bool,
}

impl NullCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames emitted so far. Lets a test assert that input forced a frame.
    pub const fn frames_emitted(&self) -> u64 {
        self.frame
    }
}

impl VisualCapture for NullCapture {
    fn start(
        &mut self,
        _surface: &SurfaceHandle,
        request: &SurfaceRequest,
    ) -> Result<(), CaptureError> {
        self.started = true;
        self.width = request.width;
        self.height = request.height;
        self.fps = request.fps.max(1);
        Ok(())
    }

    fn next_sample(&mut self) -> Option<EncodedSample> {
        if !self.started {
            return None;
        }
        // Every frame is a keyframe when forced, which is what a real encoder
        // does on an input-driven capture — the client must be able to render
        // immediately rather than wait for the next GOP boundary.
        let keyframe = self.forced || self.frame == 0;
        self.forced = false;
        let pts_micros = self.frame * 1_000_000 / u64::from(self.fps);
        self.frame += 1;
        Some(EncodedSample {
            data: self.frame.to_be_bytes().to_vec(),
            pts_micros,
            keyframe,
            width: self.width,
            height: self.height,
        })
    }

    fn request_frame_now(&mut self) {
        self.forced = true;
    }

    fn set_viewport(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        if !self.started {
            return Err(CaptureError::NotStarted);
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SurfaceRequest {
        SurfaceRequest {
            kind: SurfaceKind::Browser,
            width: 1920,
            height: 1080,
            fps: 30,
        }
    }

    /// A host with no plugin must report nothing, so the client hides the
    /// control instead of offering a session that cannot start.
    #[test]
    fn the_null_provider_advertises_nothing() {
        let caps = NullSurfaceProvider.capabilities();
        assert!(!caps.any());
        assert!(!caps.supports(SurfaceKind::Browser));
        assert!(!caps.supports(SurfaceKind::Desktop));
    }

    /// And refuses rather than handing back a handle to nothing.
    #[test]
    fn the_null_provider_refuses_to_acquire() {
        assert!(NullSurfaceProvider.acquire(&request()).is_err());
    }

    /// Capture before start is not an error condition to recover from — there
    /// is simply nothing, and `None` says exactly that.
    #[test]
    fn capture_yields_nothing_before_start() {
        assert!(NullCapture::new().next_sample().is_none());
    }

    /// The pattern is deterministic and ordered, so a test can assert on exact
    /// frames rather than on "something arrived".
    #[test]
    fn the_test_pattern_is_deterministic_and_ordered() {
        let mut c = NullCapture::new();
        c.start(&SurfaceHandle("t".into()), &request()).unwrap();
        let a = c.next_sample().expect("first");
        let b = c.next_sample().expect("second");
        assert_eq!(a.data, 1u64.to_be_bytes().to_vec());
        assert_eq!(b.data, 2u64.to_be_bytes().to_vec());
        assert!(a.pts_micros < b.pts_micros, "timestamps must advance");
        assert_eq!(a.width, 1920);
    }

    /// The first frame must be a keyframe or the client has nothing to decode
    /// against.
    #[test]
    fn the_first_frame_is_a_keyframe() {
        let mut c = NullCapture::new();
        c.start(&SurfaceHandle("t".into()), &request()).unwrap();
        assert!(c.next_sample().unwrap().keyframe);
        assert!(!c.next_sample().unwrap().keyframe, "steady state is not");
    }

    /// Input must force a frame. This is the behaviour the whole product rests
    /// on — a keystroke should produce a frame before any change detector has
    /// noticed anything happened.
    #[test]
    fn input_forces_an_immediate_keyframe() {
        let mut c = NullCapture::new();
        c.start(&SurfaceHandle("t".into()), &request()).unwrap();
        let _ = c.next_sample();
        let _ = c.next_sample();
        c.request_frame_now();
        assert!(
            c.next_sample().unwrap().keyframe,
            "a forced frame must be independently decodable"
        );
    }

    /// A resize must be reflected in the samples, because input is resolved
    /// against the dimensions the sample carries.
    #[test]
    fn a_resize_reaches_the_samples() {
        let mut c = NullCapture::new();
        c.start(&SurfaceHandle("t".into()), &request()).unwrap();
        let _ = c.next_sample();
        c.set_viewport(1280, 720).unwrap();
        let after = c.next_sample().unwrap();
        assert_eq!((after.width, after.height), (1280, 720));
    }

    #[test]
    fn resizing_before_start_is_refused() {
        assert!(NullCapture::new().set_viewport(800, 600).is_err());
    }

    /// Stopping ends the stream rather than emitting frames of nothing.
    #[test]
    fn stopping_ends_the_stream() {
        let mut c = NullCapture::new();
        c.start(&SurfaceHandle("t".into()), &request()).unwrap();
        assert!(c.next_sample().is_some());
        c.stop();
        assert!(c.next_sample().is_none());
    }
}
