//! Probe the mobile-control cursor row and the LWT the allocator depends on.
//!
//! `reserve_cursor_block` retries a compare-and-set 32 times and then reports
//! contention. Contention and "the compare can never succeed" produce the same
//! message, and the difference decides whether the bug is in this repository or
//! in the database. This reads the row, then runs the allocator's exact
//! `UPDATE ... IF next_cursor = ?` with the value that row implies, and prints
//! `[applied]`.
//!
//! A `false` here, against a condition that matches the row this same program
//! just read, is a database bug and belongs upstream — not worked around.
//!
//!   CQL_ADDR=127.0.0.1:19042 CQL_KEYSPACE=ferrosa_memory \
//!     cargo run -p ferrosa-memory-core --example cursor_lwt_probe

use scylla::SessionBuilder;
use scylla::frame::response::result::CqlValue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("CQL_ADDR").unwrap_or_else(|_| "127.0.0.1:19042".to_owned());
    let keyspace = std::env::var("CQL_KEYSPACE").unwrap_or_else(|_| "ferrosa_memory".to_owned());
    // LegacySession: the same API `control_store` uses, so the probe exercises
    // the same driver path as the allocator rather than a neighbouring one.
    // Deprecated upstream, and deliberate here: `control_store` uses it, and a
    // probe that took the modern path would be testing a different code path
    // than the one that fails.
    #[allow(deprecated)]
    let session = SessionBuilder::new()
        .known_node(&addr)
        .build_legacy()
        .await?;
    println!("connected to {addr}, keyspace {keyspace}");

    let table = format!("{keyspace}.mobile_control_cursor_state");

    #[allow(deprecated)]
    let rows = session
        .query_unpaged(
            format!("SELECT tenant_id, server_fingerprint, next_cursor FROM {table}"),
            (),
        )
        .await?;
    let specs: Vec<String> = rows
        .col_specs()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    println!("columns: {specs:?}");

    let all = rows.rows_or_empty();
    println!("rows: {}", all.len());
    for row in &all {
        println!("  {:?}", row.columns);
    }

    let Some(row) = all.into_iter().next() else {
        println!("no cursor row; nothing to probe");
        return Ok(());
    };
    let tenant = row.columns[0].clone().expect("tenant_id");
    let fingerprint = row.columns[1].clone().expect("server_fingerprint");
    let Some(CqlValue::BigInt(next_cursor)) = row.columns[2].clone() else {
        println!("next_cursor is not a bigint: {:?}", row.columns[2]);
        return Ok(());
    };
    println!("next_cursor={next_cursor}");

    // Exactly the allocator's statement, with the value the row itself says is
    // there. Writing next_cursor back unchanged, so a success is harmless.
    #[allow(deprecated)]
    let result = session
        .query_unpaged(
            format!(
                "UPDATE {table} SET next_cursor = ?, updated_at = ?, reservation_token = ? \
                 WHERE tenant_id = ? AND server_fingerprint = ? IF next_cursor = ?"
            ),
            (
                next_cursor,
                chrono::Utc::now(),
                uuid::Uuid::now_v7(),
                tenant,
                fingerprint,
                next_cursor,
            ),
        )
        .await?;

    let specs: Vec<String> = result
        .col_specs()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    println!("LWT result columns: {specs:?}");
    for row in result.rows_or_empty() {
        println!("LWT row: {:?}", row.columns);
    }
    println!(
        "\nIf [applied] is absent or false while next_cursor={next_cursor} matched the \
         condition, the database did not honour the comparison."
    );
    Ok(())
}
