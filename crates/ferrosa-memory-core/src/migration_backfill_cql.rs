//! CQL-backed pieces of the streaming backfill: the per-row move transforms and
//! (next) the `RowStream` / `RowSink` / `Checkpoints` impls over a real session.
//!
//! Transforms here honor the [`crate::migration_backfill`] memory discipline:
//! they **move `CqlValue`s out of the source row** (`Option::take`) and hand the
//! unchanged ones straight to the dest INSERT, re-encoding only the column whose
//! *type* changes. Nothing is cloned; the source row is consumed.
//!
//! A fixed-arity tuple of `CqlValue` implements `SerializeRow`, so a transformed
//! row binds directly to a prepared INSERT (the "dynamic-length parameter list"
//! limitation only applies to *variable* arity).

#![allow(deprecated)] // legacy scylla deserialization API, as used across the crate

use anyhow::{Context, Result};
use scylla::frame::response::result::{CqlValue, Row};

use crate::cql_storage::ColMap;

/// Move the `CqlValue` at column `name` out of `row`, leaving `None` behind.
/// Errors if the column is absent from the projection (a schema/SELECT mismatch
/// — fail loud rather than silently substitute).
fn take_col(row: &mut Row, col_map: &ColMap, name: &str) -> Result<Option<CqlValue>> {
    let idx = *col_map
        .get(name)
        .with_context(|| format!("backfill: source projection missing column `{name}`"))?;
    Ok(row.columns.get_mut(idx).and_then(|slot| slot.take()))
}

/// Re-encode a `uuid` endpoint column as `text`: move the value out, and if it
/// is a UUID, replace it with its canonical string form. A `NULL` stays `NULL`.
/// Any other type is a real schema violation and fails loud.
fn uuid_col_to_text(row: &mut Row, col_map: &ColMap, name: &str) -> Result<CqlValue> {
    match take_col(row, col_map, name)? {
        Some(CqlValue::Uuid(u)) => Ok(CqlValue::Text(u.to_string())),
        Some(CqlValue::Text(s)) => Ok(CqlValue::Text(s)), // already migrated / idempotent
        None => Ok(CqlValue::Empty),                      // represents NULL bind; see note in tests
        Some(other) => anyhow::bail!(
            "backfill: column `{name}` expected uuid, found {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Move a column through unchanged (used for columns whose type is unchanged).
fn move_col(row: &mut Row, col_map: &ColMap, name: &str) -> Result<CqlValue> {
    Ok(take_col(row, col_map, name)?.unwrap_or(CqlValue::Empty))
}

/// The destination row for `derived_cache_by_query` with `uuid → text` endpoints.
/// A fixed 9-`CqlValue` tuple — binds straight to the v2 INSERT.
pub type DerivedCacheV2Row = (
    CqlValue, // tenant_id   (uuid, moved)
    CqlValue, // cache_key   (text, moved)
    CqlValue, // seq         (int,  moved)
    CqlValue, // src_id      (uuid -> text)
    CqlValue, // pred        (text, moved)
    CqlValue, // dst_id      (uuid -> text)
    CqlValue, // confidence  (double, moved)
    CqlValue, // rule_id     (text, moved)
    CqlValue, // computed_at (timestamp, moved)
);

/// Transform one `derived_cache_by_query` row into its text-endpoint form,
/// moving unchanged columns through and re-encoding only `src_id`/`dst_id`
/// (issue #129 option-1, via the migration-streaming backfill). Consumes the row.
pub fn rewrite_derived_cache_endpoints(
    mut row: Row,
    col_map: &ColMap,
) -> Result<DerivedCacheV2Row> {
    let row = &mut row;
    Ok((
        move_col(row, col_map, "tenant_id")?,
        move_col(row, col_map, "cache_key")?,
        move_col(row, col_map, "seq")?,
        uuid_col_to_text(row, col_map, "src_id")?,
        move_col(row, col_map, "pred")?,
        uuid_col_to_text(row, col_map, "dst_id")?,
        move_col(row, col_map, "confidence")?,
        move_col(row, col_map, "rule_id")?,
        move_col(row, col_map, "computed_at")?,
    ))
}

// ─── Production CQL implementations of the streaming-rewrite I/O traits ──────

use std::collections::HashMap;
use std::marker::PhantomData;
use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use scylla::serialize::row::SerializeRow;

use crate::cql_storage::{CqlSession, build_col_map, cql_get};
use crate::migration_backfill::{BackfillReport, Checkpoints, Cursor, RowSink, RowStream};

/// Checkpoint store backed by a `migration_backfill_progress` table.
pub struct CqlCheckpoints<'a> {
    pub session: &'a CqlSession,
    pub keyspace: &'a str,
}

impl Checkpoints for CqlCheckpoints<'_> {
    async fn load(&self, job: &str) -> Result<Option<Cursor>> {
        let q = format!(
            "SELECT cursor FROM {}.migration_backfill_progress WHERE job = ?",
            self.keyspace
        );
        let res = self.session.query_unpaged(q, (job,)).await?;
        let col_map = build_col_map(res.col_specs());
        let rows = res.rows_or_empty();
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let cursor: Vec<u8> = cql_get(row, &col_map, "cursor")?;
        Ok(Some(cursor))
    }

    async fn save(&self, job: &str, cursor: &[u8]) -> Result<()> {
        let q = format!(
            "INSERT INTO {}.migration_backfill_progress (job, cursor, updated_at) VALUES (?, ?, ?)",
            self.keyspace
        );
        self.session
            .query_unpaged(q, (job, cursor, chrono::Utc::now()))
            .await?;
        Ok(())
    }
}

/// Boxed row stream so we don't have to name scylla's iterator type. Errors are
/// normalized to `anyhow` before boxing. Not `Send`: the backfill runs inline in
/// the (single-threaded) migration path, never spawned.
type BoxedRows<'a> = Pin<Box<dyn Stream<Item = Result<Row>> + 'a>>;

/// Full-scan streaming SELECT. The driver pages internally — fmem holds one row
/// at a time, never a page.
///
/// Resume is **count-based**: `execute_iter` returns rows in a stable
/// storage/token order for a given table, so the cursor is simply the number of
/// rows consumed; resuming re-opens the scan and skips that many. (Token-range
/// resume — `WHERE token(pk) > ?` — would avoid the re-scan, but ferrosa's CQL
/// parser does not yet accept `token(<composite pk>)`; see the upstream gap.
/// Inserts are idempotent upserts, so the skipped re-read is harmless.)
pub struct CqlRowStream<'a> {
    session: &'a CqlSession,
    /// `SELECT <cols> FROM <ks>.<table>` — a full scan, no bind parameters.
    select_sql: String,
    /// Rows to skip on (re)open to resume to the checkpointed position.
    to_skip: u64,
    /// Absolute rows consumed from the logical stream (incl. skipped) — the cursor.
    consumed: u64,
    opened: Option<BoxedRows<'a>>,
}

