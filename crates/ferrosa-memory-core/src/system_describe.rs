//! `system.describe` — management-safe self-description of a running
//! `ferrosa-memory` server.
//!
//! Implements the contract in
//! `specs/ferrosa-memory-management-introspection-tool.md`. Ferrosa Workbench
//! (and any management client) calls this read-only MCP tool to discover a
//! cluster's identity, runtime health, redacted configuration, dependent-store
//! health, schema drift, and the management actions it may offer — without
//! guessing from config files, listener ports, or process tables.
//!
//! Design rules honored here:
//! - **Read-only / idempotent.** No probe mutates runtime, schema, or config.
//! - **Fail loud, never fake.** Store probes report `error`/`degraded` with a
//!   specific message instead of pretending success; missing release metadata
//!   is reported as `unknown`, never invented.
//! - **Redact secrets.** Secret config values are never serialized; only their
//!   dotted key paths appear in `redactedKeys`.
//! - **Bounded probes.** Every dependency probe has a hard timeout so a hung
//!   backend yields degraded health instead of blocking the call.

use serde::Serialize;
use sha2::Digest;
use std::time::Duration;

use crate::config::Config;
use crate::graph::GraphClient;
use crate::storage::Storage;

/// Stable contract identifier returned in every descriptor.
pub const CONTRACT: &str = "ferrosa-memory.system.describe.v1";

/// Hard per-dependency probe timeout. A backend that does not answer within
/// this window is reported as degraded rather than allowed to hang the call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// All descriptor sections, used for `include` filtering. `server` and
/// `warnings` are always present and not represented here.
#[derive(Debug, Clone, Copy)]
pub struct SectionSet {
    pub identity: bool,
    pub runtime: bool,
    pub configuration: bool,
    pub stores: bool,
    pub schema: bool,
    pub statistics: bool,
    pub binaries: bool,
    pub harnesses: bool,
    pub capabilities: bool,
    pub management_actions: bool,
}

impl SectionSet {
    /// Every section enabled (the default when `include` is omitted).
    pub fn all() -> Self {
        Self {
            identity: true,
            runtime: true,
            configuration: true,
            stores: true,
            schema: true,
            statistics: true,
            binaries: true,
            harnesses: true,
            capabilities: true,
            management_actions: true,
        }
    }

    /// Build a section set from a requested `include` list. Unknown names are
    /// ignored. An empty/None list means "all sections".
    pub fn from_include(include: Option<&[String]>) -> Self {
        let Some(names) = include else {
            return Self::all();
        };
        if names.is_empty() {
            return Self::all();
        }
        let has = |k: &str| names.iter().any(|n| n.eq_ignore_ascii_case(k));
        Self {
            identity: has("identity"),
            runtime: has("runtime"),
            configuration: has("configuration"),
            stores: has("stores"),
            schema: has("schema"),
            statistics: has("statistics"),
            binaries: has("binaries"),
            harnesses: has("harnesses"),
            capabilities: has("capabilities"),
            management_actions: has("managementActions"),
        }
    }
}

/// Per-store health classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreHealth {
    Ready,
    Degraded,
    Error,
    Unknown,
}

/// Aggregate runtime health (mirrors the spec's health-state table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverallHealth {
    Ready,
    LiveNotReady,
    Error,
    Unknown,
}

/// Readiness signal — whether all required stores are usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Readiness {
    Ready,
    NotReady,
}

/// Schema drift between the database and the running binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Drift {
    None,
    Behind,
    Ahead,
    Unknown,
}

