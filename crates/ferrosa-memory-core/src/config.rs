//! Configuration parsing for `ferrosa-memory.toml`.
//!
//! The config file is located alongside the binary, or at a path specified
//! by the `FERROSA_MEMORY_CONFIG` environment variable. All fields have
//! sensible defaults so a minimal config is just `[ferrosa]` with contact points.
//!
//! ## Sections
//!
//! - `[server]` — transport mode, HTTP port, log level
//! - `[ferrosa]` — CQL connection parameters
//! - `[memory]` — TTL, compression, and gating thresholds
//! - `[embeddings]` — embedding provider configuration
//! - `[security]` — audit and anomaly detection settings
//! - `[routing]` — guideline version and batch schedule
//! - `[rmh]` — Resonant Memory Hierarchy warmth and exploration parameters
//! - `[datalog]` — Datalog inference engine limits and caching

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub ferrosa: FerrosaCqlConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub embeddings: EmbeddingConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub graph: GraphDbConfig,
    #[serde(default)]
    pub sparql: SparqlConfig,
    #[serde(default)]
    pub viz: VizConfig,
    #[serde(default)]
    pub rmh: RmhConfig,
    #[serde(default)]
    pub datalog: DatalogConfig,
    #[serde(default)]
    pub promotion: PromotionConfig,
    #[serde(default)]
    pub enrich: EnrichConfig,
    #[serde(default)]
    pub judge: JudgeConfig,
}

#[derive(Debug, Deserialize)]
pub struct GraphDbConfig {
    #[serde(default = "default_bolt_uri")]
    pub bolt_uri: String,
    #[serde(default = "default_graph_user")]
    pub username: String,
    #[serde(default = "default_graph_pass")]
    pub password: String,
    #[serde(default = "default_http_graph_url")]
    pub http_url: String,
}

impl Default for GraphDbConfig {
    fn default() -> Self {
        Self {
            bolt_uri: default_bolt_uri(),
            username: default_graph_user(),
            password: default_graph_pass(),
            http_url: default_http_graph_url(),
        }
    }
}

/// Public SPARQL endpoint configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct SparqlConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sparql_url")]
    pub http_url: String,
}

impl Default for SparqlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_url: default_sparql_url(),
        }
    }
}

/// Visualization dashboard configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct VizConfig {
    #[serde(default = "default_viz_enabled")]
    pub enabled: bool,
    #[serde(default = "default_viz_port")]
    pub port: u16,
    /// Port to embed in rendered HTML for links pointing at viz (workbench → viz).
    /// Set when the viz listener sits behind a port mapping (e.g. podman 18766:8766).
    /// Defaults to `port` when unset.
    #[serde(default)]
    pub public_port: Option<u16>,
    /// Tenant UUID viz should read under when running in HTTP transport mode.
    ///
    /// Viz is unauthenticated (loopback-only), so the tenant cannot come from
    /// a request principal. In stdio mode this is unused — viz inherits the
    /// stdio tenant. In HTTP mode this is required if `enabled = true`.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Explicit bind address for the viz listener. When unset the runtime picks
    /// a safe default (0.0.0.0 under stdio, 127.0.0.1 under HTTP). Override to
    /// 0.0.0.0 only when the container/host port mapping already constrains
    /// exposure (e.g. podman forwarding host 127.0.0.1:X → container 0.0.0.0:Y).
    #[serde(default)]
    pub bind_addr: Option<String>,
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            enabled: default_viz_enabled(),
            port: default_viz_port(),
            public_port: None,
            tenant_id: None,
            bind_addr: None,
        }
    }
}

/// Resonant Memory Hierarchy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct RmhConfig {
    #[serde(default = "default_warmth_boost")]
    pub warmth_boost_amount: f64,
    #[serde(default = "default_neighbor_ratio")]
    pub warmth_neighbor_ratio: f64,
    #[serde(default = "default_prune_threshold")]
    pub warmth_prune_threshold: f64,
    #[serde(default = "default_warmth_cap")]
    pub warmth_cap: f64,
    #[serde(default = "default_ppr_alpha")]
    pub ppr_alpha: f64,
    #[serde(default = "default_ppr_iterations")]
    pub ppr_iterations: usize,
    #[serde(default = "default_decay_lambda")]
    pub decay_lambda: f64,
    #[serde(default = "default_max_passes")]
    pub max_explore_passes: usize,
    #[serde(default = "default_convergence")]
    pub convergence_threshold: f64,
    #[serde(default = "default_max_explore_entities")]
    pub max_explore_entities: usize,
    #[serde(default = "default_forget_threshold")]
    pub forget_threshold: f64,
    #[serde(default = "default_decay_interval_hours")]
    pub decay_interval_hours: u32,
}

impl Default for RmhConfig {
    fn default() -> Self {
        Self {
            warmth_boost_amount: default_warmth_boost(),
            warmth_neighbor_ratio: default_neighbor_ratio(),
            warmth_prune_threshold: default_prune_threshold(),
            warmth_cap: default_warmth_cap(),
            ppr_alpha: default_ppr_alpha(),
            ppr_iterations: default_ppr_iterations(),
            decay_lambda: default_decay_lambda(),
            max_explore_passes: default_max_passes(),
            convergence_threshold: default_convergence(),
            max_explore_entities: default_max_explore_entities(),
            forget_threshold: default_forget_threshold(),
            decay_interval_hours: default_decay_interval_hours(),
        }
    }
}

/// Datalog inference engine configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct DatalogConfig {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
    #[serde(default = "default_confidence_strategy")]
    pub confidence_combination: String,
}

impl Default for DatalogConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_facts: default_max_facts(),
            cache_ttl_seconds: default_cache_ttl(),
            confidence_combination: default_confidence_strategy(),
        }
    }
}

