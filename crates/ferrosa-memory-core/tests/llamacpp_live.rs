//! Live check against a real llama.cpp server.
//!
//! Ignored by default: needs `llama-server` running with an embedding model.
//! Run with:
//!   llama-server -m nomic-embed-text-v2-moe.Q4_K_M.gguf --embeddings --port 11435
//!   cargo test -p ferrosa-memory-core --test llamacpp_live -- --ignored
//!
//! Every other test in this area uses a hand-written fixture, and fixtures only
//! prove the client matches what the AUTHOR believed the server does. This one
//! asks the server.
use ferrosa_memory_core::config::EmbeddingConfig;
use ferrosa_memory_core::embedding::EmbeddingClient;

#[tokio::test]
#[ignore = "requires a running llama-server with an embedding model"]
async fn llamacpp_serves_embeddings_through_the_openai_contract() {
    let config = EmbeddingConfig {
        provider: "llamacpp".into(),
        base_url: std::env::var("FERROSA_TEST_EMBED_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11435".into()),
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
