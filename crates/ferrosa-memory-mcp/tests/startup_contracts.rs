#[test]
fn https_mode_installs_rustls_crypto_provider_before_config_load() {
    let main_rs = include_str!("../src/main.rs");
    let install = "tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default()";
    let install_pos = main_rs
        .find(install)
        .expect("MCP startup must install a rustls aws-lc crypto provider explicitly");
    let config_pos = main_rs
        .find("ferrosa_memory_core::config::load_config()")
        .expect("startup must load config");
    assert!(
        install_pos < config_pos,
        "rustls crypto provider must be installed before config/HTTP TLS setup can touch rustls"
    );

    let cargo_toml = include_str!("../Cargo.toml");
    assert!(
        cargo_toml.contains("tokio-rustls = { workspace = true }")
            || cargo_toml.contains("tokio-rustls.workspace = true"),
        "ferrosa-memory-mcp must depend directly on tokio-rustls so startup can install the provider"
    );
}

#[test]
fn reconnect_attempts_run_migrations_before_preparing_runtime_storage() {
    let main_rs = include_str!("../src/main.rs");
    let watcher_pos = main_rs
        .find("async fn cql_reconnect_watcher")
        .expect("reconnect watcher exists");
    let watcher = &main_rs[watcher_pos..];
    let migrations_pos = watcher
        .find("run_startup_migrations")
        .expect("reconnect watcher must run migrations before CqlStorage::connect");
    let connect_pos = watcher
        .find("CqlStorage::connect(&storage.cql_config)")
        .expect("reconnect watcher must eventually connect runtime storage");
    assert!(
        migrations_pos < connect_pos,
        "greenfield reconnect loop must apply migrations before runtime PREPARE"
    );
}

fn repo_file(path: &str) -> String {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate lives under crates/ferrosa-memory-mcp");
    std::fs::read_to_string(repo_root.join(path)).expect(path)
}

#[test]
fn warmth_reputation_column_is_in_fresh_ddl_and_registered_migration() {
    let migration_rs = repo_file("crates/ferrosa-memory-core/src/migration.rs");
    assert!(
        migration_rs.contains("version: 25")
            && migration_rs.contains("ddl/025_warmth_reputation.cql"),
        "schema migration registry must include the entity_warmth reputation backfill"
    );

    let base_ddl = repo_file("ddl/011_warmth_field.cql");
    assert!(
        base_ddl.contains("reputation double"),
        "fresh entity_warmth DDL must include the reputation column used by prepared statements"
    );

    let backfill_ddl = repo_file("ddl/025_warmth_reputation.cql");
    assert!(
        backfill_ddl.contains("ALTER TABLE agent_memory.entity_warmth ADD reputation double"),
        "existing clusters need an additive migration for entity_warmth.reputation"
    );
}

#[test]
fn containerfile_copy_target_has_a_documented_make_builder() {
    let containerfile = repo_file("Containerfile");
    assert!(
        containerfile.contains("COPY target-podman-linux/release/ferrosa-memory-mcp"),
        "container build context must use the documented podman-linux target directory"
    );

    let makefile = repo_file("Makefile");
    assert!(
        makefile.contains("build-podman-binary:"),
        "Makefile must expose the build command required before docker compose build"
    );
    assert!(
        makefile.contains("--target-dir target-podman-linux")
            && makefile.contains("-p ferrosa-memory-mcp"),
        "build-podman-binary must build ferrosa-memory-mcp into target-podman-linux"
    );
}

#[test]
fn runtime_init_script_generates_the_compose_mcp_config_contract() {
    let compose = repo_file("docker-compose.yml");
    assert!(
        compose.contains("network_mode: host"),
        "compose MCP service must keep host-network semantics if docs use loopback HTTP smoke tests"
    );
    assert!(
        compose.contains(
            "FERROSA_MEMORY_CONFIG: /run/secrets/ferrosa-memory/ferrosa-memory-http-podman.toml"
        ),
        "compose and runtime generator must agree on config filename"
    );

    let script = repo_file("scripts/init-runtime.sh");
    for required in [
        "ferrosa-memory-http-podman.toml",
        "http-auth.toml",
        "bind_addr = \"127.0.0.1\"",
        "auth_file = \"/run/secrets/ferrosa-memory/http-auth.toml\"",
        "contact_points = [\"127.0.0.1:19042\", \"127.0.0.1:19043\", \"127.0.0.1:19044\"]",
        "http_url = \"http://127.0.0.1:17474\"",
        "bolt_uri = \"bolt://127.0.0.1:17687\"",
        "enabled = false",
    ] {
        assert!(
            script.contains(required),
            "scripts/init-runtime.sh must generate compose-compatible runtime config containing {required:?}"
        );
    }
}

#[test]
fn onboarding_artifact_exists_at_public_raw_url_target() {
    let onboarding = repo_file("ONBOARDING.md");
    assert!(
        onboarding.contains("# Ferrosa + Ferrosa Memory Onboarding Harness"),
        "setup-memory.sh raw URL must resolve to an LLM-readable onboarding artifact"
    );
    assert!(
        onboarding.contains("https://github.com/ferrosadb/ferrosa-memory.git")
            && onboarding.contains("https://github.com/ferrosadb/ferrosa.git"),
        "onboarding artifact must point users at canonical public ferrosadb repositories"
    );
    assert!(
        !onboarding.contains("github.com/bkearns/ferrosa"),
        "public onboarding must not reference the old personal GitHub org"
    );
}
