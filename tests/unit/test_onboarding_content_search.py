"""Contract checks for the first-run content-search verification step."""

from __future__ import annotations

from pathlib import Path
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
ONBOARDING = REPO_ROOT / "ONBOARDING.md"


class OnboardingContentSearchTests(unittest.TestCase):
    def test_onboarding_proves_content_retrieval_without_semantic_ranking(self):
        text = ONBOARDING.read_text()
        phase_start = text.index("## Phase 13")
        phase_end = text.index("### Record an evolving fact", phase_start)
        phase = text[phase_start:phase_end]

        entity_name = "Ferrosa Memory onboarding"
        content_token = "c7lexicalsentinel"

        self.assertIn(entity_name, phase)
        self.assertIn(f"Content verification token: {content_token}", phase)
        self.assertIn(f'"query": "{content_token}"', phase)
        self.assertNotIn(content_token, entity_name)
        self.assertIn("embedding_status", phase)
        self.assertIn("`unavailable`", phase)
        self.assertIn("`failed`", phase)
        self.assertIn("semantic ANN ranking is unavailable", phase)
