//! Durable, tenant-scoped storage for the direct mobile control channel.
//!
//! This is deliberately separate from [`crate::storage::Storage`]. Mobile
//! control is an application coordination plane, not a graph or memory-record
//! operation, and keeping the trait separate prevents every memory storage
//! adapter from acquiring control-runtime responsibilities.
//! Correctness: cursor allocation never reuses values, replay stays bounded,
//! and command lifecycle updates cannot rewrite a terminal outcome.
//! Last revised: 2026-08-19
//! Last changed: Added validated, idempotent command lifecycle transitions.

// The repository intentionally uses scylla's LegacySession API until its
// coordinated driver migration; match cql_storage.rs so this module does not
// create a one-off deserialization stack.
#![allow(deprecated)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use scylla::frame::response::result::CqlValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::FerrosaCqlConfig;
use crate::cql_storage::{
    CqlSession, build_col_map, connect_admin_session, connect_session,
    connect_session_to_configured_nodes, cql_get,
};
use crate::migration::{MIGRATIONS, run_migrations};
use crate::types::TenantContext;

/// Number of monotonically increasing cursors in one CQL partition.
pub const CONTROL_CURSOR_BUCKET_SIZE: u64 = 4_096;
/// Largest block one allocator call may reserve.
pub const MAX_CONTROL_CURSOR_BLOCK: u64 = 4_096;
/// Largest replay page allowed onto one data-channel frame.
pub const MAX_CONTROL_REPLAY_EVENTS: usize = 256;
/// Bound for a command request, result, or event JSON body.
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 16 * 1024;

/// Inclusive cursor range reserved atomically for one server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorBlock {
    pub start: u64,
    pub end: u64,
}

/// Durable lifecycle of an idempotent typed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommandState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}

impl ControlCommandState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        serde_json::from_str(&format!("\"{value}\""))
            .map_err(|error| anyhow::anyhow!("invalid stored control command state: {error}"))
    }
}

/// One command row, keyed by the controller-provided UUIDv7 command id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlCommand {
    pub command_id: Uuid,
    pub command_type: String,
    pub request: Value,
    pub state: ControlCommandState,
    pub result: Option<Value>,
    pub result_cursor: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Mutable fields of a durable command lifecycle row.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlCommandUpdate {
    pub state: ControlCommandState,
    pub result: Option<Value>,
    pub result_cursor: Option<u64>,
    pub updated_at: DateTime<Utc>,
}

/// Outcome of inserting a command using its idempotency key.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandInsert {
    Inserted(ControlCommand),
    Duplicate(ControlCommand),
}

/// Input for an append-only durable event.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlEventDraft {
    pub cursor: u64,
    pub event_id: Uuid,
    pub command_id: Option<Uuid>,
    pub kind: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

/// One persisted event returned during replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub cursor: u64,
    pub event_id: Uuid,
    pub command_id: Option<Uuid>,
    pub kind: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

/// Bounded ordered replay result. `high_water_cursor` includes allocator gaps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlReplayPage {
    pub high_water_cursor: u64,
    pub events: Vec<ControlEvent>,
}

/// Storage contract shared by the CQL implementation and fast contract tests.
#[allow(clippy::manual_async_fn)]
pub trait ControlStore: Send + Sync {
    fn reserve_cursor_block(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        size: u64,
    ) -> impl std::future::Future<Output = anyhow::Result<CursorBlock>> + Send;

    fn append_event(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        draft: ControlEventDraft,
    ) -> impl std::future::Future<Output = anyhow::Result<ControlEvent>> + Send;

    fn events_after(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        after_cursor: Option<u64>,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<ControlReplayPage>> + Send;

    fn put_command_if_absent(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command: &ControlCommand,
    ) -> impl std::future::Future<Output = anyhow::Result<CommandInsert>> + Send;

    fn get_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<ControlCommand>>> + Send;

    fn update_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
        update: ControlCommandUpdate,
    ) -> impl std::future::Future<Output = anyhow::Result<ControlCommand>> + Send;
}

#[derive(Default)]
struct MemoryStream {
    next_cursor: u64,
    events: BTreeMap<u64, ControlEvent>,
    commands: HashMap<Uuid, ControlCommand>,
}

/// Deterministic in-memory implementation used for protocol and domain tests.
#[derive(Default)]
pub struct InMemoryControlStore {
    streams: Mutex<HashMap<(Uuid, String), MemoryStream>>,
}

