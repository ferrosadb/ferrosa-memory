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
