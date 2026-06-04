#!/usr/bin/env python3
"""Run official-corpus evaluation diagnostics.

Inputs are the local corpora populated by `scripts/download-eval-corpora.sh`.
Outputs are JSON reports under an ignored diagnostics directory by default.
"""

from __future__ import annotations

import argparse
import ast
import base64
import collections
import heapq
import json
import math
import re
import statistics
import sys
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


STOPWORDS = {
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "has",
    "have",
    "how",
    "in",
    "into",
    "is",
    "it",
    "of",
    "on",
    "or",
    "the",
    "to",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "with",
}


TOKEN_RE = re.compile(r"[A-Za-z0-9_]+")


@dataclass(frozen=True)
class Aspect:
    id: str
    content: str
    weight: float
    supporting_docs: tuple[str, ...]


@dataclass(frozen=True)
class Hit:
    id: str
    score: float
    text: str = ""


@dataclass(frozen=True)
class MemoryBenchDocument:
    id: str
    content: str
    attrs: dict[str, Any]


@dataclass(frozen=True)
class MemoryBenchPrediction:
    answer: str
    retrieved: list[Hit]
    generator: str


def tokenize(text: str) -> list[str]:
    return [
        token.lower()
        for token in TOKEN_RE.findall(text)
        if len(token) >= 2 and token.lower() not in STOPWORDS
    ]


def alpha_ndcg_at_k(
    hits: list[str], aspects: list[Aspect], *, k: int, alpha: float = 0.5
) -> float:
    ideal = ideal_alpha_dcg_at_k(aspects, k=k, alpha=alpha)
    if ideal <= 0.0:
        return 0.0
    return alpha_dcg_at_k(hits, aspects, k=k, alpha=alpha) / ideal


def alpha_dcg_at_k(
    hits: list[str], aspects: list[Aspect], *, k: int, alpha: float = 0.5
) -> float:
    covered = collections.Counter()
    dcg = 0.0
    for rank, doc_id in enumerate(hits[:k], start=1):
        gain = alpha_gain(doc_id, aspects, covered, alpha)
        if gain > 0:
            for aspect in aspects:
                if doc_id in aspect.supporting_docs:
                    covered[aspect.id] += 1
        dcg += gain / math.log2(rank + 1)
    return dcg


def ideal_alpha_dcg_at_k(aspects: list[Aspect], *, k: int, alpha: float = 0.5) -> float:
    candidate_docs = sorted({doc for aspect in aspects for doc in aspect.supporting_docs})
    covered = collections.Counter()
    dcg = 0.0
    used = set()
    for rank in range(1, k + 1):
        best_doc = None
        best_gain = 0.0
        for doc_id in candidate_docs:
            if doc_id in used:
                continue
            gain = alpha_gain(doc_id, aspects, covered, alpha)
            if gain > best_gain:
                best_gain = gain
                best_doc = doc_id
        if best_doc is None or best_gain <= 0:
            break
        used.add(best_doc)
        for aspect in aspects:
            if best_doc in aspect.supporting_docs:
                covered[aspect.id] += 1
        dcg += best_gain / math.log2(rank + 1)
    return dcg


def alpha_gain(
    doc_id: str, aspects: list[Aspect], covered: collections.Counter[str], alpha: float
) -> float:
    gain = 0.0
    for aspect in aspects:
        if doc_id in aspect.supporting_docs:
            gain += aspect.weight * ((1.0 - alpha) ** covered[aspect.id])
    return gain


def aspect_recall_at_k(hits: list[str], aspects: list[Aspect], *, k: int) -> float:
    total_weight = sum(aspect.weight for aspect in aspects)
    if total_weight <= 0.0:
        return 0.0
    covered = 0.0
    hit_set = set(hits[:k])
    for aspect in aspects:
        if hit_set.intersection(aspect.supporting_docs):
            covered += aspect.weight
    return covered / total_weight


def recall_at_k(hits: list[str], gold_docs: set[str], *, k: int) -> float:
    if not gold_docs:
        return 0.0
    return len(set(hits[:k]).intersection(gold_docs)) / len(gold_docs)


def ndcg_at_k(hits: list[str], gold_docs: set[str], *, k: int) -> float:
    dcg = 0.0
    for rank, doc_id in enumerate(hits[:k], start=1):
        if doc_id in gold_docs:
            dcg += 1.0 / math.log2(rank + 1)
    ideal = sum(1.0 / math.log2(rank + 1) for rank in range(1, min(k, len(gold_docs)) + 1))
    return 0.0 if ideal <= 0.0 else dcg / ideal