/// Promotion pipeline configuration (B10).
#[derive(Debug, Deserialize, Clone)]
pub struct PromotionConfig {
    /// Heat score threshold above which a predicate becomes a promotion candidate.
    #[serde(default = "default_promotion_threshold")]
    pub promotion_threshold: f64,
    /// Maximum total rows across all promoted predicates.
    #[serde(default = "default_size_budget")]
    pub size_budget_rows: usize,
    /// Number of days of heat data to consider.
    #[serde(default = "default_promotion_window_days")]
    pub window_days: u32,
    /// Multiplier applied to reuse benefit when scoring promotion candidates.
    #[serde(default = "default_reuse_factor")]
    pub reuse_factor: f64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            promotion_threshold: default_promotion_threshold(),
            size_budget_rows: default_size_budget(),
            window_days: default_promotion_window_days(),
            reuse_factor: default_reuse_factor(),
        }
    }
}

/// LLM enrichment pipeline configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct EnrichConfig {
    /// Base URL for the OpenAI-compatible LLM API (e.g., LM Studio).
    #[serde(default = "default_enrich_url")]
    pub llm_base_url: String,
    /// Model name to use for enrichment.
    #[serde(default = "default_enrich_model")]
    pub llm_model: String,
    /// Number of entities per LLM batch call.
    #[serde(default = "default_enrich_batch")]
    pub batch_size: usize,
    /// Maximum tokens per LLM response.
    #[serde(default = "default_enrich_max_tokens")]
    pub max_tokens: u32,
}

impl Default for EnrichConfig {
    fn default() -> Self {
        Self {
            llm_base_url: default_enrich_url(),
            llm_model: default_enrich_model(),
            batch_size: default_enrich_batch(),
            max_tokens: default_enrich_max_tokens(),
        }
    }
}

fn default_enrich_url() -> String {
    "http://localhost:1234".into()
}
fn default_enrich_model() -> String {
    "google/gemma-4-31b".into()
}
fn default_enrich_batch() -> usize {
    10
}
fn default_enrich_max_tokens() -> u32 {
    2048
}

/// Judge model configuration for evaluation and reranker-feedback workflows.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct JudgeConfig {
    /// Provider family. Supported UI probes include `ollama`, `lmstudio`, and `openai_compatible`.
    #[serde(default = "default_judge_provider")]
    pub provider: String,
    /// Base URL for the model provider.
    #[serde(default = "default_judge_base_url")]
    pub base_url: String,
    /// Model name used for judge calls.
    #[serde(default = "default_judge_model")]
    pub model: String,
    /// Optional bearer/API token. Runtime GET endpoints redact this value.
    #[serde(default)]
    pub token: Option<String>,
    /// Request timeout for judge/model discovery calls.
    #[serde(default = "default_judge_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            provider: default_judge_provider(),
            base_url: default_judge_base_url(),
            model: default_judge_model(),
            token: None,
            timeout_seconds: default_judge_timeout_seconds(),
        }
    }
}

