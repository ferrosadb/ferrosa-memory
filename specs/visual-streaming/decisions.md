---
title: "Visual streaming — decision record"
executive_summary: >
  Decisions governing browser and desktop streaming over the existing
  peer-to-peer WebRTC session. Four were taken by the stakeholder; three were
  resolved from the code and one of those overturns a cost assumption in the
  original brief. Records the reasoning, not only the choice, so a later reader
  can tell which decisions are load-bearing and which are reversible.
status: accepted
last_revised: 2026-08-22
---

# Visual streaming — decision record

Companion to [architecture.md](architecture.md), [fmea.md](fmea.md) and
[project-plan.md](project-plan.md).

Each record states what was decided, why, and what would have to change for the
decision to be revisited. Decisions resolved from the codebase cite the evidence
so they can be re-checked when the code moves.

## Resolved from the codebase

These were open questions in the brief. They did not need a stakeholder.

### D-01 — The WebRTC stack is not the same on every surface

**Decision.** Treat "our WebRTC" as three implementations, not one.

| Surface | Stack |
|---|---|
| Linux / macOS runtime | `webrtc-rs` 0.17 (`ferrosa-memory/Cargo.toml`) |
| iOS | `webrtc-rs` 0.17 via UniFFI (`ferrosa-mobile-core`, `webrtc-transport` feature) |
| Android | Google libwebrtc (`io.github.webrtc-sdk:android:144.7559.09`) |

**Why it matters.** The brief asks "what WebRTC library is currently used" as a
single question. The answer is two libraries with very different capabilities,
and the difference decides the order of the whole project — see D-04.

### D-02 — The broker already permits renegotiation; the clients do not

**Decision.** Adding a media track to a live session is transport-legal today
and blocked only by client code.

**Evidence.** `ferrosa-memory-gateway/src/signaling.rs` models the session as
`Offered → Accepted → Closed`. `post_signal` rejects anything outside
`Accepted`, and otherwise relays whatever it is given; nothing records that an
offer has already flowed or refuses a second one. So a renegotiation
offer/answer pair will relay.

What does not exist is a client that would hear it. Both shells read signals
exactly once, during connect, and then stop:

- iOS `ControlSessionDriver.awaitAnswer` returns as soon as it sees `sdp_answer`
- Android `AndroidControlConnector.pollSignal` does the same

Android additionally **destroys** signals it is not waiting for, because
`takeSignals` is destructive on the broker and non-matching kinds are dropped
(tracked as `t_76c5c374`). A renegotiation offer arriving there would be
consumed and discarded.

**Consequence.** "Reuse the existing peer connection" is the right architecture
and is cheaper than a second connection, but it is not free: it requires a
persistent signal reader on the client, which is new work the brief does not
account for. That work is a prerequisite for Milestone 1, not a later
refinement.

### D-03 — Clients are native; there is also a web console

The mobile shells are native (SwiftUI, Compose). `ferrosa-dbaas/web` is a
Next.js console. The brief's "browser or native app" is therefore a choice, not
a description — see D-04.

## Taken by the stakeholder

### D-04 — Android is the first viewer

**Decision.** The first client to render video is the Android tablet. iOS
follows once the wire format is proven. The entry point is a control next to
CONNECT in the Machines section, per D-08.

**Why.** `webrtc-rs` is RTP transport. It has no video codecs and no renderer.
Receiving video on iOS therefore means depacketizing RTP, decoding through
VideoToolbox, and rendering to a Metal or `AVSampleBufferDisplayLayer` surface —
all hand-written, and the largest single work item in the project. Android's
libwebrtc already contains H.264 decode and `SurfaceViewRenderer`.

Sending is symmetric and cheap on both: `webrtc-rs` can carry a track fed with
externally encoded H.264 through `TrackLocalStaticSample`.

So the expensive half is one specific client, and starting there would mean
building the hardest component before knowing whether the pipeline is useful.

**Revisit when.** iOS becomes the primary test surface, or `webrtc-rs` grows a
usable video receive path. Neither changes the wire format, which is the point
of ordering it this way.

### D-05 — The Linux runtime runs on a Fly machine

**Decision.** Prototype 1 targets a Fly machine, not a local container.

**Why.** The latency targets in the brief are WAN targets (<150–200 ms). A
local container measures a LAN and would report a number that cannot fail,
which is the same class of error as a test that cannot go red. Fly is also
where the gateway already runs and the deploy path is known-good.

**Cost accepted.** Slower iteration than a local container. Mitigated by keeping
capture and encode behind a trait so they can be exercised locally without a
peer — see architecture.md.

### D-06 — Browser automation is implied by runtime access, not separately gated

**Decision.** There is no `browser.automation` capability. Anything with a
runtime in the container can reach CDP on localhost.

**Why.** The stakeholder's reasoning, recorded because it is the load-bearing
part: a separate capability would be theatre, since anything with shell access
can reach the debug port regardless of what the capability says. A control that
can be trivially bypassed is worse than no control, because it invites reliance.

**Consequence, and it is not small.** This moves the real trust boundary to
*who gets a runtime at all*. Granting someone a container is granting them the
browser and everything it is logged into. That has to be reflected in how
runtime grants are authorized and audited, and it raises the stakes on browser
profile persistence (see the open items). It is recorded here so the boundary is
explicit rather than discovered later.

`browser.view` and `browser.control` remain separate — they gate a *remote*
peer, which is a different question from what a local process can do.

### D-07 — One viewer per visual session in v1

**Decision.** Exactly one attached client per visual session. Exclusive.

**Why.** One viewer means one encoder configuration, one bitrate ladder and one
congestion-control loop. It also removes the need to decide whose input wins
when two humans are attached, which is a policy question with no obvious answer
and no urgency.

**Revisit when.** Someone wants to watch an agent work without taking the
session. That is a real use case and the protocol should not make it
impossible — see architecture.md for the field reserved for it.

### D-08 — Entry point is the Machines row, beside CONNECT

**Decision.** Visual streaming is offered as a per-machine action next to
CONNECT, not as a separate screen or destination.

**Why.** Stated by the stakeholder, and it has an architectural consequence
worth making explicit: it means the capability is a property *of a machine*,
discovered and negotiated on the session already established with that machine.
It is not a separate service to be addressed, which reinforces D-02's "one
connection" choice and rules out a second peer connection for media.

It also means the first surface to show it is a shell that already has the
machine list, presence, and session state — the work lands on top of the
existing Machines section rather than beside it.

## Open, deferred with reasons

These are from the brief's Open Questions and are not needed for Milestone 1.

| # | Question | Why it can wait |
|---|---|---|
| Q4 | Should Chromium persist across compute sleep/restart? | Milestone 4. Prototype 1 can lose browser state on restart without invalidating anything it is meant to prove. |
| Q5 | Is input mainly desktop browsers or also iPad/mobile? | D-04 answers it for v1 by construction: the first client is a tablet, so touch input is in scope from the start. |
| Q6 | Do browser profiles need encrypted durable storage? | Only once profiles persist (Milestone 4). D-06 raises its priority — a persisted profile is persisted credentials. |
| Q7 | Is TURN/relay bandwidth acceptable for video? | Measure it. Unanswerable in the abstract, and the answer is a cost decision once there is a real bitrate. |
| Q9 | Is audio needed for the first useful workflow? | The brief already says no for Prototype 1. |
