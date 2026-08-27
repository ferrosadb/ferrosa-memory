---
title: Rules plane — decision record
executive_summary: >
  Phase 0 decisions for a rules surface that shows every rule governing the
  corpus, explains why one path earned its tier, and lets new rules be written
  against a rule language that is still growing. Records what code
  reconnaissance already settled, what this grill resolves, and what is
  deliberately deferred.
status: in-progress
last_updated: 2026-08-26
validated_by: code reconnaissance 2026-08-26; grill-me Phase 0 in progress
---

# Rules plane — decision record

Scope: a Rules tab in the mobile and desktop app showing the rules that govern
the corpus, an explain-this-path view, an orphan browser, and rule authoring.
Spans `ferrosa-memory` (engine, storage, control frames) and `ferrosa-mobile`
(the tab itself).

## Already settled, by reading the code

Recorded so the grill does not spend questions on things the codebase has
already answered. Every claim below has a file and line.

### R1 — Rules are in the database, and `builtin()` is only a seed

Answering the question directly: yes, they are stored, not hardcoded.

`TierRules::builtin()` (`tiers.rs:246`) is a **seed list**, applied once by
`seed_tier_rules` (`tier_store.rs:300`) and by the `seed-tiers` binary. At
runtime `load_rules` (`tier_store.rs:381`) reads both tables back out of the
store, and `resolve` runs against what it read. Editing a stored row therefore
changes behaviour; editing `builtin()` only changes what a fresh seed writes.

### R2 — A tier rule is TWO rows, and either alone is inert

- `RootRule { root, tier, note, created_by }` — what a canonical root earns
- `RootAlias { alias_prefix, canonical_root }` — what lets a real path reach it

`~/src/research/skills` is Wisdom because an alias maps that prefix onto the
canonical root `research/skills`, and a rule gives that root `Tier::Wisdom`.
Neither row does anything by itself.

### R3 — Unreachable rules are the failure mode this screen exists to catch

A rule with no alias can never fire. The code carries the incident: the comment
at `tier_store.rs:326` records `session-capture -> data` sitting in the table
looking correct while 2,791 real paths beginning `session-capture/` resolved to
no root at all. A plain list of rules displays a broken rule and a working rule
identically.

Therefore the listing must show, per rule, the aliases that reach it, and sort
unreachable rules first. The inverse gap — an alias resolving to a root no rule
covers — is equally silent and equally a misconfiguration.

Implemented and tested in `ferrosa-memory-core/src/rules_view.rs`
(`tier_rule_rows`, `dangling_aliases`), 11 tests green.

### R4 — `TierReason::Default` is too coarse for a "why" screen

`resolve` (`tiers.rs:283`) folds "no alias matched" and "alias matched, root has
no rule" into one `TierReason::Default`. Correct for tiering — the outcome is
Data either way — and wrong for a screen whose whole job is *why*, because the
two have different fixes: add an alias, versus add a rule.

`rules_view::explain_path` keeps them apart as `NoAliasMatched` and
`RootHasNoRule` without altering what `resolve` does. `RootResolver::match_of`
(`tiers.rs:146`) already returns the alias that fired, and its own doc comment
gives the reason: *"when an item lands in the wrong tier, the question is always
which rule put it there."*

### R5 — Precedence is promotion, then root, then the Data floor

A person's `Promotion` outranks any rule. Data is the floor rather than a
derived answer, so "unclassified capture" and "classified as Data" are the same
tier and must not be the same statement on screen.

### R6 — There are two rule populations, not one

- **Tier rules** — `RootRule` + `RootAlias`, above, projected to datalog by
  `TierRules::as_datalog()` as `tier(E, "wisdom") :- source_root(E, "research/skills").`
- **Expert-system rules** — `RuleEntry` in the rule registry
  (`cql_storage.rs:6315` `rule_put`, `6403` `rule_list_active`, `6370`
  `rule_list_family`), carrying families and lifecycle states.

They are different tables with different shapes and different blast radius.
Editing a tier rule re-tiers a subtree of the corpus; adding a registry rule
derives new edges. The tab shows both and must not blur them.

### R7 — An orphan browser is an anti-join, which is negation

"Entities with no edges" cannot be written as a datalog rule today, for exactly
the reason recorded in `specs/todo/feat-datalog-stratified-negation.md`.

Orphan detection exists only as an in-memory lint (`enrich.rs:439`) over
`&[EntityEntry]` and `&[Edge]` — both fully materialised. At this corpus size
that is the materialising-read shape that has already caused OOMs, so the
browser needs a paged query rather than a reuse of the lint.

