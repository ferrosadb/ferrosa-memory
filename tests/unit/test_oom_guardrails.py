"""Regression tests for local cluster OOM guardrails."""

from __future__ import annotations

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
DEV_COMPOSE = REPO_ROOT / "docker-compose.yml"
TEST_COMPOSE = REPO_ROOT / "docker-compose.test.yml"
START_TEST_CLUSTER = REPO_ROOT / "scripts" / "start-test-cluster.sh"
MEMORY_WATCHDOG = REPO_ROOT / "scripts" / "memory-watchdog.sh"


def load_compose(path: Path) -> dict:
    return yaml.safe_load(path.read_text())


class ComposeOomGuardrailTests(unittest.TestCase):
    def test_dev_cluster_services_have_hard_memory_caps(self):
        compose = load_compose(DEV_COMPOSE)
        services = compose["services"]

        for name in ("node1", "node2", "node3"):
            with self.subTest(service=name):
                service = services[name]
                self.assertEqual(service.get("mem_limit"), "2g")
                self.assertEqual(service.get("memswap_limit"), "2g")
                self.assertEqual(service.get("restart"), "on-failure:5")

        self.assertEqual(services["minio"].get("mem_limit"), "1g")
        self.assertEqual(services["minio"].get("memswap_limit"), "1g")
        self.assertEqual(services["minio"].get("restart"), "on-failure:5")

    def test_test_cluster_services_have_hard_memory_caps(self):
        compose = load_compose(TEST_COMPOSE)
        services = compose["services"]

        for name in ("node1-test", "node2-test", "node3-test"):
            with self.subTest(service=name):
                service = services[name]
                self.assertEqual(service.get("mem_limit"), "2g")
                self.assertEqual(service.get("memswap_limit"), "2g")
                self.assertEqual(service.get("restart"), "on-failure:5")

        self.assertEqual(services["minio-test"].get("mem_limit"), "1g")
        self.assertEqual(services["minio-test"].get("memswap_limit"), "1g")
        self.assertEqual(services["minio-test"].get("restart"), "on-failure:5")


class StartTestClusterOomGuardrailTests(unittest.TestCase):
    def test_env_mode_does_not_require_container_engine(self):
        env = {"PATH": "/usr/bin:/bin"}
        result = subprocess.run(
            [str(START_TEST_CLUSTER), "--env"],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("FERROSA_TEST_CQL_PORT=19542", result.stdout)

    def test_refuses_to_start_test_cluster_when_dev_cluster_is_running(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake_podman = Path(tmp) / "podman"
            fake_podman.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    if [[ "$1" == "inspect" ]]; then
                        if [[ "${@: -1}" == "fmem-dev-node1-1" ]]; then
                            echo "true"
                            exit 0
                        fi
                        exit 1
                    fi
                    echo "unexpected podman invocation: $*" >&2
                    exit 42
                    """
                )
            )
            fake_podman.chmod(0o755)

            env = os.environ.copy()
            env["PATH"] = f"{tmp}:/usr/bin:/bin"
            env["PODMAN"] = str(fake_podman)
            env.pop("FERROSA_ALLOW_PARALLEL_CLUSTERS", None)

            result = subprocess.run(
                [str(START_TEST_CLUSTER)],
                cwd=REPO_ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dev cluster is already running", result.stderr)
        self.assertIn("FERROSA_ALLOW_PARALLEL_CLUSTERS=1", result.stderr)


class MemoryWatchdogOomGuardrailTests(unittest.TestCase):
    def test_watchdog_tracks_current_dev_cluster_container_names(self):
        text = MEMORY_WATCHDOG.read_text()

        for name in (
            "fmem-dev-node1-1",
            "fmem-dev-node2-1",
            "fmem-dev-node3-1",
            "fmem-dev-minio-1",
        ):
            with self.subTest(container=name):
                self.assertIn(name, text)