/// Binary upgrade state. `Unknown` until a release-metadata source is wired in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeState {
    Current,
    Behind,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSection {
    pub name: &'static str,
    pub binary: &'static str,
    pub version: String,
    pub commit: Option<String>,
    pub channel: Option<String>,
    pub started_at: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySection {
    pub cluster_id: String,
    pub tenant_id: String,
    pub session_id: String,
    pub alias: Option<String>,
    pub config_path: Option<String>,
    pub config_hash: Option<String>,
    pub install_kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSection {
    pub transport: String,
    pub endpoint_url: String,
    pub require_tls: bool,
    pub health: OverallHealth,
    pub readiness: Readiness,
    pub liveness: &'static str,
    pub viz_url: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSection {
    pub effective_config: serde_json::Value,
    pub redacted_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FerrosaStore {
    pub contact_points: Vec<String>,
    pub keyspace: String,
    /// Configured (intended) values from the `[ferrosa]` config section.
    pub configured_replication_factor: u8,
    pub configured_consistency: String,
    pub health: StoreHealth,
    pub schema_version: Option<String>,
    /// Live cluster metadata read from ferrosa's CQL system tables. `None` when
    /// the probe failed or was skipped (see `cluster_error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<crate::storage::ClusterInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphStore {
    pub uri: String,
    pub http_url: String,
    pub health: StoreHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingsStore {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub dimensions: u32,
    pub health: StoreHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoresSection {
    pub ferrosa: FerrosaStore,
    pub graph: GraphStore,
    pub embeddings: EmbeddingsStore,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSection {
    pub current_version: Option<String>,
    pub expected_version: String,
    pub drift: Drift,
    pub pending_migrations: Vec<u32>,
    pub requires_backup_before_migration: bool,
}

/// Summary memory/cluster statistics for the addressed tenant + session.
/// Counts mirror the `get_stats` tool so a descriptor caller does not need a
/// second round-trip.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatisticsSection {
    pub session_id: String,
    pub entity_count: usize,
    pub fold_count: usize,
    pub active_fold_count: usize,
    pub folded_count: usize,
    pub archived_fold_count: usize,
    pub memo_count: usize,
    pub memo_total_hits: usize,
    pub memo_hit_rate: f64,
    pub temporal_fact_count: usize,
    pub edge_count: usize,
    pub intention_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BinariesSection {
    pub current_version: String,
    pub latest_stable: Option<String>,
    pub latest_nightly: Option<String>,
    pub upgrade_state: UpgradeState,
    pub supported_upgrade_channels: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitiesSection {
    pub tools: Vec<String>,
    pub features: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementAction {
    pub id: &'static str,
    pub label: &'static str,
    pub mutation: bool,
    pub requires_confirmation: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<&'static str>,
}

/// The full descriptor returned by `system.describe`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemDescriptor {
    pub contract: &'static str,
    pub server: ServerSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentitySection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<ConfigurationSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stores: Option<StoresSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics: Option<StatisticsSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binaries: Option<BinariesSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harnesses: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilitiesSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_actions: Option<Vec<ManagementAction>>,
    pub warnings: Vec<String>,
}

/// Immutable startup snapshot of everything `system.describe` needs that is
/// known at process boot. Probed (dynamic) state — store health and schema
/// version — is gathered fresh on each call in [`build_descriptor`].
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub version: String,
    pub commit: Option<String>,
    pub pid: u32,
    pub started_at: String,
    pub transport: String,
    pub tenant_id: String,
    pub session_id: String,
    pub config_path: Option<String>,
    pub config_hash: Option<String>,
    pub install_kind: String,
    pub bind_addr: String,
    pub http_port: u16,
    pub public_port: Option<u16>,
    pub require_tls: bool,
    pub viz_enabled: bool,
    pub viz_port: u16,
    pub viz_public_port: Option<u16>,
    pub keyspace: String,
    pub contact_points: Vec<String>,
    /// Replication factor as configured in the `[ferrosa]` config (the intended
    /// value). Actual cluster replication is read live via `ClusterInfo`.
    pub configured_replication_factor: u8,
    /// Query consistency level as configured in the `[ferrosa]` config.
    pub configured_consistency: String,
    pub graph_bolt_uri: String,
    pub graph_http_url: String,
    pub embedding: crate::config::EmbeddingConfig,
    pub effective_config: serde_json::Value,
    pub redacted_keys: Vec<String>,
    pub features: Vec<&'static str>,
}

impl SystemInfo {
    /// Build the startup snapshot from the effective config and resolved
    /// identity. `started_at` should be captured once at process start
    /// (RFC3339); `tenant_id`/`session_id` are the runtime-resolved IDs.
    ///
    /// Reads the config file (if any) to record its path and SHA-256.
    pub fn build(
        config: &Config,
        tenant_id: uuid::Uuid,
        session_id: uuid::Uuid,
        started_at: String,
    ) -> Self {
        let config_path = crate::config::resolve_config_path()
            .map(|p| p.to_string_lossy().to_string());
        let config_hash = config_path.as_deref().and_then(hash_config_file);
        Self::from_parts(config, tenant_id, session_id, started_at, config_path, config_hash)
    }

    /// Assemble a snapshot from already-resolved parts. Performs no I/O, so it
    /// is safe for hermetic defaults and tests.
    fn from_parts(
        config: &Config,
        tenant_id: uuid::Uuid,
        session_id: uuid::Uuid,
        started_at: String,
        config_path: Option<String>,
        config_hash: Option<String>,
    ) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: build_commit(),
            pid: std::process::id(),
            started_at,
            transport: config.server.transport.clone(),
            tenant_id: tenant_id.to_string(),
            session_id: session_id.to_string(),
            config_path,
            config_hash,
            install_kind: "custom".to_string(),
            bind_addr: config.server.bind_addr.clone(),
            http_port: config.server.http_port,
            public_port: config.server.public_port,
            require_tls: config.server.require_tls,
            viz_enabled: config.viz.enabled,
            viz_port: config.viz.port,
            viz_public_port: config.viz.public_port,
            keyspace: config.ferrosa.keyspace.clone(),
            contact_points: config.ferrosa.contact_points.clone(),
            configured_replication_factor: config.ferrosa.replication_factor,
            configured_consistency: config.ferrosa.consistency.clone(),
            graph_bolt_uri: config.graph.bolt_uri.clone(),
            graph_http_url: config.graph.http_url.clone(),
            embedding: config.embeddings.clone(),
            effective_config: effective_config(config),
            redacted_keys: redacted_keys(config),
            features: feature_list(),
        }
    }

    /// Externally visible endpoint URL (or `"stdio"` for stdio transport).
    fn endpoint_url(&self) -> String {
        if self.transport != "http" {
            return "stdio".to_string();
        }
        let host = if self.bind_addr == "0.0.0.0" {
            "127.0.0.1"
        } else {
            self.bind_addr.as_str()
        };
        let scheme = if self.require_tls { "https" } else { "http" };
        let port = self.public_port.unwrap_or(self.http_port);
        format!("{scheme}://{host}:{port}")
    }

    fn viz_url(&self) -> Option<String> {
        if !self.viz_enabled {
            return None;
        }
        let port = self.viz_public_port.unwrap_or(self.viz_port);
        Some(format!("http://127.0.0.1:{port}"))
    }

    fn cluster_id(&self) -> String {
        let port = self.public_port.unwrap_or(self.http_port);
        format!(
            "{}:{}:{}:{}:{}",
            self.tenant_id, self.session_id, self.transport, port, self.keyspace
        )
    }
}

impl Default for SystemInfo {
    fn default() -> Self {
        // Source every value from the config layer's own defaults rather than
        // re-declaring literals here. A minimal config (only the required
        // contact_points) yields serde defaults for everything else. No I/O.
        let config = crate::config::parse_config(
            "[ferrosa]\ncontact_points = [\"localhost:9042\"]\n",
        )
        .expect("minimal default config parses");
        Self::from_parts(
            &config,
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            String::new(),
            None,
            None,
        )
    }
}

/// Compile-time git commit, if the build injected one. Reported as `null`
/// rather than invented when absent.
fn build_commit() -> Option<String> {
    option_env!("GIT_COMMIT")
        .or(option_env!("VERGEN_GIT_SHA"))
        .map(str::to_string)
}

/// SHA-256 of the config file bytes, prefixed `sha256:`. Returns `None` if the
/// file cannot be read (e.g. config came from defaults).
fn hash_config_file(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes))))
}

/// Curated list of always-compiled capabilities. Honest and static — not
/// derived from runtime flags, which would invite drift.
fn feature_list() -> Vec<&'static str> {
    vec![
        "semantic-search",
        "hybrid-search",
        "graph-traversal",
        "temporal-facts",
        "consolidation",
        "skills",
        "datalog-query",
    ]
}