### R8 — A control frame kind that is not claimed closes the channel

`SHELL_KINDS` (`shell_extension.rs:1876`) carries its own incident note: four
`shell_knowledge*` kinds had handlers and were never listed, so each one fell
through to the built-in dispatcher, which does not know them and tears the
session down. Every new frame kind must be added to `SHELL_KINDS` *and* given a
handler; a test already holds that invariant.

## Constraints from the stakeholder

### C1 — The rule language is growing, not fixed

Negation is next (`t_64ea07e9`), and further operators follow. Any design that
freezes today's grammar into the client is wrong on the day the next operator
merges.

### C2 — A rule is a corpus item, and it can lapse

Rules are labelled as Knowledge and carry an optional expiry in the database, so
that a rule can be shared for a bounded number of days.

### C3 — A non-technical person must be able to classify information with it

The rules editor's target user is a CEO, not an engineer. Writing a rule is a
business act — deciding what counts as what — and the surface has to read that
way.

## Open — resolved by this grill

### D1 — The engine parses; the client never does

> **Extended by D7.** D1 settles who decides validity, and that answer does not
> change. D7 settles how a rule gets composed in the first place, which for a
> non-technical author cannot be by typing datalog.

**Decision.** A rule is authored as text and validated by the engine's own
`parse_rule`. The app posts the text and renders whatever error comes back,
with a position when there is one. The app contains no grammar.

Separately, the server advertises the operators it supports. That list drives
**affordances only** — autocomplete, a `not` button, an operator palette. It is
never a gate: an operator the app has never heard of is still typable, because
the app was never the thing deciding validity.

**Why.** The rule language is growing (C1). Any grammar compiled into the client
is wrong on the day the next operator merges, and the failure is not a clean
error — it is an old client that cannot write, or worse silently mis-renders, a
rule the engine accepts. That is the same shape as the device-kind mismatch that
broke Connections earlier today: two sides holding independent copies of one
vocabulary, one of them stale.

Only one copy of the grammar exists, and it is the one that does the parsing.

**Consequences.**

- Validation is a round trip. The editor must show pending state and must not
  claim a rule is good before the engine says so.
- An offline client cannot validate. It may compose and save a draft; it may not
  report a draft as valid.
- The advertised operator list is a hint, so a client that receives an empty or
  unrecognised list degrades to a plain text box rather than refusing to work.
- `parse_rule` becomes a contract surface. Its error text is now user-facing and
  needs positions, not just a message.


### D2 — Rules are stored; their conclusions are derived on read

> **Revised by D4.** The blanket "never materialise" below was narrowed: stable
> rules may materialise. The reasoning about *why* a stored conclusion is
> dangerous still stands and is what D4's conditions are built from.

**Decision (as first taken).** Writing a rule stores the rule. Reading
re-derives its conclusions from the facts as they are at read time. No derived
edge from a user-authored rule is written into the graph.

**Why.** A stored conclusion outlives the facts that justified it. Today the
engine is monotonic, so that is merely stale. Once negation lands it becomes
wrong: adding a fact can *falsify* a conclusion, and a cached derivation would
keep serving an edge that is no longer true. For a rule governing access, that
is access that should have been revoked.

Deriving on read means a rule can never answer with something that stopped being
true, and it means the rule language can grow without every past derivation
needing revisiting.

**Consequences.**

- Removes the largest risk in `specs/todo/feat-datalog-stratified-negation.md`
  §4. Invalidation of persisted derivations — up to and including DRed
  incremental view maintenance — leaves the critical path, because there is
  nothing persisted to invalidate. The negation work gets materially smaller.
- Reads pay for evaluation. `evaluate` already takes `max_iterations` and
  `max_facts` budgets; those become user-visible limits and must fail loudly
  rather than silently returning a partial derivation.
- A rule that is expensive is expensive on every read, so the tab needs to show
  what a rule costs, not only whether it parses.
- Rules authored here are deliberately NOT the same thing as the consolidation
  pipeline's derived facts, which continue to be stored. The tab must not
  present the two as one mechanism.

### D3 — A rule is Knowledge, and expiry is a clock fact rather than a state

**Decision.** A rule is a corpus item at tier Knowledge, not a separate class of
configuration object. It gets an entity, a tier, and everything that follows
from having one.

Expiry is stored as `expires_at: Option<DateTime<Utc>>` on the rule row and is
**derived at read time by comparing it to the server's clock**. It is not a
fourth `RuleState`.

