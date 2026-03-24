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

use std::path::{Path, PathBuf};

use serde::Deserialize;

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
    pub viz: VizConfig,
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

/// Visualization dashboard configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct VizConfig {
    #[serde(default = "default_viz_enabled")]
    pub enabled: bool,
    #[serde(default = "default_viz_port")]
    pub port: u16,
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            enabled: default_viz_enabled(),
            port: default_viz_port(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub require_tls: bool,
    /// Path to the TLS certificate file (PEM format).
    pub cert_path: Option<String>,
    /// Path to the TLS private key file (PEM format).
    pub key_path: Option<String>,
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            http_port: default_http_port(),
            log_level: default_log_level(),
            require_tls: false,
            cert_path: None,
            key_path: None,
            tenant_id: None,
            session_id: None,
            idle_consolidation_enabled: true,
            idle_consolidation_seconds: default_idle_seconds(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FerrosaCqlConfig {
    pub contact_points: Vec<String>,
    #[serde(default = "default_keyspace")]
    pub keyspace: String,
    #[serde(default = "default_rf")]
    pub replication_factor: u8,
    #[serde(default = "default_consistency")]
    pub consistency: String,
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

#[derive(Debug, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_base_url: String,
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: u32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            ollama_base_url: default_ollama_url(),
            model: default_embed_model(),
            dimensions: default_dimensions(),
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
    "http://localhost:11434".into()
}
fn default_embed_model() -> String {
    "nomic-embed-text".into()
}
fn default_dimensions() -> u32 {
    768
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
    "neo4j".into()
}
fn default_graph_pass() -> String {
    "neo4j".into()
}
fn default_http_graph_url() -> String {
    "http://localhost:7474".into()
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
}
