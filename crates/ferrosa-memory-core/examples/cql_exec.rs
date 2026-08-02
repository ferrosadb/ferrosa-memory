//! Execute CQL statements against a TLS-protected Ferrosa cluster.
//!
//! Exists because the Python driver cannot currently open a session against
//! ferrosa — it authenticates and then dies parsing cluster metadata
//! (t_549a7e43) — which rules out cqlsh-style tooling for bootstrap DDL. The
//! Rust driver handles the same cluster fine, so this is the usable path for
//! creating roles and keyspaces on a fresh cluster.
//!
//! Statements are read from stdin, one per line; blank lines and `--`
//! comments are skipped. Each is executed in order and failures are fatal, so
//! a half-applied bootstrap surfaces immediately rather than silently.
//!
//!   CQL_ADDR=127.0.0.1:19142 CQL_CA=/path/ca-cert.pem \
//!   CQL_USER=ferrosa_admin CQL_PASS=... \
//!     cargo run -p ferrosa-memory-core --example cql_exec < bootstrap.cql
use std::io::Read;

use openssl::ssl::{SslContextBuilder, SslMethod, SslVerifyMode};
use scylla::SessionBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"));

    let mut ctx = SslContextBuilder::new(SslMethod::tls_client())?;
    ctx.set_ca_file(env("CQL_CA"))?;
    ctx.set_verify(SslVerifyMode::PEER);
    // Dialled through a local port forward, so the address is not in the SAN
    // list; the chain is still verified against the CA above.
    ctx.verify_param_mut()
        .set_hostflags(openssl::x509::verify::X509CheckFlags::NEVER_CHECK_SUBJECT);

    let session = SessionBuilder::new()
        .known_node(env("CQL_ADDR"))
        .user(env("CQL_USER"), env("CQL_PASS"))
        .ssl_context(Some(ctx.build()))
        .connection_timeout(std::time::Duration::from_secs(20))
        .build()
        .await?;

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let mut applied = 0usize;
    for raw in input.lines() {
        let stmt = raw.trim().trim_end_matches(';');
        if stmt.is_empty() || stmt.starts_with("--") {
            continue;
        }
        session
            .query_unpaged(stmt, &[])
            .await
            .map_err(|e| format!("statement failed: {stmt}\n  {e}"))?;
        applied += 1;
        println!("ok: {stmt}");
    }
    println!("applied {applied} statement(s)");
    Ok(())
}
