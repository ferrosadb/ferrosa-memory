//! Prometheus metrics for the Ferrosa Memory MCP system.
//!
//! All memory operations emit metrics consumable by Prometheus at `/metrics`.
//! Metrics are registered once at startup via [`MemoryMetrics::new`] and shared
//! across all tool handlers via `Arc<MemoryMetrics>`.
//!
//! ## Key metrics
//!
//! - `ferrosa_memory_memo_hits_total` / `memo_misses_total` — cache efficiency
//! - `ferrosa_memory_retrieval_latency_ms` — latency by strategy
//! - `ferrosa_memory_fold_token_count` / `fold_compression_ratio` — fold sizing
//! - `ferrosa_memory_routing_strategy_total` — strategy selection distribution
//! - `ferrosa_memory_poisoning_flags_total` — anomaly detection triggers
//! - `ferrosa_memory_entity_upserts_total` — new vs matched entities

use prometheus::{CounterVec, HistogramVec, Opts, Registry, histogram_opts};

/// All Ferrosa Memory metrics, registered against a Prometheus [`Registry`].
pub struct MemoryMetrics {
    pub registry: Registry,
    pub memo_hits: CounterVec,
    pub memo_misses: CounterVec,
    pub retrieval_latency_ms: HistogramVec,
    pub fold_token_count: HistogramVec,
    pub fold_compression_ratio: HistogramVec,
    pub routing_strategy: CounterVec,
    pub poisoning_flags: CounterVec,
    pub entity_upserts: CounterVec,
    pub consolidation_runs: CounterVec,
    pub consolidation_lease_takeovers: CounterVec,
}

impl MemoryMetrics {
    /// Create and register all metrics. Call once at startup.
    ///
    /// # Errors
    ///
    /// Returns an error if metric registration fails (duplicate names).
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let memo_hits = CounterVec::new(
            Opts::new("ferrosa_memory_memo_hits_total", "Cache hits"),
            &["model_version", "tenant_id"],
        )?;

        let memo_misses = CounterVec::new(
            Opts::new("ferrosa_memory_memo_misses_total", "Cache misses"),
            &["model_version", "tenant_id"],
        )?;

