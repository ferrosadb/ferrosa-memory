---
title: ADR-009 Semantic Specificity and Calculated Fact Confidence
executive_summary:
  purpose: >-
    Prevents overloaded quantitative names from crossing persistence and API
    boundaries, distinguishes search ranking from ontology evidence, and defines
    fact_confidence as a versioned calculated projection over retained evidence.
  critical_items:
    - New public schemas use subject- and dimension-specific quantitative names.
    - search_confidence is ranking evidence and never proposition truth.
    - fact_confidence is a reproducible calculated field, not canonical evidence.
    - Any Ferrosa WASM implementation is written in Rust and versioned as a method.
    - Existing ambiguous names remain data-preserving compatibility surfaces only.
status: accepted
date: 2026-08-19
---

# ADR-009: Semantic Specificity and Calculated Fact Confidence

## Context

Ferrosa Memory currently uses `confidence` for several different concepts:
entity ingestion gates, temporal observations, search and prediction results,
claims, derived facts, durable materialization, and a fact-history heuristic.
Those values do not answer the same question. A client cannot safely determine
whether a bare `confidence` value describes retrieval relevance, extraction
quality, proposition support, rule strength, or an operational default.

The ontology verification plane needs to preserve typed component measurements
while also exposing a convenient current assessment for a versioned fact. The
database has a sandboxed WASM UDF boundary that can calculate this projection
close to the stored inputs. This is also a useful end-to-end exercise of
Ferrosa's UDF capability, provided the calculation remains reproducible,
bounded, and replaceable.

## Decision

### Increase specificity at semantic boundaries

When a name has multiple plausible meanings, qualify it by subject, dimension,
scope, or method until a consumer can interpret it without inspecting its
producer. This applies to CQL columns, public Rust records, HTTP and MCP
schemas, ontology predicates, event fields, and user-facing labels.

New public quantitative fields must not use the bare names `confidence`,
`score`, `trust`, `quality`, or `risk`. Examples of accepted names include:

```text
search_confidence
fact_confidence
retrieval_similarity
source_trust
agent_task_trust
extraction_quality
rule_strength_score
action_risk
```

A generic `value` is permitted inside a typed measurement record because its
dimension, family, scale, status, method, subject, and provenance supply the
semantic context. A lifecycle `status` is permitted when its enclosing resource
unambiguously identifies the lifecycle.

The same rule applies to ontology refinement. When a valid counterexample
breaks an overbroad rule, preserve and challenge that rule, then add explicit
domain, temporal, jurisdictional, population, or other evidence-backed scope to
any narrower replacement. Do not hide the counterexample behind an unnamed
exception.

### Separate search confidence from fact confidence

`search_confidence` describes the output of retrieval, matching, ranking, or
entity-resolution search. It may help choose what to inspect. It is not source
trust, fact support, rule strength, or permission to promote an assertion.

`fact_confidence` describes the current epistemic assessment of one immutable,
versioned fact within an explicit context and valid-time scope. It is a
calculated projection over compatible fact-evidence inputs. The component
measurements and their provenance remain canonical and independently
queryable.

`fact_confidence` must carry or resolve to:

- the fact and fact-version identifier;
- context and valid-time scope;
- calculation method ID and semantic version;
- input score and evidence identifiers;
- calibration version, when applicable;
- calculation time and implementation artifact digest; and
- an uncertainty interval or explicit limitation when the method supports one.

The value is not a certification of truth. Repetition from correlated sources
must not count as independent support. Search confidence, action risk, review
status, and unrelated judged workflow scores are excluded from the calculation.
Promotion policy evaluates those separate dimensions in addition to, rather
than through, `fact_confidence`.

### Calculate through a versioned Rust/WASM method

The first database-side implementation will be a deterministic Rust guest
compiled to the Ferrosa UDF WASM component contract. JavaScript,
AssemblyScript, and npm-based guest build pipelines are not permitted for this
calculation. The Rust toolchain, component ABI, source revision, compiler flags,
and resulting WASM digest are pinned in the method record.

The initial proof uses a scalar CQL UDF over explicit numeric inputs and returns
a bounded `double`. A native Rust oracle using the same formula must produce
equivalent results for fixed fixtures and boundary/property cases. The UDF must
fail loud on missing, non-finite, out-of-range, or semantically incompatible
inputs; it must not manufacture defaults for absent evidence.

Canonical component measurements are stored before calculation. The query path
may expose:

```text
fact_confidence_v1(component inputs...) AS fact_confidence
```

Materialization is an optimization only after the scalar UDF path is verified.
Any materialized value includes its method version and input-set digest and is
invalidated when an input, fact version, or calculation method changes.

Changing the formula creates a new method version and a reproducible
recalculation job. It never rewrites the historical component measurements.

### Preserve compatibility without extending ambiguity

Existing CQL rows and API fields remain readable during a versioned migration.
New canonical writes use the specific vocabulary. Compatibility responses may
derive a deprecated legacy `confidence` alias from the new field only when the
old endpoint's meaning was already singular and documented; otherwise the
legacy field remains unchanged until its consumer is migrated.

Primary-key or incompatible schema changes require the repository's normal
staging/copy/verification migration. No existing rows are silently dropped or
reinterpreted. The migration plan will classify every current occurrence before
renaming it; a mechanical global replacement is prohibited.

## Enforcement

`scripts/semantic_name_lint.py` scans CQL table columns, public Rust record
fields, and MCP tool schema properties. `config/semantic-name-baseline.json`
contains counted legacy exceptions. Removing an exception is allowed; adding
or increasing one fails CI and requires an explicit update to this ADR or a
successor decision.

The lint intentionally enforces new-boundary discipline before the full
compatibility migration is complete. It does not claim that the baseline names
are semantically correct.

## Consequences

- Clients can distinguish search behavior from ontology evidence without
  producer-specific knowledge.
- Fact confidence is convenient to query but remains reproducible from retained
  evidence.
- Formula changes and recalculation are explicit versioned operations.
- Promotion remains a policy decision over named dimensions, not one numeric
  threshold.
- The Rust/WASM spike tests Ferrosa UDF execution with a consequential but
  deterministic workload.
- Existing consumers require a staged API and migration plan rather than an
  unsafe global rename.

## Rejected alternatives

- **Keep bare `confidence` and document it per endpoint:** consumers still
  cannot safely combine or compare values, and documentation drift is silent.
- **Store only one composite confidence:** destroys component meaning and makes
  recalculation, audit, and policy changes irreproducible.
- **Calculate only in Workbench or another client:** creates competing formulas
  and prevents CLI, API, and alternate UI clients from observing the same value.
- **Make the WASM output canonical evidence:** couples historical truth to one
  formula version and loses the inputs needed for correction.
- **Use an npm-based WASM guest toolchain:** conflicts with the project's Rust
  implementation boundary and adds unnecessary supply-chain surface.

## Verification

1. Semantic-name lint unit fixtures reject new bare names and accept qualified
   names.
2. The repository scan passes only within the counted legacy baseline.
3. A later Rust/WASM work item must compare native and UDF results over normal,
   missing, boundary, non-finite, contradiction, and correlated-source cases.
4. A version-change test must show that the same component evidence can be
   recalculated under two method versions without overwriting either result.
5. API contract tests must prove that search confidence cannot populate fact
   confidence, source trust, rule strength, or promotion eligibility.
