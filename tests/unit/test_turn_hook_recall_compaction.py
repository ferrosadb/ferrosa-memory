"""Unit tests for compact Ferrosa Memory recall context rendering."""

from __future__ import annotations

import importlib.util
import json
import os
import unittest
from argparse import Namespace
from pathlib import Path
from typing import Any
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
HOOK = REPO_ROOT / "scripts" / "hooks" / "ferrosa-memory-turn-hook.py"


def load_hook():
    spec = importlib.util.spec_from_file_location("ferrosa_memory_turn_hook", HOOK)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tool_result(payload: dict[str, Any]) -> dict[str, Any]:
    return {"content": [{"type": "text", "text": json.dumps(payload)}]}


def raw_context_segment() -> str:
    embedded = {"message": {"role": "assistant", "content": [{"type": "text", "text": "Raw embedded."}]}}
    user = {
        "parentUuid": "46a2d151-3197-4f2e-a335-014832381c18",
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "content": f"FMT_NOW_CLEAN\n M ferrosa/src/main.rs\nassistant[9]: {json.dumps(embedded)}",
                }
            ],
        },
        "toolUseResult": {"stdout": "FMT_NOW_CLEAN\n M ferrosa/src/main.rs"},
    }
    assistant = {
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "Launched workflow w5vfp77sq with three parallel tracks.",
                }
            ],
        }
    }
    return f"user[0]: {json.dumps(user)}\nassistant[1]: {json.dumps(assistant)}"


class FakeClient:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        self.calls.append((name, arguments))
        if name == "configure":
            return tool_result({"session_id": "11111111-1111-1111-1111-111111111111"})
        if name == "check_intentions":
            return tool_result({"triggered": []})
        if name == "hybrid_search":
            return tool_result(
                {
                    "_hint": "Judge these memories.",
                    "candidate_fanout": {"total_candidates": 15},
                    "hint": "Prior context found.",
                    "reranker": {
                        "applied": True,
                        "judged_ids": ["doc1", "ctx1"],
                        "judge_scores": [1, 1],
                    },
                    "results": [
                        {
                            "id": "doc1",
                            "content": "Useful result one.",
                            "memory_kind": "semantic",
                            "score": 0.1,
                            "source": "document_bm25",
                        },
                        {
                            "id": "ctx1",
                            "content": raw_context_segment(),
                            "memory_kind": "episodic",
                            "score": 0.05,
                            "source": "context_ann",
                            "result_type": "context_segment",
                        },
                    ],
                }
            )
        raise AssertionError(f"unexpected tool call: {name}")


class RecallCompactionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_hook()

    def test_empty_triggered_intentions_are_suppressed(self) -> None:
        self.assertEqual(self.module.compact_recall_block('{"triggered":[]}'), [])

    def test_hybrid_search_json_compacts_to_hints_and_content(self) -> None:
        payload = {
            "_hint": "Judge these memories.",
            "candidate_fanout": {"total_candidates": 15},
            "hint": "Prior context found.",
            "reranker": {"applied": True, "judged_ids": ["result-1"], "judge_scores": [1]},
            "results": [{"id": "result-1", "content": "Useful result.", "memory_kind": "procedural"}],
        }
        self.assertEqual(
            self.module.compact_recall_block(json.dumps(payload)),
            ["Procedural memory: Useful result."],
        )

    def test_hints_only_are_suppressed(self) -> None:
        payload = {"_hint": "Judge these memories.", "hint": "Prior context found.", "results": []}
        self.assertEqual(self.module.compact_recall_block(json.dumps(payload)), [])

    def test_recall_context_does_not_include_full_json_metadata(self) -> None:
        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.0,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=0,
            allowed_kinds={"episodic", "procedural", "semantic"},
        )
        context = self.module.recall_context(
            FakeClient(),
            {"prompt": "Implement it", "cwd": "/repo"},
            args,
        )
        self.assertIn("Ferrosa Memory context for cwd=/repo:", context)
        self.assertIn("Semantic memory: Useful result one.", context)
        self.assertIn("Episodic memory: Tool result: FMT_NOW_CLEAN", context)
        self.assertIn("Episodic memory: Assistant: Launched workflow w5vfp77sq", context)
        self.assertNotIn("Judge these memories.", context)
        self.assertNotIn("Prior context found.", context)
        self.assertNotIn('{"triggered":[]}', context)
        self.assertNotIn("user[0]", context)
        self.assertNotIn("assistant[1]", context)
        self.assertNotIn("assistant[9]", context)
        self.assertNotIn("parentUuid", context)
        self.assertNotIn("candidate_fanout", context)
        self.assertNotIn("document_bm25", context)

    def test_raw_context_segment_compacts_to_readable_lines(self) -> None:
        self.assertEqual(
            self.module.compact_result_content(raw_context_segment()),
            [
                "Tool result: FMT_NOW_CLEAN\n M ferrosa/src/main.rs",
                "Assistant: Launched workflow w5vfp77sq with three parallel tracks.",
            ],
        )

    def test_plain_context_segment_compacts_to_readable_lines(self) -> None:
        self.assertEqual(
            self.module.compact_result_content(
                "user[0]: Remember SESSION_CONTEXT_SENTINEL.\n"
                "assistant[1]: SESSION_CONTEXT_SENTINEL should surface."
            ),
            [
                "User: Remember SESSION_CONTEXT_SENTINEL.",
                "Assistant: SESSION_CONTEXT_SENTINEL should surface.",
            ],
        )

    def test_min_score_filters_low_scored_results(self) -> None:
        payload = {
            "results": [
                {"content": "High score result.", "score": 0.08},
                {"content": "Low score result.", "score": 0.01},
                {"content": "Missing score result."},
            ]
        }
        self.assertEqual(
            self.module.compact_recall_block(json.dumps(payload), min_score=0.05, require_judgment=False),
            ["Semantic memory: High score result."],
        )

    def test_positive_judged_singleton_below_min_score_is_silent(self) -> None:
        payload = {
            "reranker": {
                "applied": True,
                "judged_ids": ["weak"],
                "judge_scores": [1],
            },
            "results": [
                {
                    "id": "weak",
                    "content": "A stale but judge-approved context segment.",
                    "memory_kind": "episodic",
                    "score": 0.024,
                }
            ],
        }
        self.assertEqual(
            self.module.compact_recall_block(json.dumps(payload), min_score=0.062),
            [],
        )

    def test_reranker_judgments_filter_rejected_and_abstained_results(self) -> None:
        payload = {
            "reranker": {
                "applied": True,
                "judged_ids": ["keep", "reject", "neutral", "abstain"],
                "judge_scores": [1, -1, 0, "-"],
            },
            "results": [
                {"id": "keep", "content": "Keep me.", "score": 0.08},
                {"id": "reject", "content": "Reject me.", "score": 0.08},
                {"id": "neutral", "content": "Neutral me.", "score": 0.08},
                {"id": "abstain", "content": "Abstain me.", "score": 0.08},
                {"id": "unjudged", "content": "Unjudged survives.", "score": 0.08},
            ],
        }
        self.assertEqual(
            self.module.compact_recall_block(json.dumps(payload), min_judge_score=1.0),
            ["Semantic memory: Keep me."],
        )

    def test_search_is_silent_without_positive_judged_results(self) -> None:
        payload = {
            "_hint": "Judge these memories.",
            "reranker": {
                "applied": True,
                "judged_ids": ["reject", "abstain"],
                "judge_scores": [-1, "-"],
            },
            "results": [
                {"id": "reject", "content": "Reject me.", "score": 0.09},
                {"id": "abstain", "content": "Abstain me.", "score": 0.09},
                {"id": "unjudged", "content": "Unjudged me.", "score": 0.09},
            ],
        }
        self.assertEqual(self.module.compact_recall_block(json.dumps(payload)), [])

    def test_triggered_intentions_still_surface(self) -> None:
        payload = {"triggered": [{"description": "Review auth error handling"}]}
        self.assertEqual(
            self.module.compact_recall_block(json.dumps(payload)),
            ["Intention: Review auth error handling"],
        )

    def test_query_overlap_filters_irrelevant_positive_results(self) -> None:
        self.assertEqual(
            self.module.query_overlap_count({"memory", "hook", "installer"}, "unrelated build output"),
            0,
        )
        self.assertEqual(
            self.module.query_overlap_count({"memory", "hook", "installer"}, "memory hook installer"),
            3,
        )

    def test_allowed_kinds_filter_memory_categories(self) -> None:
        payload = {
            "results": [
                {"content": "Prior turn.", "memory_kind": "episodic"},
                {"content": "How to run it.", "memory_kind": "procedural"},
                {"content": "Durable fact.", "memory_kind": "semantic"},
            ]
        }
        self.assertEqual(
            self.module.compact_recall_block(
                json.dumps(payload),
                require_judgment=False,
                allowed_kinds={"procedural", "semantic"},
            ),
            [
                "Procedural memory: How to run it.",
                "Semantic memory: Durable fact.",
            ],
        )

    def test_recall_context_passes_relevance_filters_to_search(self) -> None:
        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.062,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=0,
            allowed_kinds={"procedural", "semantic"},
        )
        client = FakeClient()

        self.module.recall_context(
            client,
            {"prompt": "memory hook installer", "cwd": "/repo"},
            args,
        )

        search_call = [call for call in client.calls if call[0] == "hybrid_search"][0]
        self.assertEqual(search_call[1]["session_id"], "11111111-1111-1111-1111-111111111111")
        self.assertEqual(search_call[1]["scope"], "session")
        self.assertEqual(search_call[1]["min_score"], 0.062)
        self.assertEqual(search_call[1]["memory_kinds"], ["procedural", "semantic"])

    def test_recall_context_derives_session_from_payload_workspace(self) -> None:
        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.062,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=0,
            allowed_kinds={"semantic"},
            harness="claude",
        )
        client = FakeClient()

        self.module.recall_context(
            client,
            {
                "prompt": "memory hook installer",
                "cwd": "/Users/bkearns/src/ferrosa-suite",
                "session_id": "claude-marketing-session",
            },
            args,
        )

        configure_call = [call for call in client.calls if call[0] == "configure"][0]
        self.assertEqual(
            configure_call[1]["session_start"]["workspace"],
            "/Users/bkearns/src/ferrosa-suite",
        )
        self.assertEqual(
            configure_call[1]["session_start"]["agent_session_id"],
            "claude-marketing-session",
        )

    def test_recall_context_falls_back_to_cross_session_procedural_when_session_search_is_empty(self) -> None:
        class SessionThenGlobalClient(FakeClient):
            def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
                self.calls.append((name, arguments))
                if name == "configure":
                    return tool_result({"session_id": "22222222-2222-2222-2222-222222222222"})
                if name == "check_intentions":
                    return tool_result({"triggered": []})
                if name == "hybrid_search" and arguments["scope"] == "session":
                    return tool_result({"results": []})
                if name == "hybrid_search" and arguments["scope"] == "both":
                    return tool_result(
                        {
                            "reranker": {
                                "applied": True,
                                "judged_ids": ["global"],
                                "judge_scores": [1],
                            },
                            "results": [
                                {
                                    "id": "global",
                                    "content": "memory hook installer global fallback",
                                    "memory_kind": "procedural",
                                    "score": 0.6,
                                }
                            ],
                        }
                    )
                raise AssertionError(f"unexpected tool call: {name}")

        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.062,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=2,
            allowed_kinds={"procedural", "semantic"},
        )
        client = SessionThenGlobalClient()

        context = self.module.recall_context(
            client,
            {"prompt": "memory hook installer", "cwd": "/repo"},
            args,
        )

        search_scopes = [call[1]["scope"] for call in client.calls if call[0] == "hybrid_search"]
        self.assertEqual(search_scopes, ["session", "both"])
        search_scores = [call[1]["min_score"] for call in client.calls if call[0] == "hybrid_search"]
        self.assertEqual(search_scores, [0.062, 0.35])
        self.assertIn("Procedural memory: memory hook installer global fallback", context)

    def test_cross_session_fallback_requests_only_procedural_by_default(self) -> None:
        class SessionThenGlobalClient(FakeClient):
            def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
                self.calls.append((name, arguments))
                if name == "configure":
                    return tool_result({"session_id": "33333333-3333-3333-3333-333333333333"})
                if name == "check_intentions":
                    return tool_result({"triggered": []})
                if name == "hybrid_search" and arguments["scope"] == "session":
                    return tool_result({"results": []})
                if name == "hybrid_search" and arguments["scope"] == "both":
                    return tool_result(
                        {
                            "reranker": {
                                "applied": True,
                                "judged_ids": ["episodic", "procedural"],
                                "judge_scores": [1, 1],
                            },
                            "results": [
                                {
                                    "id": "episodic",
                                    "content": "Assistant: unrelated LinkedIn profile copy",
                                    "memory_kind": "episodic",
                                    "score": 0.8,
                                },
                                {
                                    "id": "procedural",
                                    "content": "ferrosa memory hook installer policy",
                                    "memory_kind": "procedural",
                                    "score": 0.8,
                                },
                            ],
                        }
                    )
                raise AssertionError(f"unexpected tool call: {name}")

        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.062,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=2,
            allowed_kinds={"episodic", "procedural", "semantic"},
        )
        client = SessionThenGlobalClient()

        context = self.module.recall_context(
            client,
            {"prompt": "ferrosa memory hook installer", "cwd": "/Users/bkearns/src/ferrosa-suite"},
            args,
        )

        both_call = [call for call in client.calls if call[0] == "hybrid_search" and call[1]["scope"] == "both"][0]
        self.assertEqual(both_call[1]["memory_kinds"], ["procedural"])
        self.assertIn("Procedural memory: ferrosa memory hook installer policy", context)
        self.assertNotIn("LinkedIn", context)

    def test_recall_context_derives_workspace_session_without_payload_session_id(self) -> None:
        args = Namespace(
            limit=5,
            max_context_chars=4000,
            min_score=0.062,
            min_judge_score=1.0,
            require_judgment=True,
            include_hints=False,
            min_query_terms=0,
            allowed_kinds={"semantic"},
            harness="claude",
        )
        client = FakeClient()

        env = os.environ.copy()
        env.pop("FERROSA_MEMORY_AGENT_SESSION_ID", None)
        env.pop("CLAUDE_CODE_SESSION_ID", None)
        env.pop("CODEX_THREAD_ID", None)
        with patch.dict(os.environ, env, clear=True):
            self.module.recall_context(
                client,
                {
                    "prompt": "memory hook installer",
                    "cwd": "/Users/bkearns/src/ferrosa-suite/ferrosa",
                },
                args,
            )

        configure_call = [call for call in client.calls if call[0] == "configure"][0]
        self.assertEqual(
            configure_call[1]["session_start"]["agent_session_id"],
            "workspace:/Users/bkearns/src/ferrosa-suite/ferrosa",
        )
        self.assertIn("session_id", configure_call[1]["session_start"])

    def test_ingest_turn_uses_config_example_tenant_by_default(self) -> None:
        class IngestClient(FakeClient):
            def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
                self.calls.append((name, arguments))
                if name == "configure":
                    return tool_result({"session_id": "11111111-1111-1111-1111-111111111111"})
                if name in {"ingest_entities", "ctx_ingest"}:
                    return tool_result({"ok": True})
                raise AssertionError(f"unexpected tool call: {name}")

        args = Namespace(harness="generic", event="turn-end")
        client = IngestClient()
        payload = {
            "prompt": "Remember the tenant default.",
            "assistant_response": "Stored under the documented local tenant.",
            "cwd": "/repo",
            "session_id": "agent-session-1",
        }

        with patch.dict(os.environ, {}, clear=True):
            self.module.ingest_turn(client, payload, args)

        ingest_call = [call for call in client.calls if call[0] == "ingest_entities"][0]
        self.assertEqual(
            ingest_call[1]["tenant_id"],
            "00000000-0000-0000-0000-000000000001",
        )


if __name__ == "__main__":
    unittest.main()