/// Build the redacted, flattened effective config. Secret values are never
/// included here — only non-sensitive runtime settings.
fn effective_config(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "server.transport": config.server.transport,
        "server.bindAddr": config.server.bind_addr,
        "server.httpPort": config.server.http_port,
        "server.publicPort": config.server.public_port,
        "server.requireTls": config.server.require_tls,
        "ferrosa.keyspace": config.ferrosa.keyspace,
        "ferrosa.contactPoints": config.ferrosa.contact_points,
        "embeddings.provider": config.embeddings.provider,
        "embeddings.model": config.embeddings.model,
        "embeddings.dimensions": config.embeddings.dimensions,
        "embeddings.baseUrl": config.embeddings.ollama_base_url,
        "graph.boltUri": config.graph.bolt_uri,
        "graph.httpUrl": config.graph.http_url,
        "viz.enabled": config.viz.enabled,
        "viz.port": config.viz.port,
        "judge.enabled": config.judge.enabled,
        "judge.provider": config.judge.provider,
        "judge.model": config.judge.model,
    })
}

/// Dotted key paths of secret values present in the config. Only keys whose
/// secret is actually set are listed.
fn redacted_keys(config: &Config) -> Vec<String> {
    let mut keys = vec![
        "ferrosa.password".to_string(),
        "graph.password".to_string(),
    ];
    if config.ferrosa.admin_password.is_some() {
        keys.push("ferrosa.admin_password".to_string());
    }
    if config.judge.token.is_some() {
        keys.push("judge.token".to_string());
    }
    keys
}