impl ControlStore for InMemoryControlStore {
    async fn reserve_cursor_block(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        size: u64,
    ) -> anyhow::Result<CursorBlock> {
        validate_stream(server_fingerprint)?;
        validate_block_size(size)?;
        let mut streams = self.streams.lock().await;
        let stream = streams
            .entry((ctx.tenant_id, server_fingerprint.to_owned()))
            .or_insert_with(|| MemoryStream {
                next_cursor: 1,
                ..MemoryStream::default()
            });
        let start = stream.next_cursor;
        let end = start
            .checked_add(size - 1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| anyhow::anyhow!("mobile control cursor space exhausted"))?;
        stream.next_cursor = end + 1;
        Ok(CursorBlock { start, end })
    }

    async fn append_event(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        draft: ControlEventDraft,
    ) -> anyhow::Result<ControlEvent> {
        validate_stream(server_fingerprint)?;
        validate_event(&draft)?;
        let mut streams = self.streams.lock().await;
        let stream = streams
            .get_mut(&(ctx.tenant_id, server_fingerprint.to_owned()))
            .ok_or_else(|| anyhow::anyhow!("event cursor was not reserved"))?;
        if draft.cursor >= stream.next_cursor {
            anyhow::bail!("event cursor {} was not reserved", draft.cursor);
        }
        if stream.events.contains_key(&draft.cursor) {
            anyhow::bail!("event cursor {} is already occupied", draft.cursor);
        }
        let event = ControlEvent {
            cursor: draft.cursor,
            event_id: draft.event_id,
            command_id: draft.command_id,
            kind: draft.kind,
            payload: draft.payload,
            created_at: draft.created_at,
        };
        stream.events.insert(event.cursor, event.clone());
        Ok(event)
    }

    async fn events_after(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        after_cursor: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<ControlReplayPage> {
        validate_stream(server_fingerprint)?;
        validate_replay_limit(limit)?;
        let streams = self.streams.lock().await;
        let Some(stream) = streams.get(&(ctx.tenant_id, server_fingerprint.to_owned())) else {
            return Ok(ControlReplayPage {
                high_water_cursor: 0,
                events: Vec::new(),
            });
        };
        let after = after_cursor.unwrap_or(0);
        let mut events: Vec<_> = stream
            .events
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .take(limit + 1)
            .map(|(_, event)| event.clone())
            .collect();
        let has_more = events.len() > limit;
        events.truncate(limit);
        let high_water_cursor = if has_more {
            events.last().map_or(after, |event| event.cursor)
        } else {
            stream.next_cursor.saturating_sub(1)
        };
        Ok(ControlReplayPage {
            high_water_cursor,
            events,
        })
    }

    async fn put_command_if_absent(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command: &ControlCommand,
    ) -> anyhow::Result<CommandInsert> {
        validate_stream(server_fingerprint)?;
        validate_command(command)?;
        let mut streams = self.streams.lock().await;
        let stream = streams
            .entry((ctx.tenant_id, server_fingerprint.to_owned()))
            .or_insert_with(|| MemoryStream {
                next_cursor: 1,
                ..MemoryStream::default()
            });
        if let Some(existing) = stream.commands.get(&command.command_id) {
            return Ok(CommandInsert::Duplicate(existing.clone()));
        }
        stream.commands.insert(command.command_id, command.clone());
        Ok(CommandInsert::Inserted(command.clone()))
    }

    async fn get_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
    ) -> anyhow::Result<Option<ControlCommand>> {
        validate_stream(server_fingerprint)?;
        Ok(self
            .streams
            .lock()
            .await
            .get(&(ctx.tenant_id, server_fingerprint.to_owned()))
            .and_then(|stream| stream.commands.get(&command_id).cloned()))
    }

    async fn update_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
        update: ControlCommandUpdate,
    ) -> anyhow::Result<ControlCommand> {
        validate_stream(server_fingerprint)?;
        validate_command_update(&update)?;
        let mut streams = self.streams.lock().await;
        let command = streams
            .get_mut(&(ctx.tenant_id, server_fingerprint.to_owned()))
            .and_then(|stream| stream.commands.get_mut(&command_id))
            .ok_or_else(|| anyhow::anyhow!("mobile control command {command_id} does not exist"))?;
        if command_matches_update(command, &update) {
            return Ok(command.clone());
        }
        validate_command_transition(command.state, update.state)?;
        command.state = update.state;
        command.result = update.result;
        command.result_cursor = update.result_cursor;
        command.updated_at = update.updated_at;
        Ok(command.clone())
    }
}

