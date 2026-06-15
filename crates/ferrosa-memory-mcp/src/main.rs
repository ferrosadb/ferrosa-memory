//! # ferrosa-memory-mcp
//!
//! MCP server binary that exposes Ferrosa's memory tools via stdio or HTTP+SSE.
//!
//! Connects to a real Ferrosa cluster via CQL (cdrs-tokio). If the initial
//! connection fails, starts serving immediately with a "reconnecting" backend
//! that returns errors, while a background task retries with exponential backoff.
//! Never falls back to mock storage — mock silently loses data.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ferrosa_memory_core::auth;
use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::config::{Config, validate_shared_http_config};
use ferrosa_memory_core::context_segment::{ContextSegment, TemporalEdge};
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::dispatch;
use ferrosa_memory_core::graph::GraphClient;
use ferrosa_memory_core::http;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::transport;
use ferrosa_memory_core::types::*;
use futures_util::StreamExt;
use scylla::frame::response::result::{CqlValue, Row};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

const SPARQL_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Generate a 32-byte random key for signing stateless `forget` tokens.
///
/// No `rand` dependency is in this crate, so we derive the bytes from two
/// fresh v4 UUIDs (16 random bytes each). The key lives only in process memory
/// and is regenerated on each server start, so outstanding tokens are
/// invalidated by a restart.
fn random_forget_token_key() -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    key
}

/// Storage wrapper that holds an `Option<CqlStorage>` behind a `RwLock`.
///
/// When `inner` is `None`, the server is still reconnecting — all Storage
/// methods return a descriptive error. Once connected, the background task
/// swaps in `Some(cql)` and all subsequent calls route to the real backend.
///
/// If a connected session starts returning connection errors (e.g. rolling
/// restart), the delegate macro marks it disconnected and signals the
/// reconnect watcher to re-establish the session.
///
/// ## Cancel safety
///
/// A generation counter prevents a stale connection-error callback from
/// disconnecting a freshly reconnected session. `mark_disconnected` only
/// nulls the session when the generation matches the one captured before
/// the failed query. `notify_one` is called while the write lock is still
/// held so the signal cannot be lost to cancellation.
struct ReconnectingStorage {
    inner: RwLock<Option<Arc<CqlStorage>>>,
    /// Monotonically increasing generation — bumped on every `set_connected`.
    /// Prevents stale errors from disconnecting a fresh session.
    generation: AtomicU64,
    /// Signalled when a connection error is detected, waking the reconnect loop.
    reconnect_signal: tokio::sync::Notify,
    /// Config needed to reconnect (stashed at creation time).
    cql_config: FerrosaCqlConfig,
    /// Whether this storage owner is responsible for schema migrations before
    /// opening a runtime CQL session. Secondary readers rely on the primary
    /// watcher so startup DDL is ordered and not duplicated.
    run_migrations_on_connect: bool,
    graph: Option<Arc<GraphClient>>,
    sparql: Option<SparqlPassthrough>,
}

#[derive(Clone)]
struct SparqlPassthrough {
    http_url: String,
    username: String,
    password: String,
    client: reqwest::Client,
}

impl SparqlPassthrough {
    fn new(http_url: String, username: String, password: String) -> Self {
        Self {
            http_url,
            username,
            password,
            client: reqwest::Client::new(),
        }
    }

    fn query_url(&self) -> String {
        if self.http_url.ends_with("/sparql") {
            self.http_url.clone()
        } else {
            format!("{}/sparql", self.http_url.trim_end_matches('/'))
        }
    }
}

async fn read_sparql_response_body_bounded(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));

    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("SPARQL response exceeded {max_bytes} bytes");
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|e| anyhow::anyhow!("SPARQL response is not UTF-8: {e}"))
}

struct LimitedSparqlResult {
    columns: serde_json::Value,
    rows: Vec<serde_json::Value>,
    total_rows: usize,
    truncated: bool,
}

fn parse_sparql_response_limited(body: &str, limit: usize) -> anyhow::Result<LimitedSparqlResult> {
    let mut deserializer = serde_json::Deserializer::from_str(body);
    SparqlResponseSeed { limit }
        .deserialize(&mut deserializer)
        .map_err(anyhow::Error::from)
}

struct SparqlResponseSeed {
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for SparqlResponseSeed {
    type Value = LimitedSparqlResult;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SparqlResponseVisitor { limit: self.limit })
    }
}

struct SparqlResponseVisitor {
    limit: usize,
}

impl<'de> Visitor<'de> for SparqlResponseVisitor {
    type Value = LimitedSparqlResult;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a SPARQL JSON results object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut columns = serde_json::Value::Array(Vec::new());
        let mut rows = Vec::with_capacity(self.limit.min(1024));
        let mut total_rows = 0usize;
        let mut truncated = false;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "head" => {
                    let head = map.next_value::<serde_json::Value>()?;
                    columns = head
                        .get("vars")
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
                }
                "results" => {
                    let parsed = map.next_value_seed(SparqlResultsSeed { limit: self.limit })?;
                    rows = parsed.rows;
                    total_rows = parsed.total_rows;
                    truncated = parsed.truncated;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(LimitedSparqlResult {
            columns,
            rows,
            total_rows,
            truncated,
        })
    }
}

struct ParsedBindings {
    rows: Vec<serde_json::Value>,
    total_rows: usize,
    truncated: bool,
}

struct SparqlResultsSeed {
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for SparqlResultsSeed {
    type Value = ParsedBindings;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SparqlResultsVisitor { limit: self.limit })
    }
}

struct SparqlResultsVisitor {
    limit: usize,
}

impl<'de> Visitor<'de> for SparqlResultsVisitor {
    type Value = ParsedBindings;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a SPARQL results object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut parsed = ParsedBindings {
            rows: Vec::with_capacity(self.limit.min(1024)),
            total_rows: 0,
            truncated: false,
        };

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "bindings" => {
                    parsed = map.next_value_seed(SparqlBindingsSeed { limit: self.limit })?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(parsed)
    }
}

struct SparqlBindingsSeed {
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for SparqlBindingsSeed {
    type Value = ParsedBindings;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(SparqlBindingsVisitor { limit: self.limit })
    }
}

struct SparqlBindingsVisitor {
    limit: usize,
}

impl<'de> Visitor<'de> for SparqlBindingsVisitor {
    type Value = ParsedBindings;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a SPARQL bindings array")
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut rows = Vec::with_capacity(self.limit.min(1024));
        let mut total_rows = 0usize;

        while rows.len() < self.limit {
            let Some(row) = seq.next_element::<serde_json::Value>()? else {
                return Ok(ParsedBindings {
                    rows,
                    total_rows,
                    truncated: false,
                });
            };
            rows.push(row);
            total_rows += 1;
        }

        let mut truncated = false;
        while seq.next_element::<IgnoredAny>()?.is_some() {
            total_rows += 1;
            truncated = true;
        }

        Ok(ParsedBindings {
            rows,
            total_rows,
            truncated,
        })
    }
}

impl ReconnectingStorage {
    /// Create in "reconnecting" state — no backend available yet.
    fn disconnected(
        config: FerrosaCqlConfig,
        graph: Option<Arc<GraphClient>>,
        sparql: Option<SparqlPassthrough>,
    ) -> Self {
        Self::disconnected_with_migration_mode(config, graph, sparql, true)
    }

    /// Create a reconnecting storage wrapper for an independent read path.
    ///
    /// Viz uses this to avoid sharing the primary MCP CQL session/RPC lanes
    /// with bulk ingest while still relying on the primary watcher to run
    /// ordered schema migrations.
    fn disconnected_secondary_reader(
        config: FerrosaCqlConfig,
        graph: Option<Arc<GraphClient>>,
        sparql: Option<SparqlPassthrough>,
    ) -> Self {
        Self::disconnected_with_migration_mode(config, graph, sparql, false)
    }

    fn disconnected_with_migration_mode(
        config: FerrosaCqlConfig,
        graph: Option<Arc<GraphClient>>,
        sparql: Option<SparqlPassthrough>,
        run_migrations_on_connect: bool,
    ) -> Self {
        Self {
            inner: RwLock::new(None),
            generation: AtomicU64::new(0),
            reconnect_signal: tokio::sync::Notify::new(),
            cql_config: config,
            run_migrations_on_connect,
            graph,
            sparql,
        }
    }

    /// Swap in a newly connected CQL backend and bump the generation.
    async fn set_connected(&self, cql: CqlStorage) {
        let mut guard = self.inner.write().await;
        *guard = Some(Arc::new(cql));
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Read the current generation (captured before a query, checked after).
    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn graph_client(&self) -> anyhow::Result<&Arc<GraphClient>> {
        self.graph
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("graph client is not configured"))
    }

    fn is_ready(&self) -> bool {
        self.inner
            .try_read()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    async fn current_cql(&self) -> Option<Arc<CqlStorage>> {
        self.inner.read().await.as_ref().cloned()
    }

    /// Mark as disconnected and signal the reconnect watcher, but only if the
    /// generation hasn't changed since the caller observed the error. This
    /// prevents a stale error from a pre-reconnect query from killing a fresh
    /// session.
    ///
    /// The signal is sent while the write lock is held so the pair
    /// (set-to-None, notify) is atomic w.r.t. cancellation.
    async fn mark_disconnected(&self, observed_gen: u64) {
        let mut guard = self.inner.write().await;
        let current = self.generation.load(Ordering::Acquire);
        if current != observed_gen {
            // A reconnect already happened — this is a stale error.
            return;
        }
        if guard.is_some() {
            tracing::warn!("CQL connection lost — entering reconnecting mode");
            *guard = None;
            // Signal while lock is held: even if this future is cancelled after
            // the signal, the None state is already visible to readers.
            self.reconnect_signal.notify_one();
        }
    }
}

/// Error returned when CQL is not yet connected.
const NOT_CONNECTED_MSG: &str = "CQL connection not yet established, retrying in background...";

/// Returns true if the error looks like a connection / transport failure
/// (as opposed to a query-level error like "table not found").
fn is_connection_error(err: impl std::fmt::Display) -> bool {
    let msg = err.to_string().to_lowercase();

    // Ferrosa/Cassandra query-level consistency failures are storage results,
    // not transport failures. Treating them as connection loss poisons the
    // shared MCP storage handle and can turn a best-effort read timeout into a
    // write-path outage for the rest of the tool call.
    if msg.contains("read timeout: cl=") || msg.contains("write timeout: cl=") {
        return false;
    }

    msg.contains("broken pipe")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("transport")
        || msg.contains("channel closed")
        || msg.contains("io error")
        || msg.contains("timed out")
        || msg.contains("not connected")
        || msg.contains("eof")
        // Stale prepared statements after node restart — need full reconnect
        // to re-prepare all statements.
        || msg.contains("column or udt property")
}

fn cql_cell_to_json(row: &Row, index: usize) -> serde_json::Value {
    let cell = match row.columns.get(index) {
        Some(c) => c,
        None => return serde_json::Value::Null,
    };
    match cell {
        None => serde_json::Value::Null,
        Some(CqlValue::Text(s)) | Some(CqlValue::Ascii(s)) => serde_json::Value::String(s.clone()),
        Some(CqlValue::BigInt(n)) => serde_json::Value::from(*n),
        Some(CqlValue::Counter(c)) => serde_json::Value::from(c.0),
        Some(CqlValue::Int(n)) => serde_json::Value::from(*n),
        Some(CqlValue::SmallInt(n)) => serde_json::Value::from(*n),
        Some(CqlValue::TinyInt(n)) => serde_json::Value::from(*n),
        Some(CqlValue::Boolean(b)) => serde_json::Value::Bool(*b),
        Some(CqlValue::Double(f)) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(CqlValue::Float(f)) => serde_json::Number::from_f64(f64::from(*f))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(CqlValue::Uuid(u)) => serde_json::Value::String(u.to_string()),
        Some(CqlValue::Timeuuid(u)) => serde_json::Value::String(u.to_string()),
        Some(CqlValue::Timestamp(ts)) => {
            // ts.0 is milliseconds since epoch (CqlTimestamp wraps i64)
            let millis = ts.0;
            serde_json::Value::String(
                chrono::DateTime::from_timestamp_millis(millis)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| format!("<ts:{millis}>")),
            )
        }
        Some(CqlValue::Blob(bytes)) => {
            serde_json::Value::String(format!("<blob:{} bytes>", bytes.len()))
        }
        other => serde_json::Value::String(format!("<{}>", cql_value_type_name(other))),
    }
}

