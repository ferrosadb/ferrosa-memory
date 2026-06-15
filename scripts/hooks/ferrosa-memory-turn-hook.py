#!/usr/bin/env python3
"""Ferrosa Memory lifecycle hook helper for Codex, Claude, and Hermes.

The hook is intentionally best-effort: failures are logged to stderr and the
agent turn continues. Use `recall` for pre-turn context injection and
`ingest-turn` for opt-in turn artifact capture.

Correctness: Correct when lifecycle events keep agent turns running, recall
context is compact and human-usable, and ingestion preserves enough turn
evidence for later search.
Last revised: 2026-06-15
Last changed: Emit recall context only when a result has a positive reranker
judgment, keeping unjudged or low-confidence searches silent.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any


DEFAULT_URL = "http://127.0.0.1:18765/mcp"
DEFAULT_TENANT_ID = "9a5f8fbf-d842-4d30-8ea5-1aa931e618a8"
DEFAULT_SESSION_ID = "00000000-0000-0000-0000-000000000000"
RAW_CONTEXT_LINE = re.compile(r"^(?P<prefix>[A-Za-z_]+)\[\d+\]:\s*(?P<payload>\{.*\})$")
RAW_CONTEXT_ANY_LINE = re.compile(r"^(?P<prefix>[A-Za-z_]+)\[\d+\]:\s*(?P<payload>.*)$")
RAW_CONTEXT_PREFIX = re.compile(r"^[A-Za-z_]+\[\d+\]:")
TOKEN_RE = re.compile(r"[A-Za-z][A-Za-z0-9_-]{2,}")
QUERY_STOPWORDS = {
    "about",
    "after",
    "again",
    "also",
    "and",
    "are",
    "because",
    "been",
    "before",
    "better",
    "but",
    "can",
    "did",
    "does",
    "doing",
    "done",
    "for",
    "from",
    "get",
    "had",
    "has",
    "have",
    "how",
    "into",
    "its",
    "just",
    "let",
    "like",
    "now",
    "our",
    "out",
    "should",
    "that",
    "the",
    "then",
    "there",
    "these",
    "this",
    "those",
    "through",
    "was",
    "were",
    "what",
    "when",
    "where",
    "with",
    "would",
    "you",
    "your",
}


def eprint(message: str) -> None:
    print(f"[ferrosa-memory-hook] {message}", file=sys.stderr)


def read_payload() -> dict[str, Any]:
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            return {}
        parsed = json.loads(raw)
        return parsed if isinstance(parsed, dict) else {}
    except Exception as exc:
        eprint(f"invalid hook payload: {exc}")
        return {}


def env_auth_header() -> str | None:
    header = os.environ.get("FERROSA_MEMORY_AUTH_HEADER")
    if header:
        return header
    token = os.environ.get("FERROSA_MEMORY_MCP_AUTH")
    if token:
        return token if token.lower().startswith("basic ") else f"Basic {token}"
    user = os.environ.get("FERROSA_MEMORY_MCP_USER")
    password = os.environ.get("FERROSA_MEMORY_MCP_PASSWORD")
    if user and password:
        encoded = base64.b64encode(f"{user}:{password}".encode()).decode()
        return f"Basic {encoded}"
    return None


def env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def env_float(name: str, default: float) -> float:
    value = os.environ.get(name)
    if value is None:
        return default
    try:
        return float(value)
    except ValueError:
        eprint(f"invalid {name}={value!r}; using {default}")
        return default


def env_csv(name: str, default: set[str]) -> set[str]:
    value = os.environ.get(name)
    if value is None:
        return set(default)
    return parse_csv_set(value, default)


def parse_csv_set(value: str, default: set[str]) -> set[str]:
    parsed = {item.strip().lower() for item in value.split(",") if item.strip()}
    return parsed or set(default)


def query_terms(text: str) -> set[str]:
    return {token.lower() for token in TOKEN_RE.findall(text) if token.lower() not in QUERY_STOPWORDS}


def query_overlap_count(prompt_terms: set[str], text: str) -> int:
    if not prompt_terms:
        return 0
    return len(prompt_terms & query_terms(text))


class McpClient:
    def __init__(self, url: str, timeout: float) -> None:
        self.url = url
        self.timeout = timeout
        self.next_id = 1
        self.auth_header = env_auth_header()

    def request(self, method: str, params: dict[str, Any] | None = None) -> Any:
        body = {
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params or {},
        }
        self.next_id += 1
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            self.url,
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        if self.auth_header:
            req.add_header("Authorization", self.auth_header)
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            parsed = json.loads(resp.read().decode())
        if "error" in parsed:
            raise RuntimeError(parsed["error"])
        return parsed.get("result")

    def initialize(self) -> None:
        self.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ferrosa-memory-turn-hook", "version": "0.1"},
            },
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> Any:
        return self.request("tools/call", {"name": name, "arguments": arguments})


def extract_prompt(payload: dict[str, Any]) -> str:
    for key in ("prompt", "user_prompt", "user_message", "message"):
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    extra = payload.get("extra")
    if isinstance(extra, dict):
        for key in ("user_message", "prompt", "message"):
            value = extra.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return ""


def extract_response(payload: dict[str, Any]) -> str:
    for key in ("assistant_response", "response", "final_response"):
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    extra = payload.get("extra")
    if isinstance(extra, dict):
        for key in ("assistant_response", "response", "final_response"):
            value = extra.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return ""


def compact_text(value: Any, limit: int = 4000) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value.strip()[:limit]
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True)[:limit]
    except Exception:
        return str(value)[:limit]


def object_content(obj: dict[str, Any]) -> str:
    for key in ("content", "text", "output", "result", "message", "args", "arguments"):
        value = obj.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
        if isinstance(value, (dict, list)):
            return compact_text(value)
    return compact_text(obj)


def artifact_from_object(obj: Any, source: str) -> dict[str, str] | None:
    if not isinstance(obj, dict):
        text = compact_text(obj)
        return {"source": source, "name": source, "content": text} if text else None
    role = obj.get("role") or obj.get("type") or obj.get("kind") or obj.get("event")
    name = obj.get("name") or obj.get("tool_name") or obj.get("function") or role or source
    content = object_content(obj)
    if not content:
        return None
    return {
        "source": source,
        "name": str(name)[:128],
        "role": str(role or "tool")[:64],
        "content": content[:4000],
    }


def append_artifacts(value: Any, source: str, out: list[dict[str, str]], limit: int = 20) -> None:
    if len(out) >= limit:
        return
    if isinstance(value, list):
        for item in value:
            append_artifacts(item, source, out, limit)
            if len(out) >= limit:
                return
    else:
        artifact = artifact_from_object(value, source)
        if artifact:
            out.append(artifact)


def extract_tool_artifacts(payload: dict[str, Any]) -> list[dict[str, str]]:
    artifacts: list[dict[str, str]] = []
    for key in (
        "tool_calls",
        "tool_results",
        "tool_outputs",
        "tool_uses",
        "function_calls",
        "function_results",
        "events",
    ):
        if key in payload:
            append_artifacts(payload[key], key, artifacts)
    extra = payload.get("extra")
    if isinstance(extra, dict):
        for key in ("tool_calls", "tool_results", "tool_outputs", "events"):
            if key in extra:
                append_artifacts(extra[key], f"extra.{key}", artifacts)
    return artifacts[:20]


def transcript_tail(payload: dict[str, Any]) -> tuple[str, str, list[dict[str, str]]]:
    path = payload.get("transcript_path")
    if not isinstance(path, str) or not path:
        return "", "", []
    transcript = Path(path).expanduser()
    if not transcript.exists() or transcript.stat().st_size > 20_000_000:
        return "", "", []
    user = ""
    assistant = ""
    artifacts: list[dict[str, str]] = []
    try:
        lines = transcript.read_text(errors="ignore").splitlines()[-200:]
        for line in lines:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            text = json.dumps(obj, ensure_ascii=False)
            role = obj.get("role") or obj.get("type") or obj.get("message", {}).get("role")
            if role == "user" and not user:
                user = text[:4000]
            elif role == "assistant":
                assistant = text[:4000]
            role_text = str(role or "").lower()
            if (
                "tool" in role_text
                or "function" in role_text
                or obj.get("tool_call_id")
                or obj.get("tool_name")
            ):
                append_artifacts(obj, "transcript", artifacts)
        return user, assistant, artifacts[:20]
    except Exception as exc:
        eprint(f"transcript read failed: {exc}")
        return "", "", []


def cwd_from_payload(payload: dict[str, Any]) -> str:
    value = payload.get("cwd") or payload.get("workspace") or os.getcwd()
    return str(value)


def first_string(payload: dict[str, Any], keys: tuple[str, ...]) -> str:
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    extra = payload.get("extra")
    if isinstance(extra, dict):
        for key in keys:
            value = extra.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return ""


def agent_session_id(payload: dict[str, Any]) -> str:
    return (
        first_string(
            payload,
            (
                "session_id",
                "transcript_path",
                "conversation_id",
                "thread_id",
                "run_id",
                "codex_thread_id",
                "claude_session_id",
            ),
        )
        or os.environ.get("FERROSA_MEMORY_AGENT_SESSION_ID", "").strip()
        or os.environ.get("CLAUDE_CODE_SESSION_ID", "").strip()
        or os.environ.get("CODEX_THREAD_ID", "").strip()
    )


def configure_session_start(client: McpClient, payload: dict[str, Any], args: argparse.Namespace) -> str:
    cwd = cwd_from_payload(payload)
    metadata = {
        "agent": args.harness,
        "workspace": cwd,
        "cwd": cwd,
    }
    external = agent_session_id(payload)
    if external:
        metadata["agent_session_id"] = external
    result = client.call_tool("configure", {"session_start": metadata})
    blocks = content_blocks(result)
    for block in blocks:
        try:
            parsed = json.loads(block)
        except json.JSONDecodeError:
            continue
        sid = parsed.get("session_id")
        if isinstance(sid, str) and sid:
            return sid
    return ""


def current_fmem_session_id(client: McpClient) -> str:
    result = client.call_tool("configure", {})
    blocks = content_blocks(result)
    for block in blocks:
        try:
            parsed = json.loads(block)
        except json.JSONDecodeError:
            continue
        sid = parsed.get("session_id")
        if isinstance(sid, str) and sid:
            return sid
    return DEFAULT_SESSION_ID


def stable_turn_id(payload: dict[str, Any], harness: str, prompt: str, response: str) -> str:
    for key in ("turn_id", "tool_use_id"):
        value = payload.get(key)
        if isinstance(value, str) and value:
            return value
    digest = hashlib.sha256(
        "\n".join(
            [
                harness,
                str(payload.get("session_id") or ""),
                cwd_from_payload(payload),
                prompt[:1000],
                response[:1000],
            ]
        ).encode()
    ).hexdigest()
    return digest[:32]


def content_blocks(result: Any) -> list[str]:
    blocks = []
    if isinstance(result, dict):
        for block in result.get("content", []) or []:
            if isinstance(block, dict) and block.get("type") == "text":
                text = block.get("text")
                if isinstance(text, str) and text.strip():
                    blocks.append(text.strip())
    return blocks


def clean_recall_text(value: str) -> str:
    return "\n".join(
        line for line in value.strip().splitlines() if not RAW_CONTEXT_PREFIX.match(line.strip())
    ).strip()


def content_texts(value: Any) -> list[str]:
    texts: list[str] = []
    if isinstance(value, str) and value.strip():
        cleaned = clean_recall_text(value)
        if cleaned:
            texts.append(cleaned)
    elif isinstance(value, dict):
        for key in ("text", "content", "stdout", "stderr"):
            text = value.get(key)
            if isinstance(text, str) and text.strip():
                cleaned = clean_recall_text(text)
                if cleaned:
                    texts.append(cleaned)
        for key in ("message", "toolUseResult"):
            texts.extend(content_texts(value.get(key)))
    elif isinstance(value, list):
        for item in value:
            texts.extend(content_texts(item))
    return texts


def transcript_entry_text(prefix: str, entry: dict[str, Any]) -> str:
    message = entry.get("message")
    role = prefix
    if isinstance(message, dict) and isinstance(message.get("role"), str):
        role = message["role"]

    content = message.get("content") if isinstance(message, dict) else None
    texts = content_texts(content)
    if not texts:
        texts = content_texts(entry.get("toolUseResult"))
    if not texts:
        return ""

    label = "Assistant" if role == "assistant" else "User"
    if role == "user" and isinstance(content, list):
        if any(isinstance(item, dict) and item.get("type") == "tool_result" for item in content):
            label = "Tool result"
    return f"{label}: {' '.join(texts)[:1200]}"


def compact_raw_context_segment(text: str) -> list[str]:
    pieces: list[str] = []
    for line in text.splitlines():
        stripped = line.strip()
        match = RAW_CONTEXT_LINE.match(stripped)
        if match:
            try:
                entry = json.loads(match.group("payload"))
            except json.JSONDecodeError:
                continue
            if isinstance(entry, dict):
                rendered = transcript_entry_text(match.group("prefix"), entry)
                if rendered:
                    pieces.append(rendered)
            continue

        plain_match = RAW_CONTEXT_ANY_LINE.match(stripped)
        if not plain_match:
            continue
        payload = plain_match.group("payload").strip()
        if not payload:
            continue
        prefix = plain_match.group("prefix").lower()
        label = "Assistant" if prefix == "assistant" else "User"
        if "tool" in prefix:
            label = "Tool result"
        pieces.append(f"{label}: {payload[:1200]}")
    return pieces


def compact_result_content(content: str) -> list[str]:
    compacted = compact_raw_context_segment(content)
    if compacted:
        return compacted
    if any(RAW_CONTEXT_PREFIX.match(line.strip()) for line in content.splitlines()):
        return []
    return [content.strip()] if content.strip() else []


def result_memory_kind(result: dict[str, Any]) -> str:
    kind = result.get("memory_kind")
    if isinstance(kind, str) and kind.strip():
        return kind.strip().lower()
    result_type = result.get("result_type")
    source = result.get("source")
    if result_type == "context_segment" or source in {"context_bm25", "context_ann", "fold_ann"}:
        return "episodic"
    if result_type == "document_chunk":
        return "semantic"
    return "semantic"


def label_memory_piece(kind: str, text: str) -> str:
    labels = {
        "episodic": "Episodic memory",
        "procedural": "Procedural memory",
        "semantic": "Semantic memory",
    }
    return f"{labels.get(kind, 'Memory')}: {text}"


def result_score(result: dict[str, Any]) -> float | None:
    score = result.get("score")
    if isinstance(score, int | float):
        return float(score)
    if isinstance(score, str):
        try:
            return float(score)
        except ValueError:
            return None
    return None


def judge_score(value: Any) -> float | None:
    if isinstance(value, int | float):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def reranker_judgments(parsed: dict[str, Any], min_judge_score: float) -> dict[str, bool]:
    reranker = parsed.get("reranker")
    if not isinstance(reranker, dict) or not reranker.get("applied"):
        return {}
    judged_ids = reranker.get("judged_ids")
    judge_scores = reranker.get("judge_scores")
    if not isinstance(judged_ids, list) or not isinstance(judge_scores, list):
        return {}

    judgments: dict[str, bool] = {}
    for result_id, score_value in zip(judged_ids, judge_scores, strict=False):
        if not isinstance(result_id, str):
            continue
        score = judge_score(score_value)
        judgments[result_id] = score is not None and score >= min_judge_score
    return judgments


def triggered_intentions(parsed: dict[str, Any]) -> list[str]:
    triggered = parsed.get("triggered")
    if not isinstance(triggered, list):
        return []

    pieces: list[str] = []
    for item in triggered:
        if isinstance(item, dict):
            text = (
                item.get("description")
                or item.get("content")
                or item.get("context")
                or item.get("name")
                or item.get("id")
            )
            if isinstance(text, str) and text.strip():
                pieces.append(f"Intention: {text.strip()[:1200]}")
        elif isinstance(item, str) and item.strip():
            pieces.append(f"Intention: {item.strip()[:1200]}")
    return pieces


def compact_recall_block(
    text: str,
    min_score: float = 0.0,
    min_judge_score: float = 1.0,
    require_judgment: bool = True,
    include_hints: bool = False,
    allowed_kinds: set[str] | None = None,
) -> list[str]:
    """Extract only agent-useful context from a tool result text block."""
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        return [text] if text.strip() else []
    if not isinstance(parsed, dict):
        return [text] if text.strip() else []

    intention_pieces = triggered_intentions(parsed)
    if intention_pieces:
        return intention_pieces

    hints: list[str] = []
    for key in ("_hint", "hint"):
        value = parsed.get(key)
        if isinstance(value, str) and value.strip():
            hints.append(value.strip())

    judgments = reranker_judgments(parsed, min_judge_score)
    result_pieces: list[str] = []
    results = parsed.get("results")
    if isinstance(results, list):
        for result in results:
            if isinstance(result, dict):
                kind = result_memory_kind(result)
                if allowed_kinds is not None and kind not in allowed_kinds:
                    continue
                result_id = result.get("id")
                if judgments:
                    if not isinstance(result_id, str) or not judgments.get(result_id, False):
                        continue
                elif require_judgment:
                    continue
                score = result_score(result)
                if score is None and min_score > 0:
                    continue
                if score is not None and score < min_score:
                    continue
                content = result.get("content")
                if isinstance(content, str) and content.strip():
                    result_pieces.extend(
                        label_memory_piece(kind, piece) for piece in compact_result_content(content)
                    )

    if not result_pieces:
        return []
    if include_hints:
        return hints + result_pieces
    return result_pieces


def recall_blocks(
    result: Any,
    min_score: float = 0.0,
    min_judge_score: float = 1.0,
    require_judgment: bool = True,
    include_hints: bool = False,
    allowed_kinds: set[str] | None = None,
) -> list[str]:
    pieces: list[str] = []
    for block in content_blocks(result):
        pieces.extend(
            compact_recall_block(
                block,
                min_score=min_score,
                min_judge_score=min_judge_score,
                require_judgment=require_judgment,
                include_hints=include_hints,
                allowed_kinds=allowed_kinds,
            )
        )
    return pieces


def recall_context(client: McpClient, payload: dict[str, Any], args: argparse.Namespace) -> str:
    prompt = extract_prompt(payload)
    if not prompt:
        return ""
    cwd = cwd_from_payload(payload)
    session_id = current_fmem_session_id(client)
    min_score = max(0.0, args.min_score)
    min_judge_score = args.min_judge_score
    require_judgment = args.require_judgment
    include_hints = args.include_hints
    allowed_kinds = args.allowed_kinds
    min_query_terms = max(0, args.min_query_terms)
    prompt_terms = query_terms(prompt)
    pieces: list[str] = []

    def search_recall(
        scope: str,
        require_judgment_for_scope: bool,
        scope_min_score: float,
        scope_min_query_terms: int,
    ) -> list[str]:
        search_args = {
            "session_id": session_id,
            "query": prompt[:4000],
            "limit": args.limit,
            "scope": scope,
            "cwd": cwd,
            "min_score": scope_min_score,
        }
        if allowed_kinds is not None:
            search_args["memory_kinds"] = sorted(allowed_kinds)
        search = client.call_tool(
            "hybrid_search",
            search_args,
        )
        search_pieces = recall_blocks(
            search,
            min_score=scope_min_score,
            min_judge_score=min_judge_score,
            require_judgment=require_judgment_for_scope,
            include_hints=include_hints,
            allowed_kinds=allowed_kinds,
        )
        if scope_min_query_terms > 0:
            search_pieces = [
                piece
                for piece in search_pieces
                if query_overlap_count(prompt_terms, piece) >= scope_min_query_terms
            ]
        return search_pieces

    try:
        intentions = client.call_tool(
            "check_intentions",
            {"context": prompt[:2000], "repo": cwd},
        )
        pieces.extend(
            recall_blocks(
                intentions,
                min_score=min_score,
                min_judge_score=min_judge_score,
                require_judgment=False,
                include_hints=False,
                allowed_kinds=None,
            )
        )
    except Exception as exc:
        eprint(f"check_intentions failed: {exc}")
    try:
        # Session-local recall is already scoped to the current conversation,
        # so query overlap + score is a stronger guard than a small reranker set
        # that may abstain on single-result searches.
        search_pieces = search_recall(
            "session",
            require_judgment_for_scope=False,
            scope_min_score=min_score,
            scope_min_query_terms=min_query_terms,
        )
        if not search_pieces:
            search_pieces = search_recall(
                "both",
                require_judgment_for_scope=require_judgment,
                scope_min_score=max(min_score, 0.35),
                scope_min_query_terms=max(min_query_terms, 3),
            )
        pieces.extend(search_pieces)
    except Exception as exc:
        eprint(f"hybrid_search failed: {exc}")
    if not pieces:
        return ""
    joined = "\n".join(pieces)
    return f"Ferrosa Memory context for cwd={cwd}:\n{joined[: args.max_context_chars]}"


def ingest_turn(client: McpClient, payload: dict[str, Any], args: argparse.Namespace) -> None:
    prompt = extract_prompt(payload)
    response = extract_response(payload)
    artifacts = extract_tool_artifacts(payload)
    if not prompt and not response:
        prompt, response, transcript_artifacts = transcript_tail(payload)
        artifacts.extend(transcript_artifacts)
    if not prompt and not response:
        return
    cwd = cwd_from_payload(payload)
    session_id = current_fmem_session_id(client)
    turn_id = stable_turn_id(payload, args.harness, prompt, response)
    entity_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"ferrosa-memory-hook:{args.harness}:{session_id}:{turn_id}"))
    text = "\n\n".join(
        part
        for part in [
            f"User: {prompt}" if prompt else "",
            f"Assistant: {response}" if response else "",
            (
                "Tool artifacts:\n"
                + "\n".join(
                    f"- {artifact.get('name', 'tool')}: {artifact.get('content', '')[:1000]}"
                    for artifact in artifacts[:10]
                )
                if artifacts
                else ""
            ),
        ]
        if part
    )
    common_attrs = {
        "harness": args.harness,
        "hook_event_name": payload.get("hook_event_name") or args.event,
        "session_id": session_id,
        "turn_id": turn_id,
        "cwd": cwd,
        "workspace": cwd,
        "working_directory": cwd,
        "captured_at_ms": int(time.time() * 1000),
    }
    try:
        client.call_tool(
            "ingest_entities",
            {
                "tenant_id": os.environ.get("FERROSA_MEMORY_TENANT_ID", DEFAULT_TENANT_ID),
                "entities": [
                    {
                        "id": entity_id,
                        "name": f"{args.harness} turn {turn_id}",
                        "entity_type": "turn",
                        "context": text[:16000],
                        "confidence": 0.7,
                        "attrs": common_attrs,
                    }
                ],
                "edges": [],
                "options": {
                    "embed_missing": False,
                    "on_conflict": "skip",
                    "strict_edges": True,
                },
            },
        )
    except Exception as exc:
        eprint(f"turn entity ingest failed: {exc}")

    if not env_bool("FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS", True):
        return
    messages: list[dict[str, Any]] = []
    turn_index = 0
    if prompt:
        messages.append(
            {
                "role": "user",
                "content": prompt[:131072],
                "turn_index": turn_index,
                "metadata": common_attrs,
            }
        )
        turn_index += 1
    if response:
        messages.append(
            {
                "role": "assistant",
                "content": response[:131072],
                "turn_index": turn_index,
                "metadata": common_attrs,
            }
        )
        turn_index += 1
    for artifact in artifacts[:10]:
        metadata = dict(common_attrs)
        metadata.update(
            {
                "tool_source": artifact.get("source", ""),
                "tool_name": artifact.get("name", ""),
                "tool_role": artifact.get("role", ""),
            }
        )
        messages.append(
            {
                "role": "tool",
                "content": artifact.get("content", "")[:131072],
                "turn_index": turn_index,
                "metadata": metadata,
            }
        )
        turn_index += 1
    if not messages:
        return
    try:
        client.call_tool(
            "ctx_ingest",
            {
                "session_id": session_id,
                "conversation_id": f"{args.harness}:{session_id}:{cwd}",
                "messages": messages,
                "embed_missing": env_bool("FERROSA_MEMORY_HOOK_EMBED_MISSING", False),
            },
        )
    except Exception as exc:
        eprint(f"context segment ingest failed: {exc}")


def emit_context(context: str, output_format: str, event: str) -> None:
    if not context:
        return
    if output_format == "hermes-json":
        print(json.dumps({"context": context}))
    elif output_format == "codex-json":
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": event,
                        "additionalContext": context,
                    }
                }
            )
        )
    else:
        print(context)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["session-start", "recall", "ingest-turn"], default="recall")
    parser.add_argument("--harness", choices=["codex", "claude", "hermes", "generic"], default="generic")
    parser.add_argument("--event", default="")
    parser.add_argument("--format", choices=["plain", "codex-json", "hermes-json"], default="plain")
    parser.add_argument("--mcp-url", default=os.environ.get("FERROSA_MEMORY_MCP_URL", DEFAULT_URL))
    parser.add_argument("--timeout", type=float, default=float(os.environ.get("FERROSA_MEMORY_HOOK_TIMEOUT", "8")))
    parser.add_argument("--limit", type=int, default=int(os.environ.get("FERROSA_MEMORY_HOOK_SEARCH_LIMIT", "5")))
    parser.add_argument("--min-score", type=float, default=env_float("FERROSA_MEMORY_HOOK_MIN_SCORE", 0.0))
    parser.add_argument(
        "--min-judge-score",
        type=float,
        default=env_float("FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE", 1.0),
    )
    parser.add_argument(
        "--require-judgment",
        action=argparse.BooleanOptionalAction,
        default=env_bool("FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT", True),
    )
    parser.add_argument(
        "--include-hints",
        action=argparse.BooleanOptionalAction,
        default=env_bool("FERROSA_MEMORY_HOOK_INCLUDE_HINTS", False),
    )
    parser.add_argument(
        "--min-query-terms",
        type=int,
        default=int(os.environ.get("FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS", "2")),
    )
    parser.add_argument(
        "--allowed-kinds",
        type=lambda value: parse_csv_set(value, {"episodic", "procedural", "semantic"}),
        default=env_csv(
            "FERROSA_MEMORY_HOOK_ALLOWED_KINDS",
            {"episodic", "procedural", "semantic"},
        ),
    )
    parser.add_argument("--max-context-chars", type=int, default=4000)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    payload = read_payload()
    try:
        client = McpClient(args.mcp_url, args.timeout)
        client.initialize()
        if args.mode == "session-start":
            session_id = configure_session_start(client, payload, args)
            if args.format == "hermes-json":
                print(json.dumps({"session_id": session_id}))
            elif args.format == "codex-json":
                print(json.dumps({"session_id": session_id}))
        elif args.mode == "recall":
            context = recall_context(client, payload, args)
            emit_context(context, args.format, payload.get("hook_event_name") or args.event)
        else:
            ingest_turn(client, payload, args)
            if args.format == "hermes-json":
                print("{}")
    except (urllib.error.URLError, TimeoutError, RuntimeError, OSError) as exc:
        eprint(f"{args.mode} skipped: {exc}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
