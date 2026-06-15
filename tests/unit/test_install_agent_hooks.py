"""Unit tests for scripts/install-agent-hooks.py auth wiring and verification honesty."""

from __future__ import annotations

import importlib.util
import stat
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
INSTALLER = REPO_ROOT / "scripts" / "install-agent-hooks.py"


def load_installer():
    spec = importlib.util.spec_from_file_location("install_agent_hooks", INSTALLER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WriteHookEnvAuthTests(unittest.TestCase):
    def setUp(self):
        self.module = load_installer()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.env_path = Path(self.tmp.name) / "env"

    def test_auth_header_is_written_uncommented(self):
        self.module.write_hook_env(
            self.env_path,
            "http://127.0.0.1:18765/mcp",
            auth_header="Basic dXNlcjpwYXNz",
        )
        text = self.env_path.read_text()
        self.assertIn("export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'", text)
        for line in text.splitlines():
            if "FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'" in line:
                self.assertFalse(line.lstrip().startswith("#"), f"auth line is commented out: {line}")

    def test_user_and_password_are_written_uncommented(self):
        self.module.write_hook_env(
            self.env_path,
            "http://127.0.0.1:18765/mcp",
            mcp_user="ferrosa_user",
            mcp_password="hunter2",
        )
        text = self.env_path.read_text()
        self.assertIn("export FERROSA_MEMORY_MCP_USER='ferrosa_user'", text)
        self.assertIn("export FERROSA_MEMORY_MCP_PASSWORD='hunter2'", text)

    def test_auth_header_replaces_commented_template_line(self):
        self.env_path.write_text(
            "# export FERROSA_MEMORY_AUTH_HEADER='Basic <base64 user:password>'\n"
            "export FERROSA_MEMORY_MCP_URL='http://127.0.0.1:18765/mcp'\n"
        )
        self.module.write_hook_env(
            self.env_path,
            "http://127.0.0.1:18765/mcp",
            auth_header="Basic dXNlcjpwYXNz",
        )
        text = self.env_path.read_text()
        self.assertIn("export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'", text)
        self.assertNotIn("# export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'", text)

    def test_rerun_without_auth_preserves_existing_auth_line(self):
        self.module.write_hook_env(
            self.env_path,
            "http://127.0.0.1:18765/mcp",
            auth_header="Basic dXNlcjpwYXNz",
        )
        self.module.write_hook_env(self.env_path, "http://127.0.0.1:18765/mcp")
        text = self.env_path.read_text()
        self.assertIn("export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'", text)

    def test_min_score_env_default_is_written(self):
        self.module.write_hook_env(self.env_path, "http://127.0.0.1:18765/mcp")
        text = self.env_path.read_text()
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_MIN_SCORE=${FERROSA_MEMORY_HOOK_MIN_SCORE:-0.0}",
            text,
        )
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE=${FERROSA_MEMORY_HOOK_MIN_JUDGE_SCORE:-1.0}",
            text,
        )
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT=${FERROSA_MEMORY_HOOK_REQUIRE_JUDGMENT:-true}",
            text,
        )
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_INCLUDE_HINTS=${FERROSA_MEMORY_HOOK_INCLUDE_HINTS:-false}",
            text,
        )
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS=${FERROSA_MEMORY_HOOK_MIN_QUERY_TERMS:-2}",
            text,
        )
        self.assertIn(
            "export FERROSA_MEMORY_HOOK_ALLOWED_KINDS=${FERROSA_MEMORY_HOOK_ALLOWED_KINDS:-episodic,procedural,semantic}",
            text,
        )


class VerifyWrapperHonestyTests(unittest.TestCase):
    """A wrapper that prints a 'skipped: ...' degradation notice and exits 0 must
    NOT be reported as ok — that is exactly the silent-401 failure mode."""

    def setUp(self):
        self.module = load_installer()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def _fake_wrapper(self, body: str) -> str:
        path = Path(self.tmp.name) / "wrapper.sh"
        path.write_text("#!/usr/bin/env bash\n" + body + "\n")
        path.chmod(path.stat().st_mode | stat.S_IXUSR)
        return str(path)

    def test_skipped_output_is_reported_as_failure(self):
        cmd = self._fake_wrapper(
            "echo '[ferrosa-memory-hook] ingest-turn skipped: HTTP Error 401: Unauthorized' >&2\nexit 0"
        )
        result = self.module.verify_wrapper(cmd, "ingest-turn")
        self.assertNotEqual(result, f"{cmd}: ok")
        self.assertIn("skipped", result)

    def test_clean_exit_is_reported_as_ok(self):
        cmd = self._fake_wrapper("cat >/dev/null\nexit 0")
        result = self.module.verify_wrapper(cmd, "ingest-turn")
        self.assertEqual(result, f"{cmd}: ok")


class MainAuthFlagTests(unittest.TestCase):
    def test_auth_header_flag_lands_in_env_file(self):
        module = load_installer()
        with tempfile.TemporaryDirectory() as tmp:
            install_dir = Path(tmp) / "hooks"
            argv = [
                "install-agent-hooks.py",
                "--harness", "generic",
                "--install-dir", str(install_dir),
                "--no-apply-config",
                "--auth-header", "Basic dXNlcjpwYXNz",
            ]
            old_argv = sys.argv
            sys.argv = argv
            try:
                rc = module.main()
            finally:
                sys.argv = old_argv
            self.assertEqual(rc, 0)
            text = (install_dir / "env").read_text()
            self.assertIn("export FERROSA_MEMORY_AUTH_HEADER='Basic dXNlcjpwYXNz'", text)


if __name__ == "__main__":
    unittest.main()