fn cql_value_type_name(v: &Option<CqlValue>) -> &'static str {
    match v {
        None => "null",
        Some(CqlValue::Ascii(_)) => "ascii",
        Some(CqlValue::Boolean(_)) => "boolean",
        Some(CqlValue::Blob(_)) => "blob",
        Some(CqlValue::Counter(_)) => "counter",
        Some(CqlValue::Decimal(_)) => "decimal",
        Some(CqlValue::Double(_)) => "double",
        Some(CqlValue::Float(_)) => "float",
        Some(CqlValue::Int(_)) => "int",
        Some(CqlValue::BigInt(_)) => "bigint",
        Some(CqlValue::Text(_)) => "text",
        Some(CqlValue::Timestamp(_)) => "timestamp",
        Some(CqlValue::Uuid(_)) => "uuid",
        Some(CqlValue::Varint(_)) => "varint",
        Some(CqlValue::Timeuuid(_)) => "timeuuid",
        Some(CqlValue::Inet(_)) => "inet",
        Some(CqlValue::Date(_)) => "date",
        Some(CqlValue::Time(_)) => "time",
        Some(CqlValue::SmallInt(_)) => "smallint",
        Some(CqlValue::TinyInt(_)) => "tinyint",
        Some(CqlValue::Duration(_)) => "duration",
        Some(CqlValue::List(_)) => "list",
        Some(CqlValue::Map(_)) => "map",
        Some(CqlValue::Set(_)) => "set",
        Some(CqlValue::UserDefinedType { .. }) => "udt",
        Some(CqlValue::Tuple(_)) => "tuple",
        Some(CqlValue::Empty) => "empty",
    }
}

fn normalize_public_query(query: &str) -> anyhow::Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        anyhow::bail!("query must not be empty");
    }
    if trimmed.ends_with(';') {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("{trimmed};"))
    }
}

fn stdio_tenant_id(config: &Config) -> uuid::Uuid {
    config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .unwrap_or_else(uuid::Uuid::new_v4)
}

fn parse_session_id(source: &str, raw: &str) -> anyhow::Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw).map_err(|e| anyhow::anyhow!("{source} is not a valid UUID: {e}"))
}

fn configured_or_env_session_id(config: &Config) -> anyhow::Result<Option<uuid::Uuid>> {
    if let Some(session_id) = configured_session_id(config)? {
        return Ok(Some(session_id));
    }

    if let Ok(raw) = std::env::var("FERROSA_MEMORY_SESSION_ID")
        && !raw.trim().is_empty()
    {
        return parse_session_id("FERROSA_MEMORY_SESSION_ID", raw.trim()).map(Some);
    }

    Ok(None)
}

fn configured_session_id(config: &Config) -> anyhow::Result<Option<uuid::Uuid>> {
    config
        .server
        .session_id
        .as_deref()
        .map(|s| parse_session_id("[server] session_id", s))
        .transpose()
}

fn startup_default_session_id(config: &Config, repo: &str) -> anyhow::Result<uuid::Uuid> {
    if let Some(session_id) = configured_or_env_session_id(config)? {
        return Ok(session_id);
    }

    let external = std::env::var("FERROSA_MEMORY_AGENT_SESSION_ID")
        .ok()
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
        .or_else(|| std::env::var("CODEX_THREAD_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(external) = external {
        let agent = std::env::var("FERROSA_MEMORY_AGENT")
            .ok()
            .or_else(|| {
                std::env::var("CLAUDECODE")
                    .ok()
                    .map(|_| "claude".to_string())
            })
            .unwrap_or_else(|| "unknown-agent".to_string());
        let workspace = if repo.is_empty() {
            "unknown-workspace"
        } else {
            repo
        };
        return Ok(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ferrosa-memory:agent-session:v1:{agent}:{workspace}:{external}").as_bytes(),
        ));
    }

    Ok(uuid::Uuid::new_v4())
}

#[cfg(test)]
fn build_http_validator(config: &Config) -> anyhow::Result<Arc<http::CredentialValidator>> {
    validate_shared_http_config(config)?;
    let auth_file = config
        .server
        .auth_file
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("HTTP transport requires server.auth_file"))?;
    let validator = Arc::new(auth::FileAuthValidator::from_path(auth_file)?);
    Ok(Arc::new(move |user: &str, pass: &str| {
        validator.validate(user, pass)
    }))
}

/// Macro to delegate a Storage trait method through the RwLock.
///
/// Captures the generation before the query. On connection errors, calls
/// `mark_disconnected` with the captured generation so stale errors from
/// pre-reconnect queries cannot kill a fresh session.
macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let conn_gen = $self.current_generation();
        let cql = $self.current_cql().await;
        match cql {
            Some(cql) => {
                let result = cql.$method($($arg),*).await;
                if let Err(ref e) = result {
                    if is_connection_error(e) {
                        $self.mark_disconnected(conn_gen).await;
                    }
                }
                result
            }
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }};
}