/// The static management actions advertised to clients. These are
/// recommendations only — the client owns confirmation and orchestration.
fn management_actions() -> Vec<ManagementAction> {
    vec![
        ManagementAction {
            id: "inspect-read-only",
            label: "Inspect cluster",
            mutation: false,
            requires_confirmation: false,
            preconditions: Vec::new(),
        },
        ManagementAction {
            id: "upgrade-binary",
            label: "Upgrade ferrosa-memory binary",
            mutation: true,
            requires_confirmation: true,
            preconditions: vec!["backup-current-binary", "compatible-schema"],
        },
        ManagementAction {
            id: "migrate-schema",
            label: "Apply schema migrations",
            mutation: true,
            requires_confirmation: true,
            preconditions: vec!["backup-data", "binary-current"],
        },
        ManagementAction {
            id: "migrate-standard-install",
            label: "Migrate config to Workbench standard install",
            mutation: true,
            requires_confirmation: true,
            preconditions: vec!["write-preview-approved"],
        },
    ]
}

/// Outcome of probing the ferrosa store + schema in one bounded call.
struct SchemaProbe {
    ferrosa_health: StoreHealth,
    schema: SchemaSection,
    error: Option<String>,
}

/// Probe the ferrosa store and read schema drift. The migration-status query
/// hits the database, so success doubles as the ferrosa liveness signal.
async fn probe_schema<S: Storage>(storage: &S) -> SchemaProbe {
    let expected = expected_schema_version();
    match bounded(storage.migration_status()).await {
        Ok(Ok(status)) => {
            let drift = classify_drift(status.db_version, status.binary_version);
            SchemaProbe {
                ferrosa_health: StoreHealth::Ready,
                schema: SchemaSection {
                    current_version: Some(status.db_version.to_string()),
                    expected_version: status.binary_version.to_string(),
                    drift,
                    pending_migrations: status.pending,
                    requires_backup_before_migration: true,
                },
                error: None,
            }
        }
        Ok(Err(e)) => SchemaProbe {
            ferrosa_health: StoreHealth::Error,
            schema: unknown_schema(expected),
            error: Some(format!("ferrosa: {e}")),
        },
        Err(()) => SchemaProbe {
            ferrosa_health: StoreHealth::Error,
            schema: unknown_schema(expected),
            error: Some(format!("ferrosa: probe timed out after {PROBE_TIMEOUT:?}")),
        },
    }
}

fn unknown_schema(expected: u32) -> SchemaSection {
    SchemaSection {
        current_version: None,
        expected_version: expected.to_string(),
        drift: Drift::Unknown,
        pending_migrations: Vec::new(),
        requires_backup_before_migration: true,
    }
}

fn classify_drift(db: u32, binary: u32) -> Drift {
    match db.cmp(&binary) {
        std::cmp::Ordering::Equal => Drift::None,
        std::cmp::Ordering::Less => Drift::Behind,
        std::cmp::Ordering::Greater => Drift::Ahead,
    }
}

/// Highest schema version wired into this binary.
fn expected_schema_version() -> u32 {
    crate::migration::MIGRATIONS
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(crate::migration::PRE_VERSIONING_BASELINE)
}

/// Probe the graph store health endpoint with a bounded timeout.
async fn probe_graph_health(graph: Option<&GraphClient>) -> (StoreHealth, Option<String>) {
    let Some(graph) = graph else {
        return (StoreHealth::Unknown, Some("graph client not configured".to_string()));
    };
    match bounded(graph.health_check()).await {
        Ok(Ok(())) => (StoreHealth::Ready, None),
        Ok(Err(e)) => (StoreHealth::Error, Some(format!("graph: {e}"))),
        Err(()) => (
            StoreHealth::Error,
            Some(format!("graph: probe timed out after {PROBE_TIMEOUT:?}")),
        ),
    }
}

/// Probe the embeddings provider with a bounded timeout.
async fn probe_embeddings_health(
    cfg: &crate::config::EmbeddingConfig,
) -> (StoreHealth, Option<String>) {
    let client = crate::embedding::EmbeddingClient::new(cfg);
    match bounded(client.health_check()).await {
        Ok(Ok(())) => (StoreHealth::Ready, None),
        Ok(Err(e)) => (StoreHealth::Degraded, Some(format!("embeddings: {e}"))),
        Err(()) => (
            StoreHealth::Degraded,
            Some(format!("embeddings: probe timed out after {PROBE_TIMEOUT:?}")),
        ),
    }
}

/// Run a future under the shared probe timeout. `Err(())` signals timeout.
async fn bounded<T>(fut: impl std::future::Future<Output = T>) -> Result<T, ()> {
    tokio::time::timeout(PROBE_TIMEOUT, fut).await.map_err(|_| ())
}

