#!/usr/bin/env python3
"""Install Ferrosa Memory lifecycle hook wrappers for local agent harnesses.

The installer is intentionally conservative:
- it always writes executable wrapper scripts under ~/.config/ferrosa-memory/hooks
- it patches Claude Code settings JSON when available
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
import sys
import time
from pathlib import Path


DEFAULT_MCP_URL = "http://127.0.0.1:18765/mcp"
DEFAULT_INSTALL_DIR = Path.home() / ".config" / "ferrosa-memory" / "hooks"


def log(message: str) -> None:
    print(f"[install-agent-hooks] {message}")


def shell_quote(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


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
    return found


def selected_harnesses(value: str) -> list[str]:
    if value == "auto":
        detected = detect_harnesses()
        return detected if detected else ["generic"]
    if value == "all":
        return ["codex", "claude", "hermes"]
    return [value]


def write_executable(path: Path, text: str) -> None:
    path.write_text(text)
    mode = path.stat().st_mode
    path.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def wrapper_format(harness: str) -> str:
    if harness == "codex":
        return "codex-json"
    if harness == "hermes":
        return "hermes-json"
    if harness == "claude":
        return "codex-json"
    return "plain"


def create_wrappers(install_dir: Path, hook_path: Path, mcp_url: str, harnesses: list[str]) -> dict[str, dict[str, str]]:
    install_dir.mkdir(parents=True, exist_ok=True)
    env_path = install_dir / "env"
    if not env_path.exists():
        env_path.write_text(
            "\n".join(
                [
                    "# Ferrosa Memory agent hook environment.",
                    f"export FERROSA_MEMORY_MCP_URL={shell_quote(mcp_url)}",
                    "export FERROSA_MEMORY_HOOK_TIMEOUT=${FERROSA_MEMORY_HOOK_TIMEOUT:-2.5}",
                    "# Uncomment one auth style if your MCP endpoint requires auth.",
                    "# export FERROSA_MEMORY_MCP_USER='ferrosa_user'",
                    "# export FERROSA_MEMORY_MCP_PASSWORD='ferrosa_user'",
                    "# export FERROSA_MEMORY_AUTH_HEADER='Basic <base64 user:password>'",
                    "",
                ]
            )
        )
        env_path.chmod(0o600)

    wrappers: dict[str, dict[str, str]] = {}
    for harness in harnesses:
        fmt = wrapper_format(harness)
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
        wrappers[harness] = {"recall": str(recall), "ingest_turn": str(ingest)}
    return wrappers


def backup(path: Path) -> Path:
    stamp = time.strftime("%Y%m%d%H%M%S")
    backup_path = path.with_name(f"{path.name}.bak-ferrosa-hooks-{stamp}")
    shutil.copy2(path, backup_path)
    return backup_path


def hook_entry(command: str, timeout: int = 3) -> dict[str, object]:
    return {"type": "command", "command": command, "timeout": timeout}


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
                return False
    new_entry: dict[str, object] = {"hooks": [hook_entry(command)]}
    if matcher is not None:
        new_entry["matcher"] = matcher
    event_entries.append(new_entry)
    return True


def patch_claude(settings_path: Path, wrappers: dict[str, str], dry_run: bool) -> str:
    if not settings_path.exists():
        return f"Claude settings not found at {settings_path}; snippet written only."
    settings = json.loads(settings_path.read_text())
    changed = False
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


def hermes_block(wrappers: dict[str, str]) -> str:
    return "\n".join(
        [
            "hooks:",
            "  pre_llm_call:",
            f"    - command: {json.dumps(wrappers['recall'])}",
            "      timeout: 3",
            "  on_session_end:",
            f"    - command: {json.dumps(wrappers['ingest_turn'])}",
            "      timeout: 3",
            "  on_session_finalize:",
            f"    - command: {json.dumps(wrappers['ingest_turn'])}",
            "      timeout: 3",
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
        (install_dir / "codex-hooks-note.md").write_text(
            "\n".join(
                [
                    "# Codex Ferrosa Memory hooks",
                    "",
                    "Codex MCP config is supported by this repo. If your Codex build exposes lifecycle hooks,",
                    "wire these commands to the pre-turn and turn-end events:",
                    "",
                    f"- Pre-turn recall: `{wrappers['codex']['recall']}`",
                    f"- Turn-end ingest: `{wrappers['codex']['ingest_turn']}`",
                    "",
                    "The wrapper emits Claude/Codex-style `hookSpecificOutput.additionalContext` JSON.",
                    "",
                ]
            )
        )


def verify_wrapper(command: str) -> str:
    payload = json.dumps({"prompt": "Ferrosa Memory hook installer verification", "cwd": os.getcwd()})
    try:
        proc = subprocess.run(
            [command],
            input=payload,
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
    except Exception as exc:
        return f"{command}: verification failed to launch: {exc}"
    if proc.returncode == 0:
        return f"{command}: ok"
    return f"{command}: exited {proc.returncode}: {proc.stderr.strip()[:300]}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harness", choices=["auto", "all", "codex", "claude", "hermes", "generic"], default="auto")
    parser.add_argument("--install-dir", type=Path, default=DEFAULT_INSTALL_DIR)
    parser.add_argument("--mcp-url", default=os.environ.get("FERROSA_MEMORY_MCP_URL", DEFAULT_MCP_URL))
    parser.add_argument("--claude-settings", type=Path, default=Path.home() / ".claude" / "settings.json")
    parser.add_argument("--hermes-config", type=Path, default=Path.home() / ".hermes" / "config.yaml")
    parser.add_argument("--no-apply-config", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()

    root = repo_root()
    hook_path = root / "scripts" / "hooks" / "ferrosa-memory-turn-hook.py"
    if not hook_path.exists():
        raise SystemExit(f"missing hook helper: {hook_path}")

    harnesses = selected_harnesses(args.harness)
    log(f"harnesses: {', '.join(harnesses)}")
    wrappers = create_wrappers(args.install_dir, hook_path, args.mcp_url, harnesses)
    write_snippets(args.install_dir, wrappers)

    results: list[str] = []
    if not args.no_apply_config:
        if "claude" in wrappers:
            results.append(patch_claude(args.claude_settings.expanduser(), wrappers["claude"], args.dry_run))
        if "hermes" in wrappers:
            results.append(patch_hermes(args.hermes_config.expanduser(), wrappers["hermes"], args.dry_run))
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
            log(f"{harness} recall verification: {verify_wrapper(commands['recall'])}")
    log(f"wrote manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
