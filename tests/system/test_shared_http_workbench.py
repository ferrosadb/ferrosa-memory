from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKBENCH_HTML = REPO_ROOT / "crates/ferrosa-memory-core/assets/workbench.html"
VIZ_HTML = REPO_ROOT / "crates/ferrosa-memory-core/assets/viz.html"
HTTP_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/http.rs"
DISPATCH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/dispatch.rs"
STORAGE_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/storage.rs"
MAIN_RS = REPO_ROOT / "crates/ferrosa-memory-mcp/src/main.rs"
CONFIG_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/config.rs"
BRAND_HTML = REPO_ROOT.parent / "ferrosa/docs/brand.html"


def test_t_s_001_shared_https_workflow_serves_real_clients_safely():
    """T-S-001: shared HTTPS workflow serves real clients safely."""
    http = HTTP_RS.read_text()
    main = MAIN_RS.read_text()
    config = CONFIG_RS.read_text()

    assert "require_tls: config.server.require_tls" in main
    assert "validate_shared_http_config(&config)?" in main
    assert "HTTP transport requires server.auth_file" in main
    assert '("GET", "/") => Ok(html_response("200 OK", workbench_html))' in http
    assert "Unauthorized" in http
    assert "HTTP transport requires TLS" in config


def test_t_s_002_startup_and_requests_fail_closed_on_bad_shared_config():
    """T-S-002: startup and requests fail closed on bad shared config."""
    spec = (REPO_ROOT / "crates/ferrosa-memory-core/tests/shared_http_deployment_spec.rs").read_text()
    config = CONFIG_RS.read_text()

    assert "tu001_shared_http_requires_auth_backend" in spec
    assert "tu002_tls_secret_wiring_validates_on_startup" in spec
    assert "tu004_http_mode_forbids_tenant_fallback" in spec
    assert "HTTP transport requires server.auth_file" in config
    assert "HTTP transport must not use server.tenant_id fallback" in config


def test_t_s_003_workbench_home_renders_shared_navigation():
    """T-S-003: / workbench home renders shared navigation."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()

    assert "Ferrosa Memory Operator Workbench" in html
    for label in ["Home", "Viz", "CQL Explorer", "SPARQL Explorer", "Datalog Explorer", "Rules", "Approvals"]:
        assert label in html
    assert "Knowledge Graph Console" in html
    assert 'class="top-nav nav"' in html
    assert 'href="/#home"' in html
    assert 'href="/#cql"' in html
    assert 'href="/#sparql"' in html
    assert 'href="/#datalog"' in html
    assert 'href="/#rules"' in html
    assert 'href="/#approvals"' in html
    assert "/workbench/api/sparql/query" in html
    assert '("GET", "/") => Ok(html_response("200 OK", workbench_html))' in http


def test_t_s_003a_summary_fails_loud_when_storage_is_not_ready():
    """T-S-003a: workbench summary reports not_ready instead of masking backend failures."""
    http = HTTP_RS.read_text()

    assert '("GET", "/workbench/api/summary") => {' in http
    assert '"status": if summary_error.is_some() { "not_ready" } else { "ready" }' in http
    assert '"error": summary_error' in http


def test_t_s_004_cross_view_navigation_preserves_shared_filters():
    """T-S-004: cross-view navigation preserves shared filters."""
    html = WORKBENCH_HTML.read_text()

    assert "localStorage.getItem('ferrosa_memory_session')" in html
    assert "state.currentSection" in html
    assert "switchSection" in html
    assert "location.hash" in html


def test_t_s_005_rules_manager_changes_affect_effective_runtime_results():
    """T-S-005: rules manager changes affect effective runtime results."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()
    dispatch = DISPATCH_RS.read_text()

    assert "/workbench/api/rules" in html
    assert '("GET", "/workbench/api/rules")' in http
    assert '("POST", "/workbench/api/rules")' in http
    assert '"manage_rules"' in http
    assert '"get_effective_rule_set"' in dispatch


def test_t_s_006_approval_decisions_are_durable_and_runtime_effective():
    """T-S-006: approval decisions are durable and runtime-effective."""
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()
    dispatch = DISPATCH_RS.read_text()
    storage = STORAGE_RS.read_text()

    assert "/workbench/api/approvals" in html
    assert "/approve" in html and "/reject" in html
    assert '"/workbench/api/approvals/"' in http
    assert '"manage_approvals"' in http
    assert "approval_append" in storage