/// Ferrosa/CQL-backed implementation used by the running control listener.
#[derive(Clone)]
pub struct CqlControlStore {
    session: Arc<CqlSession>,
    keyspace: String,
}

impl CqlControlStore {
    /// Which event, if any, holds `cursor`.
    ///
    /// Used to settle a conditional insert whose outcome the server did not
    /// report. The event id is the deciding evidence: it is generated per draft,
    /// so a row carrying ours proves our own write landed rather than merely
    /// that something is there.
    async fn event_owner(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        cursor: u64,
    ) -> anyhow::Result<Option<Uuid>> {
        let bucket = cursor_bucket(cursor)?;
        let query = format!(
            "SELECT event_id FROM {}.mobile_control_events \
             WHERE tenant_id = ? AND server_fingerprint = ? AND cursor_bucket = ? AND cursor = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    server_fingerprint,
                    bucket,
                    i64::try_from(cursor)?,
                ),
            )
            .await?;
        let columns = build_col_map(result.col_specs());
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(cql_get::<Uuid>(&row, &columns, "event_id")?))
    }

    /// Apply all ordered application migrations, then connect with runtime
    /// credentials. Startup fails loud if schema migration cannot complete.
    pub async fn connect(config: &FerrosaCqlConfig) -> anyhow::Result<Self> {
        let admin = connect_admin_session(config).await?;
        run_migrations(&admin, &config.keyspace)
            .await
            .map_err(|error| anyhow::anyhow!("mobile control schema migration failed: {error}"))?;
        let session = connect_session(config, &config.username, &config.password).await?;
        Ok(Self {
            session,
            keyspace: config.keyspace.clone(),
        })
    }

    /// Connect to an externally managed schema without issuing DDL.
    ///
    /// The session is restricted to the configured contact points and startup
    /// verifies the current mobile-control migration plus every required table.
    /// This is intentionally opt-in for degraded clusters and DBaaS deployments.
    pub async fn connect_existing(config: &FerrosaCqlConfig) -> anyhow::Result<Self> {
        let session =
            connect_session_to_configured_nodes(config, &config.username, &config.password).await?;
        verify_existing_control_schema(&session, &config.keyspace).await?;
        Ok(Self {
            session,
            keyspace: config.keyspace.clone(),
        })
    }

    /// Construct from an existing session after migrations have run.
    pub fn from_session(session: Arc<CqlSession>, keyspace: impl Into<String>) -> Self {
        Self {
            session,
            keyspace: keyspace.into(),
        }
    }

    async fn cursor_state(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
    ) -> anyhow::Result<Option<(u64, Option<Uuid>)>> {
        let query = format!(
            "SELECT next_cursor, reservation_token FROM {}.mobile_control_cursor_state \
             WHERE tenant_id = ? AND server_fingerprint = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, server_fingerprint))
            .await?;
        let columns = build_col_map(result.col_specs());
        // `None` means NO ROW, which is not the same as a row holding a low
        // cursor. Collapsing the two is what wedged this allocator: the caller
        // used `high_water == 0` to decide between INSERT and UPDATE, so an
        // existing row whose `next_cursor` was 0 or 1 sent it down the
        // `INSERT ... IF NOT EXISTS` path against a row that already existed.
        // That never applies, and the loop reported 32 rounds of it as
        // contention — a permanent, deterministic failure wearing the costume
        // of a transient one.
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        let next: i64 = cql_get(&row, &columns, "next_cursor")?;
        let token = cql_get::<Uuid>(&row, &columns, "reservation_token").ok();
        Ok(Some((u64::try_from(next)?.saturating_sub(1), token)))
    }

    async fn high_water(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
    ) -> anyhow::Result<u64> {
        self.cursor_state(ctx, server_fingerprint)
            .await
            .map(|state| state.map_or(0, |(high_water, _)| high_water))
    }
}