/// Delegate all Storage methods through the `RwLock<Option<CqlStorage>>`.
impl Storage for ReconnectingStorage {
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>> {
        delegate!(self, memo_get, ctx, content_hash, model_version)
    }

    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()> {
        delegate!(self, memo_touch, ctx, content_hash, model_version)
    }

    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
        delegate!(self, memo_put, ctx, entry)
    }

    // Delegate to the live CQL session so schema status reflects the actual
    // database, not the binary default. When disconnected this fails loudly
    // (NOT_CONNECTED_MSG) rather than reporting a fabricated db_version.
    async fn migration_status(
        &self,
    ) -> anyhow::Result<ferrosa_memory_core::migration::MigrationStatus> {
        delegate!(self, migration_status)
    }

    // Live cluster metadata from the ferrosa system tables. Fails loudly when
    // disconnected rather than fabricating topology.
    async fn cluster_info(
        &self,
        keyspace: &str,
    ) -> anyhow::Result<ferrosa_memory_core::storage::ClusterInfo> {
        delegate!(self, cluster_info, keyspace)
    }

    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
        delegate!(self, plan_put, ctx, node)
    }

    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>> {
        delegate!(self, plan_get, ctx, session_id, max_depth)
    }

    async fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            plan_update_status,
            ctx,
            session_id,
            depth,
            subtask_id,
            status,
            outcome_summary
        )
    }

    async fn session_task_put(
        &self,
        ctx: &TenantContext,
        task: &SessionTask,
    ) -> anyhow::Result<()> {
        delegate!(self, session_task_put, ctx, task)
    }

    async fn session_task_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        task_id: uuid::Uuid,
    ) -> anyhow::Result<Option<SessionTask>> {
        delegate!(self, session_task_get, ctx, session_id, task_id)
    }

    async fn session_task_list(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        status: Option<SessionTaskStatus>,
    ) -> anyhow::Result<Vec<SessionTask>> {
        delegate!(self, session_task_list, ctx, session_id, status)
    }

    async fn session_task_alias_put(
        &self,
        ctx: &TenantContext,
        alias: &SessionTaskAlias,
    ) -> anyhow::Result<()> {
        delegate!(self, session_task_alias_put, ctx, alias)
    }

    async fn session_task_alias_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        alias_scope: &str,
        alias: &str,
    ) -> anyhow::Result<Option<SessionTaskAlias>> {
        delegate!(
            self,
            session_task_alias_get,
            ctx,
            session_id,
            alias_scope,
            alias
        )
    }

    async fn session_task_focus_set(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        entries: &[SessionTaskFocusEntry],
    ) -> anyhow::Result<()> {
        delegate!(self, session_task_focus_set, ctx, session_id, entries)
    }

    async fn session_task_focus_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<SessionTaskFocusEntry>> {
        delegate!(self, session_task_focus_get, ctx, session_id)
    }

    async fn session_task_event_put(
        &self,
        ctx: &TenantContext,
        event: &SessionTaskEvent,
    ) -> anyhow::Result<()> {
        delegate!(self, session_task_event_put, ctx, event)
    }

    async fn session_task_policy_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Option<SessionTaskPolicy>> {
        delegate!(self, session_task_policy_get, ctx, session_id)
    }

    async fn session_task_policy_put(
        &self,
        ctx: &TenantContext,
        policy: &SessionTaskPolicy,
    ) -> anyhow::Result<()> {
        delegate!(self, session_task_policy_put, ctx, policy)
    }

    async fn fold_put(&self, ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()> {
        delegate!(self, fold_put, ctx, entry)
    }

    async fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
    ) -> anyhow::Result<Option<FoldEntry>> {
        delegate!(self, fold_get, ctx, session_id, fold_id)
    }

    async fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
        text: &str,
    ) -> anyhow::Result<()> {
        delegate!(self, fold_append, ctx, session_id, fold_id, text)
    }

    async fn fold_complete(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        fold_id: uuid::Uuid,
        summary: &str,
        embedding: Vec<f32>,
        compression_ratio: f64,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            fold_complete,
            ctx,
            session_id,
            fold_id,
            summary,
            embedding,
            compression_ratio
        )
    }

    async fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> anyhow::Result<Vec<FoldSummary>> {
        delegate!(
            self,
            fold_search,
            ctx,
            session_id,
            query_embedding,
            k,
            include_raw
        )
    }

    async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()> {
        delegate!(self, entity_put, ctx, entry)
    }

    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        name: &str,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_find_phonetic, ctx, session_id, name)
    }

    async fn entity_find_by_exact_name(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        name: &str,
        entity_type: &str,
    ) -> anyhow::Result<Option<EntityEntry>> {
        delegate!(
            self,
            entity_find_by_exact_name,
            ctx,
            session_id,
            name,
            entity_type
        )
    }

    async fn entity_get_by_id(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Option<EntityEntry>> {
        delegate!(self, entity_get_by_id, ctx, session_id, entity_id)
    }

    async fn entity_get_batch(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        entity_ids: &[uuid::Uuid],
    ) -> anyhow::Result<Vec<EntityEntry>> {
        let conn_gen = self.current_generation();
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => {
                let result = cql.entity_get_batch(ctx, session_id, entity_ids).await;
                if let Err(ref e) = result
                    && is_connection_error(e)
                {
                    self.mark_disconnected(conn_gen).await;
                }
                result
            }
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }

    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_search_ann, ctx, session_id, query_embedding, k)
    }

    async fn entity_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, entity_count, ctx, session_id)
    }

    async fn entity_count_matching(
        &self,
        ctx: &TenantContext,
        query: EntityListQuery,
    ) -> anyhow::Result<usize> {
        delegate!(self, entity_count_matching, ctx, query)
    }

    async fn fold_count(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, fold_count, ctx, session_id)
    }

    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, memo_count, ctx)
    }

    async fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        state: MemoryState,
    ) -> anyhow::Result<()> {
        delegate!(self, entity_update_state, ctx, entity_id, state)
    }

    async fn entity_delete(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<bool> {
        delegate!(self, entity_delete, ctx, session_id, entity_id)
    }

    async fn retraction_put(
        &self,
        ctx: &TenantContext,
        rec: &RetractionRecord,
    ) -> anyhow::Result<()> {
        delegate!(self, retraction_put, ctx, rec)
    }

    async fn retraction_get_latest(
        &self,
        ctx: &TenantContext,
        object_id: uuid::Uuid,
    ) -> anyhow::Result<Option<RetractionRecord>> {
        delegate!(self, retraction_get_latest, ctx, object_id)
    }

    async fn retraction_list_purgeable(
        &self,
        ctx: &TenantContext,
        now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Vec<RetractionRecord>> {
        delegate!(self, retraction_list_purgeable, ctx, now)
    }

    async fn retraction_delete(
        &self,
        ctx: &TenantContext,
        object_id: uuid::Uuid,
        retracted_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        delegate!(self, retraction_delete, ctx, object_id, retracted_at)
    }

    async fn forget_journal_put(
        &self,
        ctx: &TenantContext,
        entry: &ForgetJournalEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, forget_journal_put, ctx, entry)
    }

    async fn forget_journal_update_status(
        &self,
        ctx: &TenantContext,
        forget_id: uuid::Uuid,
        status: &str,
        step_states_json: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            forget_journal_update_status,
            ctx,
            forget_id,
            status,
            step_states_json,
            updated_at
        )
    }

    async fn forget_journal_get(
        &self,
        ctx: &TenantContext,
        forget_id: uuid::Uuid,
    ) -> anyhow::Result<Option<ForgetJournalEntry>> {
        delegate!(self, forget_journal_get, ctx, forget_id)
    }

    async fn forget_journal_list_unfinished(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ForgetJournalEntry>> {
        delegate!(self, forget_journal_list_unfinished, ctx)
    }

    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_list_session, ctx, session_id)
    }

    async fn entity_counts_by_type_and_state(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<EntityTypeStateCount>> {
        delegate!(self, entity_counts_by_type_and_state, ctx, session_id)
    }

    async fn document_chunk_put(
        &self,
        ctx: &TenantContext,
        chunk: &DocumentChunk,
    ) -> anyhow::Result<()> {
        delegate!(self, document_chunk_put, ctx, chunk)
    }

    async fn document_chunk_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        chunk_id: uuid::Uuid,
    ) -> anyhow::Result<Option<DocumentChunk>> {
        delegate!(self, document_chunk_get, ctx, session_id, chunk_id)
    }

    async fn document_chunk_search_bm25(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query: &str,
        k: usize,
    ) -> anyhow::Result<Vec<DocumentChunk>> {
        delegate!(self, document_chunk_search_bm25, ctx, session_id, query, k)
    }

    async fn document_chunk_search_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query: &str,
        k: usize,
    ) -> anyhow::Result<Vec<DocumentChunk>> {
        delegate!(
            self,
            document_chunk_search_phonetic,
            ctx,
            session_id,
            query,
            k
        )
    }

    async fn document_chunk_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<DocumentChunk>> {
        delegate!(
            self,
            document_chunk_search_ann,
            ctx,
            session_id,
            query_embedding,
            k
        )
    }

    async fn entity_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>> {
        delegate!(self, entity_list_all, ctx)
    }

    async fn entity_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<EntityEntry>>>,
    ) {
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => cql.entity_stream_all(ctx, chunk_size, tx).await,
            None => {
                let _ = tx.send(Err(anyhow::anyhow!(NOT_CONNECTED_MSG))).await;
            }
        }
    }

    async fn fold_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::FoldEntry>> {
        delegate!(self, fold_list_all, ctx)
    }

    async fn temporal_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::TemporalEvent>> {
        delegate!(self, temporal_list_all, ctx)
    }

    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()> {
        delegate!(self, temporal_put, ctx, event)
    }

    async fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Option<TemporalEvent>> {
        delegate!(self, temporal_get_current, ctx, entity_id)
    }

    async fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        event_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, temporal_invalidate, ctx, entity_id, event_id)
    }

    async fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> anyhow::Result<()> {
        delegate!(self, feedback_put, ctx, outcome)
    }

    async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>> {
        let conn_gen = self.current_generation();
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => {
                let result = cql.feedback_list_all().await;
                if let Err(ref e) = result
                    && is_connection_error(e)
                {
                    self.mark_disconnected(conn_gen).await;
                }
                result
            }
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }

    async fn delete_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, delete_session, ctx, session_id)
    }

    async fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source: uuid::Uuid,
        target: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        self.graph_client()?
            .put_folded_into_edge(ctx.tenant_id, session, source, target)
            .await
    }

    async fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity: uuid::Uuid,
        fold: uuid::Uuid,
        session: uuid::Uuid,
    ) -> anyhow::Result<()> {
        self.graph_client()?
            .put_mentioned_in_edge(ctx.tenant_id, session, entity, fold)
            .await
    }

    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        a: uuid::Uuid,
        b: uuid::Uuid,
        session: uuid::Uuid,
        strength: f32,
    ) -> anyhow::Result<()> {
        self.graph_client()?
            .put_co_occurs_edge(ctx.tenant_id, session, a, b, strength)
            .await
    }

    async fn edge_prune_stale(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<usize> {
        let graph = self.graph_client()?;
        let edges = graph.list_co_occurs_edges(ctx.tenant_id).await?;
        let mut pruned = 0;
        for edge in edges {
            let is_stale = edge.last_reinforced.is_none_or(|ts| ts < cutoff);
            if !is_stale {
                continue;
            }
            graph
                .delete_co_occurs_edge(ctx.tenant_id, edge.src_id, edge.dst_id)
                .await?;
            pruned += 1;
        }
        Ok(pruned)
    }

    async fn edge_decay_weights(&self, ctx: &TenantContext, factor: f64) -> anyhow::Result<usize> {
        let graph = self.graph_client()?;
        let edges = graph.list_co_occurs_edges(ctx.tenant_id).await?;
        let mut decayed = 0;
        for edge in edges {
            graph
                .set_co_occurs_strength(
                    ctx.tenant_id,
                    edge.src_id,
                    edge.dst_id,
                    (f64::from(edge.strength) * factor) as f32,
                )
                .await?;
            decayed += 1;
        }
        Ok(decayed)
    }

    async fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_id: uuid::Uuid,
        old_id: uuid::Uuid,
        entity: uuid::Uuid,
    ) -> anyhow::Result<()> {
        self.graph_client()?
            .put_supersedes_edge(ctx.tenant_id, entity, new_id, old_id)
            .await
    }

    async fn edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid, String)>> {
        delegate!(self, edge_list_session, ctx, session_id)
    }

    async fn edge_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid, String)>> {
        delegate!(self, edge_list_all, ctx)
    }

    async fn edge_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<(uuid::Uuid, uuid::Uuid, String)>>>,
    ) {
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => cql.edge_stream_all(ctx, chunk_size, tx).await,
            None => {
                let _ = tx.send(Err(anyhow::anyhow!(NOT_CONNECTED_MSG))).await;
            }
        }
    }

    async fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<(uuid::Uuid, String)>> {
        delegate!(self, edge_list_for_entity, ctx, entity_id)
    }

    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &ferrosa_memory_core::intention::Intention,
    ) -> anyhow::Result<()> {
        delegate!(self, intention_put, ctx, intention)
    }

    async fn intention_list(
        &self,
        ctx: &TenantContext,
        repo: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::intention::Intention>> {
        delegate!(self, intention_list, ctx, repo)
    }

    async fn intention_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::intention::Intention>> {
        delegate!(self, intention_list_all, ctx)
    }

    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        repo: &str,
        id: uuid::Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()> {
        delegate!(
            self,
            intention_update_status,
            ctx,
            repo,
            id,
            status,
            triggered_at,
            completed_at
        )
    }

    async fn tool_usage_put(
        &self,
        ctx: &TenantContext,
        tool_name: &str,
        repo: &str,
        input_bytes: i32,
        output_bytes: i32,
        estimated_tokens: i32,
        latency_ms: i32,
        error: bool,
    ) -> anyhow::Result<()> {
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => {
                let result = cql
                    .tool_usage_put(
                        ctx,
                        tool_name,
                        repo,
                        input_bytes,
                        output_bytes,
                        estimated_tokens,
                        latency_ms,
                        error,
                    )
                    .await;
                if result.is_err() {
                    return Err(anyhow::anyhow!("tool usage logging failed"));
                }
                Ok(())
            }
            None => Err(anyhow::anyhow!(NOT_CONNECTED_MSG)),
        }
    }

    async fn tool_usage_query(
        &self,
        ctx: &TenantContext,
        day: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::ToolUsageRow>> {
        delegate!(self, tool_usage_query, ctx, day)
    }

    async fn audit_put(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::AuditEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, audit_put, ctx, entry)
    }

    async fn memo_total_hits(&self, ctx: &TenantContext) -> anyhow::Result<i64> {
        delegate!(self, memo_total_hits, ctx)
    }

    async fn fold_count_by_status(
        &self,
        ctx: &TenantContext,
        status: ferrosa_memory_core::types::FoldStatus,
    ) -> anyhow::Result<usize> {
        delegate!(self, fold_count_by_status, ctx, status)
    }

    async fn temporal_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, temporal_count, ctx)
    }

    async fn edge_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, edge_count, ctx)
    }

    // --- Warmth operations (Sprint 5) ---

    async fn warmth_get(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<Option<ferrosa_memory_core::types::WarmthEntry>> {
        delegate!(self, warmth_get, ctx, entity_id)
    }

    async fn warmth_put(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::WarmthEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, warmth_put, ctx, entry)
    }

    async fn warmth_boost(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        amount: f64,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, warmth_boost, ctx, entity_id, amount, session_id)
    }

    async fn warmth_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::WarmthEntry>> {
        delegate!(self, warmth_list_session, ctx, session_id)
    }

    async fn warmth_decay_all(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        elapsed_hours: f64,
    ) -> anyhow::Result<usize> {
        delegate!(self, warmth_decay_all, ctx, session_id, elapsed_hours)
    }

    async fn warmth_delete(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, warmth_delete, ctx, entity_id)
    }

    async fn context_segment_put(
        &self,
        ctx: &TenantContext,
        segment: &ContextSegment,
    ) -> anyhow::Result<()> {
        delegate!(self, context_segment_put, ctx, segment)
    }

    async fn context_segment_get(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        segment_id: uuid::Uuid,
    ) -> anyhow::Result<Option<ContextSegment>> {
        delegate!(self, context_segment_get, ctx, session_id, segment_id)
    }

    async fn context_segment_get_by_hash(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        content_hash: &str,
    ) -> anyhow::Result<Option<ContextSegment>> {
        delegate!(
            self,
            context_segment_get_by_hash,
            ctx,
            session_id,
            content_hash
        )
    }

    async fn context_segment_search_bm25(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query: &str,
        k: usize,
    ) -> anyhow::Result<Vec<ContextSegment>> {
        delegate!(self, context_segment_search_bm25, ctx, session_id, query, k)
    }

    async fn context_segment_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<ContextSegment>> {
        delegate!(
            self,
            context_segment_search_ann,
            ctx,
            session_id,
            query_embedding,
            k
        )
    }

    async fn temporal_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &TemporalEdge,
    ) -> anyhow::Result<()> {
        delegate!(self, temporal_edge_put, ctx, edge)
    }

    async fn temporal_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        src_id: uuid::Uuid,
        edge_type: &str,
    ) -> anyhow::Result<Vec<TemporalEdge>> {
        delegate!(
            self,
            temporal_edge_list_from,
            ctx,
            session_id,
            src_id,
            edge_type
        )
    }

    async fn confidence_put(
        &self,
        ctx: &TenantContext,
        score: &ferrosa_memory_core::types::ConfidenceScore,
    ) -> anyhow::Result<()> {
        delegate!(self, confidence_put, ctx, score)
    }

    async fn confidence_get(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
        fact_hash: &str,
    ) -> anyhow::Result<Option<ferrosa_memory_core::types::ConfidenceScore>> {
        delegate!(self, confidence_get, ctx, entity_id, fact_hash)
    }

    // --- Forget / cascade-cleanup operations (CQL-backed) ---

    async fn confidence_delete_by_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        delegate!(self, confidence_delete_by_entity, ctx, entity_id)
    }

    async fn temporal_delete_by_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, temporal_delete_by_entity, ctx, entity_id)
    }

    async fn provenance_delete_by_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, provenance_delete_by_entity, ctx, entity_id)
    }

    async fn derived_cache_delete_by_entity(
        &self,
        ctx: &TenantContext,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<usize> {
        delegate!(self, derived_cache_delete_by_entity, ctx, entity_id)
    }

    // --- Rule registry operations (Sprint 5) ---

    async fn rule_put(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::RuleEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, rule_put, ctx, entry)
    }

    async fn rule_list_family(
        &self,
        ctx: &TenantContext,
        family: &str,
        state: ferrosa_memory_core::types::RuleState,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::RuleEntry>> {
        delegate!(self, rule_list_family, ctx, family, state)
    }

    async fn rule_list_active(
        &self,
        ctx: &TenantContext,
        state: ferrosa_memory_core::types::RuleState,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::RuleEntry>> {
        delegate!(self, rule_list_active, ctx, state)
    }

    async fn rule_get(
        &self,
        ctx: &TenantContext,
        rule_id: &str,
    ) -> anyhow::Result<Option<ferrosa_memory_core::types::RuleEntry>> {
        delegate!(self, rule_get, ctx, rule_id)
    }

    async fn approval_append(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::ApprovalEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, approval_append, ctx, entry)
    }

    async fn approval_list(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::ApprovalEntry>> {
        delegate!(self, approval_list, ctx, artifact_kind, artifact_ref)
    }

    async fn approval_latest(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> anyhow::Result<Option<ferrosa_memory_core::types::ApprovalEntry>> {
        delegate!(self, approval_latest, ctx, artifact_kind, artifact_ref)
    }

    async fn alias_put(
        &self,
        ctx: &TenantContext,
        entry: &ferrosa_memory_core::types::AliasEntry,
    ) -> anyhow::Result<()> {
        delegate!(self, alias_put, ctx, entry)
    }

    async fn alias_list(
        &self,
        ctx: &TenantContext,
        alias_name: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::AliasEntry>> {
        delegate!(self, alias_list, ctx, alias_name)
    }

    // --- Derived cache operations (Sprint 5) ---

    async fn derived_cache_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::DerivedFact>> {
        delegate!(self, derived_cache_get, ctx, cache_key)
    }

    async fn derived_cache_get_limited(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::DerivedFact>> {
        delegate!(self, derived_cache_get_limited, ctx, cache_key, limit)
    }

    async fn derived_cache_stream(
        &self,
        ctx: TenantContext,
        cache_key: String,
        chunk_size: usize,
        limit: Option<usize>,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<ferrosa_memory_core::types::DerivedFact>>>,
    ) {
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => {
                cql.derived_cache_stream(ctx, cache_key, chunk_size, limit, tx)
                    .await
            }
            None => {
                let _ = tx.send(Err(anyhow::anyhow!(NOT_CONNECTED_MSG))).await;
            }
        }
    }

    async fn derived_cache_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[ferrosa_memory_core::types::DerivedFact],
    ) -> anyhow::Result<()> {
        delegate!(self, derived_cache_put, ctx, cache_key, facts)
    }

    async fn derived_cache_clear(&self, ctx: &TenantContext, pred: &str) -> anyhow::Result<()> {
        delegate!(self, derived_cache_clear, ctx, pred)
    }

    async fn derived_cache_list_all(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::DerivedFactRow>> {
        delegate!(self, derived_cache_list_all, ctx, limit)
    }

    async fn derived_cache_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        delegate!(self, derived_cache_count, ctx)
    }

    async fn derived_cache_ttl_track_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[ferrosa_memory_core::types::TtlTrackEntry],
    ) -> anyhow::Result<()> {
        delegate!(self, derived_cache_ttl_track_put, ctx, cache_key, facts)
    }

    async fn derived_cache_ttl_track_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<(i32, i32)>> {
        delegate!(self, derived_cache_ttl_track_get, ctx, cache_key)
    }

    // --- Provenance operations (Sprint 5) ---

    async fn provenance_put(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
        steps: &[ferrosa_memory_core::types::ProvenanceStep],
    ) -> anyhow::Result<()> {
        delegate!(self, provenance_put, ctx, derived_edge_id, steps)
    }

    async fn provenance_get(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
    ) -> anyhow::Result<Vec<ferrosa_memory_core::types::ProvenanceStep>> {
        delegate!(self, provenance_get, ctx, derived_edge_id)
    }

    // --- Heat telemetry operations (Sprint 5) ---

    async fn heat_record(
        &self,
        ctx: &TenantContext,
        pred: &str,
        hit: bool,
        compute_ms: Option<i64>,
    ) -> anyhow::Result<()> {
        delegate!(self, heat_record, ctx, pred, hit, compute_ms)
    }

    async fn heat_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
        days: u32,
    ) -> anyhow::Result<(i64, i64)> {
        delegate!(self, heat_get, ctx, pred, days)
    }

    async fn materialized_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &MaterializedEdge,
    ) -> anyhow::Result<()> {
        delegate!(self, materialized_edge_put, ctx, edge)
    }
    async fn materialized_edges_by_src(
        &self,
        ctx: &TenantContext,
        src_id: &str,
        pred: Option<&str>,
    ) -> anyhow::Result<Vec<MaterializedEdge>> {
        delegate!(self, materialized_edges_by_src, ctx, src_id, pred)
    }
    async fn materialized_edges_by_pred(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> anyhow::Result<Vec<MaterializedEdge>> {
        delegate!(self, materialized_edges_by_pred, ctx, pred)
    }
    async fn materialized_edges_clear(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> anyhow::Result<()> {
        delegate!(self, materialized_edges_clear, ctx, pred)
    }
    async fn promoted_predicate_get(
        &self,
        ctx: &TenantContext,
        pred: &str,
    ) -> anyhow::Result<Option<PromotedPredicate>> {
        delegate!(self, promoted_predicate_get, ctx, pred)
    }
    async fn promoted_predicate_put(
        &self,
        ctx: &TenantContext,
        entry: &PromotedPredicate,
    ) -> anyhow::Result<()> {
        delegate!(self, promoted_predicate_put, ctx, entry)
    }
    async fn promoted_predicate_list(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<PromotedPredicate>> {
        delegate!(self, promoted_predicate_list, ctx)
    }

    // --- Typed edge operations ---

    async fn typed_edge_put(&self, ctx: &TenantContext, edge: &TypedEdge) -> anyhow::Result<()> {
        self.graph_client()?
            .put_typed_edge(
                ctx.tenant_id,
                edge.session_id,
                edge.src_id,
                &edge.edge_type,
                edge.dst_id,
                edge.weight,
                edge.metadata.as_deref(),
            )
            .await
    }

    async fn typed_edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        delegate!(self, typed_edge_list_session, ctx, session_id)
    }

    async fn typed_edge_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TypedEdge>> {
        delegate!(self, typed_edge_list_all, ctx)
    }

    async fn typed_edge_stream_all(
        &self,
        ctx: TenantContext,
        chunk_size: usize,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<Vec<TypedEdge>>>,
    ) {
        let cql = self.current_cql().await;
        match cql {
            Some(cql) => cql.typed_edge_stream_all(ctx, chunk_size, tx).await,
            None => {
                let _ = tx.send(Err(anyhow::anyhow!(NOT_CONNECTED_MSG))).await;
            }
        }
    }

    async fn typed_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        src_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        delegate!(self, typed_edge_list_from, ctx, session_id, src_id)
    }

    async fn typed_edge_list_to(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        dst_id: uuid::Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        // Reads are CQL-backed (the typed_edges table); routing matches
        // typed_edge_list_from.
        delegate!(self, typed_edge_list_to, ctx, session_id, dst_id)
    }

    async fn typed_edge_delete(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        src_id: uuid::Uuid,
        edge_type: &str,
        dst_id: uuid::Uuid,
    ) -> anyhow::Result<bool> {
        let exists = self
            .typed_edge_list_from(ctx, session_id, src_id)
            .await?
            .into_iter()
            .any(|edge| edge.dst_id == dst_id && edge.edge_type == edge_type);
        if !exists {
            return Ok(false);
        }
        self.graph_client()?
            .delete_typed_edge(ctx.tenant_id, session_id, src_id, edge_type, dst_id)
            .await?;
        Ok(true)
    }

    async fn delete_entity_node(
        &self,
        ctx: &TenantContext,
        session_id: uuid::Uuid,
        entity_id: uuid::Uuid,
    ) -> anyhow::Result<()> {
        self.graph_client()?
            .delete_entity_node(ctx.tenant_id, session_id, entity_id)
            .await
    }
}