fn default_judge_provider() -> String {
    "ollama".into()
}
fn default_judge_base_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_judge_model() -> String {
    "qwen3.5:27b".into()
}
fn default_judge_timeout_seconds() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Port to embed in rendered HTML for cross-page links (workbench ⇄ viz).
    /// Set when the server sits behind a port mapping (e.g. podman 18765:8765):
    /// the process listens on `http_port` inside the container, but the browser
    /// needs `public_port` on the host. Defaults to `http_port` when unset.
    #[serde(default)]
    pub public_port: Option<u16>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub require_tls: bool,
    /// Path to the TLS certificate file (PEM format).
    pub cert_path: Option<String>,
    /// Path to the TLS private key file (PEM format).
    pub key_path: Option<String>,
    /// Path to the file-backed HTTP auth principal database.
    pub auth_file: Option<String>,
    /// Fixed tenant UUID for sharing data across sessions.
    /// If not set, a random UUID is generated per session.
    pub tenant_id: Option<String>,
    /// Fixed session UUID for cross-session memory continuity.
    /// If set, all tools default to this session_id when none is provided.
    pub session_id: Option<String>,
    /// Enable automatic dream consolidation after idle timeout (default: true).
    #[serde(default = "default_true")]
    pub idle_consolidation_enabled: bool,
    /// Seconds of inactivity before triggering idle consolidation (default: 20).
    #[serde(default = "default_idle_seconds")]
    pub idle_consolidation_seconds: u64,
    /// Days after which unreinforced CO_OCCURS edges are pruned. 0 = never prune (default: 0).
    #[serde(default)]
    pub stale_edge_max_days: u64,
    /// Decay factor applied to CO_OCCURS edge weights each consolidation cycle (default: 0.95).
    /// Unreinforced edges gradually lose strength; rediscovered edges are reset to full weight.
    #[serde(default = "default_decay_factor")]
    pub edge_decay_factor: f64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            bind_addr: default_bind_addr(),
            http_port: default_http_port(),
            public_port: None,
            log_level: default_log_level(),
            require_tls: false,
            cert_path: None,
            key_path: None,
            auth_file: None,
            tenant_id: None,
            session_id: None,
            idle_consolidation_enabled: true,
            idle_consolidation_seconds: default_idle_seconds(),
            stale_edge_max_days: 0,
            edge_decay_factor: default_decay_factor(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FerrosaCqlConfig {
    pub contact_points: Vec<String>,
    #[serde(default = "default_keyspace")]
    pub keyspace: String,
    #[serde(default = "default_rf")]
    pub replication_factor: u8,
    #[serde(default = "default_consistency")]
    pub consistency: String,
    /// P0-11/W-02: In DBaaS mode, credentials come from `FERROSA_TENANT_ID` /
    /// `FERROSA_API_KEY` via `apply_dbaas_env_overrides`. In local-dev mode
    /// the TOML value is used as-is; the legacy default_ferrosa_user /
    /// default_ferrosa_password fns are removed — local configs must now
    /// set explicit values.
    #[serde(default = "default_cql_username")]
    pub username: String,
    #[serde(default = "default_cql_password")]
    pub password: String,
    /// Optional admin credentials used for a short-lived session that runs
    /// schema migrations (CREATE KEYSPACE / CREATE TABLE / etc.). When set,
    /// the runtime session still connects as `username`/`password` — only
    /// migration DDL uses the admin pair. Leave unset on auth-disabled
    /// clusters or when `username` already has DDL privileges.
    #[serde(default)]
    pub admin_username: Option<String>,
    #[serde(default)]
    pub admin_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_ttl_days")]
    pub default_ttl_days: u32,
    #[serde(default = "default_fold_ttl")]
    pub fold_ttl_days: u32,
    #[serde(default = "default_archive_days")]
    pub archive_after_days: u32,
    #[serde(default = "default_compression_threshold")]
    pub compression_threshold_tokens: u32,
    #[serde(default = "default_confidence_gate")]
    pub confidence_gate: f64,
    #[serde(default = "default_max_memo")]
    pub max_memo_results: u32,
    #[serde(default = "default_max_entities")]
    pub max_entities: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            default_ttl_days: default_ttl_days(),
            fold_ttl_days: default_fold_ttl(),
            archive_after_days: default_archive_days(),
            compression_threshold_tokens: default_compression_threshold(),
            confidence_gate: default_confidence_gate(),
            max_memo_results: default_max_memo(),
            max_entities: default_max_entities(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_base_url: String,
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: u32,
    /// Maximum characters sent to a single embedding request.
    ///
    /// Ollama tokenizes before rejecting over-context inputs, so client-side
    /// chunking prevents retry loops from turning a too-large text into CPU burn.
    #[serde(default = "default_embedding_max_input_chars")]
    pub max_input_chars: usize,
    #[serde(default = "default_ner_model")]
    pub ner_model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            ollama_base_url: default_ollama_url(),
            model: default_embed_model(),
            dimensions: default_dimensions(),
            max_input_chars: default_embedding_max_input_chars(),
            ner_model: default_ner_model(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub audit_log_enabled: bool,
    #[serde(default = "default_true")]
    pub anomaly_detection_enabled: bool,
    #[serde(default = "default_sigma")]
    pub anomaly_sigma_threshold: f64,
    #[serde(default = "default_true")]
    pub anomaly_alerts_enabled: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            audit_log_enabled: true,
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: default_sigma(),
            anomaly_alerts_enabled: true,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_guideline_version")]
    pub guideline_version: String,
    #[serde(default = "default_cron")]
    pub feedback_export_cron: String,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            guideline_version: default_guideline_version(),
            feedback_export_cron: default_cron(),
        }
    }
}

// --- defaults ---

fn default_transport() -> String {
    "stdio".into()
}
fn default_bind_addr() -> String {
    "0.0.0.0".into()
}
fn default_http_port() -> u16 {
    8765
}
fn default_log_level() -> String {
    "info".into()
}
fn default_keyspace() -> String {
    "agent_memory".into()
}
fn default_rf() -> u8 {
    3
}
fn default_consistency() -> String {
    "LOCAL_QUORUM".into()
}
fn default_ttl_days() -> u32 {
    7
}
fn default_fold_ttl() -> u32 {
    30
}
fn default_archive_days() -> u32 {
    30
}
fn default_compression_threshold() -> u32 {
    512
}
fn default_confidence_gate() -> f64 {
    0.7
}
fn default_max_memo() -> u32 {
    50
}
fn default_max_entities() -> u32 {
    10000
}
fn default_provider() -> String {
    "ollama".into()
}
fn default_ollama_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_embed_model() -> String {
    "nomic-embed-text-v2-moe".into()
}
fn default_dimensions() -> u32 {
    768
}
fn default_embedding_max_input_chars() -> usize {
    6_000
}
fn default_ner_model() -> String {
    "qwen3.5:27b".into()
}
fn default_true() -> bool {
    true
}
fn default_sigma() -> f64 {
    3.0
}
fn default_guideline_version() -> String {
    "v1".into()
}
fn default_cron() -> String {
    "0 2 * * *".into()
}
fn default_idle_seconds() -> u64 {
    20
}
fn default_decay_factor() -> f64 {
    0.95
}
fn default_viz_enabled() -> bool {
    true
}
fn default_viz_port() -> u16 {
    8766
}
fn default_bolt_uri() -> String {
    "bolt://localhost:7687".into()
}
fn default_graph_user() -> String {
    "ferrosa_admin".into()
}
fn default_graph_pass() -> String {
    "ferrosa_admin".into()
}
fn default_http_graph_url() -> String {
    "http://localhost:7474".into()
}
fn default_sparql_url() -> String {
    "http://localhost:8080".into()
}
/// P0-11/W-02: default CQL username for local-dev / non-DBaaS installs.
/// In production (FERROSA_DBAAS_MODE=true) `apply_dbaas_env_overrides`
/// overwrites this with the value from FERROSA_TENANT_ID.
fn default_cql_username() -> String {
    "ferrosa_user".into()
}
/// P0-11/W-02: default CQL password for local-dev / non-DBaaS installs.
/// In production (FERROSA_DBAAS_MODE=true) `apply_dbaas_env_overrides`
/// overwrites this with the value from FERROSA_API_KEY.
fn default_cql_password() -> String {
    "ferrosa_user".into()
}
fn default_warmth_boost() -> f64 {
    0.3
}
fn default_neighbor_ratio() -> f64 {
    0.5
}
fn default_prune_threshold() -> f64 {
    0.01
}
fn default_warmth_cap() -> f64 {
    10.0
}
fn default_ppr_alpha() -> f64 {
    0.45
}
fn default_ppr_iterations() -> usize {
    20
}
fn default_decay_lambda() -> f64 {
    0.1
}
fn default_max_passes() -> usize {
    3
}
fn default_convergence() -> f64 {
    0.1
}
fn default_max_explore_entities() -> usize {
    50
}
fn default_forget_threshold() -> f64 {
    0.05
}
fn default_decay_interval_hours() -> u32 {
    24
}
fn default_max_iterations() -> usize {
    100
}
fn default_max_facts() -> usize {
    50000
}
fn default_cache_ttl() -> u64 {
    3600
}
fn default_confidence_strategy() -> String {
    "min_parent_times_weight".to_string()
}
fn default_promotion_threshold() -> f64 {
    1000.0
}
fn default_size_budget() -> usize {
    100_000
}
fn default_promotion_window_days() -> u32 {
    7
}
fn default_reuse_factor() -> f64 {
    1.0
}

/// Resolve the config file path. Checks, in order:
/// 1. `FERROSA_MEMORY_CONFIG` env var
/// 2. `./ferrosa-memory.toml` (current directory)
/// 3. `~/.config/ferrosa/memory.toml`
pub fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FERROSA_MEMORY_CONFIG") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let cwd = Path::new("ferrosa-memory.toml");
    if cwd.exists() {
        return Some(cwd.to_path_buf());
    }

    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("ferrosa").join("memory.toml");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Load and parse config from the given TOML string.
pub fn parse_config(toml_str: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_str)
}

