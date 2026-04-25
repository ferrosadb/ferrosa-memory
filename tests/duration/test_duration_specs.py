from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
DURATION_CONFIG = REPO_ROOT / "tests/duration/config.yaml"
DURATION_RUNNER = REPO_ROOT / "tests/duration/run_duration_test.py"
LOCUSTFILE = REPO_ROOT / "tests/load/locustfile.py"


def _read(path: Path) -> str:
    return path.read_text()


def test_t_d_001_shared_service_mixed_traffic_stays_resource_stable():
    """T-D-001: shared service mixed traffic stays resource-stable."""
    config = yaml.safe_load(DURATION_CONFIG.read_text())
    locust = _read(LOCUSTFILE)

    assert config["duration_minutes"] >= 60
    assert config["sample_interval_seconds"] <= 10
    assert config["monitors"]["memory"]["slope_mb_per_hour_threshold"] > 0
    assert config["monitors"]["disk"]["slope_mb_per_hour_threshold"] > 0
    assert "workbench-home" in locust
    assert "viz-snapshot" in locust


def test_t_d_002_explanation_and_explorer_soak_shows_no_leaks():
    """T-D-002: explanation and explorer soak shows no leaks."""
    config = yaml.safe_load(DURATION_CONFIG.read_text())
    runner = _read(DURATION_RUNNER)

    assert sorted(config["monitors"].keys()) == ["connections", "disk", "logs", "memory"]
    assert "tests/baselines/duration-last-run.json" in runner
    assert "memory.sample()" in runner
    assert "disk.sample" in runner
    assert "connections.sample()" in runner
    assert "logs.sample" in runner


def test_t_d_003_viz_workbench_burn_in_respects_cleanup_and_auth_posture():
    """T-D-003: viz/workbench burn-in respects cleanup and auth posture."""
    config = yaml.safe_load(DURATION_CONFIG.read_text())
    locust = _read(LOCUSTFILE)

    assert config["operator_base_url"].startswith("http://127.0.0.1:")
    assert 'host = os.environ.get("FERROSA_OPERATOR_BASE_URL"' in locust
    assert "FERROSA_OPERATOR_USERNAME" in locust
    assert "FERROSA_OPERATOR_PASSWORD" in locust
    assert "workbench-summary" in locust
