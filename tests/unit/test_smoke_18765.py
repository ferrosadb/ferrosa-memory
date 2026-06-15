"""Unit tests for the shared HTTP smoke script fixture strategy."""

from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SMOKE_SCRIPT = REPO_ROOT / "scripts" / "smoke-18765.sh"


class Smoke18765FixtureTests(unittest.TestCase):
    def test_smoke_script_creates_isolated_graph_fixture_entities(self):
        text = SMOKE_SCRIPT.read_text()

        self.assertNotIn(
            "SELECT tenant_id, entity_id, session_id FROM agent_memory.entity_store LIMIT 2",
            text,
        )
        self.assertIn('"name":"upsert_entity"', text)
        self.assertIn("smoke-${SMOKE_RUN_ID}-source", text)
        self.assertIn("smoke-${SMOKE_RUN_ID}-destination", text)