impl http::OperatorQuerySurface for ReconnectingStorage {
    async fn cql_query_passthrough(
        &self,
        _ctx: &TenantContext,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<serde_json::Value> {
        let conn_gen = self.current_generation();
        let cql = self
            .current_cql()
            .await
            .ok_or_else(|| anyhow::anyhow!(NOT_CONNECTED_MSG))?;

        let normalized = normalize_public_query(query)?;
        #[allow(deprecated)]
        let iter = cql.session().query_iter(normalized.clone(), ()).await;
        let mut iter = match iter {
            Ok(iter) => iter,
            Err(err) => {
                if is_connection_error(&err) {
                    self.mark_disconnected(conn_gen).await;
                }
                return Err(err.into());
            }
        };

        let columns: Vec<String> = iter
            .get_column_specs()
            .iter()
            .map(|spec| spec.name().to_string())
            .collect();
        let mut rendered_rows: Vec<serde_json::Value> = Vec::with_capacity(limit.min(1024));
        let mut total_rows = 0usize;
        let mut truncated = false;
        while let Some(row) = iter.next().await {
            let row = match row {
                Ok(row) => row,
                Err(err) => {
                    if is_connection_error(&err) {
                        self.mark_disconnected(conn_gen).await;
                    }
                    return Err(err.into());
                }
            };
            total_rows += 1;
            if rendered_rows.len() < limit {
                let mut object = serde_json::Map::new();
                for (index, name) in columns.iter().enumerate() {
                    object.insert(name.clone(), cql_cell_to_json(&row, index));
                }
                rendered_rows.push(serde_json::Value::Object(object));
            } else {
                truncated = true;
                break;
            }
        }

        Ok(serde_json::json!({
            "query": query,
            "columns": columns,
            "rows": rendered_rows,
            "count": rendered_rows.len(),
            "total_rows": total_rows,
            "truncated": truncated,
            "source": "ferrosa-cql",
        }))
    }

    async fn sparql_query_passthrough(
        &self,
        _ctx: &TenantContext,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<serde_json::Value> {
        let sparql = self
            .sparql
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SPARQL passthrough is not configured"))?;
        let response = sparql
            .client
            .post(sparql.query_url())
            .basic_auth(&sparql.username, Some(&sparql.password))
            .header("Content-Type", "application/sparql-query")
            .header("Accept", "application/sparql-results+json")
            .body(query.to_string())
            .send()
            .await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = read_sparql_response_body_bounded(response, SPARQL_MAX_RESPONSE_BYTES).await?;
        if !status.is_success() {
            anyhow::bail!(
                "SPARQL passthrough failed: {} {}",
                status.as_u16(),
                body.trim()
            );
        }

        let parsed = parse_sparql_response_limited(&body, limit)?;

        Ok(serde_json::json!({
            "query": query,
            "columns": parsed.columns,
            "rows": parsed.rows,
            "count": parsed.total_rows.min(limit),
            "total_rows": parsed.total_rows,
            "truncated": parsed.truncated,
            "content_type": content_type,
            "source": "ferrosa-sparql",
        }))
    }
}

/// Backoff schedule for CQL reconnection: 1s, 2s, 4s, 8s, 16s, then 30s.
fn next_backoff(attempt: u32) -> Duration {
    if attempt < 5 {
        Duration::from_secs(1 << attempt)
    } else {
        Duration::from_secs(30)
    }
}

fn migrations_enabled() -> bool {
    matches!(
        std::env::var("FERROSA_MIGRATIONS_ENABLED").ok().as_deref(),
        None | Some("true" | "1" | "on" | "yes")
    )
}

async fn run_schema_migrations_if_enabled(config: &FerrosaCqlConfig) {
    if !migrations_enabled() {
        tracing::info!(
            "FERROSA_MIGRATIONS_ENABLED is disabled; skipping schema migrations. \
             Ensure the keyspace schema is managed externally (DBaaS mode) or manually applied."
        );
        return;
    }

    match ferrosa_memory_core::cql_storage::connect_admin_session(config).await {
        Ok(admin_session) => {
            match ferrosa_memory_core::migration::run_migrations(&admin_session, &config.keyspace)
                .await
            {
                Ok(0) => tracing::debug!("schema up to date"),
                Ok(n) => tracing::info!(applied = n, "schema migrations applied"),
                Err(e) => {
                    tracing::error!(
                        "schema migration failed: {e}. The keyspace may be out of sync with this binary. \
                         Investigate the failing DDL and restart. CqlStorage will attempt to connect, \
                         but runtime queries may fail if the schema is incomplete."
                    );
                }
            }
            match ferrosa_memory_core::migration::migration_status(&admin_session, &config.keyspace)
                .await
            {
                Ok(status) => tracing::info!(
                    db_version = status.db_version,
                    binary_version = status.binary_version,
                    pending = ?status.pending,
                    "schema migration status"
                ),
                Err(e) => tracing::warn!("schema migration status unavailable: {e}"),
            }
        }
        Err(e) => {
            tracing::warn!(
                "admin CQL session unavailable ({e}), skipping migrations for this reconnect attempt"
            );
        }
    }
}

/// Persistent reconnection watcher.
///
/// Waits for `reconnect_signal` (fired on initial failure or mid-operation
/// connection loss), then retries with exponential backoff until connected.
/// After connecting, goes back to waiting for the next signal — survives
/// rolling restarts, network blips, etc.
///
/// ## Cancel safety
///
/// This task is spawned via `tokio::spawn` and never aborted, so
/// cancellation within the retry loop is not a concern. The `notified()`
/// call at the top is cancel-safe per tokio docs. After successful
/// reconnection the inner state is checked to avoid spurious retry
/// cycles from stale permits.
async fn cql_reconnect_watcher(storage: Arc<ReconnectingStorage>) {
    loop {
        // Wait until someone signals that reconnection is needed.
        storage.reconnect_signal.notified().await;

        // Check if we actually need to reconnect — a permit may have been
        // stored by a stale mark_disconnected that lost the generation race,
        // or by a second error while we were already reconnecting.
        {
            let guard = storage.inner.read().await;
            if guard.is_some() {
                tracing::debug!("reconnect watcher: spurious signal, already connected");
                continue;
            }
        }

        tracing::info!("reconnect watcher: connection loss detected, starting reconnection");

        let mut attempt: u32 = 0;
        loop {
            let delay = next_backoff(attempt);
            tracing::info!(
                attempt = attempt + 1,
                delay_secs = delay.as_secs(),
                "CQL reconnection attempt scheduled"
            );
            tokio::time::sleep(delay).await;

            if storage.run_migrations_on_connect {
                run_schema_migrations_if_enabled(&storage.cql_config).await;
            } else {
                tracing::debug!(
                    "secondary CQL reader reconnect skips migrations; primary watcher owns schema"
                );
            }

            match CqlStorage::connect(&storage.cql_config).await {
                Ok(cql) => {
                    tracing::info!("CQL reconnection successful");
                    storage.set_connected(cql).await;
                    break; // back to waiting for next signal
                }
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    tracing::warn!(attempt, "CQL reconnection failed: {e}");
                }
            }
        }
    }
}

/// Idle-consolidation configuration passed from `main()`.
struct IdleConsolidationConfig {
    idle_seconds: u64,
    stale_edge_max_days: u64,
    edge_decay_factor: f64,
}

/// Background task that runs dream consolidation and edge maintenance after
/// a period of tool-call inactivity. Resets the timer on every tool call.
/// Only runs when the dirty flag indicates new writes since the last run.
async fn idle_consolidation_loop<S: Storage + Send + Sync + 'static>(
    session: Arc<dispatch::SessionState>,
    storage: Arc<S>,
    ctx: Arc<TenantContext>,
    cfg: IdleConsolidationConfig,
) {
    let timeout_dur = Duration::from_secs(cfg.idle_seconds);
    loop {
        // Wait for the first tool call activity.
        session.last_activity.notified().await;

        // Reset timer on each subsequent activity until we hit a timeout.
        while tokio::time::timeout(timeout_dur, session.last_activity.notified())
            .await
            .is_ok()
        {}

        // Only consolidate if there were writes since the last run.
        if !session.dirty.swap(false, Ordering::Relaxed) {
            continue;
        }

        let session_ids = drain_consolidation_queue(&session).await;
        let session_ids = if session_ids.is_empty() {
            match session.effective_default_session_id() {
                Some(id) => vec![id],
                None => continue,
            }
        } else {
            session_ids
        };

        for sid in session_ids {
            run_idle_consolidation(&session, storage.as_ref(), &ctx, sid, &cfg).await;
        }
    }
}

