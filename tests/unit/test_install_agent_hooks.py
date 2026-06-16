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


class AuthHeaderValidationTests(unittest.TestCase):
    def setUp(self):
        self.module = load_installer()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.env_path = Path(self.tmp.name) / "env"

    def test_validate_auth_header_accepts_clean_value(self):
        self.assertEqual(self.module.validate_auth_header("Basic dXNlcjpwYXNz"), "Basic dXNlcjpwYXNz")

    def test_validate_auth_header_strips_surrounding_whitespace(self):
        self.assertEqual(self.module.validate_auth_header("  Basic dXNlcjpwYXNz  "), "Basic dXNlcjpwYXNz")

    def test_validate_auth_header_rejects_embedded_newline(self):
        # The exact 2026-06-15 footgun: grep also matched the commented placeholder,
        # yielding a two-line value urllib rejects with "Invalid header value".
        with self.assertRaises(ValueError):
            self.module.validate_auth_header("Basic <base64 user:password>\nBasic dXNlcjpwYXNz")

    def test_validate_auth_header_rejects_placeholder(self):
        with self.assertRaises(ValueError):
            self.module.validate_auth_header("Basic <base64 user:password>")

    def test_validate_auth_header_rejects_empty(self):
        with self.assertRaises(ValueError):
            self.module.validate_auth_header("   ")

    def test_write_hook_env_rejects_multiline_auth(self):
        with self.assertRaises(ValueError):
            self.module.write_hook_env(
                self.env_path, "http://127.0.0.1:18765/mcp", auth_header="Basic a\nBasic b"
            )

    def test_default_env_omits_grepable_placeholder(self):
        # Default template must not seed a fake value that grep mistakes for a real header.
        self.module.write_hook_env(self.env_path, "http://127.0.0.1:18765/mcp")
        self.assertNotIn("<base64 user:password>", self.env_path.read_text())


class AuthConsistencyTests(unittest.TestCase):
    def setUp(self):
        self.module = load_installer()

    def test_usable_auth_detects_header(self):
        self.assertTrue(self.module.has_usable_auth("Basic abc", None, None))

    def test_usable_auth_detects_user_password_pair(self):
        self.assertTrue(self.module.has_usable_auth(None, "u", "p"))

    def test_usable_auth_false_when_only_user(self):
        self.assertFalse(self.module.has_usable_auth(None, "u", None))

    def test_server_requires_auth_but_none_configured_is_error(self):
        msg = self.module.auth_consistency_error(True, None, None, None)
        self.assertIsNotNone(msg)
        self.assertIn("auth", msg.lower())

    def test_server_requires_auth_with_header_is_ok(self):
        self.assertIsNone(self.module.auth_consistency_error(True, "Basic abc", None, None))

    def test_server_requires_auth_with_user_password_is_ok(self):
        self.assertIsNone(self.module.auth_consistency_error(True, None, "u", "p"))

    def test_server_no_auth_no_creds_is_ok(self):
        self.assertIsNone(self.module.auth_consistency_error(False, None, None, None))


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
                "--skip-auth-check",
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


class MainAuthPreflightTests(unittest.TestCase):
    def _run(self, module, extra_argv):
        with tempfile.TemporaryDirectory() as tmp:
            argv = [
                "install-agent-hooks.py",
                "--harness", "generic",
                "--install-dir", str(Path(tmp) / "hooks"),
                "--no-apply-config",
            ] + extra_argv
            old_argv = sys.argv
            sys.argv = argv
            try:
                return module.main()
            finally:
                sys.argv = old_argv

    def test_main_rejects_invalid_auth_header(self):
        module = load_installer()
        module.probe_auth_required = lambda *a, **k: True
        rc = self._run(module, ["--skip-auth-check", "--auth-header", "Basic a\nBasic b"])
        self.assertEqual(rc, 2)

    def test_main_fails_when_server_requires_auth_but_no_creds(self):
        module = load_installer()
        module.probe_auth_required = lambda *a, **k: True  # server demands auth
        rc = self._run(module, [])  # no creds configured
        self.assertEqual(rc, 3)

    def test_main_skip_auth_check_bypasses_preflight(self):
        module = load_installer()
        module.probe_auth_required = lambda *a, **k: True
        rc = self._run(module, ["--skip-auth-check"])  # bypass despite no creds
        self.assertEqual(rc, 0)

    def test_main_ok_when_server_requires_auth_and_header_supplied(self):
        module = load_installer()
        module.probe_auth_required = lambda *a, **k: True
        rc = self._run(module, ["--auth-header", "Basic dXNlcjpwYXNz"])
        self.assertEqual(rc, 0)

    def test_main_ok_when_server_unreachable(self):
        module = load_installer()
        module.probe_auth_required = lambda *a, **k: None  # unreachable -> warn, continue
        rc = self._run(module, [])
        self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main()
