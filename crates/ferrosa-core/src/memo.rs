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
    format!("{:x}", hasher.finalize())
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
}
