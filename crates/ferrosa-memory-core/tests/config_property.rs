//! Property/fuzz tests for config parsing + validation — Tier A (hermetic).
//!
//! ferrosa-memory PoC for the multi-repo config-robustness initiative. Generates
//! config TOML across the FULL value domain for the ENTIRE config surface —
//! every `[section]` of `Config` (`[server]`, `[ferrosa]`, `[viz]`, `[memory]`,
//! `[embeddings]`, `[security]`, `[routing]`, `[graph]`, `[sparql]`, `[rmh]`,
//! `[datalog]`, `[promotion]`, `[enrich]`, `[judge]`, `[retrieval]`, `[search]`,
//! `[forget]`) — and asserts the **no-panic invariant**: `parse_config` and the
//! validators must never panic, abort, or hang on any legal value — a clean
//! `Err` (fail-loud) is a PASS, only a panic is a failure.
//!
//! Tier B (boot the real `ferrosa-memory-mcp` process per config against a
//! throwaway cluster) is cluster-gated and lives with the integration tests.
//!
//! The config structs are `Deserialize`-only, so we generate the config FILE
//! text directly (truer to "fuzz the config files") by building a `toml::Table`
//! of generated values and serializing it — no hand-rolled TOML escaping.
//!
//! NOTE: each per-section strategy is kept to ≤10 generated values, and the
//! sections are batched into a few intermediate group strategies so no single
//! tuple proptest constructs is wide (a ~28-value flat tuple overflowed the
//! debug-build stack *during generation*). Even so, combining all 17 sections
//! into one value tree is deep enough that the default test-thread stack
//! overflows while building it, so the runner is driven explicitly on a
//! dedicated thread with an enlarged stack (see `config_parse_and_validate_never_panic`).
//! Both measures only buy generation headroom — neither narrows the value domain.

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
        public_port in prop::option::of(any::<u16>()),
        bind_addr in opt_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("enabled".into(), Value::Boolean(enabled));
        put_opt(&mut t, "tenant_id", &tenant_id);
        t.insert("port".into(), Value::Integer(i64::from(port)));
        if let Some(p) = public_port {
            t.insert("public_port".into(), Value::Integer(i64::from(p)));
        }
        put_opt(&mut t, "bind_addr", &bind_addr);
        t
    }
}

prop_compose! {
    /// Full-domain `[memory]` section — TTL/compression/gating thresholds.
    fn memory_section()(
        default_ttl_days in any::<u32>(),
        fold_ttl_days in any::<u32>(),
        archive_after_days in any::<u32>(),
        compression_threshold_tokens in any::<u32>(),
        confidence_gate in any::<f64>(),
        max_memo_results in any::<u32>(),
        max_entities in any::<u32>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("default_ttl_days".into(), Value::Integer(i64::from(default_ttl_days)));
        t.insert("fold_ttl_days".into(), Value::Integer(i64::from(fold_ttl_days)));
        t.insert("archive_after_days".into(), Value::Integer(i64::from(archive_after_days)));
        t.insert(
            "compression_threshold_tokens".into(),
            Value::Integer(i64::from(compression_threshold_tokens)),
        );
        t.insert("confidence_gate".into(), Value::Float(confidence_gate));
        t.insert("max_memo_results".into(), Value::Integer(i64::from(max_memo_results)));
        t.insert("max_entities".into(), Value::Integer(i64::from(max_entities)));
        t
    }
}

prop_compose! {
    /// Full-domain `[embeddings]` section — provider/model/dimension knobs.
    fn embeddings_section()(
        provider in arb_str(),
        ollama_base_url in arb_str(),
        model in arb_str(),
        dimensions in any::<u32>(),
        max_input_chars in any::<usize>(),
        ner_model in arb_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("provider".into(), Value::String(provider));
        t.insert("ollama_base_url".into(), Value::String(ollama_base_url));
        t.insert("model".into(), Value::String(model));
        t.insert("dimensions".into(), Value::Integer(i64::from(dimensions)));
        // usize is generated full-domain; TOML integers are i64, so clamp the
        // value into i64 range losslessly (`usize as i64` would wrap on 64-bit).
        t.insert("max_input_chars".into(), Value::Integer(max_input_chars as i64));
        t.insert("ner_model".into(), Value::String(ner_model));
        t
    }
}