def test_t_s_007_explanation_hides_out_of_scope_reviewer_data():
    """T-S-007: explanation hides out-of-scope reviewer data."""
    dispatch = DISPATCH_RS.read_text()

    explain_block = dispatch.split("async fn handle_explain_derived", 1)[1].split(
        "async fn handle_get_effective_rule_set", 1
    )[0]
    assert '"support_chain": chain' in explain_block
    assert '"approval_state": Value::Null' in explain_block
    assert '"reviewer"' not in explain_block
    assert '"review_note"' not in explain_block


def test_t_s_008_alias_workbench_slice_exposes_governed_browse_and_management():
    """T-S-008: alias workbench slice exposes governed browse and management."""
    dispatch = DISPATCH_RS.read_text()
    http = HTTP_RS.read_text()

    # Aliases UI tab removed (chore/remove-aliases-tab); the governed-write
    # backend — route + manage_aliases tool — is retained and still guarded.
    assert '("GET", "/workbench/api/aliases")' in http
    assert '("POST", "/workbench/api/aliases")' in http
    assert '"manage_aliases"' in http
    assert '"manage_aliases"' in dispatch
    assert '"action": "list"' in dispatch
    assert '"action": "put"' in dispatch
    assert '"action": "resolve"' in dispatch
    assert '"resolve"' in dispatch
    assert "resolve_alias(" in dispatch


def test_t_s_009_explanation_workbench_slice_posts_explicit_drilldown_queries():
    """T-S-009: explanation workbench slice posts explicit drilldown queries."""
    dispatch = DISPATCH_RS.read_text()
    http = HTTP_RS.read_text()
    html = WORKBENCH_HTML.read_text()

    assert "/workbench/api/explanations/query" in html
    assert '("POST", "/workbench/api/explanations/query")' in http
    assert '"explain_derived"' in http
    assert 'let src_id = args.get("src_id")' in dispatch
    assert 'let dst_id = args.get("dst_id")' in dispatch
    assert '"support_chain": chain' in dispatch


def test_t_s_010_rules_shared_http_supports_source_family_filters():
    """T-S-010: rules shared HTTP surface preserves source/family filters."""
    http = HTTP_RS.read_text()
    html = WORKBENCH_HTML.read_text()

    assert '("GET", "/workbench/api/rules")' in http
    assert '("GET", rules_path) if rules_path.starts_with("/workbench/api/rules?")' in http
    assert 'query_param(path, "source")' in http
    assert 'query_param(path, "family")' in http
    assert 'query_param(rules_path, "source")' in http
    assert 'query_param(rules_path, "family")' in http
    assert '"action": "list"' in http
    assert '"source": source' in http
    assert '"family": family_arg' in http
    assert 'id="rulesSourceFilter"' in html
    assert 'id="rulesFamilyFilter"' in html
    assert 'id="rulesSourceBadge"' in html
    assert 'id="rulesFamilyBadge"' in html
    assert 'id="rulesCountBadge"' in html
    assert 'id="rulesTableMeta"' in html
    assert "state.ruleFilters" in html
    assert "renderRuleFilterState" in html


def test_t_s_011_rules_governance_lifecycle_is_exposed_in_http_and_workbench():
    """T-S-011: rules governance lifecycle is exposed in HTTP and workbench."""
    http = HTTP_RS.read_text()
    html = WORKBENCH_HTML.read_text()

    assert '("POST", "/workbench/api/rules")' in http
    assert '"action": "put"' in http
    assert '"action": "deprecate"' in http
    assert '"approve" | "reject"' in http
    assert '"artifact_kind": "rule"' in http
    assert '"decision": if action == "approve" { "approved" } else { "rejected" }' in http
    assert 'id="ruleFamily"' in html
    assert 'id="rulePredicate"' in html
    assert 'id="ruleBody"' in html
    assert 'id="ruleWeight"' in html
    assert 'data-rule-deprecate=' in html
    assert "Focus" in html
    assert "Deprecate" in html