pub fn validate_shared_http_config(config: &Config) -> anyhow::Result<()> {
    if config.server.transport != "http" {
        return Ok(());
    }

    if !config.server.require_tls && !is_loopback_bind_addr(&config.server.bind_addr) {
        anyhow::bail!("HTTP transport requires TLS unless server.bind_addr is loopback-only");
    }
    if config.server.require_tls
        && (config.server.cert_path.is_none() || config.server.key_path.is_none())
    {
        anyhow::bail!("HTTP transport requires cert_path and key_path");
    }
    if config.server.auth_file.is_none() {
        anyhow::bail!("HTTP transport requires server.auth_file");
    }
    if config.server.tenant_id.is_some() {
        anyhow::bail!("HTTP transport must not use server.tenant_id fallback");
    }

    Ok(())
}

/// P0-11: process-wide counter of direct-loopback Ferrosa connections that
/// fmem made instead of going through the DBaaS proxy. Non-zero values in
/// production indicate the tenant connection path is bypassing the
/// metering / rate-limit / auth surface. The validator below refuses to
/// start with localhost contact points unless the operator has explicitly
/// opted in via `FERROSA_MEMORY_ALLOW_LOCALHOST=true`.
pub static LOOPBACK_CONNECTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the loopback-connection counter for tests and metrics scrape.
pub fn loopback_connection_count() -> u64 {
    LOOPBACK_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Returns true when *every* host portion of `endpoints` is a loopback
/// address (`localhost`, `127.0.0.1`, `::1`).
fn all_loopback(endpoints: &[String]) -> bool {
    !endpoints.is_empty()
        && endpoints.iter().all(|ep| {
            let host = ep.rsplit_once(':').map(|(h, _)| h).unwrap_or(ep.as_str());
            matches!(host.trim(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
        })
}

/// Returns true if any endpoint hostname looks like a loopback address.
fn any_loopback(endpoints: &[String]) -> bool {
    endpoints.iter().any(|ep| {
        let host = ep.rsplit_once(':').map(|(h, _)| h).unwrap_or(ep.as_str());
        matches!(host.trim(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
    })
}

/// P0-11: refuse to load a config that points fmem at a localhost Ferrosa
/// cluster unless the operator has explicitly opted in via
/// `FERROSA_MEMORY_ALLOW_LOCALHOST=true`. The literal "true" wins (typo
/// defense matching P0-04/P0-05/P0-01).
///
/// In production, fmem must connect through the DBaaS proxy (which
/// enforces tenant auth, rate limiting, and metering), not directly to
/// loopback. The counter `LOOPBACK_CONNECTIONS` is incremented every
/// time we accept a loopback config so dashboards can observe drift.
pub fn validate_tenant_connection_path(config: &Config) -> anyhow::Result<()> {
    let endpoints = &config.ferrosa.contact_points;
    if endpoints.is_empty() {
        anyhow::bail!("ferrosa.contact_points is empty — cannot connect to any Ferrosa node");
    }
    if !any_loopback(endpoints) {
        return Ok(());
    }

    let allow = std::env::var("FERROSA_MEMORY_ALLOW_LOCALHOST")
        .ok()
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !allow {
        anyhow::bail!(
            "ferrosa.contact_points contains loopback addresses ({:?}) and \
             FERROSA_MEMORY_ALLOW_LOCALHOST is not set. Refusing to start: in production \
             fmem must connect through the DBaaS proxy. Set FERROSA_MEMORY_ALLOW_LOCALHOST=true \
             for local dev / CI.",
            endpoints
        );
    }

    LOOPBACK_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if all_loopback(endpoints) {
        tracing::warn!(
            ?endpoints,
            "FERROSA_MEMORY_ALLOW_LOCALHOST=true and ALL contact_points are loopback — \
             fmem is bypassing the DBaaS proxy entirely. Local dev only."
        );
    } else {
        tracing::warn!(
            ?endpoints,
            "FERROSA_MEMORY_ALLOW_LOCALHOST=true and SOME contact_points are loopback — \
             a subset of connections will bypass the DBaaS proxy."
        );
    }
    Ok(())
}

fn is_loopback_bind_addr(bind_addr: &str) -> bool {
    matches!(
        bind_addr.trim(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// P0-11/W-02: returns true when FERROSA_DBAAS_MODE=true (exact match,
/// case-insensitive). When true, `apply_dbaas_env_overrides` must succeed
/// at startup or the process exits with a clear error.
pub fn is_dbaas_mode() -> bool {
    std::env::var("FERROSA_DBAAS_MODE")
        .ok()
        .as_deref()
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// P0-11/W-02: Overwrite the CQL contact-points, graph URL, and
/// credentials from DBaaS-specific env vars. Called at startup when
/// `is_dbaas_mode()` is true. Fails loud if any required variable is absent.
///
/// Required env vars (all must be set in DBaaS mode):
/// - `FERROSA_CQL_PROXY_ADDR`  — comma-separated `host:port` list
/// - `FERROSA_GRAPH_PROXY_ADDR` — full URL, e.g. `http://proxy.dbaas.io:7474`
/// - `FERROSA_TENANT_ID`        — used as CQL username
/// - `FERROSA_API_KEY`          — used as CQL password
pub fn apply_dbaas_env_overrides(config: &mut Config) -> anyhow::Result<()> {
    let cql_addr = std::env::var("FERROSA_CQL_PROXY_ADDR").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_CQL_PROXY_ADDR is not set. \
             Set it to the CQL proxy address (e.g. 'proxy.dbaas.ferrosa.io:9042')."
        )
    })?;
    if cql_addr.trim().is_empty() {
        anyhow::bail!(
            "FERROSA_CQL_PROXY_ADDR is set but empty — cannot connect to any Ferrosa node"
        );
    }

    let graph_addr = std::env::var("FERROSA_GRAPH_PROXY_ADDR").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_GRAPH_PROXY_ADDR is not set. \
             Set it to the graph proxy URL (e.g. 'http://proxy.dbaas.ferrosa.io:7474')."
        )
    })?;
    if graph_addr.trim().is_empty() {
        anyhow::bail!(
            "FERROSA_GRAPH_PROXY_ADDR is set but empty — cannot connect to the graph proxy"
        );
    }

    let tenant_id = std::env::var("FERROSA_TENANT_ID").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_TENANT_ID is not set. \
             Set it to the tenant UUID issued by the DBaaS control plane."
        )
    })?;
    if tenant_id.trim().is_empty() {
        anyhow::bail!("FERROSA_TENANT_ID is set but empty");
    }

    let api_key = std::env::var("FERROSA_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_API_KEY is not set. \
             Set it to the API key issued by the DBaaS control plane."
        )
    })?;
    if api_key.trim().is_empty() {
        anyhow::bail!("FERROSA_API_KEY is set but empty");
    }

    // Rewrite contact_points from the env var (comma-separated).
    config.ferrosa.contact_points = cql_addr
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if config.ferrosa.contact_points.is_empty() {
        anyhow::bail!("FERROSA_CQL_PROXY_ADDR contained no valid addresses after parsing");
    }

    // Rewrite graph URL.
    config.graph.http_url = graph_addr;

    // Replace credentials with tenant identity.
    config.ferrosa.username = tenant_id;
    config.ferrosa.password = api_key;

    tracing::info!(
        contact_points = ?config.ferrosa.contact_points,
        graph_http_url = %config.graph.http_url,
        "DBaaS mode: CQL and graph addresses set from environment"
    );
    Ok(())
}