async fn drain_consolidation_queue(session: &dispatch::SessionState) -> Vec<uuid::Uuid> {
    let mut queue = session.consolidation_queue.lock().await;
    queue.drain(..).collect()
}

/// Executes consolidation, edge weight decay, and optional stale-edge pruning.
async fn run_idle_consolidation<S: Storage>(
    session: &dispatch::SessionState,
    storage: &S,
    ctx: &TenantContext,
    session_id: uuid::Uuid,
    cfg: &IdleConsolidationConfig,
) {
    dispatch::record_consolidation_running(session, session_id).await;

    // 1. Decay existing edge weights so unreinforced edges lose strength.
    if cfg.edge_decay_factor < 1.0 {
        match storage.edge_decay_weights(ctx, cfg.edge_decay_factor).await {
            Ok(n) if n > 0 => tracing::info!(
                decayed = n,
                factor = cfg.edge_decay_factor,
                "edge decay applied"
            ),
            Err(e) => tracing::warn!("edge decay failed: {e}"),
            _ => {}
        }
    }

    // 2. Run consolidation — rediscovered edges get fresh weights.
    match ferrosa_memory_core::dream::run_consolidation(storage, ctx, session_id).await {
        Ok(r) => {
            dispatch::record_consolidation_finished(session, session_id, Ok(&r)).await;
            tracing::info!(
                entities = r.entities_processed,
                connections = r.connections_created,
                "idle consolidation complete"
            );
        }
        Err(e) => {
            let error = e.to_string();
            dispatch::record_consolidation_finished(session, session_id, Err(error.as_str())).await;
            tracing::warn!("idle consolidation failed: {e}");
        }
    }

    // 3. Optionally prune stale edges (0 = disabled).
    if cfg.stale_edge_max_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(cfg.stale_edge_max_days as i64);
        match storage.edge_prune_stale(ctx, cutoff).await {
            Ok(pruned) if pruned > 0 => {
                tracing::info!(pruned, "idle edge pruning complete")
            }
            Err(e) => tracing::warn!("idle edge pruning failed: {e}"),
            _ => {}
        }
    }
}

/// Extract a human-readable message from a panic payload.
fn panic_payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install a process-wide panic hook so no panic is ever silent.
///
/// Tokio swallows panics in spawned tasks — they vanish into a dropped
/// `JoinHandle` — which is exactly how a degraded fmem keeps "running" while a
/// supervisory task is dead, forcing long phantom hunts. This logs every panic
/// loudly (thread, location, payload) before delegating to the default hook.
fn install_fail_loud_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        tracing::error!(
            target: "fail_loud",
            thread = %thread,
            location = %location,
            payload = %panic_payload_str(info.payload()),
            "PANIC — a thread or task panicked (fail-loud)"
        );
        default_hook(info);
    }));
}

