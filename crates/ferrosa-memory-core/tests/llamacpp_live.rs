//! Live check against a real llama.cpp server.
//!
//! Requires `FERROSA_TEST_EMBED_URL` pointing at a running `llama-server` with
//! an embedding model loaded:
//!
//! ```text
//! llama-server -m nomic-embed-text-v2-moe.Q4_K_M.gguf --embeddings --port 11435
//! FERROSA_TEST_EMBED_URL=http://127.0.0.1:11435 \
//!   cargo nextest run -p ferrosa-memory-core --run-ignored all llamacpp_live
//! ```
//!
//! NOT gated by `#[ignore]` alone: this repo's cluster job runs ignored tests
//! deliberately, and it provisions a Cassandra-compatible cluster, not a model
//! server. Gating on the env var is what keeps it out of that job.
//!
//! It exists because every other test in this area uses a hand-written fixture,
//! and a fixture only proves the client matches what its AUTHOR believed the
//! server does. This one asks the server.
use ferrosa_memory_core::config::EmbeddingConfig;
use ferrosa_memory_core::embedding::EmbeddingClient;

#[tokio::test]
#[ignore = "requires FERROSA_TEST_EMBED_URL pointing at a live llama-server"]
async fn llamacpp_serves_embeddings_through_the_openai_contract() {
    let Ok(base_url) = std::env::var("FERROSA_TEST_EMBED_URL") else {
        // Loud, not silent: a green run that skipped this must say so, or the
        // coverage is quietly absent.
        eprintln!(
            "SKIPPED llamacpp_serves_embeddings_through_the_openai_contract: \
FERROSA_TEST_EMBED_URL is not set (needs a running llama-server)"
        );
        return;
    };

    let config = EmbeddingConfig {
        provider: "llamacpp".into(),
        base_url,
        model: "nomic-embed-text-v2-moe".into(),
        dimensions: 768,
        ..EmbeddingConfig::default()
    };
    let client = EmbeddingClient::new(&config);

    client
        .health_check()
        .await
        .expect("llama.cpp health check must pass against /v1/models");

    let embedding = client
        .embed("deferred work task board")
        .await
        .expect("llama.cpp must return an embedding");

    assert_eq!(embedding.len(), 768, "nomic-embed-text-v2-moe is 768-dim");
    assert!(
        embedding.iter().any(|v| *v != 0.0),
        "embedding must not be all zeros"
    );
}