/// P0-11/W-02: Load config and, when FERROSA_DBAAS_MODE=true, apply the
/// DBaaS env overrides. Exits with a clear error if required vars are absent
/// in DBaaS mode.
pub fn load_config_with_dbaas() -> anyhow::Result<Config> {
    let mut config = load_config()?;
    if is_dbaas_mode() {
        apply_dbaas_env_overrides(&mut config)?;
    }
    Ok(config)
}

/// Load config from the resolved path, or return an error if not found.
pub fn load_config() -> anyhow::Result<Config> {
    let path = resolve_config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no config file found; set FERROSA_MEMORY_CONFIG or create ferrosa-memory.toml"
        )
    })?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read config at {}: {}", path.display(), e))?;
    parse_config(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse config at {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    use serial_test::serial;

    /// P0-11: tenant connection path counter must be exposed.
    #[test]
    fn loopback_connection_count_exposes_counter() {
        let baseline = loopback_connection_count();
        assert!(loopback_connection_count() >= baseline);
    }

    /// P0-11: localhost contact_points are refused without the explicit
    /// dev opt-in.
    #[test]
    #[serial]
    fn validate_tenant_path_refuses_localhost_without_opt_in() {
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(
            res.is_err(),
            "loopback contact_points must refuse to start without opt-in: {res:?}"
        );
    }

    /// P0-11: explicit dev opt-in lets localhost contact_points through
    /// AND increments the counter.
    #[test]
    #[serial]
    fn validate_tenant_path_dev_opt_in_increments_counter() {
        unsafe {
            std::env::set_var("FERROSA_MEMORY_ALLOW_LOCALHOST", "true");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042", "localhost:19043", "localhost:19044"]
"#;
        let config = parse_config(toml).unwrap();
        let baseline = loopback_connection_count();
        let res = validate_tenant_connection_path(&config);
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        assert!(res.is_ok(), "dev opt-in must allow localhost: {res:?}");
        assert!(
            loopback_connection_count() > baseline,
            "loopback counter must advance when localhost is accepted"
        );
    }

    /// P0-11: typo defense — non-"true" values must NOT trigger opt-in.
    #[test]
    #[serial]
    fn validate_tenant_path_typo_does_not_opt_in() {
        unsafe {
            std::env::set_var("FERROSA_MEMORY_ALLOW_LOCALHOST", "yes");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        assert!(
            res.is_err(),
            "non-'true' values must not satisfy the opt-in: {res:?}"
        );
    }

    /// P0-11: production-shape config (non-loopback contact_points) passes
    /// validation regardless of the opt-in env.
    #[test]
    #[serial]
    fn validate_tenant_path_proxy_config_passes() {
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        let toml = r#"
[ferrosa]
contact_points = ["proxy.dbaas.ferrosa.io:9042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(res.is_ok(), "proxy contact_points must pass: {res:?}");
    }

    /// P0-11: empty contact_points always fails.
    #[test]
    #[serial]
    fn validate_tenant_path_empty_contact_points_fails() {
        let toml = r#"
[ferrosa]
contact_points = []
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(res.is_err(), "empty contact_points must fail: {res:?}");
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse minimal config");
        assert_eq!(config.ferrosa.contact_points, vec!["localhost:9042"]);
        assert_eq!(config.server.transport, "stdio");
        assert_eq!(config.memory.default_ttl_days, 7);
        assert_eq!(config.embeddings.dimensions, 768);
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[server]
transport = "http"
http_port = 9999
log_level = "debug"

[ferrosa]
contact_points = ["node1:9042", "node2:9042"]
keyspace = "test_memory"
replication_factor = 1
consistency = "ONE"

[memory]
default_ttl_days = 14
fold_ttl_days = 60
archive_after_days = 90
compression_threshold_tokens = 256
confidence_gate = 0.8
max_memo_results = 100

[embeddings]
provider = "openai"
ollama_base_url = "http://gpu:11434"
model = "text-embedding-3-small"
dimensions = 1536

[security]
audit_log_enabled = false
anomaly_detection_enabled = false
anomaly_sigma_threshold = 2.5
anomaly_alerts_enabled = false

[routing]
guideline_version = "v2"
feedback_export_cron = "0 3 * * *"
"#;
        let config = parse_config(toml).expect("should parse full config");
        assert_eq!(config.server.transport, "http");
        assert_eq!(config.server.bind_addr, "0.0.0.0");
        assert_eq!(config.server.http_port, 9999);
        assert_eq!(config.ferrosa.contact_points.len(), 2);
        assert_eq!(config.ferrosa.keyspace, "test_memory");
        assert_eq!(config.memory.default_ttl_days, 14);
        assert_eq!(config.memory.confidence_gate, 0.8);
        assert_eq!(config.embeddings.provider, "openai");
        assert_eq!(config.embeddings.dimensions, 1536);
        assert!(!config.security.audit_log_enabled);
        assert!(!config.security.anomaly_alerts_enabled);
        assert_eq!(config.routing.guideline_version, "v2");
    }

    #[test]
    fn parse_tls_config_fields_optional() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse without TLS fields");
        assert!(!config.server.require_tls);
        assert!(config.server.cert_path.is_none());
        assert!(config.server.key_path.is_none());
    }

    #[test]
    fn parse_tls_config_fields_present() {
        let toml = r#"
[server]
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with TLS fields");
        assert!(config.server.require_tls);
        assert_eq!(
            config.server.cert_path.as_deref(),
            Some("/etc/ssl/cert.pem")
        );
        assert_eq!(config.server.key_path.as_deref(), Some("/etc/ssl/key.pem"));
    }

    #[test]
    fn parse_auth_file_field_present() {
        let toml = r#"
[server]
auth_file = "/etc/ferrosa/auth.toml"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse auth_file");
        assert_eq!(
            config.server.auth_file.as_deref(),
            Some("/etc/ferrosa/auth.toml")
        );
    }

    #[test]
    fn parse_invalid_config_missing_required() {
        let toml = r#"
[server]
transport = "stdio"
"#;
        let result = parse_config(toml);
        assert!(result.is_err(), "should fail without [ferrosa] section");
    }

    #[test]
    fn parse_invalid_config_bad_type() {
        let toml = r#"
[ferrosa]
contact_points = "not_an_array"
"#;
        let result = parse_config(toml);
        assert!(result.is_err(), "contact_points must be an array");
    }

    #[test]
    fn default_decay_factor_returns_0_95() {
        assert!((default_decay_factor() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn server_config_default_stale_edge_max_days() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.stale_edge_max_days, 0);
    }

    #[test]
    fn server_config_default_edge_decay_factor() {
        let cfg = ServerConfig::default();
        assert!((cfg.edge_decay_factor - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn server_config_default_idle_consolidation_enabled() {
        let cfg = ServerConfig::default();
        assert!(cfg.idle_consolidation_enabled);
    }

    #[test]
    fn server_config_default_idle_seconds() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.idle_consolidation_seconds, 20);
    }

    #[test]
    fn server_config_default_transport_stdio() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.transport, "stdio");
    }

    #[test]
    fn server_config_default_http_port() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.http_port, 8765);
    }

    #[test]
    fn server_config_default_bind_addr() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.bind_addr, "0.0.0.0");
    }

    #[test]
    fn server_config_default_log_level() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn server_config_default_tls_disabled() {
        let cfg = ServerConfig::default();
        assert!(!cfg.require_tls);
        assert!(cfg.cert_path.is_none());
        assert!(cfg.key_path.is_none());
        assert!(cfg.auth_file.is_none());
    }

    #[test]
    fn server_config_default_tenant_session_none() {
        let cfg = ServerConfig::default();
        assert!(cfg.tenant_id.is_none());
        assert!(cfg.session_id.is_none());
    }

    #[test]
    fn validate_shared_http_requires_auth_file() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("auth_file"));
    }

    #[test]
    fn validate_shared_http_rejects_tenant_fallback() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "/etc/ferrosa/auth.toml"
