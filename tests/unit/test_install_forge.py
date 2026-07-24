"""Focused tests for the Forge installer and its capability validation."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALLER = REPO_ROOT / "scripts" / "install-forge.sh"
SETUP = REPO_ROOT / "setup.sh"


class ForgeInstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fake_bin = self.root / "fake-bin"
        self.fake_bin.mkdir()
        self.log = self.root / "calls.log"

    def _write_executable(self, path: Path, content: str) -> None:
        path.write_text(content)
        path.chmod(0o755)

    def _write_fake_frg(self, path: Path, capabilities: list[str]) -> None:
        self._write_executable(
            path,
            """#!/usr/bin/env python3
import json
import os
import sys

CAPABILITIES = set(%s)

if sys.argv[1:] == ["--version"]:
    print("frg 0.0-test")
    raise SystemExit(0)

if sys.argv[1:] != ["--mcp"]:
    raise SystemExit("unexpected arguments: " + repr(sys.argv[1:]))

for line in sys.stdin:
    request = json.loads(line)
    request_id = request["id"]
    if request["method"] == "initialize":
        response = {"jsonrpc": "2.0", "id": request_id, "result": {"capabilities": {}}}
    else:
        name = request["params"]["name"]
        with open(os.environ["FORGE_TEST_LOG"], "a", encoding="utf-8") as log:
            log.write("tools/call:" + name + "\\n")
        if name not in CAPABILITIES:
            response = {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "missing " + name},
            }
        else:
            response = {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"content": [{"type": "text", "text": name + " ok"}]},
            }
    print(json.dumps(response), flush=True)
"""
            % json.dumps(capabilities),
        )

    def _write_fake_git(self) -> None:
        self._write_executable(
            self.fake_bin / "git",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'git %s\\n' "$*" >> "$FORGE_TEST_LOG"
if [[ "${1:-}" == "clone" ]]; then
    dest="${!#}"
    mkdir -p "$dest/.git"
fi
""",
        )

    def _write_fake_cargo(self) -> None:
        self._write_executable(
            self.fake_bin / "cargo",
            """#!/usr/bin/env bash
set -euo pipefail
manifest=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --manifest-path)
            manifest="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done
forge_dir="$(dirname "$manifest")"
mkdir -p "$forge_dir/target/release"
cp "$FORGE_TEST_FRG" "$forge_dir/target/release/frg"
chmod 0755 "$forge_dir/target/release/frg"
printf 'cargo build\\n' >> "$FORGE_TEST_LOG"
""",
        )

    def _env(self, frg_template: Path) -> dict[str, str]:
        env = os.environ.copy()
        env["FORGE_TEST_LOG"] = str(self.log)
        env["FORGE_TEST_FRG"] = str(frg_template)
        env["PATH"] = f"{self.fake_bin}{os.pathsep}{env['PATH']}"
        return env

    def _run_installer(self, *args: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(INSTALLER), *args],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_install_rerun_updates_checkout_and_validates_capabilities(self):
        frg_template = self.root / "frg-template"
        self._write_fake_frg(frg_template, ["project_summary", "ingest"])
        self._write_fake_git()
        self._write_fake_cargo()
        forge_dir = self.root / "forge"
        bin_dir = self.root / "bin"
        env = self._env(frg_template)

        first = self._run_installer(
            "--repo",
            "https://example.invalid/forge.git",
            "--dir",
            str(forge_dir),
            "--bin-dir",
            str(bin_dir),
            env=env,
        )
        second = self._run_installer(
            "--repo",
            "https://example.invalid/forge.git",
            "--dir",
            str(forge_dir),
            "--bin-dir",
            str(bin_dir),
            env=env,
        )

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertTrue((bin_dir / "frg").is_file())
        calls = self.log.read_text().splitlines()
        self.assertEqual(sum(line.startswith("git clone") for line in calls), 1)
        self.assertEqual(sum(" fetch --prune origin" in line for line in calls), 1)
        self.assertEqual(sum(" pull --ff-only" in line for line in calls), 1)
        self.assertEqual(calls.count("tools/call:project_summary"), 2)
        self.assertEqual(calls.count("tools/call:ingest"), 2)

    def test_verify_only_rejects_binary_without_ingest(self):
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        self._write_fake_frg(bin_dir / "frg", ["project_summary"])
        env = self._env(bin_dir / "frg")

        result = self._run_installer("--verify-only", "--bin-dir", str(bin_dir), env=env)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ingest", result.stderr)
        self.assertIn("tools/call:project_summary", self.log.read_text())
        self.assertIn("tools/call:ingest", self.log.read_text())

    def test_setup_help_documents_required_forge_with_explicit_skip(self):
        result = subprocess.run(
            ["bash", str(SETUP), "--help"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--skip-forge", result.stdout)
        self.assertIn("--forge-repo", result.stdout)
        self.assertIn("controlled environments", result.stdout)
