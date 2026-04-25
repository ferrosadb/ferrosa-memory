from __future__ import annotations

import os

from locust import HttpUser, between, task


class FerrosaWorkbenchUser(HttpUser):
    wait_time = between(1, 3)
    host = os.environ.get("FERROSA_OPERATOR_BASE_URL", "http://127.0.0.1:8766")

    def on_start(self) -> None:
        username = os.environ.get("FERROSA_OPERATOR_USERNAME")
        password = os.environ.get("FERROSA_OPERATOR_PASSWORD")
        if username and password:
            self.client.auth = (username, password)

    @task(1)
    def workbench_home(self) -> None:
        self.client.get("/", name="workbench-home")

    @task(1)
    def workbench_summary(self) -> None:
        self.client.get("/workbench/api/summary", name="workbench-summary")

    @task(3)
    def viz_snapshot(self) -> None:
        self.client.get("/viz/snapshot", name="viz-snapshot")

    @task(2)
    def derived_facts_panel(self) -> None:
        self.client.get(
            "/viz/api/derived_facts?session_id=00000000-0000-0000-0000-000000000000&limit=25",
            name="viz-derived-facts",
        )

    @task(1)
    def anomaly_stream_probe(self) -> None:
        self.client.get("/subscribe/anomalies", name="anomaly-subscribe")
