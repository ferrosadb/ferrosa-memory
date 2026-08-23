//! Delete one server's mobile-control cursor row.
//!
//! An operational unblock, not a fix. Ferrosa's `UPDATE ... IF <bigint> = ?`
//! always reports not-applied, so the allocator's compare-and-set can never
//! succeed against an existing row — but `INSERT ... IF NOT EXISTS` performs no
//! comparison and still works. Removing the row therefore buys exactly one
//! session, after which the next allocation hits the same wall.
//!
//! Deliberately narrow: it takes ONE fingerprint and refuses to run without it.
//! A sweep over the table would take out other servers' cursors too, and the
//! blast radius of "reset all durable control history" is not what this is for.
//!
//! Resetting the server's cursor without also clearing the client's stored
//! resume position leaves the two disagreeing — the client asks to resume from
//! a cursor the server has never issued and waits for events that will not
//! come. Clear both, or neither.
//!
//!   CQL_ADDR=127.0.0.1:19042 CQL_KEYSPACE=agent_memory \
//!     cargo run -p ferrosa-memory-core --example cursor_reset -- <fingerprint>

use scylla::SessionBuilder;

// The legacy session API is deprecated upstream and deliberate here: it is
// what `control_store` uses, so a probe on the modern path would exercise a
// different code path than the one being diagnosed.
#[allow(deprecated)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fingerprint = std::env::args()
        .nth(1)
        .ok_or("pass the server fingerprint to reset; refusing to guess or to clear every row")?;
    let addr = std::env::var("CQL_ADDR").unwrap_or_else(|_| "127.0.0.1:19042".to_owned());
    let keyspace = std::env::var("CQL_KEYSPACE").unwrap_or_else(|_| "agent_memory".to_owned());

    // Deprecated upstream, deliberate here: `control_store` uses it, so this
    // reads and writes the same way the code being unblocked does.
    #[allow(deprecated)]
    let session = SessionBuilder::new()
        .known_node(&addr)
        .build_legacy()
        .await?;
    let table = format!("{keyspace}.mobile_control_cursor_state");

    // Show what is about to go, so the operator can see it was the intended
    // row and not somebody else's.
    #[allow(deprecated)]
    let before = session
        .query_unpaged(
            format!(
                "SELECT tenant_id, server_fingerprint, next_cursor FROM {table} \
                 WHERE server_fingerprint = ? ALLOW FILTERING"
            ),
            (fingerprint.as_str(),),
        )
        .await?;
    let rows = before.rows_or_empty();
    if rows.is_empty() {
        println!("no cursor row for {fingerprint}; nothing to reset");
        return Ok(());
    }
    let mut rows_snapshot = Vec::new();
    for row in &rows {
        println!("deleting {:?}", row.columns);
        if let Some(tenant) = row.columns[0].clone() {
            rows_snapshot.push(tenant);
        }
    }

    for row in rows {
        let tenant = row.columns[0].clone().ok_or("row has no tenant_id")?;
        let scylla::frame::response::result::CqlValue::Uuid(tenant_uuid) = tenant else {
            return Err("tenant_id is not a uuid".into());
        };

        // Bound parameters first — the shape the application uses.
        #[allow(deprecated)]
        session
            .query_unpaged(
                format!("DELETE FROM {table} WHERE tenant_id = ? AND server_fingerprint = ?"),
                (tenant_uuid, fingerprint.as_str()),
            )
            .await?;

        // A plain DELETE is accepted and ignored for this row, while the same
        // statement on a fresh table with the same key shape works. The one
        // thing that distinguishes this row is that every write to it went
        // through the LWT path — so delete it the same way. `IF EXISTS`
        // compares nothing, so it avoids the coercion bug that breaks
        // `IF <bigint> = ?`.
        #[allow(deprecated)]
        let conditional = session
            .query_unpaged(
                format!(
                    "DELETE FROM {table} WHERE tenant_id = ? AND server_fingerprint = ? IF EXISTS"
                ),
                (tenant_uuid, fingerprint.as_str()),
            )
            .await?;
        for row in conditional.rows_or_empty() {
            println!("conditional delete -> {:?}", row.columns);
        }
    }

    // Verify on BOTH read paths, because they are not the same path. The scan
    // above uses ALLOW FILTERING; the allocator does a point read on the
    // partition key. A tombstone honoured by one and not the other would make
    // this look failed while the application saw it succeed, or the reverse —
    // and only the point read predicts what the next session will do.
    let mut point = 0usize;
    for row in &rows_snapshot {
        let scylla::frame::response::result::CqlValue::Uuid(tenant_uuid) = row else {
            continue;
        };
        #[allow(deprecated)]
        let direct = session
            .query_unpaged(
                format!(
                    "SELECT next_cursor FROM {table} \
                     WHERE tenant_id = ? AND server_fingerprint = ?"
                ),
                (*tenant_uuid, fingerprint.as_str()),
            )
            .await?;
        point += direct.rows_or_empty().len();
    }
    #[allow(deprecated)]
    let scan = session
        .query_unpaged(
            format!(
                "SELECT next_cursor FROM {table} \
                 WHERE server_fingerprint = ? ALLOW FILTERING"
            ),
            (fingerprint.as_str(),),
        )
        .await?;
    let filtered = scan.rows_or_empty().len();

    println!("after delete: point read sees {point} row(s), filtered scan sees {filtered}");
    if point == 0 {
        println!("reset: the next allocation will take the INSERT path and succeed once");
        Ok(())
    } else {
        Err(format!("{point} row(s) still visible to the allocator's own read path").into())
    }
}
