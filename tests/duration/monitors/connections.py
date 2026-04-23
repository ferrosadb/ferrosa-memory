from __future__ import annotations

import os
from dataclasses import dataclass

import psutil


@dataclass
class Sample:
    open_fds: int | None


def sample(pid: int | None = None) -> Sample:
    process = psutil.Process(pid or os.getpid())
    open_fds = process.num_fds() if hasattr(process, "num_fds") else None
    return Sample(open_fds=open_fds)