**Why a rule is Knowledge.** It is the thing a person worked out, which is what
the tier means. It also makes rules shareable under the sharing floor
(`shareable(E) :- tier(E, "knowledge")`) without a special case — and a rule is
close to the ideal unit to share, because it transmits a way of reasoning rather
than the data reasoned over.

**Why expiry is not a state.** `RuleState` records authorial intent — someone
deprecated this, something superseded it. Expiry is a fact about the clock. If
"expired" were a state, something would have to sweep the table and write it,
which gives two writers for one truth and a window where the row says active and
the clock disagrees. Deriving it keeps one source of truth. This is the same
split already built for entitlements today: `StoredPlan` is what was written,
`PlanGrant` is what is true now, and only the second knows about lapsing.

**The clock is the server's.** A device may not decide whether a rule has
lapsed, for the same reason it may not decide whether a plan has. It renders
what it is told and re-asks.

**An expired rule stays visible.** It stops firing; it is not deleted and not
hidden. A rule that silently vanishes turns "my sharing stopped working" into an
unanswerable question, whereas a lapsed rule shown as lapsed answers it and
offers renewal.

**Consequences.**

- Expiry is nearly free under D2. Live derivation filters lapsed rules before
  evaluating, so nothing derived from a lapsed rule can survive it. Had
  conclusions been materialised, an expired rule's edges would have outlived it
  and needed sweeping.
- Two expiries can apply to one rule: the rule's own, and the one attached to a
  share of it (sharing S3 — chosen at share time, never blank). **The effective
  expiry is the earlier of the two.** Sharing a rule for 30 days cannot extend a
  rule that lapses in 7, and a share must never be able to outlive what it
  shares.
- Tier rules are the dangerous case. A lapsed tier rule silently re-tiers a
  subtree of the corpus to Data. See the open question below.
- Storage change: `RuleEntry` gains an optional column. Additive, defaulting to
  no expiry, so existing rows keep meaning what they mean.

### D4 — A rule materialises only if it is both non-expiring and monotonic

**Decision.** A rule whose conclusions are stable may write its derived edges
into the graph. Two conditions must both hold:

1. **Non-expiring** — `expires_at` is `None`. A rule that will lapse must not
   leave edges behind that outlive it.
2. **Monotonic** — the rule body contains no negated literal.

A rule failing either condition is evaluated live on every read, as D2
described. The choice is made from the rule's own contents and dates, recorded
on the rule, and shown in the tab — not inferred silently.

**Why the second condition.** Expiry and negation are independent axes, and only
one of them is about the clock. Materialising is safe for a monotonic rule
because adding facts can only ever add conclusions, so a stored edge can go out
of date but never becomes false. Negation breaks that: a new fact can *falsify*
a conclusion, and a materialised edge then asserts something untrue with no
event to remove it.

So "non-expiring" alone is the correct rule today — there is no negation to
worry about yet — and becomes incorrect on the day `t_64ea07e9` merges. Pinning
the monotonicity condition now means negation lands without silently converting
every stored derivation into a potential false edge.

**Consequences.**

- The materialisation predicate is a property of the rule, checked in code at
  write time and again before any materialising run. It is not a flag a person
  sets.
- Adding negation to an existing non-expiring rule changes its execution mode.
  Editing a rule must therefore be able to *retract* previously materialised
  edges — an edit is not only a new derivation.
- Likewise, adding an expiry to a materialised rule must retract its edges.
- Materialised edges must be attributable to the rule that produced them, or
  neither retraction above is possible.
- The negation spec's §4 (invalidation of persisted derivations) returns to the
  critical path in reduced form: not general DRed, but "retract this rule's
  edges", which is bounded by attribution.

### D5 — Every rule counts its fires

**Decision.** The memory system records how many times each rule has fired.

**Why.** It answers a question the structural checks cannot. R3 catches a rule
that is *unreachable* — nothing can match it. A fire count catches a rule that
is perfectly reachable and simply never matches anything, which looks identical
in a listing and is equally useless. Together they distinguish three states a
rules screen must not blur: broken, inert, and working.

It also tells the materialising path (D4) which rules are worth the write, and
gives a person renewing a lapsing rule something to decide on.

**Consequences.**

- The counter is on the hot path of derivation and must not be a synchronous
  write per fire. Aggregate in memory, flush periodically. A per-fire round trip
  to storage would put the cost of counting above the cost of deriving — and
  this codebase already paid for a hot-path write of that shape once, in
  `ConnectionTracker`, at roughly a fifth of total CPU.
