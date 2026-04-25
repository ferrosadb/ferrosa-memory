from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


@dataclass
class Sample:
    bytes_used: int


def sample(path: str) -> Sample:
    target = Path(path)
    if not target.exists():
        return Sample(bytes_used=0)
    if target.is_file():
        return Sample(bytes_used=target.stat().st_size)
    return Sample(bytes_used=sum(p.stat().st_size for p in target.rglob("*") if p.is_file()))

