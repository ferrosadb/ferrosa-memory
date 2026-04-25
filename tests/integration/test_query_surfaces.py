from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKBENCH_HTML = REPO_ROOT / "crates/ferrosa-memory-core/assets/workbench.html"
HTTP_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/http.rs"
DISPATCH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/dispatch.rs"


def test_t_i_007_cql_explorer_uses_public_query_passthrough():
    """T-I-007: CQL explorer uses public query passthrough."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()

    assert "/workbench/api/cql/query" in html
    assert "/workbench/api/sparql/query" in html
    assert '("POST", "/workbench/api/cql/query")' in http
    assert "CQL passthrough contract" in html
    assert "Submitting CQL to /workbench/api/cql/query" in html
    assert 'data-template-target="sparqlQuery"' in html
    assert 'id="sparqlResultMeta"' in html
    assert 'data-open="sparql"' in html


def test_t_i_008_datalog_explorer_renders_provenance_drilldown():
    """T-I-008: Datalog explorer renders provenance drilldown."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()
    dispatch = DISPATCH_RS.read_text()

    assert "/workbench/api/datalog/query" in html
    assert "/workbench/api/explanations/query" in html
    assert '("POST", "/workbench/api/datalog/query")' in http
    assert '("POST", "/workbench/api/explanations/query")' in http
    assert '"query_derived"' in http
    assert '"explain_derived"' in http
    assert "support_chain" in dispatch
    assert "local Datalog inference contract" in html
    assert 'let src_id = args.get("src_id")' in dispatch
    assert 'let dst_id = args.get("dst_id")' in dispatch
    assert "Approvals / Explanations" in html


def test_t_i_012_query_status_text_reflects_transport_contracts():
    """T-I-012: query status text reflects passthrough and local-engine behavior."""
    html = WORKBENCH_HTML.read_text()

    assert "Submitting CQL to /workbench/api/cql/query" in html
    assert "CQL passthrough completed with" in html
    assert "Submitting SPARQL to /workbench/api/sparql/query" in html
    assert "SPARQL passthrough completed with" in html
    assert "Evaluating Datalog predicate locally" in html
    assert "Datalog completed locally" in html
    assert "Requesting explanation from /workbench/api/explanations/query" in html
    assert "Explanation request complete" in html


def test_t_i_009_alias_surface_exposes_browse_and_governed_writes():
    """T-I-009: alias surface exposes browse and governed writes."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()
    dispatch = DISPATCH_RS.read_text()

    assert "/workbench/api/aliases" in html
    assert '("GET", "/workbench/api/aliases")' in http
    assert '("POST", "/workbench/api/aliases")' in http
    assert '"manage_aliases"' in http
    assert '"action": "list"' in dispatch
    assert '"action": "put"' in dispatch
    assert '"action": "resolve"' in dispatch


def test_t_i_010_richer_query_surfaces_expose_presets_result_meta_and_drilldown_controls():
    """T-I-010: richer query surfaces expose presets, result meta, and drilldown controls."""
    html = WORKBENCH_HTML.read_text()

    assert 'data-template-target="cqlQuery"' in html
    assert 'id="cqlResultMeta"' in html
    assert "Common app tables: agent_memory.entity_store, agent_memory.rules_by_id, agent_memory.rules_by_family, agent_memory.aliases_by_name, agent_memory.approvals_by_target, agent_memory.derived_cache_by_pred" in html
    assert "agent_memory.entity_store" in html
    assert "agent_memory.derived_cache_by_pred" in html
    assert 'data-template-target="datalogPredicate"' in html
    assert 'id="datalogSessionId"' in html
    assert 'id="datalogUseActiveSession"' in html
    assert 'id="datalogResultMeta"' in html
    assert 'data-template-target="sparqlQuery"' in html
    assert 'id="sparqlQuery"' in html
    assert 'id="sparqlResultMeta"' in html
    assert 'id="explanationLimit"' in html
    assert "Selected rows automatically supply" in html
    assert "/workbench/api/cql/query" in html
    assert "/workbench/api/datalog/query" in html
    assert "/workbench/api/explanations/query" in html
    assert "/workbench/api/sparql/query" in html


def test_t_i_011_rules_query_surface_exposes_filters_and_rule_lifecycle_actions():
    """T-I-011: rules query surface exposes source/family filters and rule lifecycle actions."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()

    assert 'id="rulesFilterForm"' in html
    assert 'id="rulesSourceFilter"' in html
    assert 'id="rulesFamilyFilter"' in html
    assert 'id="ruleFamilyOptions"' in html
    assert 'id="rulePredicate"' in html
    assert 'id="ruleBody"' in html
    assert 'id="ruleWeight"' in html
    assert 'id="rulesTableMeta"' in html
    assert "builtin, registry, and effective rule sets" in html
    assert "with source and family filters preserved in the operator console" in html
    assert '("GET", "/workbench/api/rules")' in http
    assert '("GET", rules_path) if rules_path.starts_with("/workbench/api/rules?")' in http
    assert '("POST", "/workbench/api/rules")' in http
    assert '"action": "deprecate"' in http
    assert '"approve" | "reject"' in http
