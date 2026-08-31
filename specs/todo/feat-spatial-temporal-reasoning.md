---
title: Spatial and temporal reasoning, and freeing the engine
executive_summary: >
  Three items. GeoJSON as a first-class thing a rule can reason over, to the
  whole of RFC 7946. Allen's interval algebra and RCC-8, which need one
  generalisation the vocabulary is missing. And extracting the engine from the
  memory crate, which a hand-made vendored copy has already proved possible.
status: todo
priority: P60
last_updated: 2026-08-30
---

# Spatial and temporal reasoning, and freeing the engine

## Checklist

- [x] **1. GeoJSON, the whole RFC.** All seven geometry types plus Feature and
      FeatureCollection, parsed by the `geojson` crate rather than by hand, and
      the DE-9IM predicate set over them from `geo`. `geo(S)` is the bridge and
      it is deliberately the same bridge `date(S)` is — geometry arrives as
      JSON text exactly as timestamps arrive as strings.

      Coordinates are `[longitude, latitude]`, which is the reverse of how
      people say it. Getting it backwards puts San Francisco in China and
      nothing about the result looks wrong, so it gets its own test.

- [ ] **2. Composition, of which transitivity is a special case.**
      `Characteristic::Transitive` generates `p(X,Z) :- p(X,Y), p(Y,Z).` That
      is the diagonal of a composition table. The general form —

      ```rust
      Composes { with: String, implies: String }
      ```

      — gives Allen's interval algebra and RCC-8 from the same generator, with
      `Transitive` becoming `Composes { with: self, implies: self }`.

      **The honest limit.** Full Allen composition is *disjunctive*: some
      entries yield "one of {before, meets, overlaps}". Datalog has no
      disjunctive head — `;` was added to the body, not the head — so full
      constraint propagation is a solver and not this engine. Only
      single-valued entries are generated, and the ones that are dropped are
      NAMED rather than silently missing.

- [ ] **3. Extract the engine into its own crate.** `ferrosa-experts` already
      vendors it, and `VENDOR.md` is the spec: it lists exactly what is
      `Storage`-coupled. Measured, that is **4 async functions and 59 lines**
      out of ~9,500.

      `ferrosa-datalog` takes the engine, filter parser, datalog types,
      ontology and blocks. `ferrosa-memory-core` depends on it and keeps the
      four functions as its storage adapter. `ferrosa-experts` drops `vendor/`,
      which closes the staleness the pin at `317a0a30` already has.

      The guard is `datalog_pre_negation_digest.txt`: a recording made before
      any of this work, which must still match byte for byte on the other side
      of the move.
