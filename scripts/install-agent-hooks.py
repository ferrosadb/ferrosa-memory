#!/usr/bin/env python3
"""Install Ferrosa Memory lifecycle hook wrappers for local agent harnesses.

The installer is intentionally conservative:
- it always writes executable wrapper scripts under ~/.config/ferrosa-memory/hooks
- it patches Claude Code and Codex settings JSON when available
- it patches Hermes YAML only when the existing hooks block is empty
- it writes snippets for anything it cannot safely patch automatically
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import stat
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path


DEFAULT_MCP_URL = "http://127.0.0.1:18765/mcp"
DEFAULT_INSTALL_DIR = Path.home() / ".config" / "ferrosa-memory" / "hooks"


def log(message: str) -> None:
    print(f"[install-agent-hooks] {message}")


def shell_quote(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


# Documentation placeholder that must never be written as an active credential, and
# must be rejected if a caller passes it (e.g. by accidentally capturing the commented
# template line). HTTP headers are single-line, so an embedded newline is always invalid.
AUTH_HEADER_PLACEHOLDER = "Basic <base64 user:password>"


def validate_auth_header(value: str) -> str:
    """Validate an Authorization header value before it is persisted to the hook env.

    Guards the 2026-06-15 footgun where ``grep``-ing the env for the auth header also
    matched the commented placeholder, producing a two-line value that urllib later
    rejects at request time with ``ValueError: Invalid header value``. Returns the
    stripped header; raises ``ValueError`` for empty, multi-line, or placeholder input.
    """
    cleaned = value.strip()
    if not cleaned:
        raise ValueError("auth header is empty")
    if "\n" in cleaned or "\r" in cleaned:
        raise ValueError(
            "auth header must be a single line (found an embedded newline) — you likely "
            "captured the commented placeholder too; pass only the real header value"
        )
    if "<base64" in cleaned or cleaned == AUTH_HEADER_PLACEHOLDER:
        raise ValueError("auth header is the documentation placeholder, not a real credential")
    return cleaned


def has_usable_auth(auth_header: str | None, mcp_user: str | None, mcp_password: str | None) -> bool:
    """True when the configured env can authenticate: a non-empty header, or a full
    user+password pair (a lone user or lone password cannot build a Basic header)."""
    if auth_header and auth_header.strip():
        return True
    return bool(mcp_user and mcp_password)


def auth_consistency_error(
    server_requires_auth: bool,
    auth_header: str | None,
    mcp_user: str | None,
    mcp_password: str | None,
) -> str | None:
    """Return an error message if the auth posture is inconsistent, else ``None``.

    The failure we guard against: the MCP endpoint requires auth (401 unauthenticated)
    but the hook env has no usable credentials, which installs hooks that silently 401.
    """
    if server_requires_auth and not has_usable_auth(auth_header, mcp_user, mcp_password):
        return (
            "MCP endpoint requires auth (responds 401 unauthenticated) but no usable "
            "credentials are configured. Pass --auth-header, or --mcp-user with "
            "--mcp-password. Installing now would produce hooks that silently fail with 401."
        )
    return None


def probe_auth_required(mcp_url: str, timeout: float = 5.0) -> bool | None:
    """Probe the MCP endpoint unauthenticated. Returns True if it rejects the request
    (401/403), False if it accepts it, or None if the endpoint is unreachable."""
    body = json.dumps({"jsonrpc": "2.0", "method": "tools/list", "id": 1}).encode()
    req = urllib.request.Request(
        mcp_url, data=body, headers={"Content-Type": "application/json"}, method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status in (401, 403)
    except urllib.error.HTTPError as exc:
        return exc.code in (401, 403)
    except (urllib.error.URLError, OSError):
        return None


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def command_exists(name: str) -> bool:
    return shutil.which(name) is not None


def detect_harnesses() -> list[str]:
    found: list[str] = []
    home = Path.home()
    if command_exists("codex") or (home / ".codex").exists():
        found.append("codex")
    if command_exists("claude") or (home / ".claude").exists():
        found.append("claude")
    if command_exists("hermes") or (home / ".hermes").exists():
        found.append("hermes")
    if command_exists("pi") or (home / ".pi").exists():
        found.append("pi")
    return found


def selected_harnesses(value: str) -> list[str]:
    if value == "auto":
        detected = detect_harnesses()
        return detected if detected else ["generic"]
    if value == "all":
        return ["codex", "claude", "hermes", "pi"]
    return [value]


def write_executable(path: Path, text: str) -> None:
    path.write_text(text)
    mode = path.stat().st_mode
    path.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def ensure_env_line(lines: list[str], key: str, line: str, replace: bool = False, replace_if_contains: str | None = None) -> None:
    prefix = f"export {key}="
    for index, existing in enumerate(lines):
        if existing.startswith(prefix):
            if replace or (replace_if_contains is not None and replace_if_contains in existing):
                lines[index] = line
            return
    lines.append(line)


def write_hook_env(
    env_path: Path,
    mcp_url: str,
    auth_header: str | None = None,
    mcp_user: str | None = None,
    mcp_password: str | None = None,
    tenant_id: str | None = None,
) -> None:
    if auth_header is not None:
        auth_header = validate_auth_header(auth_header)
    if env_path.exists():
        lines = env_path.read_text().splitlines()
    else:
        lines = [
            "# Ferrosa Memory agent hook environment.",
            "# Uncomment one auth style if your MCP endpoint requires auth.",
            "# export FERROSA_MEMORY_MCP_USER='ferrosa_user'",
            "# export FERROSA_MEMORY_MCP_PASSWORD='ferrosa_user'",
            "# Set FERROSA_MEMORY_AUTH_HEADER to your full Authorization header value if the endpoint requires it.",
            "# Set FERROSA_MEMORY_HOOK_EMBED_MISSING=true after configuring an embedding provider.",
        ]
    ensure_env_line(lines, "FERROSA_MEMORY_MCP_URL", f"export FERROSA_MEMORY_MCP_URL={shell_quote(mcp_url)}", replace=True)
    if auth_header is not None:
        ensure_env_line(lines, "FERROSA_MEMORY_AUTH_HEADER", f"export FERROSA_MEMORY_AUTH_HEADER={shell_quote(auth_header)}", replace=True)
    if mcp_user is not None:
        ensure_env_line(lines, "FERROSA_MEMORY_MCP_USER", f"export FERROSA_MEMORY_MCP_USER={shell_quote(mcp_user)}", replace=True)
    if mcp_password is not None:
        ensure_env_line(lines, "FERROSA_MEMORY_MCP_PASSWORD", f"export FERROSA_MEMORY_MCP_PASSWORD={shell_quote(mcp_password)}", replace=True)
    if tenant_id is not None:
        # The server derives a request's tenant from the authenticated
        # principal and rejects a mismatched client tenant; the hook must send
        # THIS install's tenant (from `ferrosa-memory provision-tenant`), not a
        # shared default, or ingests are silently dropped.
        ensure_env_line(lines, "FERROSA_MEMORY_TENANT_ID", f"export FERROSA_MEMORY_TENANT_ID={shell_quote(tenant_id)}", replace=True)
    ensure_env_line(
        lines,
        "FERROSA_MEMORY_HOOK_TIMEOUT",
        "export FERROSA_MEMORY_HOOK_TIMEOUT=${FERROSA_MEMORY_HOOK_TIMEOUT:-8}",
        replace_if_contains=":-2.5",
    )
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_SEARCH_LIMIT", "export FERROSA_MEMORY_HOOK_SEARCH_LIMIT=${FERROSA_MEMORY_HOOK_SEARCH_LIMIT:-5}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_MIN_SCORE", "export FERROSA_MEMORY_HOOK_MIN_SCORE=${FERROSA_MEMORY_HOOK_MIN_SCORE:-0.0}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE", "export FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE=${FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE:-1.0}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT", "export FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT=${FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT:-true}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_INCLUDE_HINTS", "export FERROSA_MEMORY_HOOK_INCLUDE_HINTS=${FERROSA_MEMORY_HOOK_INCLUDE_HINTS:-false}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS", "export FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS=${FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS:-2}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_ALLOWED_KINDS", "export FERROSA_MEMORY_HOOK_ALLOWED_KINDS=${FERROSA_MEMORY_HOOK_ALLOWED_KINDS:-episodic,procedural,semantic}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS", "export FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS=${FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS:-true}")
    ensure_env_line(lines, "FERROSA_MEMORY_HOOK_EMBED_MISSING", "export FERROSA_MEMORY_HOOK_EMBED_MISSING=${FERROSA_MEMORY_HOOK_EMBED_MISSING:-false}")
    if not any("FERROSA_MEMORY_HOOK_EMBED_MISSING=true" in line for line in lines):
        lines.append("# Set FERROSA_MEMORY_HOOK_EMBED_MISSING=true after configuring an embedding provider.")
    env_path.write_text("\n".join(lines).rstrip() + "\n")
    env_path.chmod(0o600)


def wrapper_format(harness: str) -> str:
    if harness == "codex":
        return "codex-json"
    if harness == "hermes":
        return "hermes-json"
    if harness == "claude":
        return "codex-json"
    # pi + generic: plain text (the Pi extension feeds {prompt,response} JSON on
    # stdin and the turn hook treats `pi` as a generic harness).
    return "plain"


def create_wrappers(
    install_dir: Path,
    hook_path: Path,
    mcp_url: str,
    harnesses: list[str],
    auth_header: str | None = None,
    mcp_user: str | None = None,
    mcp_password: str | None = None,
    tenant_id: str | None = None,
) -> dict[str, dict[str, str]]:
    install_dir.mkdir(parents=True, exist_ok=True)
    env_path = install_dir / "env"
    write_hook_env(env_path, mcp_url, auth_header=auth_header, mcp_user=mcp_user, mcp_password=mcp_password, tenant_id=tenant_id)

    wrappers: dict[str, dict[str, str]] = {}
    for harness in harnesses:
        fmt = wrapper_format(harness)
        start = install_dir / f"{harness}-session-start.sh"
        recall = install_dir / f"{harness}-recall.sh"
        ingest = install_dir / f"{harness}-ingest-turn.sh"
        common = "\n".join(
            [
                "#!/usr/bin/env bash",
                "set -euo pipefail",
                f"ENV_FILE={shell_quote(str(env_path))}",
                "[ -f \"$ENV_FILE\" ] && . \"$ENV_FILE\"",
                f"HOOK={shell_quote(str(hook_path))}",
                "",
            ]
        )
        write_executable(
            start,
            common
            + "exec python3 \"$HOOK\" "
            + f"--harness {harness} --format {fmt} --mode session-start --event session-start \"$@\"\n",
        )
        write_executable(
            recall,
            common
            + "exec python3 \"$HOOK\" "
            + f"--harness {harness} --format {fmt} --mode recall --event pre-turn \"$@\"\n",
        )
        write_executable(
            ingest,
            common
            + "exec python3 \"$HOOK\" "
            + f"--harness {harness} --format plain --mode ingest-turn --event turn-end \"$@\"\n",
        )
        wrappers[harness] = {
            "session_start": str(start),
            "recall": str(recall),
            "ingest_turn": str(ingest),
        }
    return wrappers


def backup(path: Path) -> Path:
    stamp = time.strftime("%Y%m%d%H%M%S")
    backup_path = path.with_name(f"{path.name}.bak-ferrosa-hooks-{stamp}")
    shutil.copy2(path, backup_path)
    return backup_path


DEFAULT_HOOK_TIMEOUT_SECONDS = 10


def hook_entry(command: str, timeout: int = DEFAULT_HOOK_TIMEOUT_SECONDS) -> dict[str, object]:
    return {"type": "command", "command": command, "timeout": timeout}


def codex_hook_entry(
    command: str,
    status_message: str,
    timeout: int = DEFAULT_HOOK_TIMEOUT_SECONDS,
) -> dict[str, object]:
    return {"type": "command", "command": command, "timeout": timeout, "statusMessage": status_message}


def ensure_hook(settings: dict[str, object], event: str, command: str, matcher: str | None = None) -> bool:
    hooks = settings.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise ValueError("settings.hooks exists but is not an object")
    event_entries = hooks.setdefault(event, [])
    if not isinstance(event_entries, list):
        raise ValueError(f"settings.hooks.{event} exists but is not a list")
    for entry in event_entries:
        if not isinstance(entry, dict):
            continue
        for hook in entry.get("hooks", []) or []:
            if isinstance(hook, dict) and hook.get("command") == command:
                if hook.get("timeout") != DEFAULT_HOOK_TIMEOUT_SECONDS:
                    hook["timeout"] = DEFAULT_HOOK_TIMEOUT_SECONDS
                    return True
                return False
    new_entry: dict[str, object] = {"hooks": [hook_entry(command)]}
    if matcher is not None:
        new_entry["matcher"] = matcher
    event_entries.append(new_entry)
    return True


def ensure_hook_with_entry(
    settings: dict[str, object],
    event: str,
    command: str,
    entry: dict[str, object],
    matcher: str | None = None,
) -> bool:
    hooks = settings.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise ValueError("settings.hooks exists but is not an object")
    event_entries = hooks.setdefault(event, [])
    if not isinstance(event_entries, list):
        raise ValueError(f"settings.hooks.{event} exists but is not a list")
    for existing in event_entries:
        if not isinstance(existing, dict):
            continue
        for hook in existing.get("hooks", []) or []:
            if isinstance(hook, dict) and hook.get("command") == command:
                changed = False
                for key, value in entry.items():
                    if hook.get(key) != value:
                        hook[key] = value
                        changed = True
                return changed
    new_entry: dict[str, object] = {"hooks": [entry]}
    if matcher is not None:
        new_entry["matcher"] = matcher
    event_entries.append(new_entry)
    return True


def patch_claude(settings_path: Path, wrappers: dict[str, str], dry_run: bool) -> str:
    if not settings_path.exists():
        return f"Claude settings not found at {settings_path}; snippet written only."
    settings = json.loads(settings_path.read_text())
    changed = False
    changed |= ensure_hook(settings, "SessionStart", wrappers["session_start"])
    changed |= ensure_hook(settings, "UserPromptSubmit", wrappers["recall"])
    changed |= ensure_hook(settings, "Stop", wrappers["ingest_turn"])
    changed |= ensure_hook(settings, "SubagentStop", wrappers["ingest_turn"])
    changed |= ensure_hook(settings, "PreCompact", wrappers["ingest_turn"])
    if not changed:
        return f"Claude settings already include Ferrosa Memory hooks: {settings_path}"
    if dry_run:
        return f"Dry run: would patch Claude settings at {settings_path}"
    backup_path = backup(settings_path)
    settings_path.write_text(json.dumps(settings, indent=2) + "\n")
    return f"Patched Claude settings at {settings_path} (backup: {backup_path})"


def patch_codex(hooks_path: Path, wrappers: dict[str, str], dry_run: bool) -> str:
    settings: dict[str, object]
    if hooks_path.exists():
        settings = json.loads(hooks_path.read_text())
        if not isinstance(settings, dict):
            raise ValueError(f"{hooks_path} exists but is not a JSON object")
    else:
        settings = {}
    changed = False
    changed |= ensure_hook_with_entry(
        settings,
        "SessionStart",
        wrappers["session_start"],
        codex_hook_entry(wrappers["session_start"], "Starting Ferrosa Memory session"),
    )
    changed |= ensure_hook_with_entry(
        settings,
        "UserPromptSubmit",
        wrappers["recall"],
        codex_hook_entry(wrappers["recall"], "Loading Ferrosa Memory context"),
    )
    changed |= ensure_hook_with_entry(
        settings,
        "Stop",
        wrappers["ingest_turn"],
        codex_hook_entry(wrappers["ingest_turn"], "Saving Ferrosa Memory turn"),
    )
    changed |= ensure_hook_with_entry(
        settings,
        "SubagentStop",
        wrappers["ingest_turn"],
        codex_hook_entry(wrappers["ingest_turn"], "Saving Ferrosa Memory subagent turn"),
    )
    changed |= ensure_hook_with_entry(
        settings,
        "PreCompact",
        wrappers["ingest_turn"],
        codex_hook_entry(wrappers["ingest_turn"], "Saving Ferrosa Memory context before compaction"),
    )
    if not changed:
        return f"Codex hooks already include Ferrosa Memory hooks: {hooks_path}"
    if dry_run:
        return f"Dry run: would patch Codex hooks at {hooks_path}"
    hooks_path.parent.mkdir(parents=True, exist_ok=True)
    backup_path = backup(hooks_path) if hooks_path.exists() else None
    hooks_path.write_text(json.dumps(settings, indent=2) + "\n")
    if backup_path:
        return f"Patched Codex hooks at {hooks_path} (backup: {backup_path})"
    return f"Created Codex hooks at {hooks_path}"


def hermes_block(wrappers: dict[str, str]) -> str:
    return "\n".join(
        [
            "hooks:",
            "  on_session_start:",
            f"    - command: {json.dumps(wrappers['session_start'])}",
            f"      timeout: {DEFAULT_HOOK_TIMEOUT_SECONDS}",
            "  pre_llm_call:",
            f"    - command: {json.dumps(wrappers['recall'])}",
            f"      timeout: {DEFAULT_HOOK_TIMEOUT_SECONDS}",
            "  on_session_end:",
            f"    - command: {json.dumps(wrappers['ingest_turn'])}",
            f"      timeout: {DEFAULT_HOOK_TIMEOUT_SECONDS}",
            "  on_session_finalize:",
            f"    - command: {json.dumps(wrappers['ingest_turn'])}",
            f"      timeout: {DEFAULT_HOOK_TIMEOUT_SECONDS}",
            "",
        ]
    )


def patch_hermes(config_path: Path, wrappers: dict[str, str], dry_run: bool) -> str:
    block = hermes_block(wrappers)
    if not config_path.exists():
        if dry_run:
            return f"Dry run: would create Hermes config at {config_path}"
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(block + "hooks_auto_accept: false\n")
        return f"Created Hermes config at {config_path}"
    text = config_path.read_text()
    if "hooks: {}" not in text:
        return f"Hermes config has a non-empty or custom hooks block; snippet written only: {config_path}"
    if dry_run:
        return f"Dry run: would replace empty Hermes hooks block in {config_path}"
    backup_path = backup(config_path)
    config_path.write_text(text.replace("hooks: {}", block.rstrip(), 1))
    return f"Patched Hermes config at {config_path} (backup: {backup_path})"


def write_snippets(install_dir: Path, wrappers: dict[str, dict[str, str]]) -> None:
    if "claude" in wrappers:
        (install_dir / "claude-settings-snippet.json").write_text(
            json.dumps(
                {
                    "hooks": {
                        "SessionStart": [{"hooks": [hook_entry(wrappers["claude"]["session_start"])]}],
                        "UserPromptSubmit": [{"hooks": [hook_entry(wrappers["claude"]["recall"])]}],
                        "Stop": [{"hooks": [hook_entry(wrappers["claude"]["ingest_turn"])]}],
                        "SubagentStop": [{"hooks": [hook_entry(wrappers["claude"]["ingest_turn"])]}],
                        "PreCompact": [{"hooks": [hook_entry(wrappers["claude"]["ingest_turn"])]}],
                    }
                },
                indent=2,
            )
            + "\n"
        )
    if "hermes" in wrappers:
        (install_dir / "hermes-hooks-snippet.yaml").write_text(hermes_block(wrappers["hermes"]))
    if "codex" in wrappers:
        (install_dir / "codex-hooks-snippet.json").write_text(
            json.dumps(
                {
                    "hooks": {
                        "SessionStart": [
                            {
                                "hooks": [
                                    codex_hook_entry(
                                        wrappers["codex"]["session_start"],
                                        "Starting Ferrosa Memory session",
                                    )
                                ]
                            }
                        ],
                        "UserPromptSubmit": [
                            {
                                "hooks": [
                                    codex_hook_entry(
                                        wrappers["codex"]["recall"],
                                        "Loading Ferrosa Memory context",
                                    )
                                ]
                            }
                        ],
                        "Stop": [
                            {
                                "hooks": [
                                    codex_hook_entry(
                                        wrappers["codex"]["ingest_turn"],
                                        "Saving Ferrosa Memory turn",
                                    )
                                ]
                            }
                        ],
                        "SubagentStop": [
                            {
                                "hooks": [
                                    codex_hook_entry(
                                        wrappers["codex"]["ingest_turn"],
                                        "Saving Ferrosa Memory subagent turn",
                                    )
                                ]
                            }
                        ],
                        "PreCompact": [
                            {
                                "hooks": [
                                    codex_hook_entry(
                                        wrappers["codex"]["ingest_turn"],
                                        "Saving Ferrosa Memory context before compaction",
                                    )
                                ]
                            }
                        ],
                    }
                },
                indent=2,
            )
            + "\n"
        )


def verification_command(command: str) -> list[str]:
    try:
        first_line = Path(command).read_text(errors="ignore").splitlines()[0]
    except (IndexError, OSError):
        return [command]
    if first_line.startswith("#!/usr/bin/env bash") or first_line.startswith("#!/bin/bash"):
        return ["bash", command]
    if first_line.startswith("#!/usr/bin/env python3") or first_line.startswith("#!/usr/bin/python3"):
        return ["python3", command]
    return [command]


# ── Pi harness (https://pi.dev) ──────────────────────────────────────────────
# Pi has no JSON/YAML hook config to patch; it auto-loads TypeScript extensions
# from ~/.pi/agent/extensions/*.ts. We install a small extension that wires
# Pi's lifecycle events to the generated wrapper scripts: `before_agent_start`
# runs recall and injects the result; `agent_end` ingests the exchange. The
# extension shells out to the same wrappers as every other harness (which source
# the env file for the MCP url/creds), so there is a single hook implementation.
PI_EXTENSIONS_DIR = Path.home() / ".pi" / "agent" / "extensions"

PI_EXTENSION_TEMPLATE = """\
// Ferrosa Memory — Pi extension. AUTO-GENERATED by install-agent-hooks.py.
// Wires recall (before each agent run) + ingest (after) to the Ferrosa Memory
// hook wrappers. Best-effort and fail-open: it never blocks or breaks Pi if the
// memory server is unavailable.
import { spawn } from "node:child_process";
import type { ExtensionAPI, AgentMessage } from "@mariozechner/pi-coding-agent";

