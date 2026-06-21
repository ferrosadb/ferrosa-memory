"""Unit-level smoke tests for the repo-local setup.sh wrapper."""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SETUP = REPO_ROOT / "setup.sh"


class SetupScriptTests(unittest.TestCase):
    def test_no_verify_forwards_auth_header_to_hook_installer_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            env = os.environ.copy()
            env["HOME"] = str(home)
            result = subprocess.run(
                [
                    str(SETUP),
                    "--harness",
                    "generic",
                    "--skip-build",
                    "--skip-service",
                    "--no-apply-config",
                    "--no-verify",
                    "--auth-header",
                    "Basic dXNlcjpwYXNz",
                ],
                cwd=REPO_ROOT,
                env=env,
                text=True,
                capture_output=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            hook_env = home / ".config" / "ferrosa-memory" / "hooks" / "env"
            self.assertIn(
                "export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'",
                hook_env.read_text(),
            )

    def test_help_advertises_auth_flags(self) -> None:
        result = subprocess.run(
            [str(SETUP), "--help"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn("--auth-header VALUE", result.stdout)
        self.assertIn("--mcp-user USER", result.stdout)
        self.assertIn("--mcp-password PASSWORD", result.stdout)


if __name__ == "__main__":
    unittest.main()
