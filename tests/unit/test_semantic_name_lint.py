"""
Module: Verify the semantic-specificity lint for public persistence and API names.
Correctness: Correct when ambiguous quantitative names fail and qualified names pass across supported surfaces.
Last revised: 2026-08-19
Last changed: Added the initial red tests for CQL, Rust records, tool schemas, and legacy baselines.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "semantic_name_lint.py"
SPEC = importlib.util.spec_from_file_location("semantic_name_lint", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
semantic_name_lint = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = semantic_name_lint
SPEC.loader.exec_module(semantic_name_lint)


# Test list:
# - [ ] Reject a bare quantitative semantic name in a CQL table.
# - [ ] Reject a bare quantitative semantic name in a public Rust record.
# - [ ] Reject a bare quantitative semantic name in a tool input schema.
# - [ ] Accept subject-specific confidence, trust, quality, score, and risk names.
# - [ ] Permit only the counted occurrences recorded in the legacy baseline.


class SemanticNameLintTests(unittest.TestCase):
    def test_rejects_bare_confidence_in_cql_schema(self) -> None:
        source = """
        CREATE TABLE agent_memory.candidate_facts (
            fact_id uuid PRIMARY KEY,
            confidence double
        );
        """

        findings = semantic_name_lint.scan_cql(Path("ddl/999_test.cql"), source)

        self.assertEqual(
            [(finding.scope, finding.name) for finding in findings],
            [("agent_memory.candidate_facts", "confidence")],
        )

    def test_rejects_bare_score_in_public_rust_record(self) -> None:
        source = """
        pub struct CandidateFact {
            pub fact_id: String,
            pub score: f64,
        }
        """

        findings = semantic_name_lint.scan_rust_records(Path("src/types.rs"), source)

        self.assertEqual(
            [(finding.scope, finding.name) for finding in findings],
            [("CandidateFact", "score")],
        )

    def test_rejects_bare_trust_in_tool_schema(self) -> None:
        source = """
        ToolDef {
            name: "put_candidate".into(),
            input_schema: serde_json::json!({
                "properties": {
                    "trust": { "type": "number" }
                }
            }),
        }
        """

        findings = semantic_name_lint.scan_tool_schemas(
            Path("src/tool_schemas.rs"), source
        )

        self.assertEqual(
            [(finding.scope, finding.name) for finding in findings],
            [("put_candidate", "trust")],
        )

    def test_accepts_subject_specific_quantitative_names(self) -> None:
        rust_source = """
        pub struct CandidateFact {
            pub search_confidence: f64,
            pub fact_confidence: f64,
            pub source_trust: f64,
            pub extraction_quality: f64,
            pub rule_strength_score: f64,
            pub action_risk: f64,
            pub status: CandidateStatus,
        }
        """
        cql_source = """
        CREATE TABLE agent_memory.candidate_facts (
            fact_id uuid PRIMARY KEY,
            fact_confidence double,
            verification_status text
        );
        """

        self.assertEqual(
            semantic_name_lint.scan_rust_records(Path("src/types.rs"), rust_source),
            [],
        )
        self.assertEqual(
            semantic_name_lint.scan_cql(Path("ddl/999_test.cql"), cql_source), []
        )

    def test_baseline_allows_only_recorded_legacy_occurrence_count(self) -> None:
        finding = semantic_name_lint.Finding(
            surface="tool_schema_property",
            path="src/tool_schemas.rs",
            scope="put_candidate",
            name="confidence",
        )
        baseline = {finding.fingerprint: 1}

        self.assertEqual(
            semantic_name_lint.unexpected_findings([finding], baseline), []
        )
        self.assertEqual(
            semantic_name_lint.unexpected_findings([finding, finding], baseline),
            [finding],
        )


if __name__ == "__main__":
    unittest.main()