const RECALL_WRAPPER = __RECALL_WRAPPER__;
const INGEST_WRAPPER = __INGEST_WRAPPER__;
const TIMEOUT_MS = 10000;

function runHook(script: string, payload: unknown): Promise<string> {
  return new Promise((resolve) => {
    let out = "";
    let settled = false;
    const finish = (s: string) => {
      if (!settled) {
        settled = true;
        resolve(s);
      }
    };
    let child;
    try {
      child = spawn(script, [], { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      finish("");
      return;
    }
    const timer = setTimeout(() => {
      try {
        child.kill("SIGKILL");
      } catch {}
      finish("");
    }, TIMEOUT_MS);
    child.stdout?.on("data", (d) => {
      out += d.toString();
    });
    child.on("error", () => {
      clearTimeout(timer);
      finish("");
    });
    child.on("close", () => {
      clearTimeout(timer);
      finish(out);
    });
    try {
      child.stdin?.end(JSON.stringify(payload));
    } catch {
      clearTimeout(timer);
      finish("");
    }
  });
}

function extractText(message: AgentMessage | undefined): string {
  if (!message) return "";
  const content = (message as { content?: unknown }).content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((b) => (b && typeof (b as { text?: unknown }).text === "string" ? (b as { text: string }).text : ""))
      .filter(Boolean)
      .join("\\n");
  }
  return "";
}

export default function (pi: ExtensionAPI) {
  let lastPrompt = "";

  pi.on("before_agent_start", async (event) => {
    lastPrompt = event.prompt ?? "";
    const recalled = (await runHook(RECALL_WRAPPER, { prompt: lastPrompt })).trim();
    if (!recalled) return undefined;
    return {
      message: { customType: "ferrosa-memory", content: recalled, display: true },
    };
  });

  pi.on("agent_end", async (event) => {
    const messages = (event.messages ?? []) as AgentMessage[];
    let response = "";
    for (let i = messages.length - 1; i >= 0; i--) {
      if ((messages[i] as { role?: string }).role === "assistant") {
        response = extractText(messages[i]);
        break;
      }
    }
    if (!lastPrompt && !response) return;
    await runHook(INGEST_WRAPPER, { prompt: lastPrompt, assistant_response: response });
  });
}
"""


def render_pi_extension(recall_wrapper: str, ingest_wrapper: str) -> str:
    return PI_EXTENSION_TEMPLATE.replace("__RECALL_WRAPPER__", json.dumps(recall_wrapper)).replace(
        "__INGEST_WRAPPER__", json.dumps(ingest_wrapper)
    )


def install_pi_extension(
    pi_wrappers: dict[str, str],
    dry_run: bool,
    extensions_dir: Path = PI_EXTENSIONS_DIR,
) -> str:
    target = extensions_dir / "ferrosa-memory.ts"
    content = render_pi_extension(pi_wrappers["recall"], pi_wrappers["ingest_turn"])
    if dry_run:
        return f"Dry run: would write Pi extension to {target}"
    extensions_dir.mkdir(parents=True, exist_ok=True)
    if target.exists():
        backup(target)
    target.write_text(content)
    return f"Wrote Pi extension to {target} (auto-loaded by Pi)"


def verify_wrapper(command: str, mode: str) -> str:
    payload_dict = {
        "prompt": "Ferrosa Memory hook installer verification prompt",
        "assistant_response": "Ferrosa Memory hook installer verification response",
        "cwd": os.getcwd(),
        "workspace": os.getcwd(),
        "session_id": "00000000-0000-0000-0000-000000000001",
        "turn_id": f"hook-installer-{mode}",
        "hook_event_name": mode,
        "tool_results": [
            {
                "name": "verification",
                "content": "Hook verification captured tool artifact metadata.",
                "status": "ok",
            }
        ],
    }
    payload = json.dumps(payload_dict)
    try:
        proc = subprocess.run(
            verification_command(command),
            input=payload,
            text=True,
            capture_output=True,
            timeout=20,
            check=False,
        )
    except Exception as exc:
        return f"{command}: verification failed to launch: {exc}"
    if proc.returncode != 0:
        return f"{command}: exited {proc.returncode}: {proc.stderr.strip()[:300]}"
    combined = f"{proc.stdout}\n{proc.stderr}"
    if "skipped:" in combined:
        skip_line = next((line for line in combined.splitlines() if "skipped:" in line), "skipped")
        return f"{command}: FAILED (hook degraded to skip): {skip_line.strip()[:300]}"
    return f"{command}: ok"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--harness",
        choices=["auto", "all", "codex", "claude", "hermes", "pi", "generic"],
        default="auto",
    )
    parser.add_argument("--install-dir", type=Path, default=DEFAULT_INSTALL_DIR)
    parser.add_argument("--mcp-url", default=os.environ.get("FERROSA_MEMORY_MCP_URL", DEFAULT_MCP_URL))
    parser.add_argument(
        "--auth-header",
        default=os.environ.get("FERROSA_MEMORY_AUTH_HEADER"),
        help="Full Authorization header value (e.g. 'Basic <base64>') written to the hook env file.",
    )
    parser.add_argument("--mcp-user", default=os.environ.get("FERROSA_MEMORY_MCP_USER"))
    parser.add_argument("--mcp-password", default=os.environ.get("FERROSA_MEMORY_MCP_PASSWORD"))
    parser.add_argument(
        "--tenant-id",
        default=os.environ.get("FERROSA_MEMORY_TENANT_ID"),
        help="Per-install tenant UUID (from `ferrosa-memory provision-tenant`) written to the hook env "
        "as FERROSA_MEMORY_TENANT_ID. Must match the authenticated principal's tenant or ingests are dropped.",
    )
    parser.add_argument("--claude-settings", type=Path, default=Path.home() / ".claude" / "settings.json")
    parser.add_argument("--hermes-config", type=Path, default=Path.home() / ".hermes" / "config.yaml")
    parser.add_argument("--codex-hooks", type=Path, default=Path.home() / ".codex" / "hooks.json")
    parser.add_argument("--no-apply-config", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verify", action="store_true")
    parser.add_argument(
        "--skip-auth-check",
        action="store_true",
        help="Skip the MCP auth-consistency preflight (probe endpoint + match against configured creds).",
    )
    args = parser.parse_args()

    root = repo_root()
    hook_path = root / "scripts" / "hooks" / "ferrosa-memory-turn-hook.py"
    if not hook_path.exists():
        raise SystemExit(f"missing hook helper: {hook_path}")

    if args.auth_header is not None:
        try:
            args.auth_header = validate_auth_header(args.auth_header)
        except ValueError as exc:
            log(f"invalid --auth-header: {exc}")
            return 2

    if not args.skip_auth_check:
        required = probe_auth_required(args.mcp_url)
        if required is None:
            log(
                f"WARNING: could not reach {args.mcp_url} to probe its auth requirement; "
                "skipping consistency check (pass --skip-auth-check to silence)"
            )
        elif (err := auth_consistency_error(required, args.auth_header, args.mcp_user, args.mcp_password)):
            log(f"auth consistency check FAILED: {err}")
            log("Refusing to install inconsistent hooks. Re-run with --skip-auth-check to override.")
            return 3
        else:
            configured = "yes" if has_usable_auth(args.auth_header, args.mcp_user, args.mcp_password) else "no"
            log(f"auth consistency OK: server_requires_auth={required}, credentials_configured={configured}")

    harnesses = selected_harnesses(args.harness)
    log(f"harnesses: {', '.join(harnesses)}")
    wrappers = create_wrappers(
        args.install_dir,
        hook_path,
        args.mcp_url,
        harnesses,
        auth_header=args.auth_header,
        mcp_user=args.mcp_user,
        mcp_password=args.mcp_password,
        tenant_id=args.tenant_id,
    )
    write_snippets(args.install_dir, wrappers)

    results: list[str] = []
    if not args.no_apply_config:
        if "claude" in wrappers:
            results.append(patch_claude(args.claude_settings.expanduser(), wrappers["claude"], args.dry_run))
        if "codex" in wrappers:
            results.append(patch_codex(args.codex_hooks.expanduser(), wrappers["codex"], args.dry_run))
        if "hermes" in wrappers:
            results.append(patch_hermes(args.hermes_config.expanduser(), wrappers["hermes"], args.dry_run))
        if "pi" in wrappers:
            results.append(install_pi_extension(wrappers["pi"], args.dry_run))
    else:
        results.append("Skipped harness config patching because --no-apply-config was set.")

    manifest = {
        "install_dir": str(args.install_dir),
        "mcp_url": args.mcp_url,
        "detected_harnesses": detect_harnesses(),
        "installed_harnesses": harnesses,
        "wrappers": wrappers,
        "config_results": results,
    }
    manifest_path = args.install_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

    for result in results:
        log(result)
    if args.verify:
        for harness, commands in wrappers.items():
            log(f"{harness} session-start verification: {verify_wrapper(commands['session_start'], 'session-start')}")
            log(f"{harness} recall verification: {verify_wrapper(commands['recall'], 'recall')}")
            log(f"{harness} ingest verification: {verify_wrapper(commands['ingest_turn'], 'ingest-turn')}")
    log(f"wrote manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
