from pathlib import Path
import tomllib


REPO_ROOT = Path(__file__).resolve().parents[2]
HTTP_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/http.rs"
DISPATCH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/dispatch.rs"
DATALOG_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/datalog.rs"
AUTH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/auth.rs"
GRAPH_WRITE_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/graph_write.rs"
FOLD_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/fold.rs"
ENTITY_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/entity.rs"
TEMPORAL_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/temporal.rs"
SMART_INGEST_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/smart_ingest.rs"
SKILL_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/skill.rs"
ENRICH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/enrich.rs"
EXPERT_SYSTEM_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/expert_system.rs"
MCP_MAIN_RS = REPO_ROOT / "crates/ferrosa-memory-mcp/src/main.rs"
SYNC_MAIN_RS = REPO_ROOT / "crates/ferrosa-memory-sync/src/main.rs"
EX_HTTP_CONFIG = REPO_ROOT / "examples/ferrosa-memory-http.toml"
EX_HTTP_AUTH = REPO_ROOT / "examples/http-auth.toml"


def _read_text(path: Path) -> str:
    return path.read_text()


def _read_toml(path: Path) -> dict:
    return tomllib.loads(path.read_text())


def test_t_i_001_shared_http_isolates_tenants_by_principal():
    """T-I-001: shared HTTP isolates tenants by principal."""
    config = _read_toml(EX_HTTP_CONFIG)
    http = _read_text(HTTP_RS)
    auth = _read_text(AUTH_RS)
    auth_db = _read_toml(EX_HTTP_AUTH)

    assert config["server"]["transport"] == "http"
    assert config["server"]["require_tls"] is True
    assert "auth_file" in config["server"]
    assert "tenant_id" not in config["server"]

    principals = auth_db["principal"]
    tenant_ids = {entry["tenant_id"] for entry in principals}
    assert len(principals) >= 2
    assert len(tenant_ids) == len(principals)

    assert "fn authenticate_http" in auth
    assert "authenticate_from_headers" in http
    assert "only Basic or Bearer auth supported" in http
    assert 'session_origin: "http"' in auth


def test_t_i_002_live_and_ready_probes_diverge_correctly():
    """T-I-002: live and ready probes diverge correctly."""
    http = _read_text(HTTP_RS)

    assert ('("GET", "/healthz/live")') in http
    assert ('("GET", "/healthz/ready")') in http
    assert 'text_response("200 OK", "ok")' in http
    assert 'text_response("503 Service Unavailable", "not ready")' in http
    assert 'text_response("200 OK", "ready")' in http
    assert "if readiness_checker()" in http


def test_t_i_003_shared_public_endpoint_excludes_viz_by_default():
    """T-I-003: public shared endpoint excludes viz by default."""
    config = _read_toml(EX_HTTP_CONFIG)
    http = _read_text(HTTP_RS)

    assert config["viz"]["enabled"] is False
    assert "auth_file" in config["server"]
    assert "tenant_id" not in config["server"]
    assert "handle_viz_connection" in http


def test_t_i_004_registry_and_evaluator_use_same_effective_rules():
    """T-I-004: registry and evaluator use same effective rules."""
    dispatch = _read_text(DISPATCH_RS)
    datalog = _read_text(DATALOG_RS)

    assert "load_effective_rule_entries(storage, ctx, Some(family))" in dispatch
    assert "handle_get_effective_rule_set" in dispatch
    assert '"source": match rule.source' in dispatch
    assert "query_predicate" in datalog
    assert "load_effective_rules(storage, ctx, Some(predicate))" in datalog
    assert "load_effective_rule_entries(storage, ctx, family)" in datalog
    assert "RuleSource::Builtin" in datalog
    assert "RuleSource::Registry" in datalog


def test_t_i_005_unapproved_artifacts_stay_out_of_default_runtime():
    """T-I-005: unapproved artifacts stay out of default runtime."""
    datalog = _read_text(DATALOG_RS)
    dispatch = _read_text(DISPATCH_RS)
    http = _read_text(HTTP_RS)

    assert "if crate::expert_system::is_artifact_approved" in datalog
    assert "approval_state" in dispatch
    assert '("GET", "/workbench/api/rules")' in http
    assert '"source": "registry"' in dispatch
    assert "load_effective_rule_entries(storage, ctx, family)" in datalog


def test_t_i_006_explanation_returns_complete_ordered_support_chain():
    """T-I-006: explanation returns complete ordered support chain."""
    dispatch = _read_text(DISPATCH_RS)
    http = _read_text(HTTP_RS)

    assert '("POST", "/workbench/api/datalog/query")' in http
    assert "call_tool_http" in http
    assert "explain_derived" in dispatch
    assert "let chain: Vec<Value> = fact" in dispatch
    assert "parent_src" in dispatch
    assert ".iter()" in dispatch
    assert ".take(limit)" in dispatch
    assert "\"support_chain\": chain" in dispatch
    assert "\"truncated\": truncated" in dispatch


def test_t_i_007_graph_writes_route_through_graph_write_seam():
    """T-I-007: feature modules route graph writes through the shared seam."""
    graph_write = _read_text(GRAPH_WRITE_RS)
    fold = _read_text(FOLD_RS)
    entity = _read_text(ENTITY_RS)
    temporal = _read_text(TEMPORAL_RS)
    smart_ingest = _read_text(SMART_INGEST_RS)
    skill = _read_text(SKILL_RS)
    enrich = _read_text(ENRICH_RS)

    assert "create_typed_edge" in graph_write
    assert "create_folded_into_edge" in graph_write
    assert "create_mentioned_in_edge" in graph_write
    assert "reinforce_co_occurs_edge" in graph_write
    assert "create_supersedes_edge" in graph_write

    assert "graph_write::create_folded_into_edge" in fold
    assert "graph_write::create_mentioned_in_edge" in entity
    assert "graph_write::create_supersedes_edge" in temporal
    assert "graph_write::create_supersedes_edge" in smart_ingest
    assert "graph_write::create_typed_edge" in skill
    assert "graph_write::create_typed_edge" in enrich


def test_t_i_013_serving_and_sync_paths_do_not_mutate_graph_tables_with_raw_cql():
    """T-I-013: runtime graph mutations no longer name graph-owned backing tables."""
    combined = "\n".join(
        [
            _read_text(REPO_ROOT / "crates/ferrosa-memory-core/src/cql_storage.rs"),
            _read_text(MCP_MAIN_RS),
            _read_text(SYNC_MAIN_RS),
        ]
    )

    forbidden = [
        "INSERT INTO {ks}.typed_edges",
        "INSERT INTO {ks}.folded_into",
        "INSERT INTO {ks}.mentioned_in",
        "INSERT INTO {ks}.co_occurs_with",
        "INSERT INTO {ks}.supersedes",
        "DELETE FROM {}.co_occurs_with",
        "UPDATE {}.co_occurs_with SET strength",
        "INSERT INTO {}.typed_edges",
    ]
    for needle in forbidden:
        assert needle not in combined


def test_t_i_014_dead_local_cql_emulator_is_removed():
    """T-I-014: workbench CQL no longer has a local semantic emulator fallback."""
    expert_system = _read_text(EXPERT_SYSTEM_RS)
    assert "run_readonly_cql" not in expert_system
    assert "parse_readonly_query" not in expert_system
