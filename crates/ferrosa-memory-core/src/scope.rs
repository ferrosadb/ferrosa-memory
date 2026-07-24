//! Entity scope primitives: session-local vs global knowledge.
//!
//! Most entities are session-scoped (the existing default). Some entity
//! types are shared knowledge that should cross session boundaries — skills,
//! tags, concepts, decisions, patterns, code symbols. Those are "global":
//! written under a deterministic per-tenant sentinel session UUID, so a
//! single-partition read retrieves every global entity for that tenant.
//!
//! This module provides the primitives:
//!
//! - [`tenant_global_session_uuid`] — derive the sentinel for a tenant
//! - [`default_scope_for`] — the per-`entity_type` default scope policy
//! - [`resolve_storage_session`] — where an entity physically lives given
//!   its scope and the caller's session

use uuid::Uuid;

use crate::types::EntityScope;

/// Namespace UUID for deriving per-tenant global session sentinels (UUID v5).
///
/// Random once, embedded forever — every fmem build uses the same namespace
/// so sentinels are stable across deployments. Changing this value would
/// invalidate every existing global-scope entity's partition.
const FMEM_GLOBAL_SESSION_NS: Uuid = Uuid::from_u128(0xfe77_05a6_ec0a_4611_bd8c_64fd_3c3f_8faa);

/// Deterministic per-tenant session UUID under which global-scope entities
/// are stored.
///
/// Same tenant → same sentinel, every time, every process. Different tenants
/// → different sentinels (no cross-tenant leakage even when reading the
/// "global" partition).
pub fn tenant_global_session_uuid(tenant_id: Uuid) -> Uuid {
    Uuid::new_v5(&FMEM_GLOBAL_SESSION_NS, tenant_id.as_bytes())
}

/// Namespace UUID for deriving tag entity_ids (UUID v5) from
/// `(tenant_id, normalized_tag_name)`.
///
/// Using a deterministic id for every tag kills the "which id is `cloud`?"
/// lookup race in `ensure_tag_entity`: two concurrent ingests computing
/// `tenant_tag_entity_uuid(t, "cloud")` always get the same UUID, so their
/// TAGGED_AS writes can't end up pointing at the wrong tag entity.
const FMEM_TAG_ENTITY_NS: Uuid = Uuid::from_u128(0x4b1c_82a5_6f9e_4b2d_8c31_d9a0_1e7f_5cd2);

/// Deterministic entity_id for a global-scope tag, derived from the owning
/// tenant and the tag's normalized name. Caller is responsible for passing
/// the already-normalized name (see `skill::normalize_tag`) so that
/// equivalent-but-differently-cased inputs hash to the same id.
pub fn tenant_tag_entity_uuid(tenant_id: Uuid, normalized_tag_name: &str) -> Uuid {
    let mut bytes: Vec<u8> = Vec::with_capacity(16 + normalized_tag_name.len());
    bytes.extend_from_slice(tenant_id.as_bytes());
    bytes.extend_from_slice(normalized_tag_name.as_bytes());
    Uuid::new_v5(&FMEM_TAG_ENTITY_NS, &bytes)
}

/// Default scope for a given `entity_type`.
///
/// - `Global`: skill, tag, concept, decision, pattern, code_symbol, person,
///   place, org — shared knowledge that's useful across sessions.
/// - `Session`: everything else, including bug, event, preference, and any
///   unknown type — conservative default.
///
/// Callers can override; this just encodes the policy from the design doc.
pub fn default_scope_for(entity_type: &str) -> EntityScope {
    match entity_type {
        "skill" | "tag" | "concept" | "decision" | "pattern" | "code_symbol" | "person"
        | "place" | "org" => EntityScope::Global,
        _ => EntityScope::Session,
    }
}

/// Resolve the physical storage session for an entity given its scope and the
/// caller's session. Returns `(storage_session, ingested_by_session)`.
///
/// - For `Session` scope: storage session is the caller's, and
///   `ingested_by_session` is `None` because `session_id` itself retains the
///   original provenance.
/// - For `Global` scope: storage session is the tenant's global sentinel,
///   and `ingested_by_session` records the caller's session for provenance
///   and the re-rank session-affinity signal.
pub fn resolve_storage_session(
    caller_session: Uuid,
    scope: EntityScope,
    tenant_id: Uuid,
) -> (Uuid, Option<Uuid>) {
    match scope {
        EntityScope::Session => (caller_session, None),
        EntityScope::Global => (tenant_global_session_uuid(tenant_id), Some(caller_session)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_is_deterministic_per_tenant() {
        let tenant = Uuid::new_v4();
        let a = tenant_global_session_uuid(tenant);
        let b = tenant_global_session_uuid(tenant);
        assert_eq!(a, b, "same tenant must always produce the same sentinel");
    }

    #[test]
    fn sentinel_differs_across_tenants() {
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        assert_ne!(
            tenant_global_session_uuid(t1),
            tenant_global_session_uuid(t2),
            "different tenants must produce different sentinels"
        );
    }

    #[test]
    fn sentinel_differs_from_tenant_id() {
        // The sentinel must NOT equal the tenant's own UUID — that would
        // collide with entities written under a session_id == tenant_id.
        let t = Uuid::new_v4();
        assert_ne!(tenant_global_session_uuid(t), t);
    }

    #[test]
    fn sentinel_is_uuid_v5() {
        let t = Uuid::new_v4();
        let s = tenant_global_session_uuid(t);
        assert_eq!(s.get_version_num(), 5, "sentinel must be a v5 UUID");
    }

    #[test]
    fn default_scope_globals_are_global() {
        for t in [
            "skill",
            "tag",
            "concept",
            "decision",
            "pattern",
            "code_symbol",
            "person",
            "place",
            "org",
        ] {
            assert_eq!(
                default_scope_for(t),
                EntityScope::Global,
                "{t} should default to Global"
            );
        }
    }

    #[test]
    fn default_scope_session_fallback_for_unknown() {
        assert_eq!(default_scope_for("bug"), EntityScope::Session);
        assert_eq!(default_scope_for("preference"), EntityScope::Session);
        assert_eq!(default_scope_for("event"), EntityScope::Session);
        assert_eq!(default_scope_for("mystery_type"), EntityScope::Session);
    }

    #[test]
    fn resolve_session_scope_is_transparent() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let (storage, ingester) = resolve_storage_session(caller, EntityScope::Session, tenant);
        assert_eq!(
            storage, caller,
            "session scope stores under caller's session"
        );
        assert_eq!(
            ingester, None,
            "session scope does not record ingested_by_session"
        );
    }

    #[test]
    fn resolve_global_scope_routes_to_sentinel() {
        let caller = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let (storage, ingester) = resolve_storage_session(caller, EntityScope::Global, tenant);
        assert_eq!(
            storage,
            tenant_global_session_uuid(tenant),
            "global scope stores under the tenant sentinel"
        );
        assert_eq!(
            ingester,
            Some(caller),
            "global scope records the caller session as ingester"
        );
    }
}
