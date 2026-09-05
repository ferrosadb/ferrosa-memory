#!/usr/bin/env python3
"""A/B eval for hybrid-search fusion, against the real ferrosa-memory cluster.

Answers three questions the PR raises and none of the unit tests can:

  1. Does it genuinely USE fusion, or does one source dominate?
  2. Does it clutter the context window?
  3. Does it retrieve better from long documents?

Ground truth is derived from the corpus itself (known-item retrieval): a query
is built from a distinctive span of a real entity or chunk, and the correct
answer is that item's id. No hand labelling, so the query set can be regrown
whenever the corpus changes.
"""
from __future__ import annotations

import argparse
import json
import re
import statistics
import urllib.request
from collections import Counter
from dataclasses import dataclass, field

AUTH = "Basic Y29kZXg6MTY2M2FhYjdhZGNkN2UyNDA3MjJkNWIyMzAxZDQ4NGYyOGYzNDIxODE3ODgyYTM4"
STOP = {
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "then",
    "which", "their", "there", "have", "has", "was", "were", "are", "not", "but",
    "its", "it's", "you", "your", "can", "will", "would", "should", "could",
}


def rpc(port: int, name: str, arguments: dict, timeout: int = 120) -> dict:
    body = json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    }).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/mcp", data=body,
        headers={
            "Authorization": AUTH,
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        payload = json.loads(r.read().decode())
    result = payload.get("result", {})
    for item in result.get("content", []) or []:
        if item.get("type") == "text":
            try:
                return json.loads(item["text"])
            except json.JSONDecodeError:
                return {"raw": item["text"]}
    return result


def salient_terms(text: str, n: int) -> list[str]:
    """Distinctive words from a snippet — the query a person would actually type."""
    words = re.findall(r"[A-Za-z][A-Za-z0-9_-]{3,}", text or "")
    seen, out = set(), []
    for w in words:
        lw = w.lower()
        if lw in STOP or lw in seen:
            continue
        seen.add(lw)
        out.append(w)
        if len(out) >= n:
            break
    return out


@dataclass
class Case:
    """One known-item query: `query` should retrieve `target_id`."""
    query: str
    target_id: str
    kind: str                      # entity | chunk
    cwd: str | None = None
    workspace_of_target: str | None = None


@dataclass
class Outcome:
    hits: int = 0
    total: int = 0
    recip_ranks: list[float] = field(default_factory=list)
    sources: Counter = field(default_factory=Counter)
    chars: list[int] = field(default_factory=list)
    returned: list[int] = field(default_factory=list)
    errors: int = 0

    def record(self, res: dict, target: str) -> None:
        self.total += 1
        results = res.get("results") or []
        self.returned.append(len(results))
        self.chars.append(len(json.dumps(results)))
        for r in results:
            src = r.get("source") or r.get("match_source") or "unknown"
            self.sources[str(src)] += 1
        rank = next(
            (i for i, r in enumerate(results, 1)
             if str(r.get("id") or r.get("entity_id") or "") == target),
            None,
        )
        if rank:
            self.hits += 1
            self.recip_ranks.append(1.0 / rank)

    def report(self) -> dict:
        n = max(self.total, 1)
        # Fusion utilisation: 1.0 = every source contributes equally, 0 = one
        # source supplies everything. Normalised Shannon entropy over sources.
        total_hits = sum(self.sources.values())
        if total_hits and len(self.sources) > 1:
            import math
            probs = [c / total_hits for c in self.sources.values()]
            h = -sum(p * math.log(p) for p in probs if p > 0)
            utilisation = h / math.log(len(self.sources))
        else:
            utilisation = 0.0
        return {
            "queries": self.total,
            "recall": round(self.hits / n, 4),
            "mrr": round(sum(self.recip_ranks) / n, 4),
            "sources_used": len(self.sources),
            "fusion_utilisation": round(utilisation, 4),
            "top_sources": self.sources.most_common(5),
            "median_results": statistics.median(self.returned) if self.returned else 0,
            "median_chars": statistics.median(self.chars) if self.chars else 0,
            "p90_chars": (sorted(self.chars)[int(len(self.chars) * 0.9)]
                          if self.chars else 0),
            "errors": self.errors,
        }


def run(port: int, cases: list[Case], limit: int, use_cwd: bool) -> Outcome:
    out = Outcome()
    for c in cases:
        args = {"query": c.query, "limit": limit, "rerank": False}
        if use_cwd and c.cwd:
            args["cwd"] = c.cwd
        try:
            out.record(rpc(port, "search", args), c.target_id)
        except Exception:
            out.errors += 1
            out.total += 1
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", required=True, help="JSON case file from build_cases.py")
    ap.add_argument("--baseline-port", type=int, default=18790)
    ap.add_argument("--candidate-port", type=int, default=18791)
    ap.add_argument("--limit", type=int, default=10)
    args = ap.parse_args()

    cases = [Case(**c) for c in json.load(open(args.cases))]
    print(f"{len(cases)} cases, limit={args.limit}\n")

    for label, use_cwd in (("without cwd", False), ("with cwd", True)):
        print(f"=== {label} ===")
        for name, port in (("baseline(main)", args.baseline_port),
                           ("candidate(PR)", args.candidate_port)):
            rep = run(port, cases, args.limit, use_cwd).report()
            print(f"  {name:16} {json.dumps(rep)}")
        print()


if __name__ == "__main__":
    main()