        let retrieval_latency_ms = HistogramVec::new(
            histogram_opts!(
                "ferrosa_memory_retrieval_latency_ms",
                "Retrieval latency by strategy",
                vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]
            ),
            &["strategy"],
        )?;

        let fold_token_count = HistogramVec::new(
            histogram_opts!(
                "ferrosa_memory_fold_token_count",
                "Token count per fold at completion",
                vec![64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0]
            ),
            &["tenant_id"],
        )?;

        let fold_compression_ratio = HistogramVec::new(
            histogram_opts!(
                "ferrosa_memory_fold_compression_ratio",
                "Compression ratio achieved",
                vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]
            ),
            &["tenant_id"],
        )?;

        let routing_strategy = CounterVec::new(
            Opts::new(
                "ferrosa_memory_routing_strategy_total",
                "Strategy selected by router",
            ),
            &["strategy", "task_complexity"],
        )?;

        let poisoning_flags = CounterVec::new(
            Opts::new(
                "ferrosa_memory_poisoning_flags_total",
                "Anomaly detection triggers",
            ),
            &["tenant_id"],
        )?;

        let entity_upserts = CounterVec::new(
            Opts::new(
                "ferrosa_memory_entity_upserts_total",
                "Entity upserts (new vs matched)",
            ),
            &["tenant_id", "result"],
        )?;

        let consolidation_runs = CounterVec::new(
            Opts::new(
                "ferrosa_memory_consolidation_runs_total",
                "Consolidation runs completed",
            ),
            &["tenant_id", "status"],
        )?;

        let consolidation_lease_takeovers = CounterVec::new(
            Opts::new(
                "ferrosa_memory_consolidation_lease_takeovers_total",
                "Times a replica took over a stale lease from a dead peer",
            ),
            &["tenant_id"],
        )?;

        registry.register(Box::new(memo_hits.clone()))?;
        registry.register(Box::new(memo_misses.clone()))?;
        registry.register(Box::new(retrieval_latency_ms.clone()))?;
        registry.register(Box::new(fold_token_count.clone()))?;
        registry.register(Box::new(fold_compression_ratio.clone()))?;
        registry.register(Box::new(routing_strategy.clone()))?;
        registry.register(Box::new(poisoning_flags.clone()))?;
        registry.register(Box::new(entity_upserts.clone()))?;
        registry.register(Box::new(consolidation_runs.clone()))?;
        registry.register(Box::new(consolidation_lease_takeovers.clone()))?;

        Ok(Self {
            registry,
            memo_hits,
            memo_misses,
            retrieval_latency_ms,
            fold_token_count,
            fold_compression_ratio,
            routing_strategy,
            poisoning_flags,
            entity_upserts,
            consolidation_runs,
            consolidation_lease_takeovers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Encoder;

    #[test]
    fn metrics_register_without_conflict() {
        let m = MemoryMetrics::new().expect("metrics should register");
        // Increment a counter and verify it appears in output
        m.memo_hits.with_label_values(&["gpt-5", "tenant-1"]).inc();
        m.memo_misses
            .with_label_values(&["gpt-5", "tenant-1"])
            .inc();
        m.retrieval_latency_ms
            .with_label_values(&["hnsw_ann"])
            .observe(42.0);

        let mut buf = Vec::new();
        let encoder = prometheus::TextEncoder::new();
        encoder.encode(&m.registry.gather(), &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("ferrosa_memory_memo_hits_total"));
        assert!(output.contains("ferrosa_memory_memo_misses_total"));
        assert!(output.contains("ferrosa_memory_retrieval_latency_ms"));
    }

    #[test]
    fn all_eight_metrics_registered() {
        let m = MemoryMetrics::new().expect("metrics should register");

        // Touch each metric to make it observable via gather()
        m.memo_hits.with_label_values(&["m", "t"]).inc();
        m.memo_misses.with_label_values(&["m", "t"]).inc();
        m.retrieval_latency_ms
            .with_label_values(&["ann"])
            .observe(1.0);
        m.fold_token_count.with_label_values(&["t"]).observe(1.0);
        m.fold_compression_ratio
            .with_label_values(&["t"])
            .observe(0.5);
        m.routing_strategy
            .with_label_values(&["ann", "simple"])
            .inc();
        m.poisoning_flags.with_label_values(&["t"]).inc();
        m.entity_upserts.with_label_values(&["t", "new"]).inc();

        let families = m.registry.gather();
        assert_eq!(
            families.len(),
            8,
            "expected 8 metric families, got {}",
            families.len()
        );
    }

    /// Behavioral coverage for the `consolidation_runs` metric.
    ///
    /// This counter (emitted by `consolidation_worker_loop` in
    /// `ferrosa-memory-mcp`) had no test — `all_eight_metrics_registered`
    /// touches only the original eight families. The worker increments it with
    /// an owned `String` tenant id (`ctx.tenant_id.to_string()`) and a `&str`
    /// status literal; this test exercises that exact mixed-source call and
    /// asserts both the `tenant_id` and `status` labels are emitted with the
    /// right values.
    ///
    /// Note: PR #134 normalized the call to `tenant_label.as_str()`, but the
    /// prior `&[&tenant_label, "success"]` form also compiles — array-literal
    /// LUB coercion unifies `&String` and `&str` into `&str`, which satisfies
    /// `with_label_values<V: AsRef<str>>`. So this is a coverage/behavior test,
    /// not a compile guard; the `.as_str()` change is a readability tweak.
    #[test]
    fn consolidation_runs_accepts_owned_tenant_label() {
        let m = MemoryMetrics::new().expect("metrics should register");

        // Owned String tenant id mixed with &str status literals, exactly as
        // the consolidation worker does.
        let tenant_label: String = "tenant-42".to_string();
        m.consolidation_runs
            .with_label_values(&[tenant_label.as_str(), "success"])
            .inc();
        m.consolidation_runs
            .with_label_values(&[tenant_label.as_str(), "failed"])
            .inc();

        let mut buf = Vec::new();
        let encoder = prometheus::TextEncoder::new();
        encoder.encode(&m.registry.gather(), &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(
            output.contains("ferrosa_memory_consolidation_runs_total"),
            "consolidation_runs metric missing from output:\n{output}"
        );
        assert!(
            output.contains("tenant_id=\"tenant-42\""),
            "expected tenant label in output:\n{output}"
        );
        assert!(
            output.contains("status=\"success\""),
            "expected success status label:\n{output}"
        );
        assert!(
            output.contains("status=\"failed\""),
            "expected failed status label:\n{output}"
        );
    }
}