async fn verify_existing_control_schema(
    session: &CqlSession,
    keyspace: &str,
) -> anyhow::Result<()> {
    let expected = MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .ok_or_else(|| anyhow::anyhow!("migration registry is empty"))?;
    let version_query = format!("SELECT version FROM {keyspace}.schema_version WHERE version = ?");
    #[allow(deprecated)]
    let result = session
        .query_unpaged(version_query, (i32::try_from(expected)?,))
        .await
        .context("reading mobile-control schema migration ledger")?;
    if result.rows_or_empty().is_empty() {
        anyhow::bail!(
            "existing schema is not current: migration {expected} is absent; run migrations before control-listen"
        );
    }

    let [cursor_query, event_query, command_query] =
        existing_control_schema_probe_queries(keyspace);
    let tenant = Uuid::nil();
    let fingerprint = "__ferrosa_mobile_schema_probe__";
    #[allow(deprecated)]
    session
        .query_unpaged(cursor_query, (tenant, fingerprint))
        .await
        .context("probing existing mobile-control cursor schema")?;
    #[allow(deprecated)]
    session
        .query_unpaged(event_query, (tenant, fingerprint, -1_i32))
        .await
        .context("probing existing mobile-control event schema")?;
    #[allow(deprecated)]
    session
        .query_unpaged(command_query, (tenant, fingerprint))
        .await
        .context("probing existing mobile-control command schema")?;
    Ok(())
}

fn existing_control_schema_probe_queries(keyspace: &str) -> [String; 3] {
    [
        format!(
            "SELECT next_cursor, reservation_token FROM {keyspace}.mobile_control_cursor_state \
             WHERE tenant_id = ? AND server_fingerprint = ?"
        ),
        format!(
            "SELECT cursor, event_type, payload FROM {keyspace}.mobile_control_events \
             WHERE tenant_id = ? AND server_fingerprint = ? AND cursor_bucket = ?"
        ),
        format!(
            "SELECT command_id, command_type, state, request_payload \
             FROM {keyspace}.mobile_control_commands \
             WHERE tenant_id = ? AND server_fingerprint = ?"
        ),
    ]
}

impl ControlStore for CqlControlStore {
    async fn reserve_cursor_block(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        size: u64,
    ) -> anyhow::Result<CursorBlock> {
        validate_stream(server_fingerprint)?;
        validate_block_size(size)?;
        for _ in 0..32 {
            let existing = self.cursor_state(ctx, server_fingerprint).await?;
            let high_water = existing.map_or(0, |(high_water, _)| high_water);
            let start = high_water.saturating_add(1).max(1);
            let end = start
                .checked_add(size - 1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| anyhow::anyhow!("mobile control cursor space exhausted"))?;
            let next = i64::try_from(end + 1)?;
            let now = Utc::now();
            let reservation_token = Uuid::now_v7();
            let query = if existing.is_none() {
                format!(
                    "INSERT INTO {}.mobile_control_cursor_state \
                     (tenant_id, server_fingerprint, next_cursor, updated_at, reservation_token) \
                     VALUES (?, ?, ?, ?, ?) IF NOT EXISTS",
                    self.keyspace
                )
            } else {
                format!(
                    "UPDATE {}.mobile_control_cursor_state \
                     SET next_cursor = ?, updated_at = ?, reservation_token = ? \
                     WHERE tenant_id = ? AND server_fingerprint = ? IF next_cursor = ?",
                    self.keyspace
                )
            };
            #[allow(deprecated)]
            let result = if existing.is_none() {
                self.session
                    .query_unpaged(
                        query,
                        (
                            ctx.tenant_id,
                            server_fingerprint,
                            next,
                            now,
                            reservation_token,
                        ),
                    )
                    .await?
            } else {
                let expected = i64::try_from(high_water + 1)?;
                self.session
                    .query_unpaged(
                        query,
                        (
                            next,
                            now,
                            reservation_token,
                            ctx.tenant_id,
                            server_fingerprint,
                            expected,
                        ),
                    )
                    .await?
            };
            match lwt_applied(result)? {
                Some(true) => return Ok(CursorBlock { start, end }),
                Some(false) => continue,
                None => {
                    let (observed_high_water, observed_token) = self
                        .cursor_state(ctx, server_fingerprint)
                        .await?
                        .unwrap_or((0, None));
                    if observed_high_water == end && observed_token == Some(reservation_token) {
                        return Ok(CursorBlock { start, end });
                    }
                    // A concurrent winner may have overwritten our token after
                    // our write. Retrying abandons at most one block; it never
                    // reuses or double-claims a cursor.
                }
            }
        }
        // Report the state, not just the count. "Contended after 32 attempts"
        // described a race; when the cause was actually a stuck row, it sent
        // the reader looking for a competing writer that did not exist.
        let observed = self.cursor_state(ctx, server_fingerprint).await?;
        anyhow::bail!(
            "mobile control cursor allocation failed after 32 attempts for {server_fingerprint}; \
             row {}",
            match observed {
                Some((high_water, token)) =>
                    format!("high_water={high_water} reservation_token={token:?}"),
                None => "absent".to_owned(),
            }
        )
    }