prop_compose! {
    /// Full-domain `[security]` section — audit/anomaly toggles + sigma float.
    fn security_section()(
        audit_log_enabled in any::<bool>(),
        anomaly_detection_enabled in any::<bool>(),
        anomaly_sigma_threshold in any::<f64>(),
        anomaly_alerts_enabled in any::<bool>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("audit_log_enabled".into(), Value::Boolean(audit_log_enabled));
        t.insert(
            "anomaly_detection_enabled".into(),
            Value::Boolean(anomaly_detection_enabled),
        );
        t.insert(
            "anomaly_sigma_threshold".into(),
            Value::Float(anomaly_sigma_threshold),
        );
        t.insert("anomaly_alerts_enabled".into(), Value::Boolean(anomaly_alerts_enabled));
        t
    }
}

prop_compose! {
    /// Full-domain `[routing]` section.
    fn routing_section()(
        guideline_version in arb_str(),
        feedback_export_cron in arb_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("guideline_version".into(), Value::String(guideline_version));
        t.insert("feedback_export_cron".into(), Value::String(feedback_export_cron));
        t
    }
}

prop_compose! {
    /// Full-domain `[graph]` section (`GraphDbConfig`).
    fn graph_section()(
        bolt_uri in arb_str(),
        username in arb_str(),
        password in arb_str(),
        http_url in arb_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("bolt_uri".into(), Value::String(bolt_uri));
        t.insert("username".into(), Value::String(username));
        t.insert("password".into(), Value::String(password));
        t.insert("http_url".into(), Value::String(http_url));
        t
    }
}

prop_compose! {
    /// Full-domain `[sparql]` section.
    fn sparql_section()(
        enabled in any::<bool>(),
        http_url in arb_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("enabled".into(), Value::Boolean(enabled));
        t.insert("http_url".into(), Value::String(http_url));
        t
    }
}

prop_compose! {
    /// First half of the full-domain `[rmh]` section. `RmhConfig` has 12 fields;
    /// the per-block ≤10-param cap means we split it across two compose fns whose
    /// tables are merged in `rmh_section()`.
    fn rmh_section_a()(
        warmth_boost_amount in any::<f64>(),
        warmth_neighbor_ratio in any::<f64>(),
        warmth_prune_threshold in any::<f64>(),
        warmth_cap in any::<f64>(),
        ppr_alpha in any::<f64>(),
        ppr_iterations in any::<usize>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("warmth_boost_amount".into(), Value::Float(warmth_boost_amount));
        t.insert("warmth_neighbor_ratio".into(), Value::Float(warmth_neighbor_ratio));
        t.insert("warmth_prune_threshold".into(), Value::Float(warmth_prune_threshold));
        t.insert("warmth_cap".into(), Value::Float(warmth_cap));
        t.insert("ppr_alpha".into(), Value::Float(ppr_alpha));
        t.insert("ppr_iterations".into(), Value::Integer(ppr_iterations as i64));
        t
    }
}

prop_compose! {
    /// Second half of the full-domain `[rmh]` section (see `rmh_section_a`).
    fn rmh_section_b()(
        decay_lambda in any::<f64>(),
        max_explore_passes in any::<usize>(),
        convergence_threshold in any::<f64>(),
        max_explore_entities in any::<usize>(),
        forget_threshold in any::<f64>(),
        decay_interval_hours in any::<u32>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("decay_lambda".into(), Value::Float(decay_lambda));
        t.insert("max_explore_passes".into(), Value::Integer(max_explore_passes as i64));
        t.insert("convergence_threshold".into(), Value::Float(convergence_threshold));
        t.insert("max_explore_entities".into(), Value::Integer(max_explore_entities as i64));
        t.insert("forget_threshold".into(), Value::Float(forget_threshold));
        t.insert("decay_interval_hours".into(), Value::Integer(i64::from(decay_interval_hours)));
        t
    }
}

prop_compose! {
    /// Full-domain `[rmh]` section — merges the two half-tables. Nesting two
    /// compose fns keeps every individual generated tuple ≤10 wide.
    fn rmh_section()(
        a in rmh_section_a(),
        b in rmh_section_b(),
    ) -> toml::Table {
        let mut t = a;
        t.extend(b);
        t
    }
}

