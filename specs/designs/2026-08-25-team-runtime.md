---
executive_summary: >
  The Team surface lets a user define an agent team as a graph and run it inside
  Ferrosa Memory. Nodes are agent roles with declared capabilities, edges carry
  a condition and a message, and delegation happens through one authorised
  endpoint. The first team is researcher, writer and reviewer, producing a
  knowledge claim with links to every item it used. Because forwarder nodes make
  cycles structural, termination comes only from explicit bounds; because
  sandboxes do not exist yet, capability isolation is declared and shown as
  unenforced rather than assumed. Both endings reach a named responsible human,
  and no artifact is approved without one.
---

# Team Runtime

Blueprint for the Team surface and the runtime behind it. Decisions are recorded
in `../decisions/adr-009-team-runtime.md` and are treated here as settled.

## Goals

- A user defines a team as a graph and runs it.
- The first team is researcher, writer, reviewer, producing an agent team
  knowledge claim ready for human review.
- Everything the team gathers is stored and tiered, and the claim links to it.
- A run survives the client that started it.

## Non-goals for the first build

- Real sandbox enforcement. Capabilities are declared; see "Capability model".
- The software development team. Named as a second shape to prove the graph
  generalises, not built first.
- A second approvals queue. The claim feeds the existing proposed-knowledge
  queue.

## The model

A **team** is a directed graph, stored, versioned, and runnable more than once.

A **node** is an agent role: a system prompt, a model, and a set of declared
capabilities.

An **edge** from A to B carries two prompts, which do different jobs and must
not be conflated:

| Field | Job | Where it goes |
|---|---|---|
| `condition` | tells A *when* B is worth involving | into A's instructions, as the description of the delegation target |
| `message` | the template A sends | filled by A at call time |

A **run** is one execution of a team against a starting prompt. It owns the
transcript, the budget, the cycle counters, and the terminal state.

### Delegation

One endpoint, for every node:

    send_to(agent, request)

The engine resolves `agent` against the run's graph and refuses a target the
calling node has no edge to. Two properties follow from putting the check here
rather than in a prompt: a node cannot reach a teammate by being persuaded to
name it, and the refusal is a recorded event rather than a model's silent
compliance.

A node may forward — receive a request and pass it on without doing work of its
own. This is intended, and it is why routing is not baked into the toolset.

## Termination

Forwarders make cycles structural. `writer → researcher → writer` is already
one. There is no topological order to exhaust, so **every bound is explicit**:

| Bound | Set by | On exhaustion |
|---|---|---|
| review cycles | the user, per run | park and ask (below) |
| messages per run | team default, user override | park |
| token spend per run | team default, user override | park |
| wall-clock deadline | team default | park |
| no-progress | runtime | park |

No-progress deserves its own bound because the others can all be satisfied while
nothing happens: two agents can exchange messages politely and indefinitely
without the draft changing. Detect it on the artifact, not the traffic.

**Parking is not failure.** A parked run holds its state and posts a message
carrying the draft, the objection, and the provenance links. A human answers and
the run resumes. This is the same surface used to grill the user mid-run, which
is why the Team tab and the home page both show it.

The draft does not wait inside the parked run. It enters the queue immediately,
`blocked on human`, so that work in dispute is visible rather than buried in a
run nobody has opened.

## Terminal states

Both endings reach the same place. They differ in the state they arrive in and
what they carry:

| Run ended | Queue state | Carries | Next |
|---|---|---|---|
| writer and reviewer agreed | proposed | draft, provenance | responsible human reviews |
| bounds exhausted, disagreeing | blocked on human | draft, provenance, objection | responsible human breaks the tie |

There is no third path. **No artifact receives a green check without a person
acting** — agent agreement buys a better starting state, never the outcome.

Every claim names a **responsible human**. This is an assignment, not a pool: a
claim nobody owns is a claim nobody reviews, and it would sit in the queue
looking like progress.

## Capability model

Each node declares what it holds — internet, browser, filesystem, shell.

The writing team depends on this being real: the researcher has internet, the
writer does not, so delegation is forced rather than requested.

Until sandbox provisioning exists, declarations are enforced by prompt only, and
**the run says so**. A run with no sandbox behind it displays its capabilities
as unenforced, everywhere they appear. A declaration shown as though it were
enforced is worse than none, because it invites trusting isolation that is not
there.

### What isolation does not buy

Worth stating plainly, because it is easy to assume otherwise: denying the
writer internet stops the writer *acting* on the internet. It does not stop web
content *reaching* the writer — that is precisely what the researcher returns.
Capability isolation bounds action, not information. Content fetched from the
open web arrives in the writer's context as untrusted input and must be treated
as data, never as instructions. See the threat model.

## DIKW mapping

The tier model is already the right vocabulary, and the team is a consumer of it
rather than a new taxonomy:

| Flow | Tier |
|---|---|
| skills fed to the writer | Wisdom |
| context fed to the writer | Information |
| raw data the researcher returns | Data |
| summaries the researcher returns | Information |
| the published claim | Knowledge, proposed |

**Dependency worth naming:** a writer fed from Wisdom is fed from 87 entities
today, because only 24% of the skills corpus is ingested (`t_655a7c64`). The
team will work; it will be less informed than it looks.

## Provenance

The claim links to every Data and Information item that produced it. This is the
difference between a claim and an assertion, and it is the reason run state is
durable.

Agreement between writer and reviewer produces a **candidate**, never a truth —
the same rule `ontology-from-observations` states for induced ontologies.
"Ready for review" is the operative phrase.