class Bm25Index:
    def __init__(self, rows: Iterable[dict[str, Any]]):
        self.doc_len: dict[str, int] = {}
        self.documents: dict[str, str] = {}
        self.inverted: dict[str, list[tuple[str, int]]] = collections.defaultdict(list)
        self.doc_count = 0
        total_len = 0

        for row in rows:
            doc_id = str(row["id"])
            content = str(row["content"])
            terms = tokenize(content)
            counts = collections.Counter(terms)
            self.documents[doc_id] = content
            self.doc_len[doc_id] = len(terms)
            total_len += len(terms)
            self.doc_count += 1
            for term, tf in counts.items():
                self.inverted[term].append((doc_id, tf))
        self.avg_doc_len = total_len / self.doc_count if self.doc_count else 0.0

    def search(self, query: str, k: int) -> list[Hit]:
        if self.doc_count == 0:
            return []
        scores = collections.defaultdict(float)
        k1 = 1.2
        b = 0.75
        for term in set(tokenize(query)):
            postings = self.inverted.get(term)
            if not postings:
                continue
            df = len(postings)
            idf = math.log(1.0 + (self.doc_count - df + 0.5) / (df + 0.5))
            for doc_id, tf in postings:
                denom = tf + k1 * (
                    1.0 - b + b * (self.doc_len[doc_id] / max(self.avg_doc_len, 1.0))
                )
                scores[doc_id] += idf * ((tf * (k1 + 1.0)) / denom)
        top = heapq.nlargest(k, scores.items(), key=lambda item: (item[1], item[0]))
        return [
            Hit(id=doc_id, score=score, text=self.documents.get(doc_id, ""))
            for doc_id, score in top
        ]


class McpHttpClient:
    def __init__(self, url: str, username: str, password: str, timeout_seconds: float):
        self.url = url
        self.timeout_seconds = timeout_seconds
        self.next_id = 1
        token = base64.b64encode(f"{username}:{password}".encode("utf-8")).decode("ascii")
        self.headers = {
            "authorization": f"Basic {token}",
            "content-type": "application/json",
        }

    def initialize(self) -> dict[str, Any]:
        return self.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ferrosa-official-eval", "version": "0"},
            },
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        return self.request("tools/call", {"name": name, "arguments": arguments})

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        body = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        ).encode("utf-8")
        request = urllib.request.Request(
            self.url, data=body, headers=self.headers, method="POST"
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout_seconds) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"MCP HTTP {exc.code}: {detail}") from exc
        if "error" in payload:
            raise RuntimeError(f"MCP JSON-RPC error: {payload['error']}")
        result = payload.get("result", {})
        return unwrap_tool_result(result)


def unwrap_tool_result(result: dict[str, Any]) -> dict[str, Any]:
    content = result.get("content") if isinstance(result, dict) else None
    if isinstance(content, list) and content:
        text = content[0].get("text") if isinstance(content[0], dict) else None
        if isinstance(text, str):
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                return {"text": text}
    return result


def nested_get(data: dict[str, Any], path: tuple[str, ...]) -> Any:
    current: Any = data
    for key in path:
        if not isinstance(current, dict):
            return None
        current = current.get(key)
    return current


def first_present(
    data: dict[str, Any], paths: Iterable[tuple[str, ...]], default: Any = None
) -> Any:
    for path in paths:
        value = nested_get(data, path)
        if value is not None:
            return value
    return default


def int_counter(data: dict[str, Any], paths: Iterable[tuple[str, ...]]) -> int:
    value = first_present(data, paths, 0)
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def failure_list(data: dict[str, Any], paths: Iterable[tuple[str, ...]]) -> list[Any]:
    value = first_present(data, paths, [])
    if isinstance(value, list):
        return value
    if isinstance(value, int):
        return [{}] * value
    return []


def response_indexing_mode(response: dict[str, Any]) -> str | None:
    value = first_present(
        response,
        (
            ("document_indexing_mode",),
            ("document_indexing", "mode"),
            ("indexing", "mode"),
            ("storage", "document_indexing_mode"),
            ("diagnostics", "document_indexing_mode"),
        ),
    )
    return str(value) if value is not None else None


