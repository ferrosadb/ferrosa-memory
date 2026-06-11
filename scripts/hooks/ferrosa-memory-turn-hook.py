#!/usr/bin/env python3
"""Ferrosa Memory lifecycle hook helper for Codex, Claude, and Hermes.

The hook is intentionally best-effort: failures are logged to stderr and the
agent turn continues. Use `recall` for pre-turn context injection and
`ingest-turn` for opt-in turn artifact capture.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
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


def recall_context(client: McpClient, payload: dict[str, Any], args: argparse.Namespace) -> str:
    prompt = extract_prompt(payload)
    if not prompt:
        return ""
    cwd = cwd_from_payload(payload)
    pieces: list[str] = []
    try:
        intentions = client.call_tool(
            "check_intentions",
            {"context": prompt[:2000], "repo": cwd},
        )
        pieces.extend(content_blocks(intentions))
    except Exception as exc:
        eprint(f"check_intentions failed: {exc}")
    try:
        search = client.call_tool(
            "hybrid_search",
            {
                "query": prompt[:4000],
                "limit": args.limit,
                "scope": "both",
                "cwd": cwd,
            },
        )
        pieces.extend(content_blocks(search))
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
    session_id = str(payload.get("session_id") or DEFAULT_SESSION_ID)
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
                "session_id": session_id,
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
    parser.add_argument("--mode", choices=["recall", "ingest-turn"], default="recall")
    parser.add_argument("--harness", choices=["codex", "claude", "hermes", "generic"], default="generic")
    parser.add_argument("--event", default="")
    parser.add_argument("--format", choices=["plain", "codex-json", "hermes-json"], default="plain")
    parser.add_argument("--mcp-url", default=os.environ.get("FERROSA_MEMORY_MCP_URL", DEFAULT_URL))
    parser.add_argument("--timeout", type=float, default=float(os.environ.get("FERROSA_MEMORY_HOOK_TIMEOUT", "8")))
    parser.add_argument("--limit", type=int, default=int(os.environ.get("FERROSA_MEMORY_HOOK_SEARCH_LIMIT", "5")))
    parser.add_argument("--max-context-chars", type=int, default=4000)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    payload = read_payload()
    try:
        client = McpClient(args.mcp_url, args.timeout)
        client.initialize()
        if args.mode == "recall":
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
