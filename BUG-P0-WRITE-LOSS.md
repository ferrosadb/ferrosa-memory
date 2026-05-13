# P0 Bug: Acknowledged CQL Writes Silently Lost

**Filed:** 2026-04-01 | **GitHub:** ferrosadb/ferrosa#92 | **fmem entity:** 046819a9-15f0-48f7-83fa-85c1c2d3b13a

## What happened

After restarting the ferrosa-memory podman cluster (podman machine crashed, `podman machine start`, `podman compose up -d`), we restored data from backup. The restore script inserted 11,296 rows into `typed_edges` via CQL. Every INSERT was acknowledged by ferrosa with no errors. Immediately after, `SELECT COUNT(*)` confirmed 11,296. Minutes later, count returned 0. All edges silently lost.

A second identical restore succeeded permanently.

## How to reproduce

1. Stop podman machine (crash or `podman machine stop`)
2. `podman machine start`
3. `podman compose up -d` in `the ferrosa-memory repo root`
   - node1 starts as `standalone`
   - node2 joins as `pair` (seeds from node1, waits for node1 healthy)
   - node3 joins as `cluster` (seeds from node1, waits for node1+node2 healthy)
4. All 3 nodes pass healthcheck (CQL port 9042 accepting TCP)
5. Insert rows via Python cassandra-driver to `localhost:19042` (node1)
6. All inserts succeed, immediate COUNT confirms data
7. Wait 60 seconds, COUNT returns 0

## Root cause hypothesis

Progressive join creates a window where node1 accepts writes in standalone mode. When node2/node3 join, topology changes cause node1 to lose unflushed data. Possible locations in ferrosa codebase:

- **`ferrosa-cluster/`** — cluster join/mode transition logic. When transitioning from standalone to pair mode, does node1 re-initialize storage?
- **`ferrosa-storage/`** — memtable flush and commit log. Are writes flushed before topology change?  
- **`ferrosa-cql/`** — does the CQL layer acknowledge writes before they're durable?

## Additional evidence: replication not working

After ingesting a bug entity via `smart_ingest`, the entity (id `046819a9-15f0-48f7-83fa-85c1c2d3b13a`) is visible on node1 (port 19042, snippet_len=3307) but NOT visible on node2 (port 19043 — connection errors). This suggests writes are not replicating across the cluster at all, or the cluster is not fully formed despite healthchecks passing.

## What the investigating agent should check

1. **Cluster topology state**: Query each node's system tables to see if they agree on ring membership
2. **Commit log**: Check if node1's commit log has entries from the first restore that were dropped
3. **Mode transitions**: In `ferrosa-cluster/`, trace what happens to in-flight writes when `FERROSA_CLUSTER_MODE=standalone` transitions to accepting a pair join
4. **Healthcheck vs ready**: The compose healthcheck only checks TCP on 9042. The node may accept CQL connections before it's ready to durably store data. Need a ready check that verifies storage engine is initialized.
5. **Replication factor**: Check what RF the `agent_memory` keyspace uses. If RF=1 in standalone mode and doesn't increase when nodes join, data only lives on one node.

## Files to look at

- `the ferrosa-memory repo root/docker-compose.yml` — cluster topology config
- `the ferrosa repo root/ferrosa-cluster/src/` — join logic, mode transitions
- `the ferrosa repo root/ferrosa-storage/src/` — commit log, memtable, flush
- `the ferrosa repo root/ferrosa-cql/src/` — write acknowledgement path

## Environment

- ferrosa branch: `fix/load-test-bugs` (commit 8a92c63)
- ferrosa-memory branch: `feature/fix-edge-sessions`
- podman 5.8.1 on macOS (applehv)
- Python cassandra-driver with RoundRobinPolicy, protocol_version=4
