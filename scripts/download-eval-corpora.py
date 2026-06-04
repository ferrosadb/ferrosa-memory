#!/usr/bin/env python3
"""Download official evaluation corpora used by ferrosa-memory-eval.

The downloaded data is intentionally written under an ignored directory
(`.eval-corpus/` by default). Do not commit corpus files.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable

from huggingface_hub import HfApi, snapshot_download


BRIGHT_PRO_REPO = "yale-nlp/Bright-Pro"
MEMORYBENCH_REPOS = {
    "balanced": "THUIR/MemoryBench",
    "full": "THUIR/MemoryBench-Full",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Download BRIGHT-Pro and MemoryBench corpora from Hugging Face."
    )
    parser.add_argument(
        "--output-dir",
        default=".eval-corpus",
        help="Ignored local directory for downloaded corpora.",
    )
    parser.add_argument(
        "--corpus",
        action="append",
        choices=["bright-pro", "memorybench"],
        help="Corpus to download. Repeatable. Defaults to both.",
    )
    parser.add_argument(
        "--memorybench-variant",
        choices=["balanced", "full", "both"],
        default="full",
        help=(
            "MemoryBench dataset variant. 'balanced' matches THUIR/MemoryBench; "
            "'full' downloads THUIR/MemoryBench-Full."
        ),
    )
    parser.add_argument(
        "--revision",
        action="append",
        default=[],
        metavar="DATASET=REV",
        help=(
            "Optional revision pin. Dataset keys: bright-pro, "
            "memorybench-balanced, memorybench-full."
        ),
    )
    parser.add_argument(
        "--clean",
        action="store_true",
        help="Remove the target dataset directory before downloading.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would be downloaded without fetching data.",
    )
    return parser.parse_args()


def revision_map(entries: Iterable[str]) -> dict[str, str]:
    parsed: dict[str, str] = {}
    for entry in entries:
        if "=" not in entry:
            raise SystemExit(f"--revision must be DATASET=REV, got {entry!r}")
        key, rev = entry.split("=", 1)
        key = key.strip()
        rev = rev.strip()
        if not key or not rev:
            raise SystemExit(f"--revision must be DATASET=REV, got {entry!r}")
        parsed[key] = rev
    return parsed


def dataset_specs(args: argparse.Namespace) -> list[dict[str, object]]:
    requested = set(args.corpus or ["bright-pro", "memorybench"])
    specs: list[dict[str, object]] = []
    if "bright-pro" in requested:
        specs.append(
            {
                "key": "bright-pro",
                "repo_id": BRIGHT_PRO_REPO,
                "local_name": "bright-pro",
                "allow_patterns": [
                    "README.md",
                    "LICENSE",
                    "examples/*.parquet",
                    "aspects/*.parquet",
                    "documents/*.parquet",
                ],
            }
        )
    if "memorybench" in requested:
        variants = (
            ["balanced", "full"]
            if args.memorybench_variant == "both"
            else [args.memorybench_variant]
        )
        for variant in variants:
            specs.append(
                {
                    "key": f"memorybench-{variant}",
                    "repo_id": MEMORYBENCH_REPOS[variant],
                    "local_name": f"memorybench-{variant}",
                "allow_patterns": [
                    "README.md",
                    "LICENSE",
                    "dataset/**/*.arrow",
                    "corpus/*.jsonl",
                    "corpus/**/*.jsonl",
                ],
            }
        )
    return specs


def remove_if_requested(path: Path, clean: bool) -> None:
    if clean and path.exists():
        shutil.rmtree(path)


def write_manifest(
    path: Path,
    *,
    repo_id: str,
    revision: str | None,
    resolved_sha: str | None,
    allow_patterns: list[str],
    snapshot_path: str,
) -> None:
    files = []
    for file_path in sorted(path.rglob("*")):
        if file_path.is_file() and file_path.name != "manifest.json":
            files.append(
                {
                    "path": str(file_path.relative_to(path)),
                    "bytes": file_path.stat().st_size,
                }
            )
    manifest = {
        "repo_id": repo_id,
        "repo_type": "dataset",
        "requested_revision": revision,
        "resolved_sha": resolved_sha,
        "downloaded_at": datetime.now(timezone.utc).isoformat(),
        "allow_patterns": allow_patterns,
        "snapshot_path": snapshot_path,
        "file_count": len(files),
        "total_bytes": sum(item["bytes"] for item in files),
        "files": files,
    }
    (path / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def download_one(
    api: HfApi,
    output_dir: Path,
    spec: dict[str, object],
    revisions: dict[str, str],
    *,
    clean: bool,
    dry_run: bool,
) -> None:
    key = str(spec["key"])
    repo_id = str(spec["repo_id"])
    local_dir = output_dir / str(spec["local_name"])
    allow_patterns = list(spec["allow_patterns"])
    revision = revisions.get(key)

    print(f"{key}: repo={repo_id} revision={revision or '<default>'}")
    print(f"{key}: target={local_dir}")
    if dry_run:
        return

    remove_if_requested(local_dir, clean)
    local_dir.mkdir(parents=True, exist_ok=True)

    info = api.dataset_info(repo_id, revision=revision)
    snapshot_path = snapshot_download(
        repo_id=repo_id,
        repo_type="dataset",
        revision=revision,
        allow_patterns=allow_patterns,
        local_dir=str(local_dir),
        local_dir_use_symlinks=False,
    )
    write_manifest(
        local_dir,
        repo_id=repo_id,
        revision=revision,
        resolved_sha=getattr(info, "sha", None),
        allow_patterns=allow_patterns,
        snapshot_path=snapshot_path,
    )
    print(f"{key}: wrote {local_dir / 'manifest.json'}")


def main() -> None:
    args = parse_args()
    output_dir = Path(args.output_dir).expanduser().resolve()
    revisions = revision_map(args.revision)

    os.environ.setdefault("HF_HUB_ENABLE_HF_TRANSFER", "1")
    api = HfApi()
    for spec in dataset_specs(args):
        download_one(
            api,
            output_dir,
            spec,
            revisions,
            clean=args.clean,
            dry_run=args.dry_run,
        )

    if not args.dry_run:
        print(f"done: corpora are under ignored directory {output_dir}")


if __name__ == "__main__":
    main()