def parse_ingest_response(response: dict[str, Any]) -> dict[str, Any]:
    return {
        "entity_inserted": int_counter(
            response,
            (
                ("entities", "inserted"),
                ("entities", "counts", "inserted"),
                ("entity_inserted",),
            ),
        ),
        "entity_updated": int_counter(
            response,
            (
                ("entities", "updated"),
                ("entities", "counts", "updated"),
                ("entity_updated",),
            ),
        ),
        "entity_skipped": int_counter(
            response,
            (
                ("entities", "skipped"),
                ("entities", "counts", "skipped"),
                ("entity_skipped",),
            ),
        ),
        "entity_failed": failure_list(
            response,
            (
                ("entities", "failed"),
                ("entities", "failures"),
                ("entity_failed",),
            ),
        ),
        "embeddings_computed": int_counter(
            response,
            (
                ("embeddings", "computed"),
                ("embeddings", "counts", "computed"),
                ("embedding_computed",),
                ("embeddings_computed",),
            ),
        ),
        "embeddings_received": int_counter(
            response,
            (
                ("embeddings", "received"),
                ("embeddings", "counts", "received"),
                ("embedding_received",),
                ("embeddings_received",),
            ),
        ),
        "embeddings_failed": failure_list(
            response,
            (
                ("embeddings", "failed"),
                ("embeddings", "failures"),
                ("embedding_failed",),
            ),
        ),
        "document_indexing_mode": response_indexing_mode(response),
    }


def deterministic_entity_id(session_id: str, doc_id: str) -> str:
    namespace = uuid.uuid5(uuid.NAMESPACE_URL, f"ferrosa-bright-pro:{session_id}")
    return str(uuid.uuid5(namespace, doc_id))


def result_entity_id_for_mapping(result: dict[str, Any]) -> str:
    """Return the stable corpus entity id for direct entity and chunk search hits."""
    document_id = result.get("document_id")
    if document_id:
        return str(document_id)
    return str(result.get("id", ""))


class McpBrightRetriever:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.client = McpHttpClient(
            args.mcp_url, args.mcp_user, args.mcp_password, args.mcp_timeout_seconds
        )
        self.session_id = args.mcp_session_id or str(uuid.uuid4())
        self.tenant_id = args.mcp_tenant_id
        self.entity_to_doc: dict[str, str] = {}
        self.client.initialize()

    def ingest_documents(self, rows: list[dict[str, Any]], split: str) -> dict[str, Any]:
        started = time.time()
        entity_failed = []
        embedding_failed = []
        inserted = 0
        updated = 0
        skipped = 0
        embeddings_computed = 0
        embeddings_received = 0
        document_indexing_modes: set[str] = set()
        rows_to_ingest = rows
        if self.args.mcp_max_docs is not None:
            rows_to_ingest = rows_to_ingest[: self.args.mcp_max_docs]
        total = len(rows_to_ingest)
        if self.args.mcp_skip_ingest:
            for row in rows_to_ingest:
                entity_id = deterministic_entity_id(self.session_id, str(row["id"]))
                self.entity_to_doc[entity_id] = str(row["id"])
            return {
                "skipped_ingest": True,
                "documents": total,
                "mcp_embed_missing": self.args.mcp_embed_missing,
                "embeddings_computed": 0,
                "embeddings_received": 0,
                "embeddings_failed": 0,
                "document_indexing_mode": "reused-existing-session",
            }

        for offset in range(0, total, self.args.mcp_batch_size):
            batch = rows_to_ingest[offset : offset + self.args.mcp_batch_size]
            entities = []
            for row in batch:
                doc_id = str(row["id"])
                entity_id = deterministic_entity_id(self.session_id, doc_id)
                self.entity_to_doc[entity_id] = doc_id
                entities.append(
                    {
                        "id": entity_id,
                        "name": f"{self.session_id}::{doc_id}",
                        "entity_type": self.args.mcp_entity_type,
                        "context": str(row["content"])[: self.args.mcp_context_chars],
                        "confidence": 1.0,
                        "attrs": {
                            "benchmark": "bright-pro",
                            "split": split,
                            "doc_id": doc_id,
                        },
                    }
                )
            response = self.client.call_tool(
                "ingest_entities",
                {
                    "tenant_id": self.tenant_id,
                    "session_id": self.session_id,
                    "entities": entities,
                    "edges": [],
                    "options": {
                        "embed_missing": self.args.mcp_embed_missing,
                        "on_conflict": "skip",
                        "strict_edges": True,
                    },
                },
            )
            parsed = parse_ingest_response(response)
            inserted += parsed["entity_inserted"]
            updated += parsed["entity_updated"]
            skipped += parsed["entity_skipped"]
            embeddings_computed += parsed["embeddings_computed"]
            embeddings_received += parsed["embeddings_received"]
            entity_failed.extend(parsed["entity_failed"])
            embedding_failed.extend(parsed["embeddings_failed"])
            if parsed["document_indexing_mode"]:
                document_indexing_modes.add(parsed["document_indexing_mode"])
            if self.args.progress:
                print(
                    "MCP ingest "
                    f"{split}: {min(offset + len(batch), total)}/{total} "
                    f"entities inserted={inserted} updated={updated} skipped={skipped} "
                    f"entity_failed={len(entity_failed)} "
                    f"embeddings computed={embeddings_computed} "
                    f"received={embeddings_received} failed={len(embedding_failed)}",
                    flush=True,
                )
        document_indexing_mode = None
        if len(document_indexing_modes) == 1:
            document_indexing_mode = next(iter(document_indexing_modes))
        elif len(document_indexing_modes) > 1:
            document_indexing_mode = "mixed:" + ",".join(sorted(document_indexing_modes))
        return {
            "skipped_ingest": False,
            "documents": total,
            "mcp_embed_missing": self.args.mcp_embed_missing,
            "entity_inserted": inserted,
            "entity_updated": updated,
            "entity_skipped": skipped,
            "entity_failed": len(entity_failed),
            "entity_failed_samples": entity_failed[:20],
            "embeddings_computed": embeddings_computed,
            "embeddings_received": embeddings_received,
            "embeddings_failed": len(embedding_failed),
            "embeddings_failed_samples": embedding_failed[:20],
            "document_indexing_mode": document_indexing_mode,
            "elapsed_seconds": time.time() - started,
        }

    def search(self, query: str, k: int) -> list[Hit]:
        response = self.client.call_tool(
            "hybrid_search",
            {
                "session_id": self.session_id,
                "query": query,
                "limit": k,
                "scope": "session",
            },
        )
        hits = []
        for result in response.get("results", []):
            entity_id = result_entity_id_for_mapping(result)
            doc_id = self.entity_to_doc.get(entity_id, entity_id)
            hits.append(
                Hit(
                    id=doc_id,
                    score=float(result.get("score", 0.0)),
                    text=str(result.get("content", "")),
                )
            )
        return hits


