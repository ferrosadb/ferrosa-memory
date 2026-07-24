#!/usr/bin/env python3
"""Fail CI early when its three-node Ferrosa topology is not formable.

A listening CQL socket is not cluster readiness: a node can be a pair-mode
secondary and reject clients. The memory CI must start all nodes from the
shared bootstrap dependency, then use each node's web status endpoint to wait
for actual cluster mode.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

NODES = {
    "node1": ("19042", "19090"),
    "node2": ("19043", "19091"),
    "node3": ("19044", "19092"),
}


def exposes_port(ports: Any, published: str, target: str) -> bool:
    if not isinstance(ports, list):
        return False
    for port in ports:
        if isinstance(port, dict):
            if str(port.get("published")) == published and str(port.get("target")) == target:
                return True
        elif isinstance(port, str):
            address = port.split("/", 1)[0]
            parts = address.rsplit(":", 2)
            if len(parts) >= 2 and parts[-2:] == [published, target]:
                return True
    return False


def has_shared_bootstrap_dependency(depends_on: Any) -> bool:
    """Accept source YAML and Docker Compose's rendered `required: true` form."""
    if not isinstance(depends_on, dict) or set(depends_on) != {"rustfs-init"}:
        return False
    dependency = depends_on["rustfs-init"]
    return isinstance(dependency, dict) and dependency.get("condition") == "service_completed_successfully"


def validate_compose(compose: dict[str, Any]) -> list[str]:
    services = compose.get("services")
    if not isinstance(services, dict):
        return ["compose has no services mapping"]

    errors: list[str] = []
    for node, (cql_port, web_port) in NODES.items():
        service = services.get(node)
        if not isinstance(service, dict):
            errors.append(f"missing {node} service")
            continue
        if not has_shared_bootstrap_dependency(service.get("depends_on")):
            errors.append(
                f"{node} must depend only on rustfs-init completing; "
                "do not serialize cluster formation on a node TCP health check"
            )
        if not exposes_port(service.get("ports"), cql_port, "9042"):
            errors.append(f"{node} must expose CQL 9042 on host port {cql_port}")
        if not exposes_port(service.get("ports"), web_port, "9090"):
            errors.append(f"{node} must expose web 9090 on host port {web_port}")

    # The CI graph probe runs from the host through node1's published Graph
    # HTTP port. Ferrosa deliberately defaults this listener to loopback, so
    # the Docker topology must opt in explicitly.
    node1 = services.get("node1")
    node1_environment = node1.get("environment") if isinstance(node1, dict) else None
    if not isinstance(node1_environment, dict) or node1_environment.get("FERROSA_GRAPH_BIND") != "0.0.0.0:7474":
        errors.append("node1 must bind Graph HTTP on 0.0.0.0:7474 for the published CI probe")
    return errors


def load_rendered_compose(path: Path) -> dict[str, Any]:
    result = subprocess.run(
        ["docker", "compose", "-f", str(path), "config", "--format", "json"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or "docker compose config failed")
    parsed = json.loads(result.stdout)
    if not isinstance(parsed, dict):
        raise RuntimeError("docker compose config did not return an object")
    return parsed


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <compose-file>", file=sys.stderr)
        return 2
    try:
        compose = load_rendered_compose(Path(argv[1]))
    except (OSError, RuntimeError, json.JSONDecodeError) as exc:
        print(f"ERROR: cannot render CI compose: {exc}", file=sys.stderr)
        return 2
    errors = validate_compose(compose)
    if errors:
        for error in errors:
            print(f"ERROR: invalid memory CI cluster topology: {error}", file=sys.stderr)
        return 1
    print("memory CI cluster topology: valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
