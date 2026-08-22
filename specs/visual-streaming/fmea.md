---
title: "Visual streaming — failure modes and effects"
executive_summary: >
  Failure modes for browser and desktop streaming, scored by severity,
  occurrence and detection. The highest-RPN items are not the video pipeline —
  they are the renegotiation path that does not exist yet, the destructive
  signal read already breaking connects, and the fact that a remote viewer
  inherits everything the browser is logged into. Detection scores are
  deliberately harsh where the failure is silent.
status: proposed
last_revised: 2026-08-22
---

# Visual streaming — failure modes and effects

Scoring is Severity × Occurrence × Detection, each 1–10. Detection scores what
*we* would notice, not what a user would tolerate: a fault that produces no log
and no metric scores high even when it is obvious on screen, because the
operator on a phone is not the person who can fix it.

RPN ≥ 200 becomes a work item before the milestone that would hit it. RPN ≥ 50
gets a named test.

## Ranked

| # | Failure mode | Effect | S | O | D | RPN |
|---|---|---|---:|---:|---:|---:|
| F-01 | Client discards a renegotiation offer | Video never starts; session looks connected | 8 | 9 | 7 | **504** |
| F-02 | Remote viewer inherits browser credentials | Full access to logged-in sessions and secrets | 10 | 5 | 6 | **300** |
| F-03 | Encoder outruns available bitrate | Growing latency until the session is unusable | 7 | 7 | 5 | **245** |
| F-04 | Input injected into the wrong coordinates | Clicks land elsewhere; destructive at worst | 8 | 4 | 6 | **192** |
| F-05 | Capture keeps producing after viewer detaches | CPU burn and cost on an unwatched stream | 5 | 7 | 5 | 175 |
| F-06 | Browser profile persisted with credentials in it | Durable secret store nobody designed | 9 | 4 | 4 | 144 |
| F-07 | Agent and human drive simultaneously | Corrupted interaction, unclear causality | 6 | 6 | 4 | 144 |
| F-08 | ICE fails; media falls back to relay | Bandwidth cost; possible privacy surprise | 5 | 6 | 4 | 120 |
| F-09 | Xvfb dies, Chromium survives | Black stream, browser still "running" | 6 | 4 | 5 | 120 |
| F-10 | Clipboard exfiltrates without consent | Silent data egress | 8 | 3 | 5 | 120 |
| F-11 | Reconnect restarts the browser | Lost work the grace period was meant to protect | 6 | 5 | 3 | 90 |
| F-12 | Track added before hello is attested | Frames to an unverified peer | 9 | 2 | 4 | 72 |

## The ones that decide the plan

### F-01 — Client discards a renegotiation offer (RPN 504)

**Highest score in the analysis, and it is not hypothetical.** Android's
`pollSignal` calls the destructive `takeSignals` and drops every signal whose
kind it is not currently waiting for. iOS stops reading signals entirely once
the initial answer arrives. A renegotiation offer sent to either is consumed and
lost.

Occurrence is 9 because this happens on the *first* attempt, every time, until
fixed. Detection is 7 because the visible result is a session that connected
successfully and simply has no video — there is no error to read. The sender
sees a track it added; the receiver never learns it exists.

This is already breaking ordinary connects (`t_76c5c374`) independent of
streaming.

**Control.** Persistent signal reader with a buffer for unmatched kinds, landed
and tested before any capture work. Test: post an offer to a client that is not
waiting for one and assert it is applied and answered.

### F-02 — Remote viewer inherits browser credentials (RPN 300)

A viewer with `browser.control` can use any session the browser is logged into,
read anything on screen, and initiate downloads. Severity 10 — this is remote
shell with a nicer interface.

Made sharper by D-06: automation is implied by runtime access, so the browser is
drivable by anything with a runtime, and the meaningful boundary is who is
granted one. That boundary was not designed for "this also grants the browser".

**Controls.** `view` and `control` as separate grants; audit events on session
start, ownership change and clipboard; idle disconnect; live revocation. None of
these reduce severity — they reduce how long an incident lasts and whether it is
reconstructable afterwards. Recorded as a residual risk, not a solved one.

### F-03 — Encoder outruns available bitrate (RPN 245)

Encoding faster than the link drains queues frames, and queued frames become
latency that never recovers within a session. The failure feels like "it got
slow" and does not announce itself.

**Controls.** Respond to WebRTC congestion-control feedback rather than a fixed
bitrate; prefer dropping frame rate over queueing; treat idle as an opportunity
to stop producing entirely. Metric: encoded-but-unsent depth, alarmed rather
than merely charted.

### F-04 — Input at the wrong coordinates (RPN 192)

Normalized `0.0–1.0` coordinates remove client-side scaling as a cause, which is
why the protocol uses them. What remains is disagreement about the *surface*:
a viewport resize in flight, a device-scale mismatch, or a capture whose
dimensions changed without the client being told.

**Control.** Frames carry the surface dimensions they were captured at; input is
resolved against the surface the client last rendered, not the newest one. Test:
resize mid-stream and assert clicks land where the user aimed.

## Failure handling that must exist

| Fault | Required behaviour |
|---|---|
| Peer connection drops | Keep capture and browser alive 2–5 min; reattach on reconnect |
| Encoder fails | Restart encoder only; browser untouched |
| Browser crashes | Restart browser; **report** lost volatile state rather than resuming silently |
| Display server dies | Fail the session loudly; a black stream must never read as a working one |
| Viewer detaches | Stop capture and encode; do not keep burning CPU on nobody |

The recurring rule: a stream that has stopped carrying truth must not keep
looking like a stream. A black rectangle and a working desktop are visually
similar and semantically opposite.
