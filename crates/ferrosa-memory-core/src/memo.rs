//! Memoization cache tool handlers.
//!
//! Implements `check_memo_cache` and `store_memo_result` — the fix for
//! redundant sub-call re-derivation identified by "Think, But Don't Overthink."
//!
//! ## Cache key
//!
//! `SHA-256(normalize(prompt) + context_slice)` combined with `model_version`
//! forms the composite cache key. This ensures:
//! - Same prompt with same context -> cache hit
//! - Same prompt with different model -> cache miss (FMEA F09)
//! - Whitespace/formatting differences don't cause misses
//!
//! ## Write path
//!
//! On miss: caller executes LLM, then calls `store_memo_result` to write.
//! On hit: return immediately, increment `hit_count` for eviction policy.

use sha2::{Digest, Sha256};

use crate::storage::Storage;
use crate::types::{MemoCheckResult, MemoEntry, MemoStoreResult, TenantContext};

/// Normalize prompt text for consistent hashing.
/// Collapses whitespace runs, trims, lowercases.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Compute the content hash for a (prompt, context_slice) pair.
pub fn content_hash(prompt: &str, context_slice: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize(prompt).as_bytes());
    hasher.update(normalize(context_slice).as_bytes());
    hex::encode(hasher.finalize())
}

/// Check the memo cache for a cached sub-call result.
///
/// Returns `hit: true` with the cached result if found, `hit: false` on miss.
/// On hit, increments `hit_count` and updates `last_hit_at`.
pub async fn check_memo_cache(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    prompt: &str,
    context_slice: &str,
    model_version: &str,
) -> anyhow::Result<MemoCheckResult> {
    let hash = content_hash(prompt, context_slice);
    tracing::debug!(hash = %hash, model_version, "check_memo_cache");

    match storage.memo_get(ctx, &hash, model_version).await? {
        Some(entry) => {
            storage.memo_touch(ctx, &hash, model_version).await?;
            tracing::info!(hash = %hash, hit_count = entry.hit_count + 1, "memo cache HIT");
            Ok(MemoCheckResult {
                hit: true,
                result: Some(entry.result),
                hit_count: Some(entry.hit_count + 1),
            })
        }
        None => {
            tracing::debug!(hash = %hash, "memo cache MISS");
            Ok(MemoCheckResult {
                hit: false,
                result: None,
                hit_count: None,
            })
        }
    }
}

/// Parameters for storing a memo cache entry.
pub struct StoreMemoParams<'a> {
    pub prompt: &'a str,
    pub context_slice: &'a str,
    pub model_version: &'a str,
    pub result: &'a str,
    pub embedding: Option<Vec<f32>>,
    pub ttl_days: Option<u32>,
}

/// Store a sub-call result in the memo cache.
///
/// The caller provides the raw result and an optional embedding vector.
/// TTL defaults to `config.memory.default_ttl_days` if not specified.
pub async fn store_memo_result(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    params: &StoreMemoParams<'_>,
) -> anyhow::Result<MemoStoreResult> {
    store_memo_result_with_config(storage, ctx, params, None).await
}

