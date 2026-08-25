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

### Termination is a validity rule, not a runtime hope

A team with no terminal state **cannot be saved**. There are exactly three ways
to have one:

- exhausted attempts — a bound on cycles or messages,
- elapsed time — a wall-clock deadline,
- completion — an exit the graph can actually reach.

Because cycles are legal, a reachable exit is not implied by the shape of the
graph and must be checked. The editor runs the check and refuses, rather than
saving a team that will be discovered at run time to have no ending.

The check is a pure function of graph plus bounds: cheap, and testable without
running anything.

**Parking is not failure.** A parked run holds its state and posts a message
carrying the draft, the objection, and the provenance links. A human answers and
the run resumes. This is the same surface used to grill the user mid-run, which
is why the Team tab and the home page both show it.

The draft does not wait inside the parked run. It enters the queue immediately,
`blocked on human`, so that work in dispute is visible rather than buried in a
run nobody has opened.

## How a run ends

Both endings reach the same place. They differ in the state they arrive in and
what they carry:

| Run ended | Queue state | Carries | Next |
|---|---|---|---|
| writer and reviewer agreed | proposed | draft, provenance | responsible human reviews |
| bounds exhausted, disagreeing | blocked on human | draft, provenance, objection | responsible human breaks the tie |
| stopped by a person | stopped | every draft, provenance | archive, trash, or send on |

A stopped run's drafts go to the queue too. Stopping ends the run; it does not
throw the work away, and the drafts are usually the most expensive thing the run
produced.

What the queue offers a stopped draft is different from what it offers a
proposed one — it is not awaiting approval, it is awaiting a decision:

- **archive** — keep it, out of the way,
- **trash** — discard it,
- **send on** — hand it to another agent or another team for fine tuning.

Send-on is the interesting one: it makes an artifact something teams pass
between them, so a draft one team could not finish becomes another team's input.

There is no path from any of these to approved except through a person. **No artifact receives a green check without a person
acting** — agent agreement buys a better starting state, never the outcome.

Every claim names a **responsible human**. This is an assignment, not a pool: a
claim nobody owns is a claim nobody reviews, and it would sit in the queue
looking like progress.

## Running a team

The **active team page** is where a run is watched and steered. It shows the
graph, who is active, the message traffic, and every draft in flight.

### Pause and stop

Two controls, at two different levels, and a third that is not on this page at
all.

| Control | Level | Effect | Resumable |
|---|---|---|---|
| pause | the run | inter-agent delivery is deferred; agents keep working | yes |
| stop | the run | the run ends | no |
| interrupt | one agent | halts that agent, via the harness, from inside its session | yes |
| halt | team, user or org | stops EXECUTION within that scope | yes, by explicit release at that scope |
| kill | one run | terminates it and discards its work | no |

`halt all` and `kill` are break glass and behave unlike the rest — see "Break
glass" below.

**Pause stops the conversation, not the participants.** A message an agent sends
while paused is persisted and held, and goes out when the run starts again.
Nothing is refused and nothing is lost.

Two consequences follow, and the interface has to carry both:

- **Pause does not stop spend.** An agent mid-turn keeps thinking and keeps
  costing. Someone pausing to halt a runaway bill has reached for the wrong
  control — that is `stop`, or an interrupt per agent.
- **Resuming delivers a backlog.** Held messages go out together, so a run can
  get busier the instant it resumes than it was when it was paused.

Messages are addressed to a **role**, not to an agent, which settles what
happens when a teammate is swapped while messages are queued for it: the new
occupant receives them. That is the intended behaviour and the reason swapping
is useful — replace a struggling reviewer and it inherits the queue.

**Both are written.** A pause or stop held only in the runtime's memory is
undone by a restart, and a run that resumes because a process bounced has
ignored the person who stopped it. The state lives in the database and the
runtime reads it rather than remembering it.

**Both show every draft in flight**, on the active team page — every draft, not
the newest. Someone reaching for pause is usually asking what the thing looked
like before the last exchange, and a run that overwrites drafts cannot answer.
`team_artifact` is therefore append-only, each draft recording its version, its
author, and the turn that produced it.

### What each node is doing

The graph shows a state per active node:

| Colour | Meaning | What it asks of a person |
|---|---|---|
| green | working | nothing |
| orange | waiting for input | answer it |
| grey | dead | restart or swap it |

Three states rather than two, because "not working" is not one condition.
Waiting and dead both look like silence from outside, and only one of them wants
a human. `dead` must be something the runtime establishes — a crashed harness, a
lost session, an unreachable node — never inferred from a gap in traffic, or a
slow agent becomes a dead one on a busy day.

