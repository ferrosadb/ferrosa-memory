#!/usr/bin/env python3
"""Minimal traceability coverage-gap checker for blueprint-generated tests."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ID_RE = re.compile(r"T-[A-Z]+-\d{3}")


def extract_ids(path: Path) -> set[str]:
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return set()
    return set(ID_RE.findall(text))


def iter_files(paths: list[Path]) -> list[Path]:
    files: list[Path] = []
    for path in paths:
        if path.is_file():
            files.append(path)
        elif path.is_dir():
            files.extend(p for p in path.rglob("*") if p.is_file())
    return files


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print("usage: coverage_gap.py specs/test-specification.md <path> [<path>...]")
        return 2

    spec_path = Path(argv[1])
    scan_paths = [Path(arg) for arg in argv[2:]]

    expected = extract_ids(spec_path)
    observed: set[str] = set()
    for path in iter_files(scan_paths):
        observed |= extract_ids(path)

    missing = sorted(expected - observed)
    orphan = sorted(observed - expected)

    print("## Coverage Gap Report")
    print()
    print(f"Specified IDs: {len(expected)}")
    print(f"Observed IDs: {len(observed)}")
    print(f"Missing IDs: {len(missing)}")
    print(f"Orphan IDs: {len(orphan)}")
    print()

    if missing:
        print("### Missing")
        for item in missing:
            print(item)
        print()

    if orphan:
        print("### Orphan")
        for item in orphan:
            print(item)

    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

