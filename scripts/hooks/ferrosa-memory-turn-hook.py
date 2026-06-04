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


def transcript_tail(payload: dict[str, Any]) -> tuple[str, str]:
    path = payload.get("transcript_path")
    if not isinstance(path, str) or not path:
        return "", ""
    transcript = Path(path).expanduser()
    if not transcript.exists() or transcript.stat().st_size > 20_000_000:
        return "", ""
    user = ""
    assistant = ""
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
        return user, assistant
    except Exception as exc:
        eprint(f"transcript read failed: {exc}")
        return "", ""


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
    if not prompt and not response:
        prompt, response = transcript_tail(payload)
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
        ]
        if part
    )
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
                    "attrs": {
                        "harness": args.harness,
                        "hook_event_name": payload.get("hook_event_name") or args.event,
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "cwd": cwd,
                        "workspace": cwd,
                        "working_directory": cwd,
                        "captured_at_ms": int(time.time() * 1000),
                    },
                }
            ],
            "options": {
                "embed_missing": False,
                "on_conflict": "skip",
                "strict_edges": True,
            },
        },
    )


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
    parser.add_argument("--timeout", type=float, default=float(os.environ.get("FERROSA_MEMORY_HOOK_TIMEOUT", "2.5")))
    parser.add_argument("--limit", type=int, default=5)
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
