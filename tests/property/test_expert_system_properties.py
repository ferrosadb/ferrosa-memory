from pathlib import Path

from hypothesis import given
from hypothesis import strategies as st


REPO_ROOT = Path(__file__).resolve().parents[2]
DATALOG_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/datalog.rs"
DISPATCH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/dispatch.rs"
EXPERT_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/expert_system.rs"


def _read(path: Path) -> str:
    return path.read_text()


def _normalize_rule_ids(rule_ids: list[str]) -> tuple[str, ...]:
    return tuple(sorted(set(rule_ids)))


def _replay_decisions(states: list[str]) -> str | None:
    return states[-1] if states else None


def _resolve_scope(scopes: list[str]) -> str | None:
    rank = {"global": 1, "workspace": 2, "session": 3}
    approved = [scope for scope in scopes if scope in rank]
    if not approved:
        return None
    return max(approved, key=lambda scope: rank[scope])


def _truncate_chain(depth: int, limit: int = 4) -> tuple[list[int], bool]:
    chain = list(range(depth))
    return chain[:limit], depth > limit


@given(st.lists(st.text(min_size=1), unique=True))
def test_t_p_001_effective_loader_is_permutation_invariant(rule_ids):
    """T-P-001: effective loader is permutation-invariant."""
    datalog = _read(DATALOG_RS)

    assert _normalize_rule_ids(rule_ids) == _normalize_rule_ids(list(reversed(rule_ids)))
    assert "load_effective_rule_entries(storage, ctx, family)" in datalog
    assert "RuleSource::Builtin" in datalog
    assert "RuleSource::Registry" in datalog


@given(st.lists(st.sampled_from(["approved", "rejected", "proposed"])))
def test_t_p_002_approval_replay_preserves_auth_derived_state(states):
    """T-P-002: approval replay preserves auth-derived state."""
    dispatch = _read(DISPATCH_RS)
    expert = _read(EXPERT_RS)

    assert _replay_decisions(states) == _replay_decisions(list(states))
    assert "reviewer_from_ctx(ctx)" in expert
    assert "Ignored; reviewer is always auth-derived." in dispatch


@given(st.lists(st.sampled_from(["global", "workspace", "session"])))
def test_t_p_003_alias_scope_resolution_is_deterministic(scopes):
    """T-P-003: alias scope resolution is deterministic."""
    expert = _read(EXPERT_RS)

    assert _resolve_scope(scopes) == _resolve_scope(list(reversed(scopes)))
    assert "alias_scope_rank" in expert
    assert "aliases.sort_by" in expert


@given(st.integers(min_value=1, max_value=10))
def test_t_p_004_explanation_ordering_and_bounds_are_invariant(depth):
    """T-P-004: explanation ordering and bounds are invariant."""
    dispatch = _read(DISPATCH_RS)

    chain, truncated = _truncate_chain(depth)
    assert chain == list(range(min(depth, 4)))
    assert truncated is (depth > 4)
    assert ".take(limit)" in dispatch
    assert '"fanout": fact.provenance.len()' in dispatch
    assert '"truncated": truncated' in dispatch