## Data model

Seven tables. Names are indicative; the shapes are the commitment.

    team              definition, versioned
    team_node         role, model, system prompt, declared capabilities
    team_edge         from, to, condition prompt, message template
    team_run          team version, starting prompt, bounds, state
    team_message      the transcript: from, to, body, timestamp, refused flag
    team_artifact     drafts and the final claim, with provenance links,
                      queue state, and the responsible human
    team_budget       spend and counters, per run

`team_message` records refusals as well as sends. An authorisation refusal that
leaves no trace is indistinguishable from an agent that chose not to delegate,
and those are very different facts when a run goes wrong.

## Threat model

Focused on what this feature introduces, not a full STRIDE pass.

| # | Threat | Control |
|---|---|---|
| T1 | **Confused deputy via `send_to`.** A node is talked into naming a teammate it has no edge to. | Engine authorises the target against the graph. Refusals are recorded. |
| T2 | **Capability laundering through a forwarder.** A cannot reach C; B can; A asks B to ask C. | This is *intended* — delegation is the design. The control is that the graph is the policy: if A must not reach C even indirectly, the graph must not contain that path, and the editor should show reachability, not just direct edges. |
| T3 | **Prompt injection from researched content.** The researcher fetches open-web text; it lands in the writer's context. | Researcher output is stored and passed as *data*, clearly delimited, never as instructions. This is the highest-likelihood threat here and the least mitigated by capability isolation. |
| T4 | **Forged provenance.** A claim links to items it did not use, or omits ones it did. | Links are recorded by the runtime from actual message traffic, not asserted by the writer. |
| T5 | **Spend exhaustion.** A cyclic graph burns budget until something else breaks. | Per-run token and message bounds, enforced by the runtime. Unlike forge's pacing, the default here is **not** unlimited. |
| T6 | **Claim laundering.** Agent agreement is read as human approval. | Structural, not procedural: there is no transition from any agent-produced state to approved. Both terminal states await a named responsible human, and the green check is only ever awarded by one. |

## Failure modes

| Mode | Effect | Detection | Mitigation |
|---|---|---|---|
| Node never returns | Run wedged, budget intact | wall-clock bound | park |
| Two agents converge on nothing | Budget drains, artifact unchanged | no-progress bound | park |
| Runtime restarts mid-run | Run lost, if state was in memory | — | state is in the database; a run is resumable by construction |
| Deadlock undetected | Run parks, nobody notices | message surface on Team tab and home | the surface is the mitigation, which is why it is in scope |
| Claim without provenance | Unauditable knowledge | schema requires links | refuse to publish a claim with none |
| Claim with no responsible human | Sits in the queue unreviewed, indistinguishable from work in progress | assignment is required on entry | refuse to enqueue a claim that cannot name an owner |
| Capability shown as enforced when it is not | Unfounded trust in isolation | — | unenforced state rendered on every run without a sandbox |

## Plan

Ordered by what unblocks what. Each slice has a testable core that runs without
a cluster, a browser or an LLM — the parts worth handing to `/tdd` are the pure
ones.

**Slice 1 — the graph, and what it permits.** Team, node and edge storage; the
authorisation check. Pure and fully testable: given a graph and a caller, is
this target permitted? Includes reachability, for T2.

**Slice 2 — the run state machine and its bounds.** States, counters, and the
five bounds. Pure. Every bound gets a test that it fires, and a test that an
unbounded run cannot be started.

**Slice 3 — one agent, executed.** A single node runs against a prompt and
returns. No delegation. This is where the scheduler lands.

**Slice 4 — `send_to`, end to end.** Two nodes, one edge, a real delegation and
a real refusal. The first slice where a cycle is possible, so the bounds from
slice 2 are exercised for real.

**Slice 5 — storage and tiering of returns.** Researcher output stored as Data
and Information, with provenance recorded from message traffic.

**Slice 6 — the claim.** Artifact assembly, provenance links, responsible-human
assignment, and submission to the existing proposed-knowledge queue in the right
state — `proposed` when the team agreed, `blocked on human` when it did not.

**Slice 7 — the surfaces.** Team tab, graph editor, run view, and the message
section on the tab and the home page.

**Slice 8 — the writing team.** The three roles as a shipped default team,
assembled from everything above.

## Test specification

The layers that matter here, and what belongs in each.

**Pure, no I/O.** Authorisation given a graph. Reachability. Bound arithmetic.
State transitions. Progress detection. Provenance assembly from a message list.
These are the majority of the correctness surface and none of them needs a
model.

**Contract.** `send_to` refuses an unauthorised target and records the refusal.
A claim without provenance is refused. A run cannot start without bounds.

**Integration.** A two-node delegation against a real store. A run resumed after
a runtime restart — the property that justified Decision 1, so it must be
tested, not assumed.

**Live, few and deliberate.** The writing team end to end, with a real model,
producing a claim. Expensive, so bounded and rare.

## Open questions

None blocking slice 1. Recorded so they are answered before the slice that
needs them:

- **Slice 5:** who authors a stored summary — the researcher's own words, or the
  runtime recording what it returned? Provenance integrity (T4) argues for the
  runtime.
- **Slice 7:** does the graph editor allow a graph with no terminal state, given
  cycles are legal? Refusing needs a definition of "terminal"; permitting needs
  the bounds to be visible in the editor.
- **Slice 8:** how does a team version interact with a run already in flight
  against the previous version?