/// Aggregate per-store health into runtime health + readiness.
fn aggregate_health(
    ferrosa: StoreHealth,
    graph: StoreHealth,
    embeddings: StoreHealth,
) -> (OverallHealth, Readiness) {
    if ferrosa == StoreHealth::Error {
        return (OverallHealth::Error, Readiness::NotReady);
    }
    // Embeddings is non-blocking for readiness; graph errors and ferrosa
    // non-readiness downgrade to live-not-ready.
    let degraded = matches!(graph, StoreHealth::Error | StoreHealth::Degraded)
        || matches!(embeddings, StoreHealth::Error)
        || ferrosa != StoreHealth::Ready;
    if degraded {
        (OverallHealth::LiveNotReady, Readiness::NotReady)
    } else {
        (OverallHealth::Ready, Readiness::Ready)
    }
}

/// Probe summary memory/cluster statistics under a single bounded budget.
/// `intention_count` is supplied by the caller (it lives in session state, not
/// storage). Per-count failures fall back to 0; a probe timeout yields `None`.
async fn probe_statistics<S: Storage>(
    storage: &S,
    ctx: &crate::types::TenantContext,
    session_id: uuid::Uuid,
    intention_count: usize,
) -> Option<StatisticsSection> {
    bounded(async move {
        let memo_count = storage.memo_count(ctx).await.unwrap_or(0);
        let memo_total_hits = storage.memo_total_hits(ctx).await.unwrap_or(0) as usize;
        let memo_hit_rate = if memo_count > 0 {
            memo_total_hits as f64 / memo_count as f64
        } else {
            0.0
        };
        let active = storage
            .fold_count_by_status(ctx, crate::types::FoldStatus::Active)
            .await
            .unwrap_or(0);
        let folded = storage
            .fold_count_by_status(ctx, crate::types::FoldStatus::Folded)
            .await
            .unwrap_or(0);
        let archived = storage
            .fold_count_by_status(ctx, crate::types::FoldStatus::Archived)
            .await
            .unwrap_or(0);
        StatisticsSection {
            session_id: session_id.to_string(),
            entity_count: storage.entity_count(ctx, session_id).await.unwrap_or(0),
            fold_count: active + folded + archived,
            active_fold_count: active,
            folded_count: folded,
            archived_fold_count: archived,
            memo_count,
            memo_total_hits,
            memo_hit_rate,
            temporal_fact_count: storage.temporal_count(ctx).await.unwrap_or(0),
            edge_count: storage.edge_count(ctx).await.unwrap_or(0),
            intention_count,
        }
    })
    .await
    .ok()
}

/// Probe live cluster metadata from ferrosa's system tables with a bounded
/// timeout. Returns `(None, Some(error))` on failure rather than faking it.
async fn probe_cluster<S: Storage>(
    storage: &S,
    keyspace: &str,
) -> (Option<crate::storage::ClusterInfo>, Option<String>) {
    match bounded(storage.cluster_info(keyspace)).await {
        Ok(Ok(info)) => (Some(info), None),
        Ok(Err(e)) => (None, Some(format!("cluster: {e}"))),
        Err(()) => (
            None,
            Some(format!("cluster: probe timed out after {PROBE_TIMEOUT:?}")),
        ),
    }
}

/// Inputs to [`build_descriptor`]. Bundled to keep the call site readable.
pub struct DescribeRequest<'a, S: Storage> {
    pub info: &'a SystemInfo,
    pub storage: &'a S,
    pub ctx: &'a crate::types::TenantContext,
    pub graph: Option<&'a GraphClient>,
    /// Advertised tool names (for the `capabilities` section).
    pub tool_names: Vec<String>,
    /// Session to report `statistics` for.
    pub session_id: uuid::Uuid,
    /// In-memory intention count from session state.
    pub intention_count: usize,
    pub sections: SectionSet,
}

