---
executive_summary: >
  Agent teams are user-defined graphs executed by a first-class runtime inside
  Ferrosa Memory, with state in the database so a run survives the client that
  started it. Agents delegate through a single send_to(agent, request) endpoint
  whose target the engine authorises against the graph's edges; because a node
  may act purely as a forwarder the graph has cycles by design, so termination
  is never structural and every run carries explicit bounds. Node capabilities
  are declared from the outset but enforced only by prompt until sandbox
  provisioning exists, and that gap is shown rather than implied.
---

# ADR-009: Team Runtime

## Status

Accepted, 2026-08-25. Supersedes nothing. Implementation not started.

## Context

A team is a graph the user draws: agents as nodes, and edges saying when one
agent involves another. The first team is a writing team — researcher, writer,
reviewer — producing an agent team knowledge claim for human review.

The shape of the problem forces four decisions before any code, because each one
changes the schema or the trust boundary rather than the interface.

## Decision 1: The runtime lives in Ferrosa Memory

A first-class team runtime, server-side, with run state in the database.

Rejected: Codex CLI in tmux, which is this project's default worker model
elsewhere and is well understood here. Rejected: subagents inside a single
assistant session.

Both rejected for the same reason. A team run is long-lived and multi-party, and
under either alternative it belongs to whichever process started it. Close the
laptop and the run is gone, with no record of how far it got. Putting the
runtime behind the memory server makes a run an object other clients can observe
and steer — the desktop and the phone drive the same running team — and makes
its transcript durable evidence rather than terminal scrollback.

The cost is real and should be stated: this is the largest of the options, and
it means Ferrosa Memory acquires a scheduler.

## Decision 2: One delegation endpoint, authorised against the graph

Agents delegate through a single tool:

    send_to(agent, request)

Rejected: generating one tool per outgoing edge, so that the toolset itself
encodes the routing.

Per-edge tools make the graph static in a way the model should not be. An agent
may legitimately act as a pure forwarder, relaying a request onward without
doing work of its own, and a routing table baked into a toolset cannot express
that without inventing a tool per path.

Two consequences follow, and both are load-bearing:

**The engine authorises the target.** With one generic endpoint, nothing in the
tool signature stops an agent naming a teammate it has no edge to. The engine
checks the requested target against the graph's edges for that node and refuses
otherwise. This is an authorisation decision and belongs server-side, in the
runtime, never in a node's prompt.

**The graph has cycles by design.** Writer to researcher and back is already a
cycle, and forwarder nodes generalise it. Termination is therefore never
structural — no topological order exists to run out of — and must come from
explicit bounds. See Decision 4.

## Decision 3: Capabilities are declared now, enforced later, and never implied

Each node declares the capabilities its agent holds: internet, browser,
filesystem, shell. The writing team's premise depends on this — the researcher
has internet, the writer does not, so the writer is *forced* to delegate rather
than merely asked to.

Sandbox provisioning does not exist yet. Until it does, declared capabilities
are enforced by prompt only.

The rule that makes this honest: **the interface states that capabilities are
not enforced, on every run that has no sandbox behind it.** A declaration the
system cannot enforce, displayed as though it were enforced, is worse than no
declaration — it invites trusting an isolation that is not there.

When provisioning lands, the enforcement swaps in and the declaration is
unchanged.

## Decision 4: An exhausted run parks and asks

The user sets a maximum number of review cycles. When those are spent and the
writer and reviewer still disagree, the run **parks**. It posts to a message
surface at the top of the Team tab and on the home page, carrying the draft, the
reviewer's outstanding objection, and the provenance links. A human breaks the
tie.

Rejected: publishing anyway with a disputed marker, which puts a contested claim
beside agreed ones and relies on a badge to keep them apart. Rejected: discarding
the run, which throws away the most expensive part.

Parking also gives the deadlock a shape the rest of the system already has: it
becomes a message awaiting a person, which is the same surface used for grilling
the user during a run.

## Consequences

Ferrosa Memory gains a scheduler, a run state machine, and an authorisation
check it did not have. Runs become durable objects with transcripts, which is
what makes a published claim auditable — and auditability is the whole point of
the provenance links.

The team's output is a *candidate*. Writer and reviewer agreeing produces a
claim ready for review, not a truth. This is the same rule the
`ontology-from-observations` skill states for induced ontologies: frequency,
model confidence and repeated agreement are never promoted directly. The human
is the gate, and the phrase "ready for review" is load-bearing rather than
decorative.