- A count that is flushed periodically is approximate, and the screen must
  present it as such. "Fired ~1,200 times, as of 5 minutes ago" is honest;
  a precise-looking number that is quietly stale is not.
- Counts are per rule and per version. A rule edited into a new version starts
  its own count, or the number describes text nobody is running any more.

### D6 — A tier-rule change is previewed by count before it applies

**Decision.** The tab can edit tier rules and aliases, but never blind. A change
first reports how many entities move and between which tiers, and that number is
confirmed before anything is written. Registry rules need no such guard.

**Why.** The two populations differ by blast radius, not by kind. Editing
`research/skills -> wisdom` re-tiers a subtree; deleting an alias re-tiers
everything that alias reached, to Data, silently — which is precisely the
2,791-path incident in R3, arrived at deliberately instead of by omission.
Adding an alias is equally forceful in the other direction: it can make an inert
rule live and move thousands of items at once.

Tier is also what the sharing floor is built on, so a tier change is a
permissions change wearing different clothes.

**Consequences.**

- The preview is a dry run of the same resolution the write will perform, not a
  separate estimate. Two code paths would eventually disagree, and the one
  people trust is the one that does not run.
- It needs a count over the affected subtree without materialising it — the
  paging constraint from R7 applies here too.
- The preview can be stale by the time it is confirmed. It should carry the
  moment it was computed, and a large delta on apply is worth refusing rather
  than absorbing.
- Aliases get the same treatment as rules. An alias is half of a tier rule (R2),
  and guarding only the half labelled "rule" would leave the other half
  unguarded.

### D7 — Rules are composed from server-supplied vocabulary, not typed

> **Refined by D8.** The vocabulary is snap-together blocks rather than fixed
> sentences. D7's principle is unchanged: the server supplies what can be said,
> the client posts structure rather than syntax.

**Decision.** The editor presents rule-shaped sentences in plain language with
fillable slots, and the person picks and fills one. The client posts the
**template identifier and the slot values** — never generated syntax. The server
compiles that into a rule and validates it with `parse_rule` as D1 requires.

```
Anything under [ ~/src/research/skills ]  is  [ Wisdom ]
Anything tagged [ secret ]                is  [ never shared ]
Anything that is [ a claim ] with no [ approval ]  is  [ pending ]
```

A raw-datalog mode stays available for whoever wants it. It is the same
validation path, not a second one.

**Why not generate datalog in the client.** D1 removed the grammar from the app
because a client-side copy goes stale. Generating syntax puts it straight back:
a client that builds `tier(E, "wisdom") :- ...` knows the grammar, and knows it
as of the day it shipped. Posting a template id and values keeps the app
knowing *what a person meant* while the server keeps knowing *how to say it*.

The templates come from the server for the same reason. A new operator ships as
a new sentence, and every existing client can render it, because rendering a
sentence with slots requires no knowledge of what the sentence compiles to.

**Why sentences rather than a form built from a grammar description.** A grammar
description produces a form shaped like the grammar — atoms, arities, operators.
That is the machine's shape. The sentences are written per use case, in the
words of the person deciding, and there is no expectation that they cover
everything the engine can express. Coverage is what the raw mode is for.

**Consequences.**

- The catalogue of sentences is a product artifact with its own review. A
  sentence that is ambiguous in English produces a rule that is precise and
  wrong, and the person will not be able to tell.
- Each sentence needs its slots typed enough to offer real choices — a path
  picker for a path, the four tiers for a tier — or a CEO is typing free text
  into a form instead of into a text box, which is no better.
- Every sentence must state what it will do before it does it (D6), because the
  audience least able to predict a rule's blast radius is exactly this one.
- The two lints in R3 become plain-language warnings. "This rule can never
  match anything" is the message; unreachable-versus-dangling is the diagnosis
  behind it.
- The fire count (D5) becomes the primary evidence a rule works: *this rule has
  classified 1,204 items*. For this audience that sentence is worth more than
  the rule text.
- **The orphan browser is the worklist, not a debugging view.** Its job is
  "here is what your rules do not cover yet" — which is what makes writing the
  next rule an obvious act rather than an inventive one. It should be named and
  framed that way, not as "orphans".
- Several natural CEO sentences need negation — *tagged secret and never
  shared*, *a claim with no approval*. The plain-language surface is where the
  gap in `t_64ea07e9` becomes most visible, which is worth knowing when
  sequencing the two.

### D8 — The composer is block-based, in the shape of Scratch