    async fn append_event(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        draft: ControlEventDraft,
    ) -> anyhow::Result<ControlEvent> {
        validate_stream(server_fingerprint)?;
        validate_event(&draft)?;
        let high_water = self.high_water(ctx, server_fingerprint).await?;
        if draft.cursor > high_water {
            anyhow::bail!("event cursor {} was not reserved", draft.cursor);
        }
        let payload = serde_json::to_string(&draft.payload)?;
        let bucket = cursor_bucket(draft.cursor)?;
        let cursor = i64::try_from(draft.cursor)?;
        let query = format!(
            "INSERT INTO {}.mobile_control_events \
             (tenant_id, server_fingerprint, cursor_bucket, cursor, event_id, command_id, event_type, payload, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) IF NOT EXISTS",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    server_fingerprint,
                    bucket,
                    cursor,
                    draft.event_id,
                    draft.command_id,
                    draft.kind.as_str(),
                    payload,
                    draft.created_at,
                ),
            )
            .await?;
        match lwt_applied(result)? {
            Some(true) => {}
            Some(false) => {
                anyhow::bail!("event cursor {} is already occupied", draft.cursor)
            }
            None => {
                // The server did not return an [applied] column, so the driver
                // cannot say whether the conditional insert took effect. That is
                // NOT the same as it failing, and treating it as failure is what
                // made every control-session heartbeat report
                //
                //     event cursor N is already occupied
                //     this session is NOT in the durable event log
                //
                // while the row was in fact written. The claim was backwards:
                // 100% of sessions "failed", each at a FRESH cursor one block
                // further on, which is what a healthy allocator handing out new
                // blocks looks like -- not a stale one reissuing old ones.
                //
                // reserve_cursor_block already handles None this way, by reading
                // its own write back. The same server, the same driver, the same
                // ambiguity; this path simply never learned about it.
                //
                // Read back and let the row decide. Ours means the insert
                // applied. Somebody else's means the cursor really was taken.
                let row_owner = self
                    .event_owner(ctx, server_fingerprint, draft.cursor)
                    .await?;
                match row_owner {
                    Some(event_id) if event_id == draft.event_id => {}
                    Some(_) => {
                        anyhow::bail!("event cursor {} is already occupied", draft.cursor)
                    }
                    None => anyhow::bail!(
                        "event cursor {} was neither written nor occupied; the conditional \
                         insert did not apply and left no row",
                        draft.cursor
                    ),
                }
            }
        }
        Ok(ControlEvent {
            cursor: draft.cursor,
            event_id: draft.event_id,
            command_id: draft.command_id,
            kind: draft.kind,
            payload: draft.payload,
            created_at: draft.created_at,
        })
    }

    async fn events_after(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        after_cursor: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<ControlReplayPage> {
        validate_stream(server_fingerprint)?;
        validate_replay_limit(limit)?;
        let high_water = self.high_water(ctx, server_fingerprint).await?;
        let after = after_cursor.unwrap_or(0).min(high_water);
        let fetch_limit = limit + 1;
        let mut events = Vec::new();
        if after < high_water {
            let first_bucket = cursor_bucket(after.saturating_add(1))?;
            let last_bucket = cursor_bucket(high_water)?;
            for bucket in first_bucket..=last_bucket {
                if events.len() >= fetch_limit {
                    break;
                };
                let lower = if bucket == first_bucket {
                    after
                } else {
                    u64::try_from(bucket)? * CONTROL_CURSOR_BUCKET_SIZE
                };
                let Some(remaining) = replay_remaining(fetch_limit, events.len())? else {
                    break;
                };
                // LITERAL limit, not a bound one.
                //
                // A bound `LIMIT ?` is ignored by this engine and the whole
                // partition comes back, which is what overran the page and
                // underflowed the arithmetic above. `remaining` is an i32 this
                // function computed, never caller input, so interpolating it
                // introduces nothing.
                let query = format!(
                    "SELECT cursor, event_id, command_id, event_type, payload, created_at \
                     FROM {}.mobile_control_events \
                     WHERE tenant_id = ? AND server_fingerprint = ? AND cursor_bucket = ? \
                     AND cursor > ? LIMIT {remaining}",
                    self.keyspace
                );
                #[allow(deprecated)]
                let result = self
                    .session
                    .query_unpaged(
                        query,
                        (
                            ctx.tenant_id,
                            server_fingerprint,
                            bucket,
                            i64::try_from(lower)?,
                        ),
                    )
                    .await?;
                let columns = build_col_map(result.col_specs());
                for row in result.rows_or_empty() {
                    let cursor: i64 = cql_get(&row, &columns, "cursor")?;
                    let payload: String = cql_get(&row, &columns, "payload")?;
                    events.push(ControlEvent {
                        cursor: u64::try_from(cursor)?,
                        event_id: cql_get(&row, &columns, "event_id")?,
                        command_id: cql_get::<Uuid>(&row, &columns, "command_id").ok(),
                        kind: cql_get(&row, &columns, "event_type")?,
                        payload: serde_json::from_str(&payload)?,
                        created_at: cql_get(&row, &columns, "created_at")?,
                    });
                }
            }
        }
        let has_more = events.len() > limit;
        events.truncate(limit);
        let replay_high_water = if has_more {
            events.last().map_or(after, |event| event.cursor)
        } else {
            high_water
        };
        Ok(ControlReplayPage {
            high_water_cursor: replay_high_water,
            events,
        })
    }

    async fn put_command_if_absent(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command: &ControlCommand,
    ) -> anyhow::Result<CommandInsert> {
        validate_stream(server_fingerprint)?;
        validate_command(command)?;
        let request = serde_json::to_string(&command.request)?;
        let result_payload = command
            .result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let result_cursor = command.result_cursor.map(i64::try_from).transpose()?;
        let query = format!(
            "INSERT INTO {}.mobile_control_commands \
             (tenant_id, server_fingerprint, command_id, command_type, request_payload, state, result_payload, result_cursor, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) IF NOT EXISTS",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    server_fingerprint,
                    command.command_id,
                    command.command_type.as_str(),
                    request,
                    command.state.as_str(),
                    result_payload,
                    result_cursor,
                    command.created_at,
                    command.updated_at,
                ),
            )
            .await?;
        if lwt_applied(result)? == Some(true) {
            return Ok(CommandInsert::Inserted(command.clone()));
        }
        let existing = self
            .get_command(ctx, server_fingerprint, command.command_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("duplicate command row vanished after LWT"))?;
        Ok(CommandInsert::Duplicate(existing))
    }

    async fn get_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
    ) -> anyhow::Result<Option<ControlCommand>> {
        validate_stream(server_fingerprint)?;
        let query = format!(
            "SELECT command_id, command_type, request_payload, state, result_payload, result_cursor, created_at, updated_at \
             FROM {}.mobile_control_commands \
             WHERE tenant_id = ? AND server_fingerprint = ? AND command_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, server_fingerprint, command_id))
            .await?;
        let columns = build_col_map(result.col_specs());
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        let state: String = cql_get(&row, &columns, "state")?;
        let request: String = cql_get(&row, &columns, "request_payload")?;
        let result_payload = cql_get::<String>(&row, &columns, "result_payload")
            .ok()
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        let result_cursor = cql_get::<i64>(&row, &columns, "result_cursor")
            .ok()
            .map(u64::try_from)
            .transpose()?;
        Ok(Some(ControlCommand {
            command_id: cql_get(&row, &columns, "command_id")?,
            command_type: cql_get(&row, &columns, "command_type")?,
            request: serde_json::from_str(&request)?,
            state: ControlCommandState::parse(&state)?,
            result: result_payload,
            result_cursor,
            created_at: cql_get(&row, &columns, "created_at")?,
            updated_at: cql_get(&row, &columns, "updated_at")?,
        }))
    }

    async fn update_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        command_id: Uuid,
        update: ControlCommandUpdate,
    ) -> anyhow::Result<ControlCommand> {
        validate_stream(server_fingerprint)?;
        validate_command_update(&update)?;
        let existing = self
            .get_command(ctx, server_fingerprint, command_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("mobile control command {command_id} does not exist"))?;
        if command_matches_update(&existing, &update) {
            return Ok(existing);
        }
        validate_command_transition(existing.state, update.state)?;

        let result_payload = update
            .result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let result_cursor = update.result_cursor.map(i64::try_from).transpose()?;
        let query = format!(
            "UPDATE {}.mobile_control_commands \
             SET state = ?, result_payload = ?, result_cursor = ?, updated_at = ? \
             WHERE tenant_id = ? AND server_fingerprint = ? AND command_id = ? IF state = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                query,
                (
                    update.state.as_str(),
                    result_payload,
                    result_cursor,
                    update.updated_at,
                    ctx.tenant_id,
                    server_fingerprint,
                    command_id,
                    existing.state.as_str(),
                ),
            )
            .await?;
        if lwt_applied(result)? == Some(false) {
            let current = self
                .get_command(ctx, server_fingerprint, command_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("mobile control command vanished during update"))?;
            if command_matches_update(&current, &update) {
                return Ok(current);
            }
            anyhow::bail!("mobile control command changed concurrently");
        }
        let current = self
            .get_command(ctx, server_fingerprint, command_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("mobile control command vanished after update"))?;
        if !command_matches_update(&current, &update) {
            anyhow::bail!("mobile control command update could not be verified");
        }
        Ok(current)
    }
}

