"""Regression tests for the three-node memory CI topology contract."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "validate-ci-cluster-topology.py"


def load_validator():
    spec = importlib.util.spec_from_file_location("validate_ci_cluster_topology", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def valid_compose() -> dict:
    services: dict[str, dict] = {}
    for name, cql_port, web_port in (
        ("node1", "19042", "19090"),
        ("node2", "19043", "19091"),
        ("node3", "19044", "19092"),
    ):
        services[name] = {
            "depends_on": {
                "rustfs-init": {"condition": "service_completed_successfully"},
            },
            "ports": [f"{cql_port}:9042", f"{web_port}:9090"],
        }
    return {"services": services}


class ClusterTopologyValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_validator()

    def test_accepts_concurrent_nodes_with_per_node_web_status_ports(self) -> None:
        self.assertEqual(self.module.validate_compose(valid_compose()), [])

    def test_accepts_docker_compose_normalized_dependency(self) -> None:
        compose = valid_compose()
        for node in ("node1", "node2", "node3"):
            compose["services"][node]["depends_on"]["rustfs-init"]["required"] = True

        self.assertEqual(self.module.validate_compose(compose), [])

    def test_rejects_serialized_pair_mode_startup(self) -> None:
        compose = valid_compose()
        compose["services"]["node2"]["depends_on"] = {
            "node1": {"condition": "service_healthy"},
        }
        errors = self.module.validate_compose(compose)

        self.assertTrue(any("node2" in error and "rustfs-init" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
