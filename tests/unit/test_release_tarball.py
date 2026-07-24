"""Release artifact contract tests for binary-installed templates."""

from __future__ import annotations

import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
STAGER = REPO_ROOT / ".github" / "scripts" / "stage-release-tarball.sh"


class ReleaseTarballTests(unittest.TestCase):
    def test_bundles_all_published_examples(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            for binary in ("ferrosa-memory", "ferrosa-memory-mcp"):
                path = bin_dir / binary
                path.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
                path.chmod(0o755)

            env = os.environ | {"GITHUB_REF_NAME": "v0.0.0-tarball-contract"}
            subprocess.run(
                ["bash", str(STAGER), "x86_64-unknown-linux-musl", str(bin_dir)],
                check=True,
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                text=True,
            )

            tarball = REPO_ROOT / "dist" / "ferrosa-memory-v0.0.0-tarball-contract-x86_64-unknown-linux-musl.tar.gz"
            try:
                with tarfile.open(tarball) as archive:
                    names = {name.removeprefix("./") for name in archive.getnames()}
            finally:
                tarball.unlink(missing_ok=True)

        expected = {
            f"examples/{path.relative_to(REPO_ROOT / 'examples').as_posix()}"
            for path in (REPO_ROOT / "examples").rglob("*")
            if path.is_file()
        }
        self.assertTrue(expected <= names)

    def test_binary_installer_retains_auth_examples(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            stage = root / "stage"
            (stage / "config").mkdir(parents=True)
            (stage / "examples").mkdir()
            (stage / "ferrosa-memory-mcp").write_text("#!/usr/bin/env bash\n", encoding="utf-8")
            (stage / "ferrosa-memory-mcp").chmod(0o755)
            (stage / "config" / "ferrosa-memory.example.toml").write_text(
                "[server]\ntransport = 'stdio'\n",
                encoding="utf-8",
            )
            (stage / "examples" / "http-auth.toml").write_text(
                "# shared auth template\n",
                encoding="utf-8",
            )

            tarball = root / "ferrosa-memory.tar.gz"
            with tarfile.open(tarball, "w:gz") as archive:
                for path in stage.rglob("*"):
                    archive.add(path, arcname=path.relative_to(stage))

            home = root / "home"
            env = os.environ | {
                "HOME": str(home),
                "FERROSA_MEMORY_INSTALL_TARBALL": str(tarball),
            }
            subprocess.run(
                [
                    "bash",
                    str(REPO_ROOT / "docs" / "install-memory.sh"),
                    "--version",
                    "v0.0.0-installer-contract",
                    "--no-service",
                    "--no-skills",
                ],
                check=True,
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                text=True,
            )

            installed = home / ".ferrosa" / "share" / "ferrosa-memory" / "examples" / "http-auth.toml"
            self.assertEqual(installed.read_text(encoding="utf-8"), "# shared auth template\n")


if __name__ == "__main__":
    unittest.main()