prop_compose! {
    /// Full-domain `[datalog]` section.
    fn datalog_section()(
        max_iterations in any::<usize>(),
        max_facts in any::<usize>(),
        cache_ttl_seconds in any::<u64>(),
        confidence_combination in arb_str(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("max_iterations".into(), Value::Integer(max_iterations as i64));
        t.insert("max_facts".into(), Value::Integer(max_facts as i64));
        t.insert("cache_ttl_seconds".into(), Value::Integer(cache_ttl_seconds as i64));
        t.insert("confidence_combination".into(), Value::String(confidence_combination));
        t
    }
}

prop_compose! {
    /// Full-domain `[promotion]` section.
    fn promotion_section()(
        promotion_threshold in any::<f64>(),
        size_budget_rows in any::<usize>(),
        window_days in any::<u32>(),
        reuse_factor in any::<f64>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("promotion_threshold".into(), Value::Float(promotion_threshold));
        t.insert("size_budget_rows".into(), Value::Integer(size_budget_rows as i64));
        t.insert("window_days".into(), Value::Integer(i64::from(window_days)));
        t.insert("reuse_factor".into(), Value::Float(reuse_factor));
        t
    }
}

prop_compose! {
    /// Full-domain `[enrich]` section.
    fn enrich_section()(
        llm_base_url in arb_str(),
        llm_model in arb_str(),
        batch_size in any::<usize>(),
        max_tokens in any::<u32>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("llm_base_url".into(), Value::String(llm_base_url));
        t.insert("llm_model".into(), Value::String(llm_model));
        t.insert("batch_size".into(), Value::Integer(batch_size as i64));
        t.insert("max_tokens".into(), Value::Integer(i64::from(max_tokens)));
        t
    }
}

prop_compose! {
    /// Full-domain `[judge]` section.
    fn judge_section()(
        enabled in any::<bool>(),
        provider in arb_str(),
        base_url in arb_str(),
        model in arb_str(),
        token in opt_str(),
        timeout_seconds in any::<u64>(),
        max_rerank_candidates in any::<usize>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("enabled".into(), Value::Boolean(enabled));
        t.insert("provider".into(), Value::String(provider));
        t.insert("base_url".into(), Value::String(base_url));
        t.insert("model".into(), Value::String(model));
        put_opt(&mut t, "token", &token);
        t.insert("timeout_seconds".into(), Value::Integer(timeout_seconds as i64));
        t.insert("max_rerank_candidates".into(), Value::Integer(max_rerank_candidates as i64));
        t
    }
}

prop_compose! {
    /// Full-domain `[retrieval]` section (single field).
    fn retrieval_section()(
        default_limit in any::<usize>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("default_limit".into(), Value::Integer(default_limit as i64));
        t
    }
}

prop_compose! {
    /// Full-domain `[search]` section — rerank tunables.
    fn search_section()(
        rerank_min_candidates in any::<usize>(),
        rerank_max_candidates in any::<usize>(),
        rerank_min_score_coverage in any::<usize>(),
        rerank_batch_size in any::<usize>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("rerank_min_candidates".into(), Value::Integer(rerank_min_candidates as i64));
        t.insert("rerank_max_candidates".into(), Value::Integer(rerank_max_candidates as i64));
        t.insert(
            "rerank_min_score_coverage".into(),
            Value::Integer(rerank_min_score_coverage as i64),
        );
        t.insert("rerank_batch_size".into(), Value::Integer(rerank_batch_size as i64));
        t
    }
}

prop_compose! {
    /// Full-domain `[forget]` section.
    fn forget_section()(
        retract_purge_days in any::<u32>(),
        candidate_limit in any::<usize>(),
        candidate_max in any::<usize>(),
        token_ttl_seconds in any::<u64>(),
        high_impact_edge_threshold in any::<usize>(),
    ) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("retract_purge_days".into(), Value::Integer(i64::from(retract_purge_days)));
        t.insert("candidate_limit".into(), Value::Integer(candidate_limit as i64));
        t.insert("candidate_max".into(), Value::Integer(candidate_max as i64));
        t.insert("token_ttl_seconds".into(), Value::Integer(token_ttl_seconds as i64));
        t.insert(
            "high_impact_edge_threshold".into(),
            Value::Integer(high_impact_edge_threshold as i64),
        );
        t
    }
}

