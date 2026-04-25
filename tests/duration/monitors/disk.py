from __future__ import annotations

import os
from dataclasses import dataclass


@dataclass
class Sample:
    bytes_used: int


def sample(path: str) -> Sample:
    stat = os.statvfs(path)
    used = (stat.f_blocks - stat.f_bfree) * stat.f_frsize
    return Sample(bytes_used=used)

