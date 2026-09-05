#!/usr/bin/env python3
"""
Module: Reject semantically ambiguous quantitative names at public schema boundaries.
Correctness: Correct when new bare metric names fail while explicit legacy exceptions remain bounded and reviewable.
Last revised: 2026-08-19
Last changed: Added CQL, public Rust-record, and MCP tool-schema scanning with a counted legacy baseline.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
import json
from pathlib import Path
import re
import sys
from typing import Iterable, Mapping, Sequence


AMBIGUOUS_QUANTITATIVE_NAMES = frozenset(
    {"confidence", "quality", "risk", "score", "trust"}
)
DEFAULT_BASELINE = Path("config/semantic-name-baseline.json")


@dataclass(frozen=True, order=True)
class Finding:
    """One ambiguous name on a persistence or public API surface."""

    surface: str
    path: str
    scope: str
    name: str

    @property
    def fingerprint(self) -> str:
        """Return the stable, line-number-independent baseline identity."""

        return "|".join((self.surface, self.path, self.scope, self.name))


def scan_cql(path: Path, source: str) -> list[Finding]:
    """Find bare quantitative column names inside CQL CREATE TABLE statements."""

    findings: list[Finding] = []
    table: str | None = None
    create_table = re.compile(
        r"^\s*CREATE\s+TABLE(?:\s+IF\s+NOT\s+EXISTS)?\s+([a-zA-Z0-9_.]+)\s*\(",
        re.IGNORECASE,
    )
    column = re.compile(r"^\s*([a-zA-Z_][a-zA-Z0-9_]*)\s+[a-zA-Z]", re.IGNORECASE)

    for line in source.splitlines():
        if table is None:
            match = create_table.match(line)
            if match:
                table = match.group(1)
            continue
        if re.match(r"^\s*\);", line):
            table = None
            continue
        match = column.match(line)
        if match and match.group(1).lower() in AMBIGUOUS_QUANTITATIVE_NAMES:
            findings.append(
                Finding("cql_column", path.as_posix(), table, match.group(1).lower())
            )
    return findings


def scan_rust_records(path: Path, source: str) -> list[Finding]:
    """Find bare quantitative fields on public Rust structs."""

    findings: list[Finding] = []
    struct_name: str | None = None
    depth = 0
    public_struct = re.compile(r"^\s*pub\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)[^\{]*\{")
    public_field = re.compile(r"^\s*pub\s+([a-z_][a-zA-Z0-9_]*)\s*:")

    for line in source.splitlines():
        if struct_name is None:
            match = public_struct.match(line)
            if not match:
                continue
            struct_name = match.group(1)
            depth = line.count("{") - line.count("}")
            continue

        if depth == 1:
            match = public_field.match(line)
            if match and match.group(1) in AMBIGUOUS_QUANTITATIVE_NAMES:
                findings.append(
                    Finding(
                        "rust_public_field",
                        path.as_posix(),
                        struct_name,
                        match.group(1),
                    )
                )
        depth += line.count("{") - line.count("}")
        if depth <= 0:
            struct_name = None
            depth = 0
    return findings


def scan_tool_schemas(path: Path, source: str) -> list[Finding]:
    """Find bare quantitative property names in MCP tool input schemas."""

    findings: list[Finding] = []
    tool_name = "unknown-tool"
    awaiting_name = False
    tool_start = re.compile(r"^\s*ToolDef\s*\{")
    name_line = re.compile(r'^\s*name:\s*"([^"]+)"')
    json_property = re.compile(r'^\s*"([a-z_][a-zA-Z0-9_]*)"\s*:')

    for line in source.splitlines():
        if tool_start.match(line):
            tool_name = "unknown-tool"
            awaiting_name = True
            continue
        if awaiting_name:
            match = name_line.match(line)
            if match:
                tool_name = match.group(1)
                awaiting_name = False
        match = json_property.match(line)
        if match and match.group(1) in AMBIGUOUS_QUANTITATIVE_NAMES:
            findings.append(
                Finding(
                    "tool_schema_property",
                    path.as_posix(),
                    tool_name,
                    match.group(1),
                )
            )
    return findings


def unexpected_findings(
    findings: Iterable[Finding], baseline: Mapping[str, int]
) -> list[Finding]:
    """Return occurrences above each explicitly accepted legacy count."""

    remaining = Counter(baseline)
    unexpected: list[Finding] = []
    for finding in sorted(findings):
        if remaining[finding.fingerprint] > 0:
            remaining[finding.fingerprint] -= 1
        else:
            unexpected.append(finding)
    return unexpected


def scan_repository(root: Path) -> list[Finding]:
    """Scan the public schema surfaces governed by the specificity ADR."""

    findings: list[Finding] = []
    for path in sorted((root / "ddl").glob("*.cql")):
        findings.extend(scan_cql(path.relative_to(root), path.read_text()))

    source_root = root / "crates" / "ferrosa-memory-core" / "src"
    for path in sorted(source_root.rglob("*.rs")):
        relative = path.relative_to(root)
        source = path.read_text()
        findings.extend(scan_rust_records(relative, source))
        if path.name == "tool_schemas.rs":
            findings.extend(scan_tool_schemas(relative, source))
    return sorted(findings)


def load_baseline(path: Path) -> dict[str, int]:
    """Load and validate the counted legacy exception map."""

    payload = json.loads(path.read_text())
    if payload.get("version") != 1 or not isinstance(payload.get("legacy"), dict):
        raise ValueError(f"{path}: expected version 1 with a legacy object")
    baseline: dict[str, int] = {}
    for fingerprint, count in payload["legacy"].items():
        if not isinstance(fingerprint, str) or not isinstance(count, int) or count < 1:
            raise ValueError(f"{path}: invalid legacy entry {fingerprint!r}: {count!r}")
        baseline[fingerprint] = count
    return baseline


def baseline_document(findings: Iterable[Finding]) -> str:
    """Render current findings for deliberate baseline creation or review."""

    counts = Counter(finding.fingerprint for finding in findings)
    return (
        json.dumps(
            {
                "version": 1,
                "description": (
                    "Counted legacy exceptions. Reducing a count is allowed; increasing one "
                    "requires an explicit semantic-specificity review."
                ),
                "legacy": dict(sorted(counts.items())),
            },
            indent=2,
        )
        + "\n"
    )


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Reject new ambiguous quantitative names in schemas and APIs."
    )
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parent.parent
    )
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    parser.add_argument(
        "--print-baseline",
        action="store_true",
        help="print the current counted findings for deliberate baseline review",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    root = args.root.resolve()
    findings = scan_repository(root)
    if args.print_baseline:
        print(baseline_document(findings), end="")
        return 0

    baseline_path = args.baseline
    if not baseline_path.is_absolute():
        baseline_path = root / baseline_path
    try:
        baseline = load_baseline(baseline_path)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"semantic-name-lint: FAIL: {error}")
        return 2

    unexpected = unexpected_findings(findings, baseline)
    if unexpected:
        print("semantic-name-lint: FAIL")
        for finding in unexpected:
            print(
                f"  {finding.path}: {finding.surface} {finding.scope}.{finding.name} "
                "needs a subject- or dimension-specific name"
            )
        return 1

    print(
        "semantic-name-lint: ok "
        f"({len(findings)} bounded legacy occurrences; no new ambiguous names)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
