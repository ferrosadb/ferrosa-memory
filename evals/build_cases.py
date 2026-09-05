#!/usr/bin/env python3
"""Derive known-item eval cases from the real corpus.

Three families, because the PR claims to affect all three:

  entity        — recall a specific entity by distinctive words from its snippet
  long_context  — recall a chunk from deep inside a LONG document (not chunk 0),
                  which is where long-context retrieval actually gets hard
  workspace     — entities that carry a cwd/workspace property, so the
                  workspace boost has something to score; the eval can then ask
                  whether supplying a cwd helps or merely reshuffles
"""
from __future__ import annotations

import argparse
import json
import random
import re

from cassandra import ConsistencyLevel
from cassandra.cluster import Cluster
from cassandra.policies import WhiteListRoundRobinPolicy
from cassandra.query import SimpleStatement

STOP = {
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "then",
    "which", "their", "there", "have", "has", "was", "were", "are", "not", "but",
    "its", "you", "your", "can", "will", "would", "should", "could", "using",
}
WS_KEYS = ("cwd", "workspace", "working_directory", "repo", "repository")


def looks_like_identifier(w: str) -> bool:
    """UUID fragments, hex blobs and snake/camel ids are not what anyone types."""
    if re.fullmatch(r"[0-9a-fA-F]{6,}", w):
        return True
    if re.search(r"\d{4,}", w):
        return True
    if "_" in w or "-" in w:
        return True
    digits = sum(c.isdigit() for c in w)
    return digits > len(w) / 3


def salient(text: str, n: int) -> list[str]:
    """Distinctive words a person would plausibly type.

    Drops the first token: a chunk usually begins mid-word ("ries stimuli"),
    and a fragment makes the query unanswerable for reasons that have nothing
    to do with fusion. Drops identifiers for the same reason -- matching a UUID
    exercises the lexical index, not retrieval quality.
    """
    words = re.findall(r"[A-Za-z][A-Za-z0-9_-]{3,}", text or "")[1:]
    seen, out = set(), []
    for w in words:
        lw = w.lower()
        if lw in STOP or lw in seen or looks_like_identifier(w):
            continue
        seen.add(lw)
        out.append(w)
        if len(out) >= n:
            break
    return out


def connect(port: int):
    c = Cluster(
        contact_points=["127.0.0.1"], port=port,
        load_balancing_policy=WhiteListRoundRobinPolicy(["127.0.0.1"]),
        protocol_version=4, connect_timeout=15,
    )
    return c, c.connect()


def q(session, cql: str, timeout: int = 300):
    return list(session.execute(
        SimpleStatement(cql, consistency_level=ConsistencyLevel.ONE, fetch_size=5000),
        timeout=timeout,
    ))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=19042)
    ap.add_argument("--per-family", type=int, default=60)
    ap.add_argument("--terms", type=int, default=6)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=20260822)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    cluster, s = connect(args.port)
    cases: list[dict] = []

    # --- entity + workspace families -------------------------------------
    rows = q(s, "SELECT entity_id, entity_name, context_snippet, properties, "
                "entity_type FROM agent_memory.entity_store")
    plain, ws = [], []
    for r in rows:
        snippet = r.context_snippet or ""
        if len(snippet) < 80:
            continue                      # too short to make a fair query
        # `turn` entities are raw conversation JSON: querying them tests
        # whether the index can match a UUID, not whether retrieval is good.
        if (r.entity_type or "") == "turn":
            continue
        props = r.properties
        if isinstance(props, str):
            try:
                props = json.loads(props)
            except json.JSONDecodeError:
                props = {}
        cwd = None
        if isinstance(props, dict):
            for k in WS_KEYS:
                v = props.get(k)
                if isinstance(v, str) and v:
                    cwd = v
                    break
        (ws if cwd else plain).append((r, snippet, cwd))

    for r, snippet, _ in rng.sample(plain, min(args.per_family, len(plain))):
        terms = salient(snippet, args.terms)
        if len(terms) >= 3:
            cases.append({
                "query": " ".join(terms), "target_id": str(r.entity_id),
                "kind": "entity",
            })

    for r, snippet, cwd in rng.sample(ws, min(args.per_family, len(ws))):
        terms = salient(snippet, args.terms)
        if len(terms) >= 3:
            cases.append({
                "query": " ".join(terms), "target_id": str(r.entity_id),
                "kind": "workspace", "cwd": cwd, "workspace_of_target": cwd,
            })

    # --- long-context family ---------------------------------------------
    # Only documents with many chunks, and only chunks from the BACK half:
    # retrieving chunk 0 of a long document proves nothing about long context.
    chunks = q(s, "SELECT document_id, ordinal, chunk_id, content, token_count "
                  "FROM agent_memory.document_chunks")
    by_doc: dict[str, list] = {}
    for c in chunks:
        by_doc.setdefault(str(c.document_id), []).append(c)
    long_docs = [v for v in by_doc.values() if len(v) >= 8]
    rng.shuffle(long_docs)
    for doc in long_docs[: args.per_family]:
        doc.sort(key=lambda c: c.ordinal or 0)
        deep = [c for c in doc[len(doc) // 2:] if len(c.content or "") > 200]
        if not deep:
            continue
        c = rng.choice(deep)
        terms = salient(c.content, args.terms)
        if len(terms) >= 3:
            cases.append({
                "query": " ".join(terms), "target_id": str(c.chunk_id),
                "kind": "long_context",
            })

    cluster.shutdown()
    with open(args.out, "w") as fh:
        json.dump(cases, fh, indent=1)

    from collections import Counter
    print(f"{len(cases)} cases -> {args.out}")
    print(" ", Counter(c["kind"] for c in cases).most_common())


if __name__ == "__main__":
    main()
