from __future__ import annotations

import os
from dataclasses import dataclass

import psutil


@dataclass
class Sample:
    rss_mb: float


def sample(pid: int | None = None) -> Sample:
    process = psutil.Process(pid or os.getpid())
    return Sample(rss_mb=process.memory_info().rss / (1024 * 1024))