> **Amended by D14.** D8 made the block tree the stored form and the text a
> projection. With an advanced text editor shipping alongside, that inverts:
> the text is authoritative and the tree is the projection.

**Decision.** Rules are built by snapping blocks together. Blocks carry shapes,
and a shape determines what it can connect to, so a rule that does not fit
together cannot be built. No syntax is typed at any point. The client posts the
**composition tree**; the server compiles it to a rule and validates it as D1
requires.

The block definitions — their shapes, their slots, and what may connect to what
— come from the server with the blocks.

**Why blocks rather than sentences.** Fixed sentences only cover what someone
thought to write down, which is why D7 carried a coverage obligation. Blocks
compose, so a person can build a rule nobody anticipated out of pieces that were
each anticipated. That converts most of the coverage problem from "write more
sentences" into "offer the right pieces", which is a much smaller and more
finite job.

It is also the right shape for the audience. Scratch's insight is that
*malformed should be unbuildable* — you do not validate a program you could not
have assembled wrongly. A person who cannot read datalog can still see that a
block does not fit.

**Why this does not put the grammar back in the client.** The client enforces
that a round peg goes in a round hole. It does not know what round means. The
fit rules arrive with the blocks, so a new operator ships as a new block with a
new shape and every existing client can lay it out correctly without knowing
what it compiles to. Validity is still decided by `parse_rule` on the server;
shapes only stop the obvious mistakes early, where the feedback is cheap.

**Negation is a wrapper block.** It is the standard Scratch shape for it — a
block that swallows another block — and it reads as "not this" without a word of
explanation. Worth noting as a target while `t_64ea07e9` is still being built:
the visual form of negation is settled before the engine form is.

**Consequences.**

- Block definitions become a versioned wire contract: shape, slots, fit rules,
  label. Adding a block must not disturb a client that has never seen it, and a
  client must render an unknown block as inert rather than dropping it — a rule
  silently missing a block on screen is worse than one that cannot be edited.
- A saved rule is stored as its tree, not only as compiled text, or it can never
  be reopened in the editor that made it. The compiled form is a projection.
- Round-tripping is a hard requirement and a good test: tree to datalog to tree
  must be stable, for every block in the palette.
- Shape checking on the client and parsing on the server can disagree. When they
  do, the server is right, and the client must show that answer rather than
  insisting the arrangement looked fine.
- The palette is the product surface now. What a person can express is what is
  in the palette, so gaps there are the coverage obligation from D7, relocated
  and much easier to close.

### D9 — The tab opens on the pile, and its job is to empty it

**Decision.** The Rules tab opens on unclassified material. The goal is not to
browse rules; it is to drive the unclassified count to zero.

**Consequences.**

- The count is the tab's headline number and wants a direction, not just a
  value. "3,107, down 312 today" is a job in progress; "3,107" is a fact.
- Rules are a spoke. Reachable in one tap, not the landing.
- A newly-arrived batch belongs at the top of the pile, because the pile is a
  worklist and the newest thing is the one nobody has judged.

### D10 — Buckets are user-defined; DIKW is only the default set

**Decision.** Data / Information / Knowledge / Wisdom is the **seed** bucket set,
not the vocabulary. A person may create their own — "financial knowledge",
"engineering information", "trash" — and classify into those.

Each bucket carries an underlying **rung** from the DIKW ladder. A user names
the bucket; the rung is what the machinery reads.

**Why keep a rung underneath.** Several things already depend on the tiers being
*ordered*, and would break against a flat set of arbitrary labels:

- The sharing floor is written `shareable(E) :- tier(E, "wisdom") | tier(E, "knowledge")`.
  With no order there is no floor, and "share skills but not raw capture" cannot
  be stated at all.
- `resolve` treats Data as a floor, meaning "unclassified" is representable.
  Arbitrary labels have no floor to fall to.
- The four-segment meter encodes position in an order. Without one it is
  decoration.

So "financial knowledge" is a *named bucket at the Knowledge rung*. It shares
Knowledge's shareability and Knowledge's precedence, and reads in the person's
own words. This is the smallest change that gives plain language without
discarding the lattice underneath.

**"Trash" is the interesting case.** It is not a rung — it means *do not surface
this*, which is an exclusion. Two ways to place it:

1. A bucket at or below the Data rung, plus a `suppressed` flag the read path
   honours. Expressible today.
2. A genuine exclusion in the rule language — which needs `t_64ea07e9`.

Recommend (1) now and (2) once negation lands, because "everything except
trash" is the exact shape negation exists for.

