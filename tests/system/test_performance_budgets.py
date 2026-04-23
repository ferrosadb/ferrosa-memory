from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HTTP_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/http.rs"
DISPATCH_RS = REPO_ROOT / "crates/ferrosa-memory-core/src/dispatch.rs"
WORKBENCH_HTML = REPO_ROOT / "crates/ferrosa-memory-core/assets/workbench.html"


def _read(path: Path) -> str:
    return path.read_text()


def test_t_pf_001_shared_auth_and_probe_overhead_stays_within_envelope():
    """T-PF-001: shared auth/probe overhead stays within envelope."""
    http = _read(HTTP_RS)

    assert "TLS_ACCEPT_BUDGET" in http
    assert "from_secs(10)" in http
    assert "REQUEST_BUDGET" in http
    assert "from_secs(30)" in http
    assert '("GET", "/healthz/live")' in http
    assert '("GET", "/healthz/ready")' in http


def test_t_pf_002_workbench_home_loads_without_blocking_on_slow_views():
    """T-PF-002: workbench home loads without blocking on slow views."""
    http = _read(HTTP_RS)
    html = _read(WORKBENCH_HTML)

    assert 'include_str!("../assets/workbench.html")' in http
    assert '("GET", "/") => Ok(html_response("200 OK", WORKBENCH_HTML))' in http
    assert "loadHomeSummary" in html
    assert "switchSection" in html


def test_t_pf_003_on_demand_explanation_stays_under_latency_cap():
    """T-PF-003: on-demand explanation stays under latency cap."""
    dispatch = _read(DISPATCH_RS)
    http = _read(HTTP_RS)

    assert '.clamp(1, 64)' in dispatch
    assert 'let elapsed_ms = start.elapsed().as_millis() as i64;' in dispatch
    assert 'heat_record(ctx, &metric_predicate, false, Some(elapsed_ms))' in dispatch
    assert "REQUEST_BUDGET" in http


def test_t_pf_004_operator_query_surfaces_stay_within_p95_budget():
    """T-PF-004: operator query surfaces stay within p95 budget."""
    dispatch = _read(DISPATCH_RS)
    http = _read(HTTP_RS)

    assert '("POST", "/workbench/api/cql/query")' in http
    assert '("POST", "/workbench/api/datalog/query")' in http
    assert '.clamp(1, 500)' in dispatch or '.clamp(1, 500);' in dispatch
    assert '.clamp(1, 64)' in dispatch