fn validate_stream(server_fingerprint: &str) -> anyhow::Result<()> {
    if server_fingerprint.is_empty() || server_fingerprint.len() > 128 {
        anyhow::bail!("server fingerprint must contain 1..=128 bytes");
    }
    Ok(())
}

fn validate_block_size(size: u64) -> anyhow::Result<()> {
    if size == 0 || size > MAX_CONTROL_CURSOR_BLOCK {
        anyhow::bail!("cursor block size must be 1..={MAX_CONTROL_CURSOR_BLOCK}");
    }
    Ok(())
}

fn validate_replay_limit(limit: usize) -> anyhow::Result<()> {
    if limit == 0 || limit > MAX_CONTROL_REPLAY_EVENTS {
        anyhow::bail!("replay limit must be 1..={MAX_CONTROL_REPLAY_EVENTS}");
    }
    Ok(())
}

fn validate_payload(payload: &Value) -> anyhow::Result<()> {
    let size = serde_json::to_vec(payload)?.len();
    if size > MAX_CONTROL_PAYLOAD_BYTES {
        anyhow::bail!("control payload is {size} bytes and exceeds {MAX_CONTROL_PAYLOAD_BYTES}");
    }
    Ok(())
}

fn validate_event(draft: &ControlEventDraft) -> anyhow::Result<()> {
    if draft.cursor == 0 || draft.cursor > i64::MAX as u64 {
        anyhow::bail!("event cursor must be within 1..=i64::MAX");
    }
    if draft.kind.is_empty() || draft.kind.len() > 64 {
        anyhow::bail!("event type must contain 1..=64 bytes");
    }
    validate_payload(&draft.payload)
}