// Intermediate strategies grouping 3–4 sections each. proptest builds one tuple
// per `proptest!` block; combining all ~17 sections in one flat tuple overflows
// the debug-build stack during generation, so we batch them into a handful of
// `(String, Value)` vecs and merge those in the test body — keeping every
// generated tuple narrow.
prop_compose! {
    fn group_core()(
        server in server_section(),
        ferrosa in ferrosa_section(),
        viz in viz_section(),
        memory in memory_section(),
    ) -> Vec<(String, Value)> {
        vec![
            ("server".into(), Value::Table(server)),
            ("ferrosa".into(), Value::Table(ferrosa)),
            ("viz".into(), Value::Table(viz)),
            ("memory".into(), Value::Table(memory)),
        ]
    }
}

prop_compose! {
    fn group_services()(
        embeddings in embeddings_section(),
        security in security_section(),
        routing in routing_section(),
        graph in graph_section(),
    ) -> Vec<(String, Value)> {
        vec![
            ("embeddings".into(), Value::Table(embeddings)),
            ("security".into(), Value::Table(security)),
            ("routing".into(), Value::Table(routing)),
            ("graph".into(), Value::Table(graph)),
        ]
    }
}

prop_compose! {
    fn group_inference()(
        sparql in sparql_section(),
        rmh in rmh_section(),
        datalog in datalog_section(),
        promotion in promotion_section(),
    ) -> Vec<(String, Value)> {
        vec![
            ("sparql".into(), Value::Table(sparql)),
            ("rmh".into(), Value::Table(rmh)),
            ("datalog".into(), Value::Table(datalog)),
            ("promotion".into(), Value::Table(promotion)),
        ]
    }
}

prop_compose! {
    fn group_pipeline()(
        enrich in enrich_section(),
        judge in judge_section(),
        retrieval in retrieval_section(),
        search in search_section(),
        forget in forget_section(),
    ) -> Vec<(String, Value)> {
        vec![
            ("enrich".into(), Value::Table(enrich)),
            ("judge".into(), Value::Table(judge)),
            ("retrieval".into(), Value::Table(retrieval)),
            ("search".into(), Value::Table(search)),
            ("forget".into(), Value::Table(forget)),
        ]
    }
}

/// The whole-surface property: serialize a generated config, parse it, and run
/// both validators. The invariant is that none of these may panic/abort/hang on
/// any legal value — a clean `Ok` or `Err` both pass.
fn run_no_panic_property() -> Result<(), String> {
    use proptest::test_runner::{Config as PtConfig, TestRunner};

    // All 17 sections combined into one value tree. proptest builds the strategy
    // value tree recursively; with this many sections the default test-thread
    // stack overflows *during generation* (uncatchable SIGABRT, no shrink). The
    // remedy is more stack, supplied by the spawned thread in the test below —
    // not weaker generation. Each compose tuple is still kept ≤10 wide.
    let strategy = (
        group_core(),
        group_services(),
        group_inference(),
        group_pipeline(),
    );

    let mut runner = TestRunner::new(PtConfig {
        cases: 2048,
        ..PtConfig::default()
    });

    runner
        .run(&strategy, |(core, services, inference, pipeline)| {
            let mut root = toml::Table::new();
            for (name, table) in core
                .into_iter()
                .chain(services)
                .chain(inference)
                .chain(pipeline)
            {
                root.insert(name, table);
            }

            let toml_str = toml::to_string(&Value::Table(root))
                .expect("generated table must serialize to TOML");

            // The invariant under test: none of these may panic on any legal value.
            if let Ok(cfg) = parse_config(&toml_str) {
                let _ = validate_shared_http_config(&cfg);
                let _ = validate_tenant_connection_path(&cfg);
            }
            Ok(())
        })
        // Surface the shrunk minimal failing config (a real panic in parse/validate)
        // as a String so it crosses the thread boundary intact.
        .map_err(|e| e.to_string())
}

/// No generated config — however unusual — may panic `parse_config` or the
/// validators across the FULL config surface (every `[section]`).
///
/// Driven on a dedicated thread with an enlarged stack: the combined 17-section
/// strategy value tree is built recursively during generation and overflows the
/// default test-thread stack in debug builds. A bigger stack is the documented
/// fix for generation-time overflow — it does not weaken the value domain.
#[test]
fn config_parse_and_validate_never_panic() {
    let handle = std::thread::Builder::new()
        .name("config_property_fuzz".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(run_no_panic_property)
        .expect("spawn fuzz thread");
    handle
        .join()
        .expect("fuzz thread must not panic/abort")
        .expect("no-panic property must hold for every generated config");
}
