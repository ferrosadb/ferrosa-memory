"""Unit tests for scripts/install-agent-hooks.py auth wiring and verification honesty."""

from __future__ import annotations

import importlib.util
import json
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

    def test_turn_entity_ingest_failure_is_reported_as_failure(self):
        cmd = self._fake_wrapper(
            "echo '[ferrosa-memory-hook] turn entity ingest failed: tenant mismatch' >&2\nexit 0"
        )
        result = self.module.verify_wrapper(cmd, "ingest-turn")
        self.assertNotEqual(result, f"{cmd}: ok")
        self.assertIn("turn entity ingest failed", result)


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


class PiHarnessTests(unittest.TestCase):
    def setUp(self):
        self.module = load_installer()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_pi_is_a_known_harness(self):
        self.assertIn("pi", self.module.selected_harnesses("all"))
        self.assertEqual(self.module.selected_harnesses("pi"), ["pi"])

    def test_pi_wrapper_format_is_plain(self):
        # The Pi extension feeds plain {prompt,response} JSON; the turn hook
        # treats `pi` as a generic harness.
        self.assertEqual(self.module.wrapper_format("pi"), "plain")

    def test_create_wrappers_generates_pi_scripts(self):
        install_dir = Path(self.tmp.name) / "hooks"
        wrappers = self.module.create_wrappers(
            install_dir, Path("/fake/hook.py"), "http://127.0.0.1:18765/mcp", ["pi"]
        )
        self.assertIn("pi", wrappers)
        for key in ("session_start", "recall", "ingest_turn"):
            self.assertTrue(Path(wrappers["pi"][key]).exists())
        # The Pi wrappers invoke the turn hook with --harness pi.
        recall_body = Path(wrappers["pi"]["recall"]).read_text()
        self.assertIn("--harness pi", recall_body)

    def test_install_pi_extension_writes_substituted_extension(self):
        ext_dir = Path(self.tmp.name) / ".pi" / "agent" / "extensions"
        pi_wrappers = {
            "session_start": "/h/pi-session-start.sh",
            "recall": "/h/pi-recall.sh",
            "ingest_turn": "/h/pi-ingest-turn.sh",
        }
        msg = self.module.install_pi_extension(pi_wrappers, dry_run=False, extensions_dir=ext_dir)
        target = ext_dir / "ferrosa-memory.ts"
        self.assertTrue(target.exists())
        self.assertIn(str(target), msg)
        body = target.read_text()
        # Wrapper paths are substituted as JSON string literals, and the Pi
        # lifecycle events the extension subscribes to are present.
        self.assertIn('"/h/pi-recall.sh"', body)
        self.assertIn('"/h/pi-ingest-turn.sh"', body)
        self.assertIn("before_agent_start", body)
        self.assertIn("agent_end", body)
        self.assertNotIn("__RECALL_WRAPPER__", body)

    def test_install_pi_extension_dry_run_writes_nothing(self):
        ext_dir = Path(self.tmp.name) / ".pi" / "agent" / "extensions"
        pi_wrappers = {"session_start": "/h/s.sh", "recall": "/h/r.sh", "ingest_turn": "/h/i.sh"}
        msg = self.module.install_pi_extension(pi_wrappers, dry_run=True, extensions_dir=ext_dir)
        self.assertIn("Dry run", msg)
        self.assertFalse((ext_dir / "ferrosa-memory.ts").exists())