tenant_id = "00000000-0000-0000-0000-000000000001"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("tenant_id"));
    }

    #[test]
    fn validate_shared_http_accepts_required_fields() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "/etc/ferrosa/auth.toml"
"#;
        let config = parse_config(toml).unwrap();
        validate_shared_http_config(&config).expect("shared http config should validate");
    }

    #[test]
    fn validate_shared_http_accepts_loopback_without_tls() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
bind_addr = "127.0.0.1"
require_tls = false
auth_file = "/etc/ferrosa/auth.toml"
"#;
        let config = parse_config(toml).unwrap();
        validate_shared_http_config(&config).expect("loopback-only http config should validate");
    }

    #[test]
    fn validate_shared_http_rejects_non_loopback_without_tls() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
bind_addr = "0.0.0.0"
require_tls = false
auth_file = "/etc/ferrosa/auth.toml"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("loopback-only"));
    }

    #[test]
    fn parse_toml_with_stale_edge_and_decay() {
        let toml = r#"
[server]
stale_edge_max_days = 30
edge_decay_factor = 0.85

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse stale edge config");
        assert_eq!(config.server.stale_edge_max_days, 30);
        assert!((config.server.edge_decay_factor - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_toml_with_idle_consolidation() {
        let toml = r#"
[server]
idle_consolidation_enabled = false
idle_consolidation_seconds = 60

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse idle consolidation config");
        assert!(!config.server.idle_consolidation_enabled);
        assert_eq!(config.server.idle_consolidation_seconds, 60);
    }

    #[test]
    fn parse_toml_with_graph_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[graph]
bolt_uri = "bolt://remote:7687"
username = "admin"
password = "secret"
http_url = "http://remote:7474"
"#;
        let config = parse_config(toml).expect("should parse graph config");
        assert_eq!(config.graph.bolt_uri, "bolt://remote:7687");
        assert_eq!(config.graph.username, "admin");
        assert_eq!(config.graph.password, "secret");
        assert_eq!(config.graph.http_url, "http://remote:7474");
    }

    #[test]
    fn graph_config_defaults() {
        let cfg = GraphDbConfig::default();
        assert_eq!(cfg.bolt_uri, "bolt://localhost:7687");
        assert_eq!(cfg.username, "ferrosa_admin");
        assert_eq!(cfg.password, "ferrosa_admin");
        assert_eq!(cfg.http_url, "http://localhost:7474");
    }

    #[test]
    fn parse_toml_with_viz_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[viz]
enabled = false
port = 9999
"#;
        let config = parse_config(toml).expect("should parse viz config");
        assert!(!config.viz.enabled);
        assert_eq!(config.viz.port, 9999);
    }

    #[test]
    fn viz_config_defaults() {
        let cfg = VizConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 8766);
    }

    #[test]
    fn memory_config_defaults() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.default_ttl_days, 7);
        assert_eq!(cfg.fold_ttl_days, 30);
        assert_eq!(cfg.archive_after_days, 30);
        assert_eq!(cfg.compression_threshold_tokens, 512);
        assert!((cfg.confidence_gate - 0.7).abs() < f64::EPSILON);
        assert_eq!(cfg.max_memo_results, 50);
        assert_eq!(cfg.max_entities, 10000);
    }

    #[test]
    fn embedding_config_defaults() {
        let cfg = EmbeddingConfig::default();
        assert_eq!(cfg.provider, "ollama");
        assert_eq!(cfg.ollama_base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.model, "nomic-embed-text-v2-moe");
        assert_eq!(cfg.dimensions, 768);
    }

    #[test]
    fn judge_config_defaults_to_local_ollama_without_token() {
        let cfg = JudgeConfig::default();
        assert_eq!(cfg.provider, "ollama");
        assert_eq!(cfg.base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.model, "qwen3.5:27b");
        assert_eq!(cfg.token, None);
        assert_eq!(cfg.timeout_seconds, 30);
    }

    #[test]
    fn parse_judge_config_fields() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[judge]