**Consequences.**

- The bucket set becomes stored, per tenant, and seeded from DIKW rather than
  hard-coded as four.
- The meter must render N buckets, not 4. It encodes the underlying rung, so
  two buckets at the same rung show the same meter — correct, and it means the
  bucket NAME must carry the distinction visually.
- Creating a bucket is itself a decision with blast radius, since a bucket at
  the wrong rung silently changes what is shareable.
- Copy: the tab says "buckets" in the person's words; the specs keep saying
  tiers/rungs for the ordered thing underneath. Two words on purpose, because
  they are two things.

### D11 — The verb is "classify"

**Decision.** The product says *classify*, not sort, tier, or tag. It is what
the system actually does, and precision beats familiarity here. Revisit the copy
if it proves confusing in use.

### D12 — Preview always, then apply

**Decision.** Every rule previews its effect and then applies it. There is no
threshold below which the preview is skipped, and no preview-only mode that
leaves the person to apply it separately.

**Consequences.**

- Preview cost is on the critical path of every classification, so it must be
  made fast rather than optional. See the paging constraint in R7.
- Preview and apply are one flow with one confirmation, not two screens.
- The preview can go stale between showing and confirming. It carries the moment
  it was computed, and a large delta on apply is worth refusing rather than
  absorbing.

### D13 — A rule shows its yield, and acts on it

**Decision.** Opening a rule shows **what it has classified**, and offers three
actions over exactly that set:

- **Delete the rule** — the classification it produced reverts; the items return
  to the pile. Nothing is destroyed.
- **Delete the data** — the classified items themselves are removed.
- **Share everything it classified** — the whole yield, as one share.

**Why these three together.** A rule is the handle people have on a body of
material. Once it names a set, every bulk act someone wants is an act on that
set, and making them find those items again by hand is busywork.

**Consequences.**

- The three actions have wildly different reversibility and must not look alike.
  Deleting a rule is undoable by re-creating it; deleting the data is not.
  Destructive weight has to be visible before the tap, not explained after it.
- Delete-the-data needs the same preview-by-count as D6, plus a stronger
  confirmation, and should be recoverable for a period rather than immediate.
- **Sharing by rule shares a LIVE set.** The rule keeps firing, so items
  classified tomorrow join a share consented to today. That is the feature —
  "share my engineering knowledge" should stay true — but it means the blast
  radius grows after consent, which no share preview can show. A share created
  from a rule must say so plainly and must be revocable as a unit.
- This is where the sharing work and the rules work meet: the share unit stops
  being a list of items and becomes a rule. Worth carrying into the sharing
  spec's open questions.

### D14 — Text is the rule; blocks are a projection of it

**Decision.** An advanced editor ships beside the block composer, taking raw
datalog for people who want the whole grammar. Both go through the same
server-side `parse_rule` (D1). The **stored rule is its text.** A block tree is
kept alongside it as a convenience, and is valid only while it still compiles to
exactly that text.

**Why this inverts D8.** D8 stored the tree so a rule could be reopened in the
editor that made it, and that reasoning was sound while blocks were the only way
in. Two editors over one artifact cannot both be the source of truth. The
grammar is also now being implemented in full — negation has landed, aggregates
are in flight — and a fixed palette cannot keep pace with a growing language, so
the palette will always express less than the engine accepts. The form that can
represent every rule has to be the stored one.

**Consequences.**

- A rule authored in blocks round-trips: tree to text to tree, stable. That
  remains a hard requirement and a good test for every block in the palette.
- A rule edited in the advanced editor may stop being representable as blocks.
  When that happens the tree is dropped, not silently half-kept, and the rule is
  shown as an advanced rule — read-only in the composer, editable as text.
- The composer must therefore be able to say **"this rule is beyond blocks"**
  without implying anything is wrong with it. Every block-based tool that has
  ever shipped a text mode has needed that state, and the ones that hid it
  taught people not to trust the blocks.
- Round-tripping is checked by the server, not asserted by the client: the tree
  is accepted only if compiling it yields the stored text byte-for-byte.
- D7's obligation is unchanged and is the reason this is safe. No task the
  product supports may *require* the advanced editor. It is for people who want
  to go further, never the answer to "how do I express this" — a gap there is
  still a defect in the palette.
- The advanced editor needs what any code editor needs and the block composer
  did not: the engine's error text with a position (already a consequence of
  D1), and somewhere to see what a rule currently matches before saving it.

## Deferred

Captured as work items rather than answered here.
