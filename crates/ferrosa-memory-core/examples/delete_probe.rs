//! Does `DELETE` work on this cluster at all?
//!
//! Deleting a row from `mobile_control_cursor_state` was accepted and had no
//! effect — the row stayed visible to both a filtered scan and a point read on
//! the partition key, with bound parameters and with literals. Before calling
//! that a database bug it has to be separated from three cheaper explanations:
//! writes not working at all, something specific to that table, and a read path
//! ignoring tombstones.
//!
//! So: a scratch table of its own, an INSERT verified present, a DELETE
//! verified absent. Nothing in a real table, and the scratch table is dropped
//! at the end.
//!
//!   CQL_ADDR=127.0.0.1:19042 CQL_KEYSPACE=agent_memory \
//!     cargo run -p ferrosa-memory-core --example delete_probe

use scylla::SessionBuilder;
use uuid::Uuid;

// The legacy session API is deprecated upstream and deliberate here: it is
// what `control_store` uses, so a probe on the modern path would exercise a
// different code path than the one being diagnosed.
#[allow(deprecated)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("CQL_ADDR").unwrap_or_else(|_| "127.0.0.1:19042".to_owned());
    let keyspace = std::env::var("CQL_KEYSPACE").unwrap_or_else(|_| "agent_memory".to_owned());
    #[allow(deprecated)]
    let session = SessionBuilder::new()
        .known_node(&addr)
        .build_legacy()
        .await?;

    let table = format!("{keyspace}.ferrosa_delete_probe");
    // Same key shape as the table that would not delete: a COMPOSITE partition
    // key of (uuid, text). If a single-column key deletes and this one does
    // not, that is the finding.
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                 a uuid, b text, v bigint, PRIMARY KEY ((a, b)))"
            ),
            (),
        )
        .await?;

    let a = Uuid::now_v7();
    let b = "probe";

    #[allow(deprecated)]
    session
        .query_unpaged(
            format!("INSERT INTO {table} (a, b, v) VALUES (?, ?, ?)"),
            (a, b, 1i64),
        )
        .await?;

    let present = count(&session, &table, a, b).await?;
    println!("after insert: {present} row(s)  (expect 1)");
    if present != 1 {
        println!("WRITES are not landing — the delete result says nothing about DELETE");
        return Ok(());
    }

    #[allow(deprecated)]
    session
        .query_unpaged(format!("DELETE FROM {table} WHERE a = ? AND b = ?"), (a, b))
        .await?;

    let after = count(&session, &table, a, b).await?;
    println!("after delete: {after} row(s)  (expect 0)");
    println!(
        "{}",
        if after == 0 {
            "DELETE works here — the cursor-row failure is specific to that table or row"
        } else {
            "DELETE is ACCEPTED AND IGNORED on a composite partition key — database bug"
        }
    );

    // Clean up after ourselves regardless of the verdict.
    #[allow(deprecated)]
    session
        .query_unpaged(format!("DROP TABLE IF EXISTS {table}"), ())
        .await?;
    Ok(())
}

// Deprecated upstream and deliberate here: `control_store` uses this API, so a
// probe on the modern path would exercise a different code path than the one
// being diagnosed.
#[allow(deprecated)]
async fn count(
    session: &scylla::LegacySession,
    table: &str,
    a: Uuid,
    b: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    #[allow(deprecated)]
    let rows = session
        .query_unpaged(
            format!("SELECT v FROM {table} WHERE a = ? AND b = ?"),
            (a, b),
        )
        .await?;
    Ok(rows.rows_or_empty().len())
}
