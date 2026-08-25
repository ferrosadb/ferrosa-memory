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

The shape of the problem forces ten decisions before any code, because each one
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

The draft does **not** wait outside the queue while the tie is unbroken. It
enters the proposed-knowledge queue immediately, in a `blocked on human` state,
carrying the objection. A draft held invisibly inside a parked run is work
nobody can see; in the queue it is work with a state.

Parking also gives the deadlock a shape the rest of the system already has: it
becomes a message awaiting a person, which is the same surface used for grilling
the user during a run.

## Decision 5: Only a human awards the green check

Every artifact a team produces goes to the **responsible human** for review. No
path exists by which agent output becomes approved knowledge without a person
acting.

This makes the two terminal paths differ in state, not in destination:

| How the run ended | Enters the queue as | Carries |
|---|---|---|
| writer and reviewer agreed | proposed | draft, provenance links |
| bounds exhausted, still disagreeing | blocked on human | draft, provenance links, the objection |

Both wait for the same person and the same action. Agreement between agents buys
a better starting state, never the outcome.

A claim therefore has an owner, not a pool. "The responsible human" is an
assignment, and a claim that cannot name one is a claim nobody is going to
review.

## Decision 6: A graph that cannot terminate is not a valid graph

Every team must have a terminal state, and there are exactly three ways to have
one:

- **exhausted attempts** — a bound on cycles or messages,
- **elapsed time** — a wall-clock deadline,
- **completion** — an exit the graph can actually reach.

This moves termination from a runtime concern to a **validity** concern. The
editor refuses to save a team with no terminal state, rather than accepting it
and discovering at run time that nothing ends. Because cycles are legal
(Decision 2), a reachable exit is not implied by the shape of the graph and has
to be checked.

The check is a pure function of the graph and its bounds, so it is cheap and it
is testable without running anything.

## Decision 7: The definition is locked at creation; occupancy is not

A team definition — its nodes, edges, prompts and bounds — is **locked when the
team is created**. A run pins that definition. Editing produces a new team
rather than mutating one that runs may be executing against, so no run ever
changes shape underneath itself and a transcript can always be read against the
graph that produced it.

Occupancy is separate and is allowed to change, under two conditions:

1. the team is **paused**, and
2. the teammate being replaced is **not active**.

Pausing stops new messages from flowing; it does not abort work already in
flight. A teammate is active while it holds an unanswered turn, and swapping one
mid-turn would orphan a reply with nowhere to land.

So the graph is immutable and the roster is not. The distinction matters for
provenance: the claim must record which occupant produced which turn, because
"the writer said this" stops being a single agent the moment a swap is allowed.

## Decision 8: Every teammate has a session a human can enter

Each node in a run has a session the user can open to interact with that agent
directly, in the same transcript the team is using.

This is what makes a paused team useful rather than merely stopped: pause, enter
the reviewer's session, ask it what it actually objects to, then resume or swap.

Two rules keep it from corrupting the record:

- **Human turns are attributed to the human.** They enter the transcript marked
  as such. An unmarked human turn would let a person's paragraph be published as
  agent-team output, which is the inverse of the laundering Decision 5 prevents
  and just as wrong.
- **A human turn is still bounded work.** It costs the run's budget like any
  other turn, because the tokens are real.

## Decision 9: Pause and stop are written, and both show the drafts in flight

**Pause stops inter-agent communication. It does not pause the agents.**

A paused run still has agents working. What stops is delivery: a message an
agent sends is persisted and held, and goes out when the run starts again.
Nothing is lost and nothing is refused — the traffic is deferred, not blocked.

This has a consequence worth stating loudly, because the word "pause" implies
otherwise: **pause does not stop spend.** An agent mid-turn keeps thinking and
keeps costing. Someone reaching for pause to halt a runaway bill has reached for
the wrong control, and the interface should not let them believe otherwise.

To pause *an agent*, enter its session and use the harness's interrupt. That is
a different mechanism at a different level, and conflating the two would give
one control two meanings.

**Stop** ends the run and is not resumable.

Both are **written**. A pause or a stop that lives only in the runtime's memory
is undone by a restart, and a run that resumes because a process bounced is a
run that ignored the person who stopped it. The state is in the database, and
the runtime reads it rather than remembering it.

Both surfaces **show every draft of every artifact in flight**, not the latest
one. That requires drafts to be retained rather than overwritten: a run that
kept only the newest draft cannot answer "what did it have before the reviewer
pushed back", which is exactly the question someone is asking at the moment they
reach for pause or stop.

So `team_artifact` is append-only. Each draft records its version, its author —
which occupant, or which human — and the turn that produced it.

## Decision 10: The graph shows what each node is doing

The active nodes carry a state indicator:

| Colour | Meaning |
|---|---|
| green | working |
| orange | waiting for input |
| grey | dead |

Three states rather than two, because "not working" is not one condition. An
agent waiting for input is healthy and blocked; a dead one is neither, and the
difference decides whether a person should answer something or restart
something. Rendering both as idle would hide the only one that needs a human.

`dead` must be a state the runtime can actually establish — a crashed harness, a
lost session, a node that cannot be reached — and not merely the absence of
recent traffic. A quiet node and a dead node look identical from the outside,
which is exactly why the runtime has to distinguish them rather than the viewer.

## Consequences

Ferrosa Memory gains a scheduler, a run state machine, and an authorisation
check it did not have. Runs become durable objects with transcripts, which is
what makes a published claim auditable — and auditability is the whole point of
the provenance links.

The team's output is a *candidate*. Writer and reviewer agreeing produces a
claim ready for review, not a truth, and only a human awards the green check.

This is the same rule the `ontology-from-observations` skill states for induced
ontologies: frequency, model confidence and repeated agreement are never
promoted directly. The human is the gate, and "ready for review" is load-bearing
rather than decorative.
