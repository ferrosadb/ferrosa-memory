---
title: "Visual streaming — project plan"
executive_summary: >
  Sequenced plan for browser and desktop streaming. Sprint 0 is not capture —
  it is the renegotiation path, which does not exist on either client and which
  every later milestone depends on. Ordering follows the decision to make
  Android the first viewer and a Fly machine the first host, so the expensive
  iOS receive path is built last, against a wire format already proven.
status: proposed
last_revised: 2026-08-22
---

# Visual streaming — project plan

Decisions in [decisions.md](decisions.md). Design in
[architecture.md](architecture.md). Risks in [fmea.md](fmea.md).

Ordered by dependency and by risk retired per unit of work, not by the order
the brief lists its milestones. The two differ in one important way: the brief
opens with capture, and capture is not the first thing that can fail.

## Sprint 0 — Make the connection extensible

**Nothing else can start.** Adding a track to a live session requires
renegotiation, which the broker permits and no client can receive
(decisions D-02, FMEA F-01 at RPN 504).

- [ ] Android: buffer signals that are not the awaited kind instead of dropping
      them. Closes `t_76c5c374`, which is currently breaking ordinary connects.
- [ ] Android and iOS: read signals for the life of the session, not until the
      first answer
- [ ] Both: apply an inbound offer, answer it, post the answer back
- [ ] Test: post an offer to a client that is not waiting for one; assert it is
      applied and answered

**Done when** a renegotiation round trip completes on a live session with no
media involved. That is testable today, against the existing control session,
before a single frame is captured.

**Value even if streaming is cancelled:** it fixes a live bug and makes the
session capable of growing any future track or channel.

## Sprint 1 — One frame, end to end

Prove the pipeline with the least possible machinery.

- [ ] `VisualCapture` trait and the session state machine in the peer runtime
- [ ] `LinuxX11Capture`: Xvfb at 1920×1080, Chromium, X11 capture
- [ ] Software H.264 encode; feed `TrackLocalStaticSample`
- [ ] Capability exchange (`visual_capabilities`, `visual_session_start`)
- [ ] Deploy the runtime to a Fly machine (decision D-05)
- [ ] Android: attach `SurfaceViewRenderer`, render the track
- [ ] Machines row: a streaming control next to CONNECT, shown only when the
      machine reported the capability (decision D-08)
- [ ] Telemetry: capture fps, encode fps, encode latency, RTT, loss, bitrate

**Done when** a readable Chromium appears on the tablet at roughly 30 fps, over
WAN, and the numbers above are visible rather than inferred.

**Explicitly not in this sprint:** input. A stream you cannot touch is a smaller
thing to debug, and it isolates capture and encode faults from injection faults.

## Sprint 2 — Make it interactive

- [ ] Input data channel, separate from the control feed (architecture.md)
- [ ] Pointer move, down, up, enter, leave, wheel — normalized coordinates
- [ ] Keyboard with physical `code` and logical `key`, modifiers verified
- [ ] Input-driven frame capture: an event forces a frame
- [ ] Viewport resize; frames carry the dimensions they were captured at (F-04)
- [ ] Touch input, since the first client is a tablet (Open Question 5, settled
      by D-04)
- [ ] Measure true interaction latency: input → injection → encode → render

**Done when** Chromium is comfortably usable from the tablet and measured WAN
interaction latency is under 200 ms.

This is the sprint that decides whether the whole idea is good. Everything
before it is plumbing; everything after it is refinement.

## Sprint 3 — Agent and human on one browser

- [ ] Playwright/CDP alongside the human input path
- [ ] Ownership state `AGENT` · `HUMAN` · `SHARED` · `PAUSED`
- [ ] Takeover and release; agent notified on change
- [ ] Audit events for session start and ownership change

Ownership is coordination, not enforcement (D-06). Documented that way in the
protocol, so nobody builds a security assumption on it.

**Done when** the target workflow runs: the agent opens an app and navigates to
login, a human takes control and completes MFA, releases, and the agent
continues on the same browser session.

## Sprint 4 — Persistence and clipboard

- [ ] Clipboard, capability-gated, `text/plain` and `text/html`
- [ ] Reconnect grace period; browser survives a dropped stream
- [ ] Browser lifetime independent of visual-session lifetime
- [ ] **Decide and document the profile storage policy before persisting one**

That last item is a gate, not a task. A persisted Chromium profile is persisted
credentials (FMEA F-06), and D-06 means the runtime grant already implies access
to them. Persisting first and writing the policy afterwards gets the order
backwards.

## Sprint 5 — Desktop mode

- [ ] Lightweight window manager, terminal, arbitrary GUI applications
- [ ] Full-display capture, display resize, window selection

Deliberately after browser mode: smaller capture area, fixed resolution and
predictable input coordinates make browser mode the easier thing to get right
first, and it is the mode the agent workflow actually needs.

## Sprint 6 — Performance

Only once latency and usability are proven.

- [ ] Hardware encode (VAAPI, NVENC, Quick Sync, VideoToolbox)
- [ ] Dirty-region detection; adaptive fps and bitrate
- [ ] Wayland + PipeWire capture; zero-copy DMA-BUF
- [ ] Optional 60 fps

## Deferred, with the reason

| Item | Why not now |
|---|---|
| iOS viewer | `webrtc-rs` has no decoder or renderer; largest single item, and the wire format should be settled first (D-04) |
| Audio | Not needed for the first useful workflow |
| Multi-viewer | One exclusive viewer in v1 (D-07); revisit for watch-without-taking |
| AV1 | Attractive for UI content, unacceptable software encode cost in an interactive path |
| TURN bandwidth policy | Unanswerable until there is a measured bitrate (Open Question 7) |

## The measurement that matters

Every other metric is diagnostic. This one is the product:

```text
input event → injection → surface change → encode → transmit → render
```

Instrument it end to end from Sprint 2, and treat a regression in it as a
release blocker. Frame rate is a means; interaction latency is the thing being
bought.