impl<'a> CqlRowStream<'a> {
    pub fn new(session: &'a CqlSession, select_sql: String) -> Self {
        Self {
            session,
            select_sql,
            to_skip: 0,
            consumed: 0,
            opened: None,
        }
    }
}

impl RowStream for CqlRowStream<'_> {
    type Row = Row;

    async fn seek(&mut self, cursor: Cursor) -> Result<()> {
        anyhow::ensure!(
            cursor.len() == 8,
            "backfill cursor must be an 8-byte row count, got {} bytes",
            cursor.len()
        );
        self.to_skip = u64::from_be_bytes(cursor.as_slice().try_into().unwrap());
        self.opened = None; // reopen and re-skip on next pull
        Ok(())
    }

    async fn next_row(&mut self) -> Result<Option<Row>> {
        if self.opened.is_none() {
            let stmt = self.session.prepare(self.select_sql.as_str()).await?;
            let iter = self.session.execute_iter(stmt, ()).await?;
            let mut stream: BoxedRows<'_> = Box::pin(iter.map(|r| r.map_err(anyhow::Error::from)));
            // Skip already-processed rows to resume to the checkpoint.
            let mut skipped = 0u64;
            while skipped < self.to_skip {
                match stream.next().await {
                    Some(row) => {
                        row?; // surface a read error rather than silently stop
                        self.consumed += 1;
                        skipped += 1;
                    }
                    None => break, // source shorter than checkpoint (shrank) — stop
                }
            }
            self.opened = Some(stream);
        }
        let stream = self.opened.as_mut().unwrap();
        match stream.next().await {
            Some(row) => {
                let row = row?;
                self.consumed += 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn cursor(&self) -> Option<Cursor> {
        (self.consumed > 0 || self.to_skip > 0).then(|| self.consumed.to_be_bytes().to_vec())
    }
}

/// Per-row INSERT sink. `D` is a fixed-arity tuple of `CqlValue` bound straight
/// to the prepared INSERT (idempotent upsert — re-running a row is safe).
pub struct CqlRowSink<'a, D> {
    session: &'a CqlSession,
    insert: scylla::prepared_statement::PreparedStatement,
    _d: PhantomData<D>,
}

impl<'a, D> CqlRowSink<'a, D> {
    pub fn new(
        session: &'a CqlSession,
        insert: scylla::prepared_statement::PreparedStatement,
    ) -> Self {
        Self {
            session,
            insert,
            _d: PhantomData,
        }
    }
}

impl<D: SerializeRow> RowSink for CqlRowSink<'_, D> {
    type Row = D;

    async fn put(&mut self, row: D) -> Result<()> {
        self.session.execute_unpaged(&self.insert, row).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(()) // nothing buffered; each row is written as it streams
    }
}