fn validate_command(command: &ControlCommand) -> anyhow::Result<()> {
    if command.command_type.is_empty() || command.command_type.len() > 64 {
        anyhow::bail!("command type must contain 1..=64 bytes");
    }
    validate_payload(&command.request)?;
    if let Some(result) = &command.result {
        validate_payload(result)?;
    }
    if command
        .result_cursor
        .is_some_and(|cursor| cursor > i64::MAX as u64)
    {
        anyhow::bail!("result cursor exceeds i64::MAX");
    }
    Ok(())
}

fn validate_command_update(update: &ControlCommandUpdate) -> anyhow::Result<()> {
    if let Some(result) = &update.result {
        validate_payload(result)?;
    }
    if update
        .result_cursor
        .is_some_and(|cursor| cursor > i64::MAX as u64)
    {
        anyhow::bail!("result cursor exceeds i64::MAX");
    }
    Ok(())
}

fn command_matches_update(command: &ControlCommand, update: &ControlCommandUpdate) -> bool {
    command.state == update.state
        && command.result == update.result
        && command.result_cursor == update.result_cursor
}

fn validate_command_transition(
    current: ControlCommandState,
    next: ControlCommandState,
) -> anyhow::Result<()> {
    let allowed = matches!(
        (current, next),
        (ControlCommandState::Queued, ControlCommandState::Running)
            | (ControlCommandState::Queued, ControlCommandState::Failed)
            | (ControlCommandState::Queued, ControlCommandState::Cancelled)
            | (ControlCommandState::Queued, ControlCommandState::Expired)
            | (ControlCommandState::Running, ControlCommandState::Succeeded)
            | (ControlCommandState::Running, ControlCommandState::Failed)
            | (ControlCommandState::Running, ControlCommandState::Cancelled)
            | (ControlCommandState::Running, ControlCommandState::Expired)
    );
    if !allowed {
        anyhow::bail!("invalid mobile control command transition {current:?} -> {next:?}");
    }
    Ok(())
}

