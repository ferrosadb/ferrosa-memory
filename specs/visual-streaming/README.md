---
title: "Visual streaming — blueprint index"
executive_summary: >
  Browser and desktop video as a capability on the existing peer-to-peer
  control session. Start with decisions.md; the two findings that change the
  original brief are that webrtc-rs cannot render video on iOS, and that
  renegotiation is permitted by the broker but unreceivable by every client.
status: proposed
last_revised: 2026-08-22
---

# Visual streaming — blueprint

| Document | What it answers |
|---|---|
| [decisions.md](decisions.md) | What was decided and why; what the code already answered |
| [architecture.md](architecture.md) | How it fits on the existing session |
| [fmea.md](fmea.md) | How it fails, ranked |
| [project-plan.md](project-plan.md) | What order to build it in |

## The two findings that change the brief

**`webrtc-rs` is RTP transport with no codecs and no renderer.** Receiving video
on iOS means hand-writing depacketization, VideoToolbox decode and a Metal
render path. Android's libwebrtc already has all of it. That asymmetry, not
preference, is why Android is the first viewer.

**Renegotiation is permitted by the broker and unreceivable by every client.**
Both shells stop reading signals once the initial answer arrives, and Android
destroys signals it is not waiting for. "Add a track to the existing
connection" is the right design and is gated on work the brief does not
mention — which is why it is Sprint 0.