def read_parquet_rows(path: Path) -> list[dict[str, Any]]:
    import pyarrow.parquet as pq

    table = pq.read_table(path)
    return table.to_pylist()


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_jsonl(path: Path, rows: Iterable[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True) + "\n")


def bright_pro_splits(corpus_dir: Path, requested: list[str] | None) -> list[str]:
    examples_dir = corpus_dir / "examples"
    splits = sorted(path.stem for path in examples_dir.glob("*.parquet"))
    return [split for split in splits if not requested or split in requested]


def support_docs_for_example(
    example_id: int, aspects_by_example: dict[int, list[Aspect]]
) -> set[str]:
    return {
        doc_id
        for aspect in aspects_by_example.get(example_id, [])
        for doc_id in aspect.supporting_docs
    }


def select_support_closed_examples(
    examples: list[dict[str, Any]],
    aspects_by_example: dict[int, list[Aspect]],
    available_doc_ids: set[str],
    limit: int | None,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Select examples whose gold support docs are fully inside a capped corpus."""
    if limit is not None and limit <= 0:
        return [], {
            "candidate_examples": len(examples),
            "selected_examples": 0,
            "excluded_no_aspects": 0,
            "excluded_open_support": 0,
            "available_documents": len(available_doc_ids),
        }

    selected = []
    excluded_no_aspects = 0
    excluded_open_support = 0

    for row in examples:
        example_id = int(row["id"])
        aspects = aspects_by_example.get(example_id, [])
        support_docs = support_docs_for_example(example_id, aspects_by_example)
        if not aspects or not support_docs:
            excluded_no_aspects += 1
            continue
        if not support_docs.issubset(available_doc_ids):
            excluded_open_support += 1
            continue
        selected.append(row)
        if limit is not None and len(selected) >= limit:
            break

    return selected, {
        "candidate_examples": len(examples),
        "selected_examples": len(selected),
        "excluded_no_aspects": excluded_no_aspects,
        "excluded_open_support": excluded_open_support,
        "available_documents": len(available_doc_ids),
    }


def load_bright_aspects(corpus_dir: Path, split: str) -> dict[int, list[Aspect]]:
    rows = read_parquet_rows(corpus_dir / "aspects" / f"{split}.parquet")
    grouped: dict[int, list[Aspect]] = collections.defaultdict(list)
    prefix = f"{split}-"
    for row in rows:
        aspect_id = str(row["id"])
        if not aspect_id.startswith(prefix):
            continue
        remainder = aspect_id[len(prefix) :]
        example_id_text = remainder.split("-", 1)[0]
        try:
            example_id = int(example_id_text)
        except ValueError:
            continue
        grouped[example_id].append(
            Aspect(
                id=aspect_id,
                content=str(row["content"]),
                weight=float(row["weight"]),
                supporting_docs=tuple(str(doc) for doc in row["supporting_docs"]),
            )
        )
    return grouped


def summarize(values: list[float]) -> dict[str, float]:
    if not values:
        return {"mean": 0.0, "median": 0.0, "min": 0.0, "max": 0.0}
    return {
        "mean": statistics.fmean(values),
        "median": statistics.median(values),
        "min": min(values),
        "max": max(values),
    }


def run_bright_pro(args: argparse.Namespace) -> int:
    corpus_dir = Path(args.corpus_dir).expanduser().resolve() / "bright-pro"
    if not corpus_dir.exists():
        print(f"missing BRIGHT-Pro corpus: {corpus_dir}", file=sys.stderr)
        print("run: scripts/download-eval-corpora.sh --corpus bright-pro", file=sys.stderr)
        return 2

    output_dir = Path(args.output_dir).expanduser().resolve()
    splits = bright_pro_splits(corpus_dir, args.split)
    if not splits:
        print(f"no BRIGHT-Pro splits found under {corpus_dir}", file=sys.stderr)
        return 2

    all_cases = []
    failures = []
    split_summaries = {}
    ingest_summaries = {}
    sampling_summaries = {}
    remaining = args.limit_examples
    mcp_retriever = McpBrightRetriever(args) if args.backend == "mcp-http" else None

    for split in splits:
        examples = read_parquet_rows(corpus_dir / "examples" / f"{split}.parquet")
        aspects_by_example = load_bright_aspects(corpus_dir, split)
        document_rows = read_parquet_rows(corpus_dir / "documents" / f"{split}.parquet")
        if mcp_retriever and args.mcp_max_docs is not None:
            document_rows = document_rows[: args.mcp_max_docs]
            available_doc_ids = {str(row["id"]) for row in document_rows}
            examples, sampling_summary = select_support_closed_examples(
                examples, aspects_by_example, available_doc_ids, remaining
            )
            sampling_summary["support_doc_closed"] = True
            sampling_summaries[split] = sampling_summary
        else:
            candidate_count = len(examples)
            if remaining is not None:
                examples = examples[:remaining]
            sampling_summaries[split] = {
                "candidate_examples": candidate_count,
                "selected_examples": len(examples),
                "excluded_no_aspects": 0,
                "excluded_open_support": 0,
                "available_documents": len(document_rows),
                "support_doc_closed": False,
            }
        if remaining is not None:
            remaining -= len(examples)
            if remaining <= 0:
                remaining = 0
        if mcp_retriever:
            print(f"BRIGHT-Pro {split}: ingesting documents through MCP {args.mcp_url}")
            ingest_summaries[split] = mcp_retriever.ingest_documents(document_rows, split)
            index = None
        else:
            print("BRIGHT-Pro local BM25: parser/scoring diagnostic, not Ferrosa retrieval")
            print(f"BRIGHT-Pro {split}: indexing documents")
            index = Bm25Index(document_rows)
        split_cases = []

        for row in examples:
            example_id = int(row["id"])
            query = str(row["query"])
            aspects = aspects_by_example.get(example_id, [])
            hits = (
                mcp_retriever.search(query, args.k)
                if mcp_retriever
                else index.search(query, args.k)
            )
            hit_ids = [hit.id for hit in hits]
            gold_docs = {doc for aspect in aspects for doc in aspect.supporting_docs}
            case = {
                "split": split,
                "id": example_id,
                "query": query,
                "alpha_ndcg": alpha_ndcg_at_k(hit_ids, aspects, k=args.k, alpha=args.alpha),
                "aspect_recall": aspect_recall_at_k(hit_ids, aspects, k=args.k),
                "recall": recall_at_k(hit_ids, gold_docs, k=args.k),
                "ndcg": ndcg_at_k(hit_ids, gold_docs, k=args.k),
                "gold_doc_count": len(gold_docs),
                "aspect_count": len(aspects),
                "hits": [{"id": hit.id, "score": hit.score} for hit in hits[: args.failure_hits]],
            }
            split_cases.append(case)
            all_cases.append(case)

            missing_aspects = [
                aspect
                for aspect in aspects
                if not set(hit_ids[: args.k]).intersection(aspect.supporting_docs)
            ]
            if (
                case["aspect_recall"] < args.failure_aspect_recall
                or case["alpha_ndcg"] < args.failure_alpha_ndcg
            ):
                failures.append(
                    {
                        "split": split,
                        "id": example_id,
                        "query": query,
                        "alpha_ndcg": case["alpha_ndcg"],
                        "aspect_recall": case["aspect_recall"],
                        "recall": case["recall"],
                        "ndcg": case["ndcg"],
                        "failure_reasons": failure_reasons(case, args),
                        "missing_aspects": [
                            {
                                "id": aspect.id,
                                "weight": aspect.weight,
                                "content": aspect.content,
                                "supporting_docs": list(aspect.supporting_docs),
                            }
                            for aspect in missing_aspects
                        ],
                        "top_hits": case["hits"],
                    }
                )
        split_summaries[split] = summarize_cases(split_cases)
        if remaining == 0:
            break

    ingest_summary = summarize_ingest(ingest_summaries)
    report = {
        "suite": "bright-pro",
        "backend": args.backend,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "corpus_dir": str(corpus_dir),
        "mcp_url": args.mcp_url if args.backend == "mcp-http" else None,
        "mcp_session_id": mcp_retriever.session_id if mcp_retriever else None,
        "mcp_embed_missing": args.mcp_embed_missing if args.backend == "mcp-http" else None,
        "document_indexing_mode": ingest_summary["document_indexing_mode"],
        "k": args.k,
        "alpha": args.alpha,
        "case_count": len(all_cases),
        "failure_count": len(failures),
        "summary": summarize_cases(all_cases),
        "splits": split_summaries,
        "sampling": sampling_summaries,
        "ingest_summary": ingest_summary,
        "ingest": ingest_summaries,
        "failure_report": str(output_dir / "bright-pro-failures.jsonl"),
    }
    write_json(output_dir / "bright-pro-report.json", report)
    write_jsonl(output_dir / "bright-pro-failures.jsonl", failures)
    if args.include_cases:
        write_jsonl(output_dir / "bright-pro-cases.jsonl", all_cases)
    print(json.dumps(report["summary"], indent=2, sort_keys=True))
    if args.backend == "mcp-http":
        print(json.dumps({"ingest_summary": ingest_summary}, indent=2, sort_keys=True))
    print(f"wrote {output_dir / 'bright-pro-report.json'}")
    print(f"wrote {output_dir / 'bright-pro-failures.jsonl'}")
    return 0


def failure_reasons(case: dict[str, Any], args: argparse.Namespace) -> list[str]:
    reasons = []
    if case["aspect_recall"] <= 0.0:
        reasons.append("zero_aspect_recall")
    elif case["aspect_recall"] < args.failure_aspect_recall:
        reasons.append("low_aspect_recall")
    if case["alpha_ndcg"] < args.failure_alpha_ndcg:
        reasons.append("low_alpha_ndcg")
    if not case["hits"]:
        reasons.append("no_hits")
    return reasons


def summarize_cases(cases: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "alpha_ndcg": summarize([case["alpha_ndcg"] for case in cases]),
        "aspect_recall": summarize([case["aspect_recall"] for case in cases]),
        "recall": summarize([case["recall"] for case in cases]),
        "ndcg": summarize([case["ndcg"] for case in cases]),
    }


def summarize_ingest(ingest_summaries: dict[str, dict[str, Any]]) -> dict[str, Any]:
    totals = {
        "documents": 0,
        "entity_inserted": 0,
        "entity_updated": 0,
        "entity_skipped": 0,
        "entity_failed": 0,
        "embeddings_computed": 0,
        "embeddings_received": 0,
        "embeddings_failed": 0,
    }
    modes = set()
    skipped_ingest = False
    for summary in ingest_summaries.values():
        skipped_ingest = skipped_ingest or bool(summary.get("skipped_ingest"))
        for key in totals:
            totals[key] += int(summary.get(key, 0) or 0)
        mode = summary.get("document_indexing_mode")
        if mode:
            modes.add(str(mode))
    if len(modes) == 1:
        document_indexing_mode = next(iter(modes))
    elif len(modes) > 1:
        document_indexing_mode = "mixed:" + ",".join(sorted(modes))
    else:
        document_indexing_mode = None
    return {
        **totals,
        "skipped_ingest": skipped_ingest,
        "document_indexing_mode": document_indexing_mode,
    }


def read_arrow_rows(path: Path) -> list[dict[str, Any]]:
    import pyarrow.ipc as ipc

    with path.open("rb") as handle:
        try:
            reader = ipc.open_stream(handle)
            return reader.read_all().to_pylist()
        except Exception:
            handle.seek(0)
            reader = ipc.open_file(handle)
            return reader.read_all().to_pylist()


def parse_json_field(value: Any) -> tuple[Any | None, str | None]:
    if isinstance(value, (dict, list)):
        return value, None
    if value is None:
        return None, "missing"
    try:
        return json.loads(value), None
    except Exception as exc:
        return None, str(exc)


def run_memorybench(args: argparse.Namespace) -> int:
    variant = args.memorybench_variant
    corpus_dir = Path(args.corpus_dir).expanduser().resolve() / f"memorybench-{variant}"
    dataset_dir = corpus_dir / "dataset"
    if not dataset_dir.exists():
        print(f"missing MemoryBench corpus: {dataset_dir}", file=sys.stderr)
        print(
            f"run: scripts/download-eval-corpora.sh --corpus memorybench --memorybench-variant {variant}",
            file=sys.stderr,
        )
        return 2

    output_dir = Path(args.output_dir).expanduser().resolve()
    dataset_paths = sorted(path for path in dataset_dir.iterdir() if path.is_dir())
    if args.dataset:
        wanted = set(args.dataset)
        dataset_paths = [path for path in dataset_paths if path.name in wanted]

    summaries = []
    failures = []
    for dataset_path in dataset_paths:
        dataset_name = dataset_path.name
        split_counts = {}
        field_counts = collections.Counter()
        malformed_info = 0
        evidence_rows = 0
        feedback_rows = 0
        dialog_rows = 0
        sampled_failures = []
        for split in ["train", "test"]:
            arrow_files = sorted((dataset_path / split).glob("*.arrow"))
            rows = []
            for arrow_file in arrow_files:
                rows.extend(read_arrow_rows(arrow_file))
            if args.limit_rows is not None:
                rows = rows[: args.limit_rows]
            split_counts[split] = len(rows)
            for row in rows:
                field_counts.update(row.keys())
                parsed_info, info_error = parse_json_field(row.get("info"))
                if info_error and row.get("info") not in (None, ""):
                    malformed_info += 1
                    if len(sampled_failures) < args.failure_limit:
                        sampled_failures.append(
                            {
                                "dataset": dataset_name,
                                "split": split,
                                "test_idx": row.get("test_idx"),
                                "reason": "malformed_info",
                                "error": info_error,
                            }
                        )
                if isinstance(parsed_info, dict) and parsed_info.get("evidence"):
                    evidence_rows += 1
                if any(key.startswith("implicit_feedback") for key in row):
                    feedback_rows += 1
                if any(key.startswith("dialog") for key in row):
                    dialog_rows += 1
        summary = {
            "dataset": dataset_name,
            "splits": split_counts,
            "fields": sorted(field_counts.keys()),
            "malformed_info_rows": malformed_info,
            "rows_with_evidence": evidence_rows,
            "rows_with_feedback_fields": feedback_rows,
            "rows_with_dialog_fields": dialog_rows,
        }
        summaries.append(summary)
        failures.extend(sampled_failures)

    report = {
        "suite": "memorybench",
        "mode": "official_corpus_audit",
        "paper_score_runnable": False,
        "paper_score_blockers": [
            "Ferrosa prediction generation adapter for MemoryBench tasks is not implemented.",
            "Task-native judges/metrics from the MemoryBench repo require additional model/API configuration.",
            "This audit validates local corpus shape and identifies data issues, but does not reproduce Table 10/12 scores.",
        ],
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "corpus_dir": str(corpus_dir),
        "dataset_count": len(summaries),
        "datasets": summaries,
        "failure_report": str(output_dir / "memorybench-failures.jsonl"),
    }
    write_json(output_dir / "memorybench-report.json", report)
    write_jsonl(output_dir / "memorybench-failures.jsonl", failures)
    print(
        json.dumps(
            {
                "dataset_count": report["dataset_count"],
                "paper_score_runnable": report["paper_score_runnable"],
                "failure_count": len(failures),
            },
            indent=2,
            sort_keys=True,
        )
    )
    print(f"wrote {output_dir / 'memorybench-report.json'}")
    return 0


def run_self_test() -> int:
    aspects = [
        Aspect("a1", "first", 1.0, ("d1",)),
        Aspect("a2", "second", 1.0, ("d2",)),
    ]
    assert aspect_recall_at_k(["d1"], aspects, k=1) == 0.5
    assert aspect_recall_at_k(["d1", "d2"], aspects, k=2) == 1.0
    assert recall_at_k(["d1"], {"d1", "d2"}, k=1) == 0.5
    assert ndcg_at_k(["noise", "d1"], {"d1"}, k=2) < 1.0
    assert alpha_ndcg_at_k(["d1", "d2"], aspects, k=2) == 1.0

    index = Bm25Index(
        [
            {"id": "d1", "content": "aspect aware retrieval evidence"},
            {"id": "d2", "content": "unrelated material"},
        ]
    )
    assert index.search("retrieval evidence", 1)[0].id == "d1"

    selected, sampling = select_support_closed_examples(
        [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}],
        {
            1: [Aspect("a1", "closed", 1.0, ("d1",))],
            2: [Aspect("a2", "outside", 1.0, ("d3",))],
            3: [Aspect("a3", "closed multi", 1.0, ("d1", "d2"))],
        },
        {"d1", "d2"},
        limit=None,
    )
    assert [row["id"] for row in selected] == [1, 3]
    assert sampling["excluded_open_support"] == 1
    assert sampling["excluded_no_aspects"] == 1
    assert result_entity_id_for_mapping({"id": "chunk", "document_id": "doc"}) == "doc"
    assert result_entity_id_for_mapping({"id": "entity"}) == "entity"

    parsed = parse_ingest_response(
        {
            "entities": {
                "counts": {"inserted": 2, "updated": 1, "skipped": 3},
                "failed": [{"id": "bad-doc"}],
            },
            "embeddings": {
                "counts": {"computed": 4, "received": 5},
                "failed": [{"id": "no-embedding"}],
            },
            "document_indexing": {"mode": "storage-ann"},
        }
    )
    assert parsed["entity_inserted"] == 2
    assert parsed["entity_updated"] == 1
    assert parsed["entity_skipped"] == 3
    assert len(parsed["entity_failed"]) == 1
    assert parsed["embeddings_computed"] == 4
    assert parsed["embeddings_received"] == 5
    assert len(parsed["embeddings_failed"]) == 1
    assert parsed["document_indexing_mode"] == "storage-ann"
    print("self-test passed")
    return 0


def default_output_dir() -> str:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"diagnostics/eval-runs/{stamp}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run official Ferrosa eval corpora.")
    parser.add_argument("--self-test", action="store_true")
    subparsers = parser.add_subparsers(dest="command")

    bright = subparsers.add_parser("bright-pro", help="Run BRIGHT-Pro retrieval eval.")
    bright.add_argument("--corpus-dir", default=".eval-corpus")
    bright.add_argument("--output-dir", default=default_output_dir())
    bright.add_argument(
        "--backend",
        choices=["mcp-http", "bm25-local"],
        default="mcp-http",
        help="System under test. Use bm25-local only as a parser/scoring diagnostic.",
    )
    bright.add_argument("--split", action="append")
    bright.add_argument("--limit-examples", type=int)
    bright.add_argument("--k", type=int, default=25)
    bright.add_argument("--alpha", type=float, default=0.5)
    bright.add_argument("--failure-alpha-ndcg", type=float, default=0.5)
    bright.add_argument("--failure-aspect-recall", type=float, default=1.0)
    bright.add_argument("--failure-hits", type=int, default=10)
    bright.add_argument("--include-cases", action="store_true")
    bright.add_argument("--mcp-url", default="http://127.0.0.1:18775/mcp")
    bright.add_argument("--mcp-user", default="user")
    bright.add_argument("--mcp-password", default="pass")
    bright.add_argument(
        "--mcp-tenant-id",
        default="00000000-0000-0000-0000-00000000e075",
        help="Tenant UUID from the HTTP auth principal.",
    )
    bright.add_argument("--mcp-session-id")
    bright.add_argument("--mcp-timeout-seconds", type=float, default=60.0)
    bright.add_argument("--mcp-batch-size", type=int, default=100)
    bright.add_argument(
        "--mcp-entity-type",
        default="document",
        help="Entity type used for corpus documents when ingesting through MCP.",
    )
    bright.add_argument("--mcp-context-chars", type=int, default=16000)
    bright.add_argument("--mcp-max-docs", type=int)
    bright.add_argument(
        "--mcp-embed-missing",
        action="store_true",
        help=(
            "Pass embed_missing=true to ingest_entities so the MCP server computes "
            "missing corpus document embeddings before retrieval."
        ),
    )
    bright.add_argument(
        "--mcp-skip-ingest",
        action="store_true",
        help="Reuse an existing deterministic session corpus; only rebuild ID mapping locally.",
    )
    bright.add_argument("--progress", action="store_true")

    memory = subparsers.add_parser("memorybench", help="Audit official MemoryBench corpus.")
    memory.add_argument("--corpus-dir", default=".eval-corpus")
    memory.add_argument("--output-dir", default=default_output_dir())
    memory.add_argument("--memorybench-variant", choices=["balanced", "full"], default="full")
    memory.add_argument("--dataset", action="append")
    memory.add_argument("--limit-rows", type=int)
    memory.add_argument("--failure-limit", type=int, default=20)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_self_test()
    if args.command == "bright-pro":
        return run_bright_pro(args)
    if args.command == "memorybench":
        return run_memorybench(args)
    print("choose a command: bright-pro or memorybench", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
