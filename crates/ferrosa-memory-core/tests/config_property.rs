//! Property/fuzz tests for config parsing + validation — Tier A (hermetic).
//!
//! ferrosa-memory PoC for the multi-repo config-robustness initiative. Generates
//! config TOML across the FULL value domain for the startup-critical sections
//! (`[server]`, `[ferrosa]`, `[viz]`) and asserts the **no-panic invariant**:
//! `parse_config` and the validators must never panic, abort, or hang on any
//! legal value — a clean `Err` (fail-loud) is a PASS, only a panic is a failure.
//!
//! Tier B (boot the real `ferrosa-memory-mcp` process per config against a
//! throwaway cluster) is cluster-gated and lives with the integration tests.
//!
//! The config structs are `Deserialize`-only, so we generate the config FILE
//! text directly (truer to "fuzz the config files") by building a `toml::Table`
//! of generated values and serializing it — no hand-rolled TOML escaping.
//!
//! NOTE: the per-section strategies are kept to ≤10 generated values each and
//! composed as a small top-level tuple. proptest builds one big tuple per
//! `proptest!` block, and a very wide flat tuple (≈28 values) overflows the
//! debug-build stack *during generation* — composing in groups avoids that.

use ferrosa_memory_core::config::{
    parse_config, validate_shared_http_config, validate_tenant_connection_path,
};
use proptest::prelude::*;
use toml::Value;

/// Strings spanning the legal value domain: a varied charset plus specific edge
/// cases that historically trip config code — empty, whitespace, valid + invalid
/// UUIDs, host:port, transport keywords, and an embedded newline.
fn arb_str() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => "[a-zA-Z0-9:._/@+-]{0,48}",
        1 => Just(String::new()),
        1 => Just("   ".to_string()),
        1 => Just("127.0.0.1:19042".to_string()),
        1 => Just("0.0.0.0".to_string()),
        1 => Just("stdio".to_string()),
        1 => Just("http".to_string()),
        1 => Just("00000000-0000-0000-0000-000000000001".to_string()),
        1 => Just("not-a-uuid".to_string()),
        1 => Just("ferrosa\nbroken".to_string()),
    ]
}

fn opt_str() -> impl Strategy<Value = Option<String>> {
    prop::option::of(arb_str())
}

/// Insert a string key only when present — mirrors a config file that omits the
/// key (falling back to the serde default) vs sets it.
fn put_opt(t: &mut toml::Table, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        t.insert(key.to_string(), Value::String(s.clone()));
    }
}

prop_compose! {
    /// Full-domain `[server]` section — the HTTP/TLS/tenant area where the
    /// recent startup bugs lived.
    fn server_section()(
        transport in arb_str(),
        bind_addr in arb_str(),
        http_port in any::<u16>(),
        public_port in prop::option::of(any::<u16>()),
        require_tls in any::<bool>(),
        cert_path in opt_str(),
        key_path in opt_str(),
        auth_file in opt_str(),
        tenant_id in opt_str(),
        edge_decay in any::<f64>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("transport".into(), Value::String(transport));
        t.insert("bind_addr".into(), Value::String(bind_addr));
        t.insert("http_port".into(), Value::Integer(i64::from(http_port)));
        if let Some(p) = public_port {
            t.insert("public_port".into(), Value::Integer(i64::from(p)));
        }
        t.insert("require_tls".into(), Value::Boolean(require_tls));
        put_opt(&mut t, "cert_path", &cert_path);
        put_opt(&mut t, "key_path", &key_path);
        put_opt(&mut t, "auth_file", &auth_file);
        put_opt(&mut t, "tenant_id", &tenant_id);
        t.insert("edge_decay_factor".into(), Value::Float(edge_decay));
        t
    }
}

prop_compose! {
    /// Full-domain `[ferrosa]` section (the required CQL connection block).
    fn ferrosa_section()(
        contact_points in prop::collection::vec(arb_str(), 0..4),
        keyspace in arb_str(),
        rf in any::<u8>(),
        consistency in arb_str(),
        username in opt_str(),
        password in opt_str(),
        admin_username in opt_str(),
        admin_password in opt_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "contact_points".into(),
            Value::Array(contact_points.into_iter().map(Value::String).collect()),
        );
        t.insert("keyspace".into(), Value::String(keyspace));
        t.insert("replication_factor".into(), Value::Integer(i64::from(rf)));
        t.insert("consistency".into(), Value::String(consistency));
        put_opt(&mut t, "username", &username);
        put_opt(&mut t, "password", &password);
        put_opt(&mut t, "admin_username", &admin_username);
        put_opt(&mut t, "admin_password", &admin_password);
        t
    }
}

prop_compose! {
    /// Full-domain `[viz]` section (tenant required under HTTP — recent bug).
    fn viz_section()(
        enabled in any::<bool>(),
        tenant_id in opt_str(),
        port in any::<u16>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("enabled".into(), Value::Boolean(enabled));
        put_opt(&mut t, "tenant_id", &tenant_id);
        t.insert("port".into(), Value::Integer(i64::from(port)));
        t
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// No generated config — however unusual — may panic `parse_config` or the
    /// validators. A clean Ok or Err both pass; a panic shrinks to a minimal
    /// reproducing config.
    #[test]
    fn config_parse_and_validate_never_panic(
        server in server_section(),
        ferrosa in ferrosa_section(),
        viz in viz_section(),
    ) {
        let mut root = toml::Table::new();
        root.insert("server".into(), Value::Table(server));
        root.insert("ferrosa".into(), Value::Table(ferrosa));
        root.insert("viz".into(), Value::Table(viz));

        let toml_str =
            toml::to_string(&Value::Table(root)).expect("generated table must serialize to TOML");

        // The invariant under test: none of these may panic on any legal value.
        if let Ok(cfg) = parse_config(&toml_str) {
            let _ = validate_shared_http_config(&cfg);
            let _ = validate_tenant_connection_path(&cfg);
        }
    }
}
