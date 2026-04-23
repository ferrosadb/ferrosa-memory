"""Shared pytest fixtures for repo-level blueprint harness tests."""

from __future__ import annotations

import os
from typing import Iterator

import httpx
import pytest


def _default_base_url() -> str:
    return os.environ.get("FERROSA_BASE_URL", "http://127.0.0.1:8765")


@pytest.fixture(scope="session")
def base_url() -> str:
    return _default_base_url()


@pytest.fixture(scope="session")
def operator_base_url() -> str:
    return os.environ.get("FERROSA_OPERATOR_BASE_URL", _default_base_url())


@pytest.fixture
def client(base_url: str) -> Iterator[httpx.Client]:
    with httpx.Client(base_url=base_url, timeout=10.0) as session:
        yield session