/// Spawn a task expected to run for the lifetime of the process.
///
/// If it ever returns (its loop exited) that is logged at ERROR — a supervisory
/// task quietly ending is a fail-loud violation and the root cause of many
/// "why did fmem silently stop doing X" investigations. Panics inside `fut` are
/// caught by [`install_fail_loud_panic_hook`].
fn spawn_critical<F>(name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        fut.await;
        tracing::error!(
            target: "fail_loud",
            task = name,
            "CRITICAL TASK EXITED — a task that should run for the process \
             lifetime returned; fmem is now degraded (fail-loud)"
        );
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let debug = std::env::args().any(|a| a == "--debug");

    let default_filter = if debug {
        "debug,scylla=warn,hyper=info,reqwest=info"
    } else {
        "ferrosa_memory_core=warn,ferrosa_memory_mcp=warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .init();

    // Fail loud: make every panic (including in spawned tasks tokio would
    // otherwise swallow) a visible ERROR log line. Installed before any task
    // is spawned.
    install_fail_loud_panic_hook();

    if debug {
        tracing::info!("ferrosa-memory-mcp starting (debug mode)");
    }

    let config = match ferrosa_memory_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_memory_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:19042\"]\n",
            )?
        }
    };

    if config.server.transport == "http" {
        validate_shared_http_config(&config)?;
    }

    let tenant_id = stdio_tenant_id(&config);

    // Resolve repo for intention scoping and session-start identity:
    // CLAUDE_PROJECT_DIR env > empty. Other harnesses can set
    // FERROSA_MEMORY_AGENT_SESSION_ID/FERROSA_MEMORY_AGENT and call configure
    // from their session-start hook to rotate the runtime session.
    let repo = std::env::var("CLAUDE_PROJECT_DIR").unwrap_or_else(|_| String::new());
    if repo.is_empty() {
        tracing::warn!("CLAUDE_PROJECT_DIR not set — intentions will require explicit repo param");
    } else {
        tracing::info!(repo = %repo, "intention scoping: repo from CLAUDE_PROJECT_DIR");
    }

    let default_session_id = startup_default_session_id(&config, &repo)?;

    // Immutable startup snapshot for the `system_describe` management tool.
    // Captured here, while the full effective config and resolved identity are
    // in scope; dynamic store/schema health is probed per call.
    let system_info = Arc::new(ferrosa_memory_core::system_describe::SystemInfo::build(
        &config,
        tenant_id,
        default_session_id,
        chrono::Utc::now().to_rfc3339(),
    ));
    let metrics = Arc::new(ferrosa_memory_core::metrics::MemoryMetrics::new()?);
    tracing::info!("metrics registered");
    let sparql = config.sparql.enabled.then(|| {
        SparqlPassthrough::new(
            config.sparql.http_url.clone(),
            config.ferrosa.username.clone(),
            config.ferrosa.password.clone(),
        )
    });

    // Build the graph client without a startup health check. Graph operations
    // still fail loudly per request if Ferrosa's graph endpoint is unavailable,
    // but MCP initialize must not wait on backend probes.
    let graph_client = match ferrosa_memory_core::graph::GraphClient::from_config(
        &ferrosa_memory_core::graph::GraphConfig {
            http_url: config.graph.http_url.clone(),
            username: config.graph.username.clone(),
            password: config.graph.password.clone(),
            keyspace: config.ferrosa.keyspace.clone(),
        },
    ) {
        Ok(graph) => Some(Arc::new(graph)),
        Err(e) => {
            tracing::warn!("graph client configuration failed ({e}), graph traversals disabled");
            None
        }
    };

    // Start disconnected and let the watcher perform migrations plus runtime
    // CQL connect in the background. This keeps MCP initialize independent of
    // CQL availability while preserving the migration-before-prepare invariant.
    let storage = Arc::new(ReconnectingStorage::disconnected(
        config.ferrosa.clone(),
        graph_client.clone(),
        sparql.clone(),
    ));

    // Always spawn the reconnect watcher — it handles both initial failure
    // and mid-operation connection loss (rolling restarts, network blips).
    spawn_critical(
        "cql_reconnect_watcher",
        cql_reconnect_watcher(Arc::clone(&storage)),
    );
    storage.reconnect_signal.notify_one();

    // Load dynamic type registry from the database (falls back to defaults).
    //
    // The 3s budget was burning on cold-cluster startups: seed ran as 5
    // sequential INSERTs outside the timeout, then the two loads ran
    // serially inside it. Now seed (parallelized, see
    // `CqlStorage::seed_sprint1_types`) runs *concurrently* with both
    // loads under a single 10s budget. A fresh install may race — seed
    // lands after the loads read — but the next startup sees the
    // seeded rows, and defaults are safe in the meantime.
    let (entity_types, edge_types) = {
        let guard = storage.inner.read().await;
        if let Some(ref cql) = *guard {
            let load_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let (seed_res, et, edg) = tokio::join!(
                    cql.seed_sprint1_types(),
                    cql.load_entity_types(),
                    cql.load_edge_types(),
                );
                if let Err(e) = seed_res {
                    tracing::warn!(error = %e, "type registry seed failed (non-fatal)");
                }
                (et, edg)
            })
            .await;
            match load_result {
                Ok((et, edg)) => {
                    tracing::info!(
                        entity_types = et.len(),
                        edge_types = edg.len(),
                        "loaded type registry"
                    );
                    (et, edg)
                }
                Err(_) => {
                    tracing::warn!("type registry load timed out (10s), using defaults");
                    (
                        ferrosa_memory_core::cql_storage::CqlStorage::default_entity_types(),
                        Vec::new(),
                    )
                }
            }
        } else {
            tracing::info!("no CQL connection yet, using default type registry");
            (
                ferrosa_memory_core::cql_storage::CqlStorage::default_entity_types(),
                Vec::new(),
            )
        }
    };

    // Embedding provider health check is non-fatal and must not block MCP
    // initialize. Tool calls still fail clearly if embeddings are unavailable.
    let embeddings_config = config.embeddings.clone();
    tokio::spawn(async move {
        let embed_health = ferrosa_memory_core::embedding::EmbeddingClient::new(&embeddings_config);
        match embed_health.health_check().await {
            Ok(()) => tracing::info!(
                provider = %embeddings_config.provider,
                url = %embeddings_config.ollama_base_url,
                model = %embeddings_config.model,
                "embedding provider reachable and model loaded"
            ),
            Err(e) => tracing::warn!(
                provider = %embeddings_config.provider,
                url = %embeddings_config.ollama_base_url,
                model = %embeddings_config.model,
                error = %e,
                "embedding provider check failed — tools that require embeddings \
                 (smart_ingest, hybrid_search, retrieve_fold_context, retrieve_entities) \
                 will continue with lexical/phonetic/graph fallback where possible; \
                 semantic ANN quality will be degraded and advertised eval results may \
                 not be reproducible. Start Ollama and ensure '{}' is pulled: \
                 `ollama pull {}`",
                embeddings_config.model,
                embeddings_config.model
            ),
        }
    });

    // Start visualization server if enabled.
    //
    // Viz is unauthenticated. Under stdio transport we bind 0.0.0.0 (local trust
    // model). Under HTTP transport we force loopback (127.0.0.1) and require an
    // explicit `[viz] tenant_id` — the spec bans tenant fallback in HTTP mode.
    let shared_event_bus = Arc::new(ferrosa_memory_core::viz::EventBus::new());
    if config.viz.enabled {
        let transport = config.server.transport.as_str();
        let (default_bind, viz_tenant_id) = match transport {
            "stdio" => ("0.0.0.0", tenant_id),
            "http" => {
                let raw = config.viz.tenant_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "viz.enabled = true under HTTP transport requires [viz] tenant_id"
                    )
                })?;
                let parsed = uuid::Uuid::parse_str(raw)
                    .map_err(|e| anyhow::anyhow!("[viz] tenant_id is not a valid UUID: {e}"))?;
                ("127.0.0.1", parsed)
            }
            _ => ("0.0.0.0", tenant_id),
        };
        let viz_bind: String = config
            .viz
            .bind_addr
            .clone()
            .unwrap_or_else(|| default_bind.to_string());
        let viz_bus = Arc::clone(&shared_event_bus);
        let viz_port = config.viz.port;
        let viz_storage = Arc::new(ReconnectingStorage::disconnected_secondary_reader(
            config.ferrosa.clone(),
            graph_client.clone(),
            sparql.clone(),
        ));
        spawn_critical(
            "cql_reconnect_watcher_viz",
            cql_reconnect_watcher(Arc::clone(&viz_storage)),
        );
        viz_storage.reconnect_signal.notify_one();
        tracing::info!(
            "viz server using dedicated CQL reader connection isolated from MCP ingest traffic"
        );
        let viz_ctx = Arc::new(auth::authenticate_stdio(viz_tenant_id));
        let viz_session_id = default_session_id;
        let shell_routes = http::ShellRouteConfig {
            workbench_scheme: if config.server.require_tls {
                "https".into()
            } else {
                "http".into()
            },
            workbench_port: config.server.public_port.unwrap_or(config.server.http_port),
            viz_scheme: "http".into(),
            viz_port: config.viz.public_port.unwrap_or(viz_port),
        };
        let viz_bind_for_log = viz_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = http::serve_viz(
                &viz_bind,
                viz_port,
                viz_bus,
                viz_storage,
                viz_ctx,
                viz_session_id,
                shell_routes,
            )
            .await
            {
                tracing::warn!("viz server error: {e}");
            }
        });
        tracing::info!(
            "viz dashboard at http://{}:{}/viz",
            if viz_bind_for_log == "0.0.0.0" {
                "localhost".to_string()
            } else {
                viz_bind_for_log
            },
            config.viz.port
        );
    }

    match config.server.transport.as_str() {
        "stdio" => {
            let ctx = Arc::new(auth::authenticate_stdio(tenant_id));
            tracing::info!(tenant_id = %tenant_id, "serving on stdio");

            let storage_ref = Arc::clone(&storage);
            let ctx_ref = Arc::clone(&ctx);
            let repo_lock = std::sync::OnceLock::new();
            if !repo.is_empty() {
                let _ = repo_lock.set(repo.clone());
            }
            let session = Arc::new(dispatch::SessionState {
                event_bus: Arc::clone(&shared_event_bus),
                default_session_id: Some(default_session_id),
                repo: repo_lock,
                embed_provider: config.embeddings.provider.clone(),
                ollama_base_url: config.embeddings.ollama_base_url.clone(),
                ner_model: config.embeddings.ner_model.clone(),
                embed_model: config.embeddings.model.clone(),
                embed_dimensions: config.embeddings.dimensions,
                entity_types: entity_types.clone(),
                edge_types: edge_types.clone(),
                graph: graph_client.clone(),
                enrich_llm_url: config.enrich.llm_base_url.clone(),
                enrich_llm_model: config.enrich.llm_model.clone(),
                judge_config: Arc::new(tokio::sync::Mutex::new(config.judge.clone())),
                retrieval_default_limit: Arc::new(std::sync::atomic::AtomicUsize::new(
                    config.retrieval.default_limit.clamp(1, 50),
                )),
                system_info: Arc::clone(&system_info),
                forget: config.forget.clone(),
                forget_token_key: random_forget_token_key(),
                ..dispatch::SessionState::default()
            });
            tracing::info!(session_id = %default_session_id, "using server-owned default session_id");

            // Load persisted intentions from CQL (repo-scoped).
            if !repo.is_empty() {
                let load_storage = Arc::clone(&storage);
                let load_ctx = Arc::clone(&ctx);
                let load_session = Arc::clone(&session);
                let load_repo = repo.clone();
                tokio::spawn(async move {
                    match load_storage.intention_list(&load_ctx, &load_repo).await {
                        Ok(intentions) if !intentions.is_empty() => {
                            let count = intentions.len();
                            load_session.intentions.lock().await.load(intentions);
                            tracing::info!(count, repo = %load_repo, "loaded persisted intentions");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to load intentions from storage");
                        }
                    }
                });
            }
            let session_ref = Arc::clone(&session);

            let handler: transport::Handler = Box::new(move |method: &str, params| {
                let storage = Arc::clone(&storage_ref);
                let ctx = Arc::clone(&ctx_ref);
                let session = Arc::clone(&session_ref);
                let method = method.to_string();
                Box::pin(async move {
                    dispatch::dispatch(&method, params, storage.as_ref(), ctx.as_ref(), &session)
                        .await
                })
            });

            // Spawn idle consolidation background task.
            if config.server.idle_consolidation_enabled {
                let idle_cfg = IdleConsolidationConfig {
                    idle_seconds: config.server.idle_consolidation_seconds,
                    stale_edge_max_days: config.server.stale_edge_max_days,
                    edge_decay_factor: config.server.edge_decay_factor,
                };
                let idle_session = Arc::clone(&session);
                let idle_storage = Arc::clone(&storage);
                let idle_ctx = Arc::clone(&ctx);
                spawn_critical(
                    "idle_consolidation_loop",
                    idle_consolidation_loop(idle_session, idle_storage, idle_ctx, idle_cfg),
                );
                tracing::info!(
                    idle_seconds = config.server.idle_consolidation_seconds,
                    decay_factor = config.server.edge_decay_factor,
                    prune_days = config.server.stale_edge_max_days,
                    "idle consolidation enabled"
                );
            }

            transport::serve_stdio(handler).await?;
        }
        "http" => {
            tracing::info!("serving on HTTP port {}", config.server.http_port);
            let auth_validator =
                Arc::new(auth::FileAuthValidator::from_path(
                    config.server.auth_file.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("HTTP transport requires server.auth_file")
                    })?,
                )?);
            let closure_validator: Arc<http::CredentialValidator> = Arc::new({
                let v = Arc::clone(&auth_validator);
                move |user: &str, pass: &str| v.validate(user, pass)
            });

            let sighup_validator = Arc::clone(&auth_validator);
            #[cfg(unix)]
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let mut stream =
                    signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
                loop {
                    stream.recv().await;
                    tracing::info!(path = %sighup_validator.path(), "SIGHUP received, reloading auth file");
                    match sighup_validator.reload() {
                        Ok(count) => tracing::info!(principals = count, "auth file reloaded"),
                        Err(e) => {
                            tracing::error!(error = %e, "failed to reload auth file, keeping old principals")
                        }
                    }
                }
            });

            let readiness_storage = Arc::clone(&storage);
            let repo_lock = std::sync::OnceLock::new();
            if !repo.is_empty() {
                let _ = repo_lock.set(repo.clone());
            }
            let session = Arc::new(dispatch::SessionState {
                event_bus: Arc::clone(&shared_event_bus),
                default_session_id: Some(default_session_id),
                repo: repo_lock,
                embed_provider: config.embeddings.provider.clone(),
                ollama_base_url: config.embeddings.ollama_base_url.clone(),
                ner_model: config.embeddings.ner_model.clone(),
                embed_model: config.embeddings.model.clone(),
                embed_dimensions: config.embeddings.dimensions,
                entity_types: entity_types.clone(),
                edge_types: edge_types.clone(),
                graph: graph_client.clone(),
                enrich_llm_url: config.enrich.llm_base_url.clone(),
                enrich_llm_model: config.enrich.llm_model.clone(),
                judge_config: Arc::new(tokio::sync::Mutex::new(config.judge.clone())),
                retrieval_default_limit: Arc::new(std::sync::atomic::AtomicUsize::new(
                    config.retrieval.default_limit.clamp(1, 50),
                )),
                system_info: Arc::clone(&system_info),
                forget: config.forget.clone(),
                forget_token_key: random_forget_token_key(),
                ..dispatch::SessionState::default()
            });

            http::serve_http(
                http::HttpConfig {
                    bind_addr: config.server.bind_addr.clone(),
                    port: config.server.http_port,
                    request_budget: std::time::Duration::from_secs(
                        config.server.request_timeout_seconds.clamp(1, 300),
                    ),
                    require_tls: config.server.require_tls,
                    cert_path: config.server.cert_path.clone(),
                    key_path: config.server.key_path.clone(),
                    readiness_checker: Arc::new(move || readiness_storage.is_ready()),
                    shell_routes: http::ShellRouteConfig {
                        workbench_scheme: if config.server.require_tls {
                            "https".into()
                        } else {
                            "http".into()
                        },
                        workbench_port: config
                            .server
                            .public_port
                            .unwrap_or(config.server.http_port),
                        viz_scheme: "http".into(),
                        viz_port: config.viz.public_port.unwrap_or(config.viz.port),
                    },
                    session,
                },
                storage,
                metrics,
                closure_validator,
            )
            .await?;
        }
        other => {
            anyhow::bail!("unsupported transport: {other}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- is_connection_error tests ---

    // --- fail-loud hardening ---

    #[test]
    fn panic_payload_extracts_str_and_string_and_falls_back() {
        let as_str: &str = "boom";
        assert_eq!(panic_payload_str(&as_str), "boom");
        let owned: String = "kaboom".to_string();
        assert_eq!(panic_payload_str(&owned), "kaboom");
        let other: i32 = 42;
        assert_eq!(panic_payload_str(&other), "<non-string panic payload>");
    }

    #[tokio::test]
    async fn spawn_critical_runs_the_task() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = Arc::clone(&ran);
        spawn_critical("test_task", async move {
            ran2.store(true, Ordering::SeqCst);
        })
        .await
        .unwrap();
        assert!(
            ran.load(Ordering::SeqCst),
            "spawn_critical must drive the future"
        );
    }

    #[test]
    fn is_connection_error_broken_pipe() {
        let err = anyhow::anyhow!("Broken pipe");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_connection_reset() {
        let err = anyhow::anyhow!("Connection reset by peer");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_connection_refused() {
        let err = anyhow::anyhow!("Connection refused");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_transport() {
        let err = anyhow::anyhow!("Transport error occurred");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_channel_closed() {
        let err = anyhow::anyhow!("Channel closed unexpectedly");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_io_error() {
        let err = anyhow::anyhow!("IO error: something went wrong");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn normalize_public_query_rejects_empty_input() {
        let err = normalize_public_query("   ").unwrap_err();
        assert!(err.to_string().contains("query must not be empty"));
    }

    #[test]
    fn normalize_public_query_appends_semicolon_once() {
        assert_eq!(
            normalize_public_query("SELECT * FROM agent_memory.entity_store LIMIT 1").unwrap(),
            "SELECT * FROM agent_memory.entity_store LIMIT 1;"
        );
        assert_eq!(
            normalize_public_query("SELECT * FROM agent_memory.entity_store LIMIT 1;").unwrap(),
            "SELECT * FROM agent_memory.entity_store LIMIT 1;"
        );
    }

    #[test]
    fn is_connection_error_timed_out() {
        let err = anyhow::anyhow!("Request timed out");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_false_for_cluster_read_timeout() {
        let err = anyhow::anyhow!(
            "server error: storage error: invalid data: cluster: read timeout: CL=LOCAL_QUORUM, received=1, required=2, data_present=true"
        );
        assert!(
            !is_connection_error(&err),
            "query-level quorum read timeouts must not poison the shared CQL session"
        );
    }

    #[test]
    fn is_connection_error_not_connected() {
        let err = anyhow::anyhow!("Not connected to server");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_eof() {
        let err = anyhow::anyhow!("Unexpected EOF");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_case_insensitive() {
        let err = anyhow::anyhow!("BROKEN PIPE from server");
        assert!(is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_false_for_query_error() {
        let err = anyhow::anyhow!("table not found: agent_memory.entities");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_false_for_auth_error() {
        let err = anyhow::anyhow!("authentication failed: bad credentials");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_false_for_parse_error() {
        let err = anyhow::anyhow!("failed to parse CQL response");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn is_connection_error_false_for_generic_error() {
        let err = anyhow::anyhow!("something went wrong");
        assert!(!is_connection_error(&err));
    }

    #[test]
    fn stdio_tenant_id_uses_configured_value() {
        let config = ferrosa_memory_core::config::parse_config(
            r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
tenant_id = "00000000-0000-0000-0000-000000000123"
"#,
        )
        .unwrap();

        assert_eq!(
            stdio_tenant_id(&config),
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap()
        );
    }

    #[test]
    fn build_http_validator_loads_auth_file() {
        let tenant_id = uuid::Uuid::new_v4();
        let auth_path =
            std::env::temp_dir().join(format!("ferrosa-auth-{}.toml", uuid::Uuid::new_v4()));
        fs::write(
            &auth_path,
            format!(
                "[[principal]]\nusername = \"alice\"\npassword_sha256 = \"{}\"\ntenant_id = \"{}\"\n",
                {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(b"s3cret");
                    hex::encode(hasher.finalize())
                },
                tenant_id
            ),
        )
        .unwrap();

        let config = ferrosa_memory_core::config::parse_config(&format!(
            r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "{}"
"#,
            auth_path.display()
        ))
        .unwrap();

        let validator = build_http_validator(&config).unwrap();
        assert_eq!(validator("alice", "s3cret"), Some(tenant_id));
        assert_eq!(validator("alice", "wrong"), None);

        let _ = fs::remove_file(auth_path);
    }

    #[tokio::test]
    async fn idle_queue_drain_returns_explicit_sessions_and_clears_queue() {
        let session = dispatch::SessionState::default();
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        {
            let mut queue = session.consolidation_queue.lock().await;
            queue.push_back(first);
            queue.push_back(second);
        }

        assert_eq!(
            drain_consolidation_queue(&session).await,
            vec![first, second]
        );
        assert!(drain_consolidation_queue(&session).await.is_empty());
    }

    #[tokio::test]
    async fn reconnecting_storage_reports_readiness() {
        let cfg = FerrosaCqlConfig {
            contact_points: vec!["localhost:19042".into()],
            keyspace: "agent_memory".into(),
            replication_factor: 3,
            consistency: "LOCAL_QUORUM".into(),
            username: "ferrosa_user".into(),
            password: "ferrosa_user".into(),
            admin_username: None,
            admin_password: None,
        };
        let storage = ReconnectingStorage::disconnected(cfg, None, None);
        assert!(!storage.is_ready());
    }

    #[tokio::test]
    async fn sparql_passthrough_bounds_large_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
            let body = format!(
                r#"{{"head":{{"vars":["s"]}},"results":{{"bindings":[{}]}}}}"#,
                (0..128)
                    .map(|i| format!(r#"{{"s":{{"type":"literal","value":"row-{i}"}}}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                .await
                .unwrap();
        });

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/sparql"))
            .body("SELECT ?s WHERE { ?s ?p ?o }")
            .send()
            .await
            .unwrap();

        let err = read_sparql_response_body_bounded(response, 128)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("SPARQL response exceeded 128 bytes"),
            "error must name the response cap: {err}"
        );
        server.await.unwrap();
    }

    #[test]
    fn sparql_result_parser_keeps_only_limit_bindings() {
        let body = r#"{
            "head": {"vars": ["s"]},
            "results": {"bindings": [
                {"s": {"type": "literal", "value": "row-1"}},
                {"s": {"type": "literal", "value": "row-2"}},
                {"s": {"type": "literal", "value": "row-3"}}
            ]}
        }"#;

        let parsed = parse_sparql_response_limited(body, 1).unwrap();

        assert_eq!(parsed.columns, serde_json::json!(["s"]));
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.total_rows, 3);
        assert!(parsed.truncated);
        assert_eq!(
            parsed.rows[0]["s"]["value"],
            serde_json::Value::String("row-1".into())
        );
    }

    #[test]
    fn reconnecting_storage_overrides_viz_stream_methods_with_cql_delegates() {
        let source = include_str!("main.rs");
        let impl_start = source
            .find("impl Storage for ReconnectingStorage")
            .expect("ReconnectingStorage must implement Storage");
        let impl_source = &source[impl_start..];
        for method in [
            "entity_stream_all",
            "edge_stream_all",
            "typed_edge_stream_all",
        ] {
            let needle = format!("cql.{method}(ctx, chunk_size, tx).await");
            assert!(
                impl_source.contains(&needle),
                "ReconnectingStorage must delegate {method} to CqlStorage streaming override, not fall back to Storage's materializing default"
            );
        }
        assert!(
            impl_source.contains("cql.derived_cache_stream(ctx, cache_key, chunk_size, limit, tx)"),
            "ReconnectingStorage must delegate derived_cache_stream to CqlStorage streaming override, not fall back to Storage's materializing default"
        );
    }

    #[test]
    fn reconnecting_storage_stream_methods_drop_read_lock_before_awaiting_sends() {
        let source = include_str!("main.rs");
        let impl_start = source
            .find("impl Storage for ReconnectingStorage")
            .expect("ReconnectingStorage must implement Storage");
        let impl_source = &source[impl_start..];
        for (method, next_method) in [
            ("entity_stream_all", "fold_list_all"),
            ("edge_stream_all", "edge_list_for_entity"),
            ("typed_edge_stream_all", "typed_edge_list_from"),
            ("derived_cache_stream", "derived_cache_put"),
        ] {
            let method_start = impl_source
                .find(&format!("async fn {method}"))
                .unwrap_or_else(|| panic!("{method} must exist"));
            let tail = &impl_source[method_start..];
            let method_end = tail
                .find(&format!("async fn {next_method}"))
                .unwrap_or(tail.len());
            let method_source = &tail[..method_end];
            assert!(
                method_source.contains("let cql = self.current_cql().await;"),
                "{method} must clone the current CQL backend before awaiting stream sends"
            );
            assert!(
                !method_source.contains("let guard = self.inner.read().await;"),
                "{method} must not hold the reconnect RwLock read guard while awaiting stream sends"
            );
        }
    }

    #[test]
    fn reconnecting_storage_delegate_macro_does_not_hold_read_lock_across_await() {
        let source = include_str!("main.rs");
        let macro_start = source
            .find("macro_rules! delegate")
            .expect("delegate macro must exist");
        let macro_tail = &source[macro_start..];
        let macro_end = macro_tail
            .find("/// Delegate all Storage methods through")
            .expect("delegate macro section must end before Storage impl");
        let macro_source = &macro_tail[..macro_end];

        assert!(
            macro_source.contains("let cql = $self.current_cql().await;"),
            "delegate macro must clone the current CQL backend before awaiting storage calls"
        );
        assert!(
            !macro_source.contains("let guard = $self.inner.read().await;"),
            "delegate macro must not hold the reconnect RwLock read guard across awaited storage calls"
        );
    }

    #[test]
    fn reconnecting_storage_clones_arc_not_cql_backend() {
        let source = include_str!("main.rs");
        let impl_start = source
            .find("impl ReconnectingStorage")
            .expect("ReconnectingStorage impl must exist");
        let impl_tail = &source[impl_start..];
        let impl_end = impl_tail
            .find("/// Error returned when CQL is not yet connected.")
            .expect("ReconnectingStorage impl section must end before error constants");
        let impl_source = &impl_tail[..impl_end];
        assert!(
            source.contains("inner: RwLock<Option<Arc<CqlStorage>>>"),
            "ReconnectingStorage must store Arc<CqlStorage> so each request clones only a pointer"
        );
        assert!(
            impl_source.contains("async fn current_cql(&self) -> Option<Arc<CqlStorage>>"),
            "current_cql must return Arc<CqlStorage>, not clone the prepared-statement cache"
        );
        assert!(
            impl_source.contains("*guard = Some(Arc::new(cql));"),
            "set_connected must wrap the backend in Arc before publishing it"
        );
        assert!(
            !impl_source.contains("async fn current_cql(&self) -> Option<CqlStorage>"),
            "current_cql must not clone CqlStorage directly"
        );
    }

    #[test]
    fn tool_usage_logging_does_not_use_connection_error_delegate() {
        let source = include_str!("main.rs");
        let impl_start = source
            .find("impl Storage for ReconnectingStorage")
            .expect("ReconnectingStorage must implement Storage");
        let impl_source = &source[impl_start..];
        let method_start = impl_source
            .find("async fn tool_usage_put")
            .expect("tool_usage_put override must exist");
        let tail = &impl_source[method_start..];
        let method_end = tail
            .find("async fn tool_usage_query")
            .expect("tool_usage_put section must end before tool_usage_query");
        let method_source = &tail[..method_end];

        assert!(
            !method_source.contains("delegate!("),
            "best-effort telemetry must not use the generic delegate path"
        );
        assert!(
            !method_source.contains("is_connection_error"),
            "best-effort telemetry must not format/inspect driver errors"
        );
        assert!(
            method_source.contains("tool usage logging failed"),
            "telemetry failures should collapse to a bounded local error"
        );
    }

    #[test]
    fn operator_cql_passthrough_does_not_hold_read_lock_while_streaming_rows() {
        let source = include_str!("main.rs");
        let impl_start = source
            .find("impl http::OperatorQuerySurface for ReconnectingStorage")
            .expect("operator query impl must exist");
        let impl_source = &source[impl_start..];
        let method_start = impl_source
            .find("async fn cql_query_passthrough")
            .expect("cql_query_passthrough must exist");
        let tail = &impl_source[method_start..];
        let method_end = tail
            .find("async fn sparql_query_passthrough")
            .unwrap_or(tail.len());
        let method_source = &tail[..method_end];

        assert!(
            method_source.contains(".current_cql()"),
            "operator passthrough must clone the CQL backend before query/row awaits"
        );
        assert!(
            !method_source.contains("self.inner.read().await"),
            "operator passthrough must not hold the reconnect RwLock read guard while streaming rows"
        );
    }

    #[test]
    fn startup_main_does_not_await_backend_connects_before_serving() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn main()").expect("main must exist");
        let main_tail = &source[main_start..];
        let test_start = main_tail.find("#[cfg(test)]").unwrap_or(main_tail.len());
        let main_source = &main_tail[..test_start];

        for forbidden in [
            "GraphClient::connect(",
            "connect_admin_session(&config.ferrosa).await",
            "CqlStorage::connect(&config.ferrosa).await",
        ] {
            assert!(
                !main_source.contains(forbidden),
                "main must not block MCP startup on backend probe `{forbidden}`"
            );
        }

        assert!(
            main_source.contains("ReconnectingStorage::disconnected("),
            "main should start with reconnecting storage and let the watcher connect in the background"
        );
        assert!(
            main_source.contains("spawn_critical(")
                && main_source.contains("cql_reconnect_watcher(Arc::clone(&storage))"),
            "main should spawn the background CQL reconnect watcher (fail-loud supervised) \
             before serving transports"
        );
        assert!(
            main_source.contains("tokio::spawn(async move {\n        let embed_health"),
            "embedding provider health checks should run in a background task"
        );
    }

    #[test]
    fn reconnect_watcher_runs_migrations_before_runtime_cql_connect() {
        let source = include_str!("main.rs");
        let watcher_start = source
            .find("async fn cql_reconnect_watcher")
            .expect("reconnect watcher must exist");
        let watcher_source = &source[watcher_start..];
        let migration_pos = watcher_source
            .find("run_schema_migrations_if_enabled(&storage.cql_config).await")
            .expect("watcher must run migrations");
        let connect_pos = watcher_source
            .find("CqlStorage::connect(&storage.cql_config).await")
            .expect("watcher must connect runtime CQL");

        assert!(
            migration_pos < connect_pos,
            "migrations must run before runtime CQL prepare/connect attempts"
        );
    }

    #[test]
    fn reconnect_watcher_can_skip_migrations_for_secondary_readers() {
        let source = include_str!("main.rs");
        let watcher_start = source
            .find("async fn cql_reconnect_watcher")
            .expect("reconnect watcher must exist");
        let watcher_source = &source[watcher_start..];

        assert!(
            watcher_source.contains("if storage.run_migrations_on_connect"),
            "secondary readers must be able to connect without running duplicate migrations"
        );
        assert!(
            watcher_source.contains("run_schema_migrations_if_enabled(&storage.cql_config).await"),
            "primary reconnect watcher must still run migrations before CQL connect"
        );
    }

    #[test]
    fn viz_server_uses_dedicated_reconnecting_storage() {
        let source = include_str!("main.rs");
        let viz_start = source
            .find("// Start visualization server if enabled.")
            .expect("viz startup block must exist");
        let viz_tail = &source[viz_start..];
        let next_section = viz_tail
            .find("// Start idle consolidation")
            .unwrap_or(viz_tail.len());
        let viz_source = &viz_tail[..next_section];

        assert!(
            viz_source.contains("ReconnectingStorage::disconnected_secondary_reader("),
            "viz must use its own reconnecting storage so graph scans do not share the primary ingest CQL session"
        );
        assert!(
            viz_source.contains("cql_reconnect_watcher(Arc::clone(&viz_storage))"),
            "viz storage must have its own reconnect watcher and CQL session"
        );
        assert!(
            !viz_source.contains("let viz_storage = Arc::clone(&storage);"),
            "viz must not clone the primary MCP storage handle"
        );
    }

    // --- next_backoff tests ---

    #[test]
    fn next_backoff_attempt_0() {
        assert_eq!(next_backoff(0), Duration::from_secs(1));
    }

    #[test]
    fn next_backoff_attempt_1() {
        assert_eq!(next_backoff(1), Duration::from_secs(2));
    }

    #[test]
    fn next_backoff_attempt_2() {
        assert_eq!(next_backoff(2), Duration::from_secs(4));
    }

    #[test]
    fn next_backoff_attempt_3() {
        assert_eq!(next_backoff(3), Duration::from_secs(8));
    }

    #[test]
    fn next_backoff_attempt_4() {
        assert_eq!(next_backoff(4), Duration::from_secs(16));
    }

    #[test]
    fn next_backoff_attempt_5_caps_at_30() {
        assert_eq!(next_backoff(5), Duration::from_secs(30));
    }

    #[test]
    fn next_backoff_attempt_10_still_capped() {
        assert_eq!(next_backoff(10), Duration::from_secs(30));
    }

    #[test]
    fn next_backoff_attempt_100_still_capped() {
        assert_eq!(next_backoff(100), Duration::from_secs(30));
    }

    #[test]
    fn next_backoff_u32_max_capped() {
        assert_eq!(next_backoff(u32::MAX), Duration::from_secs(30));
    }

    /// Verify the NOT_CONNECTED_MSG constant is non-empty.
    #[test]
    fn not_connected_msg_is_non_empty() {
        assert!(!NOT_CONNECTED_MSG.is_empty());
        assert!(NOT_CONNECTED_MSG.contains("CQL"));
    }
}