class LateHarnessRepairTests(unittest.TestCase):
    """Automatic repair must rescan a controlled home on every invocation."""

    def setUp(self):
        self.module = load_installer()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / "home"
        self.install_dir = Path(self.tmp.name) / "hooks"
        self.home.mkdir()
        # Command discovery is environment-dependent. The test exercises the
        # config-directory detection path without consulting the real machine.
        self.module.command_exists = lambda _name: False

    def _run_auto(self, *extra_args):
        return self.module.main(
            [
                "--harness",
                "auto",
                "--home",
                str(self.home),
                "--install-dir",
                str(self.install_dir),
                "--skip-auth-check",
            ]
            + list(extra_args)
        )

    def _manifest(self):
        return json.loads((self.install_dir / "manifest.json").read_text())

    def test_auto_repair_detects_late_claude_and_goose_without_real_home(self):
        self.assertEqual(self._run_auto(), 0)
        first = self._manifest()
        self.assertEqual(first["detected_harnesses"], [])
        self.assertEqual(first["installed_harnesses"], ["generic"])
        self.assertNotIn("config_results", first)
        generic = first["harness_outcomes"]["generic"]
        self.assertFalse(generic["detection"]["detected"])
        self.assertEqual(generic["hook_registration"]["status"], "not_applicable")
        self.assertEqual(generic["mcp_registration"]["status"], "not_applicable")

        # Simulate installing Claude Code and Goose after the first Ferrosa run.
        (self.home / ".claude").mkdir()
        (self.home / ".config" / "goose").mkdir(parents=True)
        (self.home / ".config" / "goose" / "config.yaml").write_text("default_provider: test\n")
        mcp_url = "http://127.0.0.1:18767/mcp"

        self.assertEqual(
            self._run_auto(
                "--mcp-url",
                mcp_url,
                "--mcp-user",
                "goose_user",
                "--mcp-password",
                "goose_password",
            ),
            0,
        )
        repaired = self._manifest()
        self.assertEqual(repaired["detected_harnesses"], ["claude", "goose"])
        self.assertEqual(repaired["installed_harnesses"], ["claude", "goose"])

        claude = repaired["harness_outcomes"]["claude"]
        self.assertTrue(claude["detection"]["detected"])
        self.assertEqual(claude["hook_registration"]["status"], "created")
        self.assertEqual(claude["mcp_registration"]["status"], "created")
        self.assertIsNone(claude["required_action"])

        settings_path = self.home / ".claude" / "settings.json"
        settings = json.loads(settings_path.read_text())
        self.assertIn("hooks", settings)
        self.assertEqual(
            settings["mcpServers"]["ferrosa-memory"],
            {
                "type": "http",
                "url": mcp_url,
                "headers": {"Authorization": "Basic Z29vc2VfdXNlcjpnb29zZV9wYXNzd29yZA=="},
            },
        )

        goose = repaired["harness_outcomes"]["goose"]
        self.assertTrue(goose["detection"]["detected"])
        self.assertEqual(goose["hook_registration"]["status"], "unsupported")
        self.assertEqual(goose["mcp_registration"]["status"], "updated")
        self.assertIn("not configurable", goose["required_action"])

        goose_path = self.home / ".config" / "goose" / "config.yaml"
        goose_config = goose_path.read_text()
        self.assertIn("default_provider: test", goose_config)
        self.assertIn("extensions:\n  ferrosa-memory:", goose_config)
        self.assertIn("type: streamable_http", goose_config)
        self.assertIn(f'uri: "{mcp_url}"', goose_config)
        self.assertIn("headers:\n      Authorization: \"Basic Z29vc2VfdXNlcjpnb29zZV9wYXNzd29yZA==\"", goose_config)
        self.assertIn("envs: {}", goose_config)
        self.assertNotIn("cmd:", goose_config)

        self.assertEqual(self._run_auto("--mcp-url", mcp_url), 0)
        rerun = self._manifest()
        self.assertEqual(
            rerun["harness_outcomes"]["claude"]["hook_registration"]["status"],
            "already_registered",
        )
        self.assertEqual(
            rerun["harness_outcomes"]["claude"]["mcp_registration"]["status"],
            "already_registered",
        )
        self.assertEqual(
            rerun["harness_outcomes"]["goose"]["mcp_registration"]["status"],
            "already_registered",
        )
        self.assertEqual(goose_path.read_text().count("  ferrosa-memory:\n"), 1)
        self.assertTrue(settings_path.is_relative_to(self.home))
        self.assertTrue(goose_path.is_relative_to(self.home))

    def test_goose_empty_extensions_mapping_is_upgraded_idempotently(self):
        config, changed = self.module.updated_goose_config(
            "extensions: {}\n", "http://127.0.0.1:18767/mcp"
        )
        self.assertTrue(changed)
        self.assertIn("extensions:\n  ferrosa-memory:", config)
        self.assertIn("type: streamable_http", config)

        rerun, changed = self.module.updated_goose_config(config, "http://127.0.0.1:18767/mcp")
        self.assertFalse(changed)
        self.assertEqual(rerun, config)

    def test_goose_streamable_http_merge_preserves_existing_extension_fields(self):
        original = """\
extensions:
  ferrosa-memory:
    name: Custom Ferrosa Memory
    cmd: ferrosa-memory
    headers:
      X-Trace: \"keep\"
    description: Keep this description
"""
        updated, changed = self.module.updated_goose_config(
            original,
            "http://127.0.0.1:18767/mcp",
            {"Authorization": "Bearer token"},
        )
        self.assertTrue(changed)
        self.assertIn("name: Custom Ferrosa Memory", updated)
        self.assertIn("description: Keep this description", updated)
        self.assertIn('X-Trace: "keep"', updated)
        self.assertIn('Authorization: "Bearer token"', updated)
        self.assertIn("type: streamable_http", updated)
        self.assertIn('uri: "http://127.0.0.1:18767/mcp"', updated)
        self.assertIn("envs: {}", updated)
        self.assertNotIn("cmd:", updated)


if __name__ == "__main__":
    unittest.main()
