.PHONY: build-podman test-unit test-contracts test-integration test-system test-property \
	test-security test-load test-load-smoke test-duration test-baseline \
	test-live test-all test-coverage-gap

PYTHON ?= python3
PIP ?= $(PYTHON) -m pip
PYTEST ?= $(PYTHON) -m pytest
PODMAN ?= podman
PODMAN_BUILD_IMAGE ?= docker.io/library/rust:1

build-podman:
	command -v $(PODMAN) >/dev/null
	mkdir -p "$(HOME)/.cargo/registry"
	$(PODMAN) run --rm \
		-v "$(CURDIR)":/work \
		-v "$(HOME)/.cargo/registry":/usr/local/cargo/registry \
		-w /work \
		-e CARGO_TARGET_DIR=/work/target-podman-linux \
		$(PODMAN_BUILD_IMAGE) \
		bash -lc '. /usr/local/cargo/env && export DEBIAN_FRONTEND=noninteractive && apt-get update -qq && apt-get install -y --no-install-recommends cmake >/dev/null && cargo build --release -p ferrosa-memory-mcp'

test-unit: check-viz-theme
	cargo test --workspace --lib

# The two light-theme token blocks in the web assets are duplicated because CSS
# cannot alias one from the other, and duplicated blocks drift. They already did
# once, silently — a token added to the explicit block never reached the
# prefers-color-scheme copy, which only shows up on a light-preference host with
# no saved choice.
check-viz-theme:
	python3 scripts/check-viz-theme.py

test-contracts:
	cargo test --workspace --test shared_http_deployment_spec --test expert_system_rules_spec --test expert_system_governance_spec --test tool_catalog_contract

test-live:
	./scripts/start-test-cluster.sh
	cargo test --workspace -- --ignored

test-integration:
	$(PYTEST) tests/integration -v

test-system:
	$(PYTEST) tests/system -v

test-property:
	$(PYTEST) tests/property -v

test-security:
	$(PYTEST) tests/system/test_shared_http_workbench.py -v -k "t_s_007 or t_s_008"

test-load-smoke:
	locust -f tests/load/locustfile.py --headless --config tests/load/profiles/smoke.json

test-load:
	locust -f tests/load/locustfile.py --headless --config tests/load/profiles/load.json

test-duration:
	cd tests/duration && $(PYTHON) run_duration_test.py --config config.yaml

test-baseline:
	cd tests/duration && $(PYTHON) run_duration_test.py --baseline --config config.yaml

test-all: test-unit test-contracts test-integration test-system test-property test-security

test-coverage-gap:
	$(PYTHON) scripts/coverage_gap.py specs/test-specification.md crates/ferrosa-memory-core/tests tests
