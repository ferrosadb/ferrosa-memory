//! Live CQL integration test — minimal cdrs-tokio connection.
//! Run with: cargo test -p ferrosa-memory-core --test cql_live -- --ignored --nocapture

use std::sync::Arc;

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::NodeTcpConfigBuilder;
use cdrs_tokio::cluster::session::{SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;

#[tokio::test]
#[ignore]
async fn cdrs_connect_and_query() {
    eprintln!("building node config...");
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config failed");

    eprintln!("building session (this is where it hangs/fails)...");
    let session = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config).build(),
    )
    .await
    .expect("session build timed out")
    .expect("session build failed");

    eprintln!("connected! running query...");
    let envelope = session
        .query("SELECT keyspace_name FROM system_schema.keyspaces")
        .await
        .expect("query failed");

    let body = envelope.response_body().expect("response body");
    let rows = body.into_rows().expect("rows");
    eprintln!("got {} rows", rows.len());
    assert!(!rows.is_empty(), "should have at least system keyspaces");
}

#[tokio::test]
#[ignore]
async fn cdrs_prepare_statement() {
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config");

    let session = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config)
        .build()
        .await
        .expect("session");

    eprintln!("preparing statement...");
    let prepared = session
        .prepare(
            "SELECT * FROM agent_memory.memo_cache WHERE content_hash = ? AND model_version = ?",
        )
        .await;
    match &prepared {
        Ok(_) => eprintln!("prepare succeeded!"),
        Err(e) => eprintln!("prepare failed: {e}"),
    }
    prepared.expect("prepare failed");
}

#[tokio::test]
#[ignore]
async fn prepare_vector_column() {
    let node_config = NodeTcpConfigBuilder::new()
        .with_contact_point("127.0.0.1:19042".into())
        .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider))
        .build()
        .await
        .expect("node config");
    let session = TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config)
        .build()
        .await
        .expect("session");

    eprintln!("PREPARE vector INSERT...");
    match session
        .prepare("INSERT INTO agent_memory.test_vector_blob (id, embedding) VALUES (?, ?)")
        .await
    {
        Ok(_) => eprintln!("  OK"),
        Err(e) => panic!("PREPARE vector INSERT failed: {e}"),
    }
}