provider = "lmstudio"
base_url = "http://127.0.0.1:1234"
model = "qwen3"
token = "secret"
timeout_seconds = 12
"#;
        let config = parse_config(toml).expect("should parse judge config");
        assert_eq!(config.judge.provider, "lmstudio");
        assert_eq!(config.judge.base_url, "http://127.0.0.1:1234");
        assert_eq!(config.judge.model, "qwen3");
        assert_eq!(config.judge.token.as_deref(), Some("secret"));
        assert_eq!(config.judge.timeout_seconds, 12);
    }

    #[test]
    fn security_config_defaults() {
        let cfg = SecurityConfig::default();
        assert!(cfg.audit_log_enabled);
        assert!(cfg.anomaly_detection_enabled);
        assert!((cfg.anomaly_sigma_threshold - 3.0).abs() < f64::EPSILON);
        assert!(cfg.anomaly_alerts_enabled);
    }

    #[test]
    fn routing_config_defaults() {
        let cfg = RoutingConfig::default();
        assert_eq!(cfg.guideline_version, "v1");
        assert_eq!(cfg.feedback_export_cron, "0 2 * * *");
    }

    #[test]
    fn parse_toml_defaults_for_optional_sections() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with all defaults");
        // Verify all default sections are populated
        assert_eq!(config.server.transport, "stdio");
        assert!(config.viz.enabled);
        assert_eq!(config.graph.bolt_uri, "bolt://localhost:7687");
        assert_eq!(config.memory.default_ttl_days, 7);
        assert_eq!(config.embeddings.provider, "ollama");
        assert!(config.security.audit_log_enabled);
        assert_eq!(config.routing.guideline_version, "v1");
        assert!((config.rmh.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert_eq!(config.datalog.max_iterations, 100);
    }

    #[test]
    fn embedding_config_has_ner_model_default() {
        let config = EmbeddingConfig::default();
        assert_eq!(config.ner_model, "qwen3.5:27b");
    }

    #[test]
    fn parse_toml_with_ner_model() {
        let toml_str = r#"
[server]
transport = "stdio"

[ferrosa]
contact_points = ["localhost:9042"]

[embeddings]
ner_model = "llama3:8b"
"#;
        let config = parse_config(toml_str).unwrap();
        assert_eq!(config.embeddings.ner_model, "llama3:8b");
    }

    #[test]
    fn parse_toml_with_tenant_and_session_ids() {
        let toml = r#"
[server]
tenant_id = "550e8400-e29b-41d4-a716-446655440000"
session_id = "660e8400-e29b-41d4-a716-446655440000"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse tenant/session IDs");
        assert_eq!(
            config.server.tenant_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            config.server.session_id.as_deref(),
            Some("660e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn rmh_config_defaults() {
        let cfg = RmhConfig::default();
        assert!((cfg.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert!((cfg.warmth_neighbor_ratio - 0.5).abs() < f64::EPSILON);
        assert!((cfg.warmth_prune_threshold - 0.01).abs() < f64::EPSILON);
        assert!((cfg.warmth_cap - 10.0).abs() < f64::EPSILON);
        assert!((cfg.ppr_alpha - 0.45).abs() < f64::EPSILON);
        assert_eq!(cfg.ppr_iterations, 20);
        assert!((cfg.decay_lambda - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.max_explore_passes, 3);
        assert!((cfg.convergence_threshold - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.max_explore_entities, 50);
    }

    #[test]
    fn datalog_config_defaults() {
        let cfg = DatalogConfig::default();
        assert_eq!(cfg.max_iterations, 100);
        assert_eq!(cfg.max_facts, 50000);
        assert_eq!(cfg.cache_ttl_seconds, 3600);
        assert_eq!(cfg.confidence_combination, "min_parent_times_weight");
    }

    #[test]
    fn test_config_defaults_rmh() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with rmh defaults");
        assert!((config.rmh.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert!((config.rmh.ppr_alpha - 0.45).abs() < f64::EPSILON);
        assert_eq!(config.rmh.ppr_iterations, 20);
        assert_eq!(config.rmh.max_explore_passes, 3);
        assert!((config.rmh.warmth_cap - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_defaults_datalog() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with datalog defaults");
        assert_eq!(config.datalog.max_iterations, 100);
        assert_eq!(config.datalog.max_facts, 50000);
        assert_eq!(config.datalog.cache_ttl_seconds, 3600);
        assert_eq!(
            config.datalog.confidence_combination,
            "min_parent_times_weight"
        );
    }

    #[test]
    fn test_config_rmh_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[rmh]
warmth_boost_amount = 0.5
ppr_alpha = 0.85
max_explore_passes = 5
"#;
        let config = parse_config(toml).expect("should parse with rmh overrides");
        assert!((config.rmh.warmth_boost_amount - 0.5).abs() < f64::EPSILON);
        assert!((config.rmh.ppr_alpha - 0.85).abs() < f64::EPSILON);
        assert_eq!(config.rmh.max_explore_passes, 5);
        // Non-overridden fields keep defaults
        assert!((config.rmh.warmth_neighbor_ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_datalog_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[datalog]
max_iterations = 200
max_facts = 100000
cache_ttl_seconds = 7200
confidence_combination = "product"
"#;
        let config = parse_config(toml).expect("should parse with datalog overrides");
        assert_eq!(config.datalog.max_iterations, 200);
        assert_eq!(config.datalog.max_facts, 100000);
        assert_eq!(config.datalog.cache_ttl_seconds, 7200);
        assert_eq!(config.datalog.confidence_combination, "product");
    }

    #[test]
    fn promotion_config_defaults() {
        let cfg = PromotionConfig::default();
        assert!((cfg.promotion_threshold - 1000.0).abs() < f64::EPSILON);
        assert_eq!(cfg.size_budget_rows, 100_000);
        assert_eq!(cfg.window_days, 7);
        assert!((cfg.reuse_factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_defaults_promotion() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with promotion defaults");
        assert!((config.promotion.promotion_threshold - 1000.0).abs() < f64::EPSILON);
        assert_eq!(config.promotion.size_budget_rows, 100_000);
        assert_eq!(config.promotion.window_days, 7);
        assert!((config.promotion.reuse_factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_promotion_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[promotion]
promotion_threshold = 2000.0
size_budget_rows = 50000
window_days = 14
reuse_factor = 1.5
"#;
        let config = parse_config(toml).expect("should parse with promotion overrides");
        assert!((config.promotion.promotion_threshold - 2000.0).abs() < f64::EPSILON);
        assert_eq!(config.promotion.size_budget_rows, 50000);
        assert_eq!(config.promotion.window_days, 14);
        assert!((config.promotion.reuse_factor - 1.5).abs() < f64::EPSILON);
    }

    // ── W-02 tests ───────────────────────────────────────────────────────────

    /// P0-11/W-02: is_dbaas_mode returns true only on literal "true".
    #[test]
    #[serial]
    fn dbaas_mode_requires_exact_true() {
        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "true") }
        assert!(
            is_dbaas_mode(),
            "FERROSA_DBAAS_MODE=true must enable DBaaS mode"
        );

        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "TRUE") }
        assert!(
            is_dbaas_mode(),
            "case-insensitive TRUE must enable DBaaS mode"
        );

        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "yes") }
        assert!(
            !is_dbaas_mode(),
            "non-'true' values must not enable DBaaS mode"
        );

        unsafe { std::env::remove_var("FERROSA_DBAAS_MODE") }
        assert!(!is_dbaas_mode(), "absent var must not enable DBaaS mode");
    }

    fn minimal_config_with_proxy() -> Config {
        let toml = r#"
[ferrosa]
contact_points = ["proxy.dbaas.ferrosa.io:9042"]
"#;
        parse_config(toml).expect("minimal proxy config must parse")
    }

    /// P0-11/W-02: apply_dbaas_env_overrides populates config from env vars.
    #[test]
    #[serial]
    fn dbaas_env_overrides_populate_config() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-abc-123");
            std::env::set_var("FERROSA_API_KEY", "sk-test-key");
        }
        let mut config = minimal_config_with_proxy();
        apply_dbaas_env_overrides(&mut config)
            .expect("env overrides must succeed when all vars set");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        assert_eq!(
            config.ferrosa.contact_points,
            vec!["proxy.dbaas.ferrosa.io:9042"]
        );
        assert_eq!(config.graph.http_url, "http://graph.dbaas.ferrosa.io:7474");
        assert_eq!(config.ferrosa.username, "tenant-abc-123");
        assert_eq!(config.ferrosa.password, "sk-test-key");
    }

    /// P0-11/W-02: apply_dbaas_env_overrides parses comma-separated CQL addrs.
    #[test]
    #[serial]
    fn dbaas_env_overrides_multi_contact_points() {
        unsafe {
            std::env::set_var(
                "FERROSA_CQL_PROXY_ADDR",
                "proxy1.dbaas.ferrosa.io:9042, proxy2.dbaas.ferrosa.io:9042",
            );
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-multi");
            std::env::set_var("FERROSA_API_KEY", "sk-multi-key");
        }
        let mut config = minimal_config_with_proxy();
        apply_dbaas_env_overrides(&mut config).expect("multi-addr must succeed");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        assert_eq!(config.ferrosa.contact_points.len(), 2);
        assert_eq!(
            config.ferrosa.contact_points[0],
            "proxy1.dbaas.ferrosa.io:9042"
        );
        assert_eq!(
            config.ferrosa.contact_points[1],
            "proxy2.dbaas.ferrosa.io:9042"
        );
    }

    /// P0-11/W-02: missing FERROSA_CQL_PROXY_ADDR causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_cql_addr_fails() {
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing CQL addr must fail");
        unsafe {
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_CQL_PROXY_ADDR"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_GRAPH_PROXY_ADDR causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_graph_addr_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing graph addr must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_GRAPH_PROXY_ADDR"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_TENANT_ID causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_tenant_id_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing tenant id must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_TENANT_ID"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_API_KEY causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_api_key_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing api key must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_API_KEY"),
            "error must name the missing variable, got: {msg}"
        );
    }
}
