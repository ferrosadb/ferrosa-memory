# ferrosa-memory-core

`ferrosa-memory-core` owns the Ferrosa Memory storage abstraction, MCP tool
dispatch, CQL-backed application tables, and HTTP/workbench handlers. The MCP
binary supplies transport and connection lifecycle around this crate.

## Entity Scope Contract

`list_entities`, `get_stats` (including the `stats` alias), and
`count_entities_by_type` use the same entity scope defaults:

| Request | `list_entities` | `get_stats` / `stats` | `count_entities_by_type` |
| --- | --- | --- | --- |
| Omit `session_id` | Tenant-wide listing (`scope: "all"`) | Tenant-wide stats (`scope: "tenant"`) | Tenant-wide histogram (`scope: "tenant"`) |
| Supply `session_id` | That session | That session | That session |

`list_entities` honors an explicit `scope` or `include_cross_session` override;
without either, an explicit `session_id` selects session scope. The two count
tools report `session_id: null` for their tenant-wide default and the supplied
UUID for session-scoped results.

The storage layer must aggregate tenant-wide type counts from paged CQL rows
using the minimal `entity_type` and `state` projection. Do not materialize all
tenant rows merely to build a histogram.

See [`../../specs/implemented/feat-count-entities-by-type.md`](../../specs/implemented/feat-count-entities-by-type.md)
for the detailed response contract and invariants.
