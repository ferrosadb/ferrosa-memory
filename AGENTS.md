# Agent Rules

## Schema Migrations

All schema changes must include a versioned migration. Never change a schema without bumping the migration version and registering the migration in order.

Migrations must be automatic, ordered, and data-preserving:

- A deployment at version `N` must be able to reach version `M` by applying every migration `N+1`, `N+2`, ... `M` in sequence.
- A migration must preserve, transform, or explicitly supersede legacy rows. It must not damage, silently drop, or orphan old data.
- If a primary-key or incompatible type change requires table recreation, use a staging/copy/swap migration with row-count verification and a recoverable failure mode.
- Startup migration logic must fail loud on schema drift or copy mismatch rather than continuing with a partially migrated schema.