### Swapping a teammate

The definition is locked at creation (ADR-009 Decision 7). The roster is not.

A teammate may be replaced when both hold:

1. the team is **paused**, and
2. that teammate is **not active** — it holds no unanswered turn.

Swapping mid-turn would orphan a reply with nowhere to land, which is why the
idle precondition is a refusal rather than a warning.

Because occupancy can change, provenance records **which occupant** produced
each turn. "The writer said this" stops being a single agent the moment a swap
is permitted.

### Sessions

Every teammate has a session a human can enter, writing into the same transcript
the team is using. This is what makes pause useful rather than merely stopped:
pause, enter the reviewer's session, find out what it actually objects to, then
resume or swap.

Two rules keep the record honest:

- **Human turns are attributed to the human.** An unmarked human turn lets a
  person's paragraph ship as agent-team output — the inverse of the laundering
  Decision 5 prevents, and just as wrong.
- **A human turn spends budget.** The tokens are real.

### Break glass

Two global controls for when something is wrong rather than merely unwanted.

**Halt** stops execution. Not delivery — execution. This is the control `pause`
is repeatedly mistaken for, and the one that actually stops the bill.

It is scoped at three levels: one **team**, every team a **user** manages, or an
entire **organisation**. Org scope arrives with human teams collaborating with
agent teams; it is not built first, but the scope field exists from the start,
because retrofitting a scope onto a boolean means revisiting every read of it.

Halt is therefore not a switch but a **hold**, and holds stack:

- a team may be held at team, user and org level simultaneously,
- execution requires **zero** holds covering it,
- so releasing one hold resumes nothing while another still applies.

**Release authority must match or exceed the hold's scope.** A user cannot
release an org halt. Without that rule the org control is advisory, and an
advisory break-glass control is not one.

**A halted team names the hold that stops it and who set it.** "Halted" alone
sends a person to a button that will refuse them; "halted by your organisation"
sends them to a person. That is the difference between a control that explains
itself and one that looks broken.

**Kill** ends a run and discards its work. Unlike halt it takes no scope — it
names one run, because a control that destroys work should never accept a
wildcard.

#### What halt can actually do depends on the sandbox

A managed sandbox can often be suspended and resumed — frozen mid-execution,
continued where it stopped. Most backends can; not all. Suspend is declared per
backend, like every other capability, and the run says which behaviour it will
get *before* someone reaches for the control:

| Backend | Halt | Resume |
|---|---|---|
| suspend-capable | freezes execution in place | the turn continues from where it stopped |
| not suspend-capable | abandons the turn in flight | the turn restarts from its last durable point |

This is why "halt stops the bill" needs a qualifier. A suspended sandbox stops
consuming; an abandoned turn has already spent what it spent, and restarting it
spends again. Without suspend, halt bounds future cost rather than eliminating
it.

**Resume re-establishes rather than assumes.** A sandbox frozen for an hour
wakes to timed-out connections, possibly expired credentials, and a memory
server that may have restarted. Resume reconnects and fails loudly if it cannot,
rather than continuing against a stale handle — the same class of fault as
holding a peer address frozen at startup.

| | Halt | Kill | Stop |
|---|---|---|---|
| running agents | stopped, resumable | terminated | finish, then end |
| held and in-flight messages | preserved | discarded | preserved |
| drafts | preserved | discarded | **go to the queue** |
| run record and audit trail | preserved | preserved | preserved |

Three rules apply to both and to nothing else:

**Written first, acted on second.** State reaches the database before anything
is torn down. A break-glass control that lives in a process is worthless exactly
when it is needed, because whatever made someone reach for it may be what
restarts the process. On restart the runtime reads a halt and stays halted.

**Release is explicit.** Nothing resumes on a timer or because a process came
back. Someone halted everything deliberately; the system does not decide when
that intent has expired.

**The record outlives what they destroy.** Kill discards drafts; it does not
discard the fact that a run existed and was killed, or who did it and why.
Otherwise a killed run and a run that never happened look identical, and the
most consequential action in the system leaves the least evidence.

Kill confirms before acting, and names what will be lost — the run, and how many
drafts — rather than warning generically. It exists for work that must not be
kept: bad data, a prompt injection that took, output nobody should act on. If it
merely stopped, that work would be waiting in the queue.

## Topology