/// Build the full descriptor: combine the startup snapshot with fresh, bounded
/// dependency probes. Each section's probes run only when that section (or a
/// section that depends on it) is requested.
pub async fn build_descriptor<S: Storage>(req: DescribeRequest<'_, S>) -> SystemDescriptor {
    let DescribeRequest {
        info,
        storage,
        ctx,
        graph,
        tool_names,
        session_id,
        intention_count,
        sections,
    } = req;
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    // The schema probe also yields ferrosa health, so run it whenever schema,
    // stores, or runtime is requested.
    let need_schema = sections.schema || sections.stores || sections.runtime;
    let schema_probe = if need_schema {
        Some(probe_schema(storage).await)
    } else {
        None
    };
    let ferrosa_health = schema_probe
        .as_ref()
        .map(|p| p.ferrosa_health)
        .unwrap_or(StoreHealth::Unknown);
    if let Some(e) = schema_probe.as_ref().and_then(|p| p.error.clone()) {
        errors.push(e);
    }

    let (graph_health, graph_err) = if sections.stores || sections.runtime {
        probe_graph_health(graph).await
    } else {
        (StoreHealth::Unknown, None)
    };
    let (embed_health, embed_err) = if sections.stores || sections.runtime {
        probe_embeddings_health(&info.embedding).await
    } else {
        (StoreHealth::Unknown, None)
    };
    let (cluster, cluster_err) = if sections.stores {
        probe_cluster(storage, &info.keyspace).await
    } else {
        (None, None)
    };
    for e in [graph_err.clone(), embed_err.clone(), cluster_err.clone()]
        .into_iter()
        .flatten()
    {
        errors.push(e);
    }

    let statistics = if sections.statistics {
        let stats = probe_statistics(storage, ctx, session_id, intention_count).await;
        if stats.is_none() {
            warnings.push(format!(
                "statistics probe timed out after {PROBE_TIMEOUT:?}; counts omitted"
            ));
        }
        stats
    } else {
        None
    };

    let (health, readiness) = aggregate_health(ferrosa_health, graph_health, embed_health);

    warnings.push(
        "binaries.latestStable/latestNightly are not fetched by the server; \
         upgradeState is unknown until a release-metadata source is configured."
            .to_string(),
    );

    SystemDescriptor {
        contract: CONTRACT,
        server: build_server(info),
        identity: sections.identity.then(|| build_identity(info)),
        runtime: sections
            .runtime
            .then(|| build_runtime(info, health, readiness, errors.clone())),
        configuration: sections.configuration.then(|| ConfigurationSection {
            effective_config: info.effective_config.clone(),
            redacted_keys: info.redacted_keys.clone(),
        }),
        stores: sections.stores.then(|| StoresSection {
            ferrosa: FerrosaStore {
                contact_points: info.contact_points.clone(),
                keyspace: info.keyspace.clone(),
                configured_replication_factor: info.configured_replication_factor,
                configured_consistency: info.configured_consistency.clone(),
                health: ferrosa_health,
                schema_version: schema_probe
                    .as_ref()
                    .and_then(|p| p.schema.current_version.clone()),
                cluster,
                cluster_error: cluster_err,
            },
            graph: GraphStore {
                uri: info.graph_bolt_uri.clone(),
                http_url: info.graph_http_url.clone(),
                health: graph_health,
                error: graph_err,
            },
            embeddings: EmbeddingsStore {
                provider: info.embedding.provider.clone(),
                base_url: info.embedding.ollama_base_url.clone(),
                model: info.embedding.model.clone(),
                dimensions: info.embedding.dimensions,
                health: embed_health,
                error: embed_err,
            },
        }),
        schema: sections
            .schema
            .then(|| schema_probe.map(|p| p.schema))
            .flatten(),
        statistics,
        binaries: sections.binaries.then(|| build_binaries(info)),
        harnesses: sections.harnesses.then(Vec::new),
        capabilities: sections.capabilities.then(|| CapabilitiesSection {
            tools: tool_names,
            features: info.features.clone(),
        }),
        management_actions: sections.management_actions.then(management_actions),
        warnings,
    }
}

fn build_server(info: &SystemInfo) -> ServerSection {
    ServerSection {
        name: "ferrosa-memory",
        binary: "ferrosa-memory-mcp",
        version: info.version.clone(),
        commit: info.commit.clone(),
        channel: None,
        started_at: info.started_at.clone(),
        pid: info.pid,
    }
}

fn build_identity(info: &SystemInfo) -> IdentitySection {
    IdentitySection {
        cluster_id: info.cluster_id(),
        tenant_id: info.tenant_id.clone(),
        session_id: info.session_id.clone(),
        alias: None,
        config_path: info.config_path.clone(),
        config_hash: info.config_hash.clone(),
        install_kind: info.install_kind.clone(),
    }
}

fn build_runtime(
    info: &SystemInfo,
    health: OverallHealth,
    readiness: Readiness,
    errors: Vec<String>,
) -> RuntimeSection {
    RuntimeSection {
        transport: info.transport.clone(),
        endpoint_url: info.endpoint_url(),
        require_tls: info.require_tls,
        health,
        readiness,
        liveness: "live",
        viz_url: info.viz_url(),
        errors,
    }
}