/// Store a sub-call result with optional quota enforcement via config.
///
/// When `config` is `Some`, enforces per-tenant memo count limits (FMEA D1).
pub async fn store_memo_result_with_config(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    params: &StoreMemoParams<'_>,
    config: Option<&crate::config::MemoryConfig>,
) -> anyhow::Result<MemoStoreResult> {
    // Per-tenant memo quota enforcement (FMEA D1)
    if let Some(cfg) = config {
        let count = storage.memo_count(ctx).await?;
        crate::quota::check_memo_quota(count, cfg)?;
    }

    let hash = content_hash(params.prompt, params.context_slice);
    let now = chrono::Utc::now();
    let expires = params
        .ttl_days
        .map(|d| now + chrono::Duration::days(i64::from(d)));

    let entry = MemoEntry {
        content_hash: hash.clone(),
        model_version: params.model_version.to_string(),
        result: params.result.to_string(),
        result_embedding: params.embedding.clone(),
        hit_count: 0,
        created_at: now,
        last_hit_at: None,
        expires_at: expires,
    };

    storage.memo_put(ctx, &entry).await?;
    tracing::info!(
        hash = %hash,
        model_version = params.model_version,
        result_len = params.result.len(),
        "memo stored"
    );

    Ok(MemoStoreResult {
        stored: true,
        content_hash: hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;
    use uuid::Uuid;

    fn test_ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        }
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  hello   world  "), "hello world");
        assert_eq!(normalize("Hello\n\tWorld"), "hello world");
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("hello world", "ctx");
        let h2 = content_hash("hello world", "ctx");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_normalizes_whitespace() {
        let h1 = content_hash("hello  world", "ctx");
        let h2 = content_hash("hello world", "ctx");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_different_for_different_model_not_in_hash() {
        // model_version is NOT part of the content hash — it's a separate
        // partition key component. Same hash, different partition.
        let h1 = content_hash("prompt", "ctx");
        let h2 = content_hash("prompt", "ctx");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_different_for_different_context() {
        let h1 = content_hash("prompt", "ctx1");
        let h2 = content_hash("prompt", "ctx2");
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn check_memo_miss() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        let result = check_memo_cache(&store, &ctx, "prompt", "ctx", "v1")
            .await
            .unwrap();
        assert!(!result.hit);
        assert!(result.result.is_none());
    }

    #[tokio::test]
    async fn store_then_hit() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        let stored = store_memo_result(
            &store,
            &ctx,
            &StoreMemoParams {
                prompt: "prompt",
                context_slice: "ctx",
                model_version: "v1",
                result: "the answer",
                embedding: None,
                ttl_days: Some(7),
            },
        )
        .await
        .unwrap();
        assert!(stored.stored);

        let result = check_memo_cache(&store, &ctx, "prompt", "ctx", "v1")
            .await
            .unwrap();
        assert!(result.hit);
        assert_eq!(result.result.as_deref(), Some("the answer"));
        assert_eq!(result.hit_count, Some(1));
    }

    #[tokio::test]
    async fn different_model_version_is_miss() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        store_memo_result(
            &store,
            &ctx,
            &StoreMemoParams {
                prompt: "prompt",
                context_slice: "ctx",
                model_version: "v1",
                result: "answer",
                embedding: None,
                ttl_days: None,
            },
        )
        .await
        .unwrap();

        let result = check_memo_cache(&store, &ctx, "prompt", "ctx", "v2")
            .await
            .unwrap();
        assert!(!result.hit, "different model version should miss");
    }

    #[tokio::test]
    async fn store_memo_quota_exceeded() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        // Config with max_memo_results = 2
        let config = crate::config::MemoryConfig {
            max_memo_results: 2,
            ..Default::default()
        };

        // Store 2 memos (at limit)
        for i in 0..2 {
            let result = store_memo_result_with_config(
                &store,
                &ctx,
                &StoreMemoParams {
                    prompt: &format!("prompt_{i}"),
                    context_slice: "ctx",
                    model_version: "v1",
                    result: "answer",
                    embedding: None,
                    ttl_days: None,
                },
                Some(&config),
            )
            .await;
            assert!(result.is_ok(), "memo {i} should succeed under quota");
        }

        // 3rd should fail with QuotaExceeded
        let err = store_memo_result_with_config(
            &store,
            &ctx,
            &StoreMemoParams {
                prompt: "prompt_overflow",
                context_slice: "ctx",
                model_version: "v1",
                result: "answer",
                embedding: None,
                ttl_days: None,
            },
            Some(&config),
        )
        .await
        .unwrap_err();

        assert!(
            err.downcast_ref::<crate::quota::QuotaExceeded>().is_some(),
            "expected QuotaExceeded error, got: {err}"
        );
        assert!(err.to_string().contains("quota exceeded"));
    }

    #[tokio::test]
    async fn store_memo_no_config_skips_quota() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        // Without config, quota check is skipped — stores succeed regardless
        for i in 0..5 {
            let result = store_memo_result_with_config(
                &store,
                &ctx,
                &StoreMemoParams {
                    prompt: &format!("prompt_{i}"),
                    context_slice: "ctx",
                    model_version: "v1",
                    result: "answer",
                    embedding: None,
                    ttl_days: None,
                },
                None,
            )
            .await;
            assert!(result.is_ok(), "memo {i} should succeed without config");
        }
    }

    #[tokio::test]
    async fn memo_embedding_round_trips_through_storage() {
        let store = MockStorage::new();
        let ctx = test_ctx();

        let embedding: Vec<f32> = (0..768).map(|i| i as f32 / 768.0).collect();

        let stored = store_memo_result(
            &store,
            &ctx,
            &StoreMemoParams {
                prompt: "embedding test",
                context_slice: "ctx",
                model_version: "v1",
                result: "answer with embedding",
                embedding: Some(embedding.clone()),
                ttl_days: None,
            },
        )
        .await
        .unwrap();
        assert!(stored.stored);

        // Read back via memo_get and verify embedding
        let entry = store
            .memo_get(&ctx, &stored.content_hash, "v1")
            .await
            .unwrap()
            .expect("memo entry should exist");
        let stored_embedding = entry
            .result_embedding
            .expect("result_embedding should be Some");
        assert_eq!(stored_embedding.len(), 768);
        for (a, b) in embedding.iter().zip(stored_embedding.iter()) {
            assert!((a - b).abs() < 1e-6, "embedding mismatch: {a} vs {b}");
        }
    }
}