Execution does not run inside the memory server. It runs in **coordinator
processes**, in the shape of the streamer: installed, supervised, signed, and
responsible for the machines they can reach.

    Ferrosa Memory        state, authorisation, transcripts, artifacts, holds
        |
        |   ..... WebRTC control channel (the streamer's) .....
        |
        +-- host coordinator          container runtime on that host
        |
        +-- delegate coordinator      cloud credentials; itself a coordinator
                |
                +-- cloud VMs

Coordinators speak to the database and to agents over the **same WebRTC control
channel the streamer uses**. They are peers on that plane, so a coordinator does
not need to sit beside a database node.

What it does need is direct control of whatever runs sandboxes — **cloud
credentials, or host access to the container runtime**. The adjacency
requirement is therefore inverted from the obvious one: *near the runtime, far
from the database is fine; the reverse is useless.* A coordinator with a perfect
database connection and no way to start a container coordinates nothing.

Coordinators **nest**. A delegate takes direction from above and coordinates the
machines below it, so "the coordinator" is a role rather than a singleton.

The split follows the line this project already draws: decisions that say who
may do what stay in the audited half, platform mechanics do not.

| Concern | Where |
|---|---|
| team state, transcripts, artifacts, holds | Ferrosa Memory |
| `send_to` authorisation | Ferrosa Memory |
| running sandboxes and VMs | the coordinator |

### Leases, and why break glass still works

A halt is written before it acts. In one process that is enough. Across a tree
it is not: a delegate that cannot reach the database cannot see the halt, and a
coordinator that cannot see a halt is one still running agents.

So **every coordinator executes on a lease** — a short, renewable right to run,
renewed against the database. If renewal fails, for any reason, the coordinator
stops by itself.

A halt is therefore enforced by **absence rather than delivery**. Cutting a
coordinator off is as effective as telling it to stop, which inverts the usual
failure mode where losing contact means losing control.

The cost, stated rather than discovered: a database outage stops agent work.
For a system that spends money and touches the internet, that is the correct
direction to fail.

Renewal travels over WebRTC, which puts the lease behind a transport whose
failure modes are known here and have been observed on this system: ICE failing
mid-session with throughput at zero and no packets reported lost; inbound UDP
dropped by a host firewall, which fails only on the local network; a TLS
expectation mismatch that hangs rather than refusing.

Each expires a lease and stops a coordinator — correct, and it makes the lease
duration a real decision:

- **too short**, an ordinary reconnect stops agent work
- **too long**, a coordinator that has been cut off keeps spending

Pick it deliberately and **render an expiry as an expiry**. In this system that
class of failure has previously surfaced as "the display is blank" and "the
stream stalled", and a coordinator that stopped because it lost its lease should
say exactly that.

### A coordinator is as privileged as what it can provision

The sharpest security consequence in this design, and nothing else here
mitigates it.

`send_to` authorisation lives in Memory and is audited, but it governs which
agent may ask which other agent for work. It says nothing about what a
*coordinator* may do, because the coordinator is downstream of that decision —
it executes what was already authorised.

| Coordinator holds | Effective privilege |
|---|---|
| a container socket | run anything on that host |
| cloud credentials | create machines, execute code on them, spend money |

Compromise one and the graph's authorisation is irrelevant. Three controls:

1. **Credentials scoped to provisioning** — create and destroy this
   coordinator's sandboxes, nothing else. Not a general-purpose cloud identity.
2. **Privilege declared and visible**, like the node capabilities, and for the
   same reason. "This coordinator can start containers on your laptop" is
   something an operator should be told once, plainly.
3. **Actions recorded where the coordinator cannot edit them** — in Memory, on
   the run, like every other control.

### What survives what

| Event | Run state | Execution |
|---|---|---|
| the client closes | survives | continues |
| the laptop closes | survives | stops for host sandboxes, continues in the cloud |
| a coordinator dies | survives | resumes when it returns, or another takes the lease |
| the database is unreachable | survives | **stops everywhere**, by lease expiry |

### Operational requirements

A signed, supervised process on a user's machine inherits problems this project
has already solved once, in the streamer:

- sign with a stable identity, or every update loses its permissions
- launch so the operating system attributes it correctly, rather than executing
  the binary inside the bundle
- re-sign when replacing it in place, or it is killed on next start
- allowlist inbound traffic where needed, and say plainly when it is not —
  otherwise it fails only on the network where it should work best

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

### Provenance survives a handoff

A draft sent on to another team carries where it came from. The chain is
attributed by origin rather than flattened: inherited evidence stays marked as
gathered by the earlier team, in that run, at that time.

This is what makes send-on safe. Absorbed provenance would let the receiving
team's claim cite sources indistinguishable from its own work, and would end the
audit trail at exactly the handoff where it most needs to continue.

## Data model

Seven tables. Names are indicative; the shapes are the commitment.

    team              definition, locked at creation
    team_node         role, model, system prompt, declared capabilities
    team_edge         from, to, condition prompt, message template
    team_run          pinned team version, starting prompt, bounds, state
    team_message      the transcript: from, to (a ROLE, not an agent), body,
                      timestamp, refused flag, held-during-pause flag, and
                      whether the author was an agent occupant or a human
    team_artifact     APPEND-ONLY drafts and the final claim, each with version,
                      author, originating turn, provenance links, queue state
                      and responsible human
    team_occupancy    which agent occupies which role, over time, so a turn can
                      be attributed after a swap
    team_node_state   working / waiting / dead, per node, established by the
                      runtime rather than inferred from silence
    team_control      pause, resume, stop, kill: who, when, why -- written
                      BEFORE the effect, so a restart cannot resume what a
                      person halted, and a kill leaves a record of itself
    coordinator       one row per coordinator: parent, scope, declared sandbox
                      backends, declared privilege (container socket / cloud
                      credentials), and its lease expiry
    sandbox_backend   per backend: declared capabilities, including whether it
                      can suspend and resume
    halt_hold         one row per active hold: scope (team/user/org), the
                      subject it covers, who set it, when, why. Execution needs
                      zero rows covering a run; release deletes one row and
                      requires authority at that scope
    team_session      the per-teammate session a human can enter
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
| T6a | **Human work published as agent-team output.** A person writes a paragraph in a teammate's session; the claim ships it unattributed. | Human turns are marked in the transcript and in draft authorship. Same failure as T6, pointing the other way. |
| T7 | **Compromised coordinator.** It holds a container socket or cloud credentials; graph authorisation does not constrain it. | Provisioning-scoped credentials, declared and visible privilege, and an action record it cannot edit. Not fully mitigated — a coordinator is trusted with what it can provision. |
| T8 | **Lease starvation as denial of service.** Disrupting the control channel stops all agent work. | Accepted: failing closed is the intended direction. Lease duration is tuned so an ordinary reconnect does not trigger it. |
| T6c | **Sent-on draft carries borrowed authority.** A stopped draft handed to another team arrives with provenance its new authors did not gather. | Provenance travels but is attributed by origin: inherited evidence renders as inherited, naming the team and run that gathered it. A claim cannot present another team's work as its own. |
| T6b | **A stopped run resumes.** Pause or stop held in memory is lost to a restart. | Control state is written; the runtime reads it rather than remembering it. |
| T6 | **Claim laundering.** Agent agreement is read as human approval. | Structural, not procedural: there is no transition from any agent-produced state to approved. Both terminal states await a named responsible human, and the green check is only ever awarded by one. |

## Failure modes

| Mode | Effect | Detection | Mitigation |
|---|---|---|---|
| Node never returns | Run wedged, budget intact | wall-clock bound | park |
| Two agents converge on nothing | Budget drains, artifact unchanged | no-progress bound | park |
| Runtime restarts mid-run | Run lost, if state was in memory | — | state is in the database; a run is resumable by construction |
| Deadlock undetected | Run parks, nobody notices | message surface on Team tab and home | the surface is the mitigation, which is why it is in scope |
| Claim without provenance | Unauditable knowledge | schema requires links | refuse to publish a claim with none |
| Pause mistaken for a spend brake | Bill keeps growing while the operator believes it stopped | — | the control names what it does; `halt all` is the global brake, `stop` and interrupt the narrower ones |
| Halt released by a restart | Everything resumes after someone halted it on purpose | — | halt is written before it acts, and release is explicit |
| A narrow release undoes a broad hold | A user resumes work an org halted | — | holds stack and release requires authority at the hold's scope |
| Lease expires on a transient reconnect | Agent work stops during ordinary network noise | expiry is rendered as expiry | lease duration tuned above reconnect time |
| Coordinator far from its runtime | Coordinates nothing despite a healthy database link | — | adjacency is to the runtime, and is a placement requirement not a preference |
| Coordinator keeps running after a halt it never saw | Break glass has a hole where nobody can look | lease not renewed | the coordinator stops itself; enforcement is by absence, not delivery |
| Database outage stops all agent work | Teams idle during an incident | lease expiry | intended, and documented rather than discovered |
| Delegate outlives its parent | Orphaned cloud VMs spending money | lease not renewed | same lease rule applies at every level of the tree |
| Halt assumed to freeze on a backend that cannot | Turn discarded when the operator expected it preserved | — | the run states which behaviour it will get before the control is used |
| Resume continues against a stale handle | Silent failure against a connection that died while frozen | — | resume re-establishes dependencies and fails loudly |
| Halted team shows no reason | Person retries a release that will refuse them | — | the hold, its scope and who set it are shown |
| Kill leaves no trace | A killed run is indistinguishable from one that never ran | — | the record survives what kill destroys |
| A slow node reported dead | Person restarts healthy work | — | dead is established by the runtime, never inferred from a gap in traffic |
| Resume delivers a backlog at once | Sudden load spike on resume | — | expected; bounds still apply, and held messages are visible while paused |
| Swap attempted mid-turn | Orphaned reply, corrupted transcript | occupant is active | refuse the swap; it is a precondition, not a warning |
| Draft overwritten | "What did it look like before?" is unanswerable at the moment it is asked | — | artifacts are append-only |
| Team edited under a running run | Transcript no longer readable against its graph | — | definition locked at creation; editing makes a new team |
| Graph with no ending saved | Discovered only at run time, after spending | editor validity check | refuse to save |
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

**Slice 3 — the coordinator, and one agent executed.** A coordinator process
that takes a lease, renews it, and stops when it cannot. A single node runs
against a prompt and returns. No delegation, no nesting. The lease behaviour is
pure and testable ahead of the process existing.

**Slice 4 — `send_to`, end to end.** Two nodes, one edge, a real delegation and
a real refusal. The first slice where a cycle is possible, so the bounds from
slice 2 are exercised for real.

**Slice 5 — storage and tiering of returns.** Researcher output stored as Data
and Information, with provenance recorded from message traffic.

**Slice 6 — the claim.** Artifact assembly, provenance links, responsible-human
assignment, and submission to the existing proposed-knowledge queue in the right
state — `proposed` when the team agreed, `blocked on human` when it did not.

**Slice 7 — run control.** Pause, resume, stop, written; occupancy swap with
its idle precondition; append-only drafts. All pure state-machine work with one
storage dependency, and all testable without a model.

**Slice 8 — the surfaces.** Team tab, graph editor including the terminal-state
check, the active team page showing drafts in flight with pause and stop, the
per-teammate sessions, and the message section on the tab and the home page.

**Slice 9 — nesting.** A delegate coordinator that is itself a coordinator, and
cloud VMs beneath it. Deferred until a single coordinator works, because the
lease rule is what makes nesting safe and it should be proven at one level
first.

**Slice 10 — the writing team.** The three roles as a shipped default team,
assembled from everything above.

## Test specification

The layers that matter here, and what belongs in each.

**Pure, no I/O.** Authorisation given a graph. Reachability. Bound arithmetic.
Run state transitions, including pause, resume and stop. Message hold and
release: a message sent while paused is persisted, not delivered, and goes out
on resume — in order. Role addressing across a swap: a message queued for a role
reaches whoever occupies it at delivery. Progress detection.
Provenance assembly from a message list. The terminal-state validity check, in
all three of its forms and in the negative case that must be refused. The swap
preconditions -- paused, and the occupant idle. Attribution of a turn to the
occupant that held the role at the time.

These are the majority of the correctness surface and none of them needs a
model, a cluster or a browser.

**Contract.** `send_to` refuses an unauthorised target and records the refusal.
A claim without provenance is refused. A team with no terminal state cannot be
saved. A claim with no responsible human cannot be enqueued. A swap against an
active occupant is refused. A stopped run's drafts reach the queue; a killed run's do not, and its record
still does. A halt survives a runtime restart and releases only explicitly. Holds stack:
with a team hold and an org hold present, releasing the team hold leaves the run
halted. A release attempted below the hold's scope is refused. Node state
is written by the runtime and never derived from silence.

**Integration.** A two-node delegation against a real store. A run resumed after
a runtime restart — the property that justified Decision 1, so it must be
tested, not assumed. A run **stopped** before a restart and still stopped after
it, which is the property Decision 9 exists for. A coordinator whose lease
cannot renew stops on its own, which is the property that makes break glass
survive a partition. Drafts retained across a pause,
so the active team page can show every one.

**Live, few and deliberate.** The writing team end to end, with a real model,
producing a claim. Expensive, so bounded and rare.

## Open questions

None blocking slice 1. Recorded so they are answered before the slice that
needs them:

- **Slice 5:** who authors a stored summary — the researcher's own words, or the
  runtime recording what it returned? Provenance integrity (T4) argues for the
  runtime.
- **Slice 7:** entering a session does not pause the run — pause is a run-level
  control and interrupt is the per-agent one — but should entering one *offer*
  the interrupt prominently? A person opening a session usually wants the agent
  to stop talking first.
- **Slice 1:** org-scope halt implies an organisation model — accounts, roles,
  and who counts as an org authority. None of that exists yet. The scope field
  and the stacking rule can be built now; the org *level* waits on it.