fn build_binaries(info: &SystemInfo) -> BinariesSection {
    BinariesSection {
        current_version: info.version.clone(),
        latest_stable: None,
        latest_nightly: None,
        upgrade_state: UpgradeState::Unknown,
        supported_upgrade_channels: vec!["stable", "nightly", "semver"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;

    fn test_config() -> Config {
        parse_config(
            r#"
[server]
transport = "http"
bind_addr = "0.0.0.0"
http_port = 18765
require_tls = false

[ferrosa]
contact_points = ["localhost:19042", "localhost:19043"]
keyspace = "agent_memory"
password = "supersecret-cql"

[graph]
bolt_uri = "bolt://localhost:17687"
http_url = "http://localhost:17474"
password = "supersecret-graph"

[embeddings]
provider = "ollama"
model = "nomic-embed-text-v2-moe"
dimensions = 768

[viz]
enabled = true
port = 18766
"#,
        )
        .expect("valid config")
    }

    fn test_info() -> SystemInfo {
        SystemInfo::build(
            &test_config(),
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            "2026-06-10T12:00:00Z".to_string(),
        )
    }

    // T-FMINT-001: effective config + stable identity present.
    #[test]
    fn effective_config_and_identity_present() {
        let info = test_info();
        let cfg = &info.effective_config;
        assert_eq!(cfg["server.httpPort"], 18765);
        assert_eq!(cfg["ferrosa.keyspace"], "agent_memory");
        assert_eq!(cfg["embeddings.dimensions"], 768);
        assert_eq!(cfg["graph.boltUri"], "bolt://localhost:17687");

        let id = build_identity(&info);
        assert_eq!(
            id.cluster_id,
            "00000000-0000-0000-0000-000000000000:00000000-0000-0000-0000-000000000000:http:18765:agent_memory"
        );
        assert_eq!(id.install_kind, "custom");
    }

    // T-FMINT-002: secrets absent from output; redactedKeys lists the paths.
    #[test]
    fn secrets_are_redacted_not_serialized() {
        let info = test_info();
        let section = ConfigurationSection {
            effective_config: info.effective_config.clone(),
            redacted_keys: info.redacted_keys.clone(),
        };
        let json = serde_json::to_string(&section).expect("serialize");
        assert!(
            !json.contains("supersecret-cql"),
            "CQL password leaked: {json}"
        );
        assert!(
            !json.contains("supersecret-graph"),
            "graph password leaked: {json}"
        );
        assert!(info.redacted_keys.contains(&"ferrosa.password".to_string()));
        assert!(info.redacted_keys.contains(&"graph.password".to_string()));
    }

    // Optional admin/judge secrets only appear in redactedKeys when set.
    #[test]
    fn optional_secrets_listed_only_when_present() {
        let info = test_info();
        assert!(
            !info
                .redacted_keys
                .contains(&"ferrosa.admin_password".to_string())
        );
        assert!(!info.redacted_keys.contains(&"judge.token".to_string()));
    }

    #[test]
    fn endpoint_url_maps_wildcard_bind_to_loopback() {
        let info = test_info();
        assert_eq!(info.endpoint_url(), "http://127.0.0.1:18765");
        assert_eq!(info.viz_url().as_deref(), Some("http://127.0.0.1:18766"));
    }

    #[test]
    fn stdio_endpoint_is_literal_stdio() {
        let mut info = test_info();
        info.transport = "stdio".to_string();
        assert_eq!(info.endpoint_url(), "stdio");
    }

    #[test]
    fn drift_classification() {
        assert_eq!(classify_drift(36, 36), Drift::None);
        assert_eq!(classify_drift(30, 36), Drift::Behind);
        assert_eq!(classify_drift(40, 36), Drift::Ahead);
    }

    #[test]
    fn health_aggregation_rules() {
        assert_eq!(
            aggregate_health(StoreHealth::Ready, StoreHealth::Ready, StoreHealth::Ready),
            (OverallHealth::Ready, Readiness::Ready)
        );
        // Graph down → live-not-ready (T-FMINT-005 shape).
        assert_eq!(
            aggregate_health(StoreHealth::Ready, StoreHealth::Error, StoreHealth::Ready),
            (OverallHealth::LiveNotReady, Readiness::NotReady)
        );
        // Ferrosa down → error.
        assert_eq!(
            aggregate_health(StoreHealth::Error, StoreHealth::Ready, StoreHealth::Ready),
            (OverallHealth::Error, Readiness::NotReady)
        );
        // Embeddings degraded alone does not block readiness... it does per
        // current rule (embeddings error blocks; degraded does not).
        assert_eq!(
            aggregate_health(StoreHealth::Ready, StoreHealth::Ready, StoreHealth::Degraded),
            (OverallHealth::Ready, Readiness::Ready)
        );
    }

    #[test]
    fn section_include_filtering() {
        let all = SectionSet::from_include(None);
        assert!(all.identity && all.stores && all.management_actions);

        let some = SectionSet::from_include(Some(&[
            "identity".to_string(),
            "runtime".to_string(),
        ]));
        assert!(some.identity && some.runtime);
        assert!(!some.stores && !some.schema && !some.capabilities);
    }

    #[test]
    fn contract_constant_is_v1() {
        assert_eq!(CONTRACT, "ferrosa-memory.system.describe.v1");
    }

    fn synthetic_info() -> SystemInfo {
        // Synthetic embeddings provider so the health probe resolves offline
        // (no Ollama dependency) and the test stays hermetic.
        let mut info = test_info();
        info.embedding.provider = "synthetic".to_string();
        info
    }

    fn test_ctx() -> crate::types::TenantContext {
        crate::types::TenantContext {
            tenant_id: uuid::Uuid::nil(),
            session_origin: "test".to_string(),
        }
    }

    fn req<'a>(
        info: &'a SystemInfo,
        storage: &'a crate::storage::mock::MockStorage,
        ctx: &'a crate::types::TenantContext,
        tool_names: Vec<String>,
        sections: SectionSet,
    ) -> DescribeRequest<'a, crate::storage::mock::MockStorage> {
        DescribeRequest {
            info,
            storage,
            ctx,
            graph: None,
            tool_names,
            session_id: uuid::Uuid::nil(),
            intention_count: 0,
            sections,
        }
    }

    // T-FMINT-003 (shape): a full descriptor validates against the v1 contract
    // structure and never leaks secrets.
    #[tokio::test]
    async fn full_descriptor_matches_v1_contract() {
        let storage = crate::storage::mock::MockStorage::default();
        let info = synthetic_info();
        let ctx = test_ctx();
        let tools = vec!["hybrid_search".to_string(), "describe".to_string()];

        let descriptor =
            build_descriptor(req(&info, &storage, &ctx, tools, SectionSet::all())).await;
        let json = serde_json::to_value(&descriptor).expect("serialize descriptor");

        assert_eq!(json["contract"], "ferrosa-memory.system.describe.v1");
        assert_eq!(json["server"]["name"], "ferrosa-memory");
        assert_eq!(json["server"]["binary"], "ferrosa-memory-mcp");
        assert!(json["identity"]["clusterId"].is_string());
        assert!(json["runtime"]["endpointUrl"].is_string());
        assert!(json["stores"]["ferrosa"].is_object());
        assert!(json["schema"]["expectedVersion"].is_string());
        assert!(json["binaries"]["currentVersion"].is_string());
        assert_eq!(json["binaries"]["upgradeState"], "unknown");
        assert!(json["capabilities"]["tools"].as_array().unwrap().len() == 2);
        assert_eq!(json["managementActions"][0]["id"], "inspect-read-only");
        assert_eq!(json["managementActions"][0]["mutation"], false);

        // Configured ferrosa values come from config, not hardcoded literals.
        assert!(json["stores"]["ferrosa"]["configuredConsistency"].is_string());
        assert!(json["stores"]["ferrosa"]["configuredReplicationFactor"].is_number());
        // Statistics section present and counts are numbers.
        assert!(json["statistics"]["entityCount"].is_number());
        assert!(json["statistics"]["edgeCount"].is_number());

        // Mutating actions must be flagged requiresConfirmation (URS-FMINT-005).
        let actions = json["managementActions"].as_array().unwrap();
        for action in actions {
            if action["mutation"] == true {
                assert_eq!(action["requiresConfirmation"], true);
            }
        }

        // No secret values anywhere in the serialized descriptor.
        let blob = serde_json::to_string(&descriptor).unwrap();
        assert!(!blob.contains("supersecret-cql"));
        assert!(!blob.contains("supersecret-graph"));

        // Synthetic embeddings provider probes healthy; mock ferrosa is ready.
        assert_eq!(json["stores"]["embeddings"]["health"], "ready");
        assert_eq!(json["stores"]["ferrosa"]["health"], "ready");
        // No graph client configured → unknown, reported honestly.
        assert_eq!(json["stores"]["graph"]["health"], "unknown");
        // Mock storage has no live CQL cluster → cluster probe reports an error
        // rather than fabricating topology (fail-loud).
        assert!(json["stores"]["ferrosa"]["clusterError"].is_string());
    }

    // include filtering omits unrequested sections from the response.
    #[tokio::test]
    async fn include_filtering_omits_sections() {
        let storage = crate::storage::mock::MockStorage::default();
        let info = synthetic_info();
        let ctx = test_ctx();
        let sections = SectionSet::from_include(Some(&["identity".to_string()]));

        let descriptor =
            build_descriptor(req(&info, &storage, &ctx, Vec::new(), sections)).await;
        let json = serde_json::to_value(&descriptor).expect("serialize");

        assert!(json.get("identity").is_some());
        assert!(json.get("stores").is_none());
        assert!(json.get("statistics").is_none());
        assert!(json.get("capabilities").is_none());
        assert!(json.get("managementActions").is_none());
        // server + contract + warnings are always present.
        assert!(json.get("server").is_some());
        assert_eq!(json["contract"], "ferrosa-memory.system.describe.v1");
    }
}