/// How many more events this replay page may take, or `None` when it is full.
///
/// Saturating and `None`-terminated rather than a bare subtraction. The loop
/// guarded only `events.len() == fetch_limit`, so a page that came back LARGER
/// than asked walked straight past it and underflowed `fetch_limit - len` into
/// a huge `usize`. The conversion then failed with "out of range integral type
/// conversion attempted", which the session surfaced as a control protocol
/// violation and hung up — every session, immediately, on one machine.
///
/// A page CAN come back larger: this engine ignores a bound `LIMIT ?` and
/// returns the whole partition, so the only limit that binds is a literal one.
/// This function is correct either way, which is the point — it does not rely
/// on the database having honoured anything.
fn replay_remaining(fetch_limit: usize, collected: usize) -> anyhow::Result<Option<i32>> {
    let remaining = fetch_limit.saturating_sub(collected);
    if remaining == 0 {
        return Ok(None);
    }
    Ok(Some(i32::try_from(remaining)?))
}

fn cursor_bucket(cursor: u64) -> anyhow::Result<i32> {
    if cursor == 0 {
        anyhow::bail!("cursor zero is not a durable event cursor");
    }
    i32::try_from((cursor - 1) / CONTROL_CURSOR_BUCKET_SIZE)
        .map_err(|_| anyhow::anyhow!("cursor bucket exceeds CQL int range"))
}

fn lwt_applied(result: scylla::LegacyQueryResult) -> anyhow::Result<Option<bool>> {
    let columns = build_col_map(result.col_specs());
    let Some(index) = columns.get("[applied]") else {
        return Ok(None);
    };
    let Some(row) = result.rows_or_empty().into_iter().next() else {
        return Ok(None);
    };
    match row.columns.get(*index).cloned().flatten() {
        Some(CqlValue::Boolean(value)) => Ok(Some(value)),
        _ => anyhow::bail!("conditional CQL [applied] value is not boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_schema_probe_is_partition_bounded() {
        let queries = existing_control_schema_probe_queries("agent_memory");
        assert_eq!(queries.len(), 3);
        for query in queries {
            assert!(query.contains("WHERE tenant_id = ? AND server_fingerprint = ?"));
            assert!(!query.contains("system_schema"));
            assert!(!query.contains("LIMIT"));
        }
    }

    #[test]
    fn existing_schema_probe_covers_every_control_table() {
        let queries = existing_control_schema_probe_queries("agent_memory");
        assert!(queries[0].contains("mobile_control_cursor_state"));
        assert!(queries[0].contains("reservation_token"));
        assert!(queries[1].contains("mobile_control_events"));
        assert!(queries[1].contains("cursor_bucket = ?"));
        assert!(queries[1].contains("payload"));
        assert!(queries[2].contains("mobile_control_commands"));
        assert!(queries[2].contains("request_payload"));
    }

    #[test]
    fn an_overrun_page_reports_full_rather_than_underflowing() {
        assert_eq!(replay_remaining(10, 25).unwrap(), None);
        assert_eq!(replay_remaining(1, usize::MAX).unwrap(), None);
    }

    #[test]
    fn an_exactly_full_page_is_full() {
        assert_eq!(replay_remaining(10, 10).unwrap(), None);
    }

    #[test]
    fn a_partial_page_asks_for_the_difference() {
        assert_eq!(replay_remaining(10, 4).unwrap(), Some(6));
        assert_eq!(replay_remaining(10, 0).unwrap(), Some(10));
    }

    #[test]
    fn a_limit_beyond_cql_int_range_is_refused() {
        assert!(replay_remaining(usize::MAX, 0).is_err());
    }
}
