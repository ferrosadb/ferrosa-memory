from __future__ import annotations

import argparse
import json
from pathlib import Path

import yaml

from monitors import connections, disk, logs, memory


def main() -> int:
    parser = argparse.ArgumentParser(description="Duration test harness stub")
    parser.add_argument("--config", required=True)
    parser.add_argument("--baseline", action="store_true")
    args = parser.parse_args()

    with open(args.config, "r", encoding="utf-8") as fh:
        config = yaml.safe_load(fh)

    snapshot = {
        "baseline_mode": args.baseline,
        "memory": memory.sample().__dict__,
        "disk": disk.sample(config["monitors"]["disk"]["path"]).__dict__,
        "connections": connections.sample().__dict__,
        "logs": logs.sample(config["monitors"]["logs"]["path"]).__dict__,
    }

    output_path = Path("tests/baselines/duration-last-run.json")
    output_path.write_text(json.dumps(snapshot, indent=2), encoding="utf-8")
    print(json.dumps(snapshot, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