def test_t_s_012_viz_surface_uses_shared_nav_and_ferrosa_brand_tokens():
    """T-S-012: viz keeps global navigation in the top bar and viz utilities in the left rail."""
    if not BRAND_HTML.exists():
        pytest.skip(
            f"{BRAND_HTML} not present (sibling ferrosa repo not checked out); "
            "brand-token coverage needs a co-located ferrosa working copy"
        )
    html = VIZ_HTML.read_text()
    http = HTTP_RS.read_text()
    brand = BRAND_HTML.read_text()

    assert '(method == "GET" || method == "HEAD") && path == "/viz"' in http
    assert 'origin_for_host(&shell_routes.viz_scheme, host, shell_routes.viz_port)' in http
    assert "redirect_response" in http
    assert 'class="top-nav"' in html
    assert 'data-shell-nav="home"' in html
    assert 'href="/#home"' in html
    assert 'data-shell-nav="viz"' in html
    assert 'data-shell-nav="cql"' in html
    assert 'data-shell-nav="sparql"' in html
    assert 'data-shell-nav="rules"' in html
    assert "window.__FMEM_WORKBENCH_SCHEME__" in html
    assert "window.__FMEM_WORKBENCH_PORT__" in html
    assert "configureShellNav()" in html
    assert "const wsHost = configuredPort > 0 ? `${location.hostname}:${configuredPort}` : location.host;" in html
    assert "const wsUrl = `${protocol}//${wsHost}/viz/ws`;" in html
    assert "ws = new WebSocket(wsUrl);" in html
    assert "28766" not in html
    assert "28767" not in html
    assert ">Home<" in html
    assert "Viz Controls" in html
    for token in ["#e2725b", "#d4a574", "Inter", "JetBrains Mono", "Georgia"]:
        assert token in brand
        assert token in html
    assert "Ferrosa" in html


def test_t_s_013_viz_home_kpis_expose_nodes_edges_and_derived_fact_counts():
    """T-S-013: viz surface moves utility actions into the left rail instead of a KPI strip."""
    html = VIZ_HTML.read_text()
    http = HTTP_RS.read_text()

    assert "/viz/api/derived_facts" in http
    assert "total_nodes: Some(total_n)" in http
    assert "total_edges: Some(total_e)" in http
    assert 'id="side-rail"' in html
    assert "Derived Facts" in html
    assert "LLM Settings" in html
    assert "home-nodes" not in html
    assert "home-derived-facts" not in html
    assert 'data-kpi="nodes"' not in html
    assert 'data-kpi="edges"' not in html
    assert 'data-kpi="derived-facts"' not in html


def test_t_s_014_workbench_shell_uses_ferrosa_brand_tokens_and_home_graph_kpis():
    """T-S-014: workbench uses the Ferrosa shell and graph KPIs."""
    if not BRAND_HTML.exists():
        pytest.skip(
            f"{BRAND_HTML} not present (sibling ferrosa repo not checked out); "
            "brand-token coverage needs a co-located ferrosa working copy"
        )
    html = WORKBENCH_HTML.read_text()
    http = HTTP_RS.read_text()
    brand = BRAND_HTML.read_text()

    for token in ["#e2725b", "#d4a574", "Inter", "JetBrains Mono", "Georgia"]:
        assert token in brand
        assert token in html
    assert "Ferrosa Memory" in html
    assert "Knowledge Graph Console" in html
    assert "window.__FMEM_VIZ_SCHEME__" in html
    assert "window.__FMEM_VIZ_PORT__" in html
    assert "configureShellLinks" in html
    assert "CQL passthrough contract" in html
    assert "SPARQL passthrough contract" in html
    assert "local Datalog" in html
    assert "Submitting CQL to /workbench/api/cql/query" in html
    assert "/workbench/api/sparql/query" in html
    assert "28766" not in html
    assert "28767" not in html
    assert 'data-kpi="nodes"' in html
    assert 'data-kpi="edges"' in html
    assert 'data-kpi="derived-facts"' in html
    assert '"node_count": node_count' in http
    assert '"edge_count": edge_count' in http
    assert '"derived_fact_count": derived_fact_count' in http


def test_t_s_015_sparql_workbench_surface_exposes_public_pass_through():
    """T-S-015: SPARQL workbench surface exposes public pass-through query contract."""
    html = WORKBENCH_HTML.read_text()

    assert 'data-section="sparql"' in html
    assert 'data-open="sparql"' in html
    assert 'id="sparqlForm"' in html
    assert 'id="sparqlQuery"' in html
    assert 'id="sparqlStatus"' in html
    assert 'id="sparqlResultMeta"' in html
    assert 'id="sparqlOutput"' in html
    assert "/workbench/api/sparql/query" in html
    assert "public SPARQL passthrough contract" in html
    assert "including result bindings" in html