/// The fixed projection (name → index) for the derived-cache backfill SELECT, so
/// the tested name-based [`rewrite_derived_cache_endpoints`] transform threads
/// through without consulting the live result metadata.
fn derived_cache_projection() -> ColMap {
    [
        "tenant_id",
        "cache_key",
        "seq",
        "src_id",
        "pred",
        "dst_id",
        "confidence",
        "rule_id",
        "computed_at",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, name)| (name.to_string(), i))
    .collect::<HashMap<_, _>>()
}

/// Backfill `derived_cache_by_query` → `derived_cache_by_query_v2` (uuid → text
/// endpoints) via the streaming-rewrite primitive. Caller must have created the
/// v2 table + the `migration_backfill_progress` table and applied grants.
pub async fn backfill_derived_cache_endpoints(
    session: &CqlSession,
    keyspace: &str,
) -> Result<BackfillReport> {
    let select_sql = format!(
        "SELECT tenant_id, cache_key, seq, src_id, pred, dst_id, confidence, rule_id, \
         computed_at \
         FROM {keyspace}.derived_cache_by_query"
    );
    let insert = session
        .prepare(format!(
            "INSERT INTO {keyspace}.derived_cache_by_query_v2 \
             (tenant_id, cache_key, seq, src_id, pred, dst_id, confidence, rule_id, computed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .await?;

    let mut source = CqlRowStream::new(session, select_sql);
    let mut sink: CqlRowSink<DerivedCacheV2Row> = CqlRowSink::new(session, insert);
    let checkpoints = CqlCheckpoints { session, keyspace };
    let projection = derived_cache_projection();

    crate::migration_backfill::streaming_rewrite(
        &mut source,
        &mut sink,
        &checkpoints,
        "derived_cache_uuid_to_text",
        500,
        move |row| rewrite_derived_cache_endpoints(row, &projection),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    /// Build a synthetic positional row + name→index map (no cluster needed).
    fn synthetic_row(cols: Vec<(&str, Option<CqlValue>)>) -> (Row, ColMap) {
        let mut col_map: ColMap = HashMap::new();
        let mut columns = Vec::new();
        for (i, (name, val)) in cols.into_iter().enumerate() {
            col_map.insert(name.to_string(), i);
            columns.push(val);
        }
        (Row { columns }, col_map)
    }

    fn derived_row(src: Uuid, dst: Uuid) -> (Row, ColMap) {
        synthetic_row(vec![
            ("tenant_id", Some(CqlValue::Uuid(Uuid::from_u128(1)))),
            (
                "cache_key",
                Some(CqlValue::Text("consolidation:abc".into())),
            ),
            ("seq", Some(CqlValue::Int(7))),
            ("src_id", Some(CqlValue::Uuid(src))),
            ("pred", Some(CqlValue::Text("isa".into()))),
            ("dst_id", Some(CqlValue::Uuid(dst))),
            ("confidence", Some(CqlValue::Double(0.9))),
            ("rule_id", Some(CqlValue::Text("isa:-instance_of".into()))),
            (
                "computed_at",
                Some(CqlValue::Timestamp(scylla::frame::value::CqlTimestamp(123))),
            ),
        ])
    }

    #[test]
    fn rewrites_uuid_endpoints_to_text_and_moves_the_rest() {
        let src = Uuid::from_u128(0xAA);
        let dst = Uuid::from_u128(0xBB);
        let (row, col_map) = derived_row(src, dst);

        let out = rewrite_derived_cache_endpoints(row, &col_map).unwrap();

        // endpoints are now text with the canonical uuid form
        assert_eq!(out.3, CqlValue::Text(src.to_string()));
        assert_eq!(out.5, CqlValue::Text(dst.to_string()));
        // unchanged columns moved through untouched
        assert_eq!(out.0, CqlValue::Uuid(Uuid::from_u128(1)));
        assert_eq!(out.1, CqlValue::Text("consolidation:abc".into()));
        assert_eq!(out.2, CqlValue::Int(7));
        assert_eq!(out.4, CqlValue::Text("isa".into()));
        assert_eq!(out.6, CqlValue::Double(0.9));
        assert_eq!(out.7, CqlValue::Text("isa:-instance_of".into()));
    }

    #[test]
    fn idempotent_on_already_text_endpoints() {
        // Re-running the backfill over already-migrated rows must not fail.
        let (row, col_map) = synthetic_row(vec![
            ("tenant_id", Some(CqlValue::Uuid(Uuid::from_u128(1)))),
            ("cache_key", Some(CqlValue::Text("k".into()))),
            ("seq", Some(CqlValue::Int(0))),
            ("src_id", Some(CqlValue::Text("already-text".into()))),
            ("pred", Some(CqlValue::Text("p".into()))),
            ("dst_id", Some(CqlValue::Text("conversation_turn".into()))),
            ("confidence", Some(CqlValue::Double(1.0))),
            ("rule_id", Some(CqlValue::Text("r".into()))),
            (
                "computed_at",
                Some(CqlValue::Timestamp(scylla::frame::value::CqlTimestamp(1))),
            ),
        ]);
        let out = rewrite_derived_cache_endpoints(row, &col_map).unwrap();
        assert_eq!(out.3, CqlValue::Text("already-text".into()));
        assert_eq!(out.5, CqlValue::Text("conversation_turn".into()));
    }

    #[test]
    fn missing_projection_column_fails_loud() {
        // src_id absent from the SELECT projection — must error, not silently nil.
        let (row, col_map) = synthetic_row(vec![
            ("tenant_id", Some(CqlValue::Uuid(Uuid::from_u128(1)))),
            ("cache_key", Some(CqlValue::Text("k".into()))),
            ("seq", Some(CqlValue::Int(0))),
            ("pred", Some(CqlValue::Text("p".into()))),
            ("dst_id", Some(CqlValue::Uuid(Uuid::from_u128(2)))),
            ("confidence", Some(CqlValue::Double(1.0))),
            ("rule_id", Some(CqlValue::Text("r".into()))),
            (
                "computed_at",
                Some(CqlValue::Timestamp(scylla::frame::value::CqlTimestamp(1))),
            ),
        ]);
        let err = rewrite_derived_cache_endpoints(row, &col_map).unwrap_err();
        assert!(err.to_string().contains("missing column `src_id`"), "{err}");
    }

    #[test]
    fn wrong_endpoint_type_fails_loud() {
        let (row, col_map) = synthetic_row(vec![
            ("tenant_id", Some(CqlValue::Uuid(Uuid::from_u128(1)))),
            ("cache_key", Some(CqlValue::Text("k".into()))),
            ("seq", Some(CqlValue::Int(0))),
            ("src_id", Some(CqlValue::Int(99))), // not a uuid/text
            ("pred", Some(CqlValue::Text("p".into()))),
            ("dst_id", Some(CqlValue::Uuid(Uuid::from_u128(2)))),
            ("confidence", Some(CqlValue::Double(1.0))),
            ("rule_id", Some(CqlValue::Text("r".into()))),
            (
                "computed_at",
                Some(CqlValue::Timestamp(scylla::frame::value::CqlTimestamp(1))),
            ),
        ]);
        let err = rewrite_derived_cache_endpoints(row, &col_map).unwrap_err();
        assert!(err.to_string().contains("expected uuid"), "{err}");
    }
}
