"""Contract checks for the shipped HTTP-auth example tenant boundary."""

from __future__ import annotations

import tomllib
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = REPO_ROOT / "config" / "ferrosa-memory.example.toml"
SINGLE_USER_AUTH = REPO_ROOT / "examples" / "http-auth.toml"
MULTI_TENANT_AUTH = REPO_ROOT / "examples" / "http-auth-multi-tenant.toml"


def load_toml(path: Path) -> dict:
    with path.open("rb") as source:
        return tomllib.load(source)


class HttpAuthTenantContractTests(unittest.TestCase):
    def test_single_user_auth_maps_to_the_default_memory_tenant(self):
        default_config = load_toml(DEFAULT_CONFIG)
        auth_example = load_toml(SINGLE_USER_AUTH)

        principals = auth_example["principal"]
        self.assertEqual(len(principals), 1)
        self.assertEqual(principals[0]["username"], "ferrosa_user")
        self.assertEqual(
            principals[0]["tenant_id"],
            default_config["server"]["tenant_id"],
        )

    def test_multi_tenant_example_keeps_principal_tenants_distinct(self):
        auth_example = load_toml(MULTI_TENANT_AUTH)
        tenants = {principal["tenant_id"] for principal in auth_example["principal"]}

        self.assertGreater(len(tenants), 1)
