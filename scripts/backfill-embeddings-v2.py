#!/usr/bin/env python3
"""
Phase 0 of the rich-entity backfill: re-embed every stored vector with a
new embedding model. Schema stays at 768-d; only the vectors change.

Usage:
    FMEM_CONFIG=/Users/bkearns/.config/ferrosa-memory.toml \
    FMEM_TARGET_MODEL=nomic-embed-text-v2-moe \
    FMEM_OLLAMA_URL=http://localhost:11434 \
    python3 scripts/backfill-embeddings-v2.py

Options (env vars):
    FMEM_CONFIG          path to ferrosa-memory.toml (required)
    FMEM_TARGET_MODEL    target embedding model (default: nomic-embed-text-v2-moe)
    FMEM_OLLAMA_URL      ollama base URL (default: http://localhost:11434)
    FMEM_DRY_RUN=1       count what would change, don't write
    FMEM_PROGRESS_EVERY  progress-report interval (default: 100 rows)
    FMEM_CQL_PORT        single port to target (default: iterate 19042,19043,19044)

**Operational contract**

The MCP must be stopped during the run. Any write that lands on the old
model while this script is mid-flight leaves a stray v1 vector behind,
defeating the point of the migration. Stop the LaunchAgent first:

    launchctl unload ~/Library/LaunchAgents/com.ferrosa-memory.mcp.plist

After this script exits 0:
  1. Update FMEM config: model = "nomic-embed-text-v2-moe"
  2. launchctl load ~/Library/LaunchAgents/com.ferrosa-memory.mcp.plist

**What gets re-embedded**

  - entity_store.entity_embedding (source: entity_name)
  - entity_store.description_embedding (source: description, when set)
  - trajectory_folds.fold_embedding (source: fold_summary, when set)
  - memo_cache: TRUNCATE (cache with expires_at; rebuilds on demand)

Fails loud on embedding-provider outage: exits 1 with a partial-progress
count. Re-running is safe — same text + same model → same vector.
"""

import os
import sys
import time
import urllib.request
import urllib.error
import json
import uuid

from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy
from cassandra.query import SimpleStatement

# --- Config ----------------------------------------------------------

CONFIG_PATH = os.environ.get("FMEM_CONFIG")
TARGET_MODEL = os.environ.get("FMEM_TARGET_MODEL", "nomic-embed-text-v2-moe")
OLLAMA_URL = os.environ.get("FMEM_OLLAMA_URL", "http://localhost:11434").rstrip("/")
DRY_RUN = bool(os.environ.get("FMEM_DRY_RUN"))
PROGRESS_EVERY = int(os.environ.get("FMEM_PROGRESS_EVERY", "100"))
CQL_PORTS = (
    [int(os.environ["FMEM_CQL_PORT"])]
    if os.environ.get("FMEM_CQL_PORT")
    else [19042, 19043, 19044]
)

if not CONFIG_PATH:
    sys.exit("FMEM_CONFIG env var is required (path to ferrosa-memory.toml)")


def parse_toml_minimal(path):
    """Extract just the fields we need — keyspace, tenant_id, dimensions."""
    d = {"keyspace": "agent_memory", "dimensions": 768, "tenant_id": None}
    with open(path) as f:
        section = None
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1]
                continue
            if "=" in line:
                k, v = [x.strip() for x in line.split("=", 1)]
                v = v.strip().strip('"')
                if section == "server" and k == "tenant_id":
                    d["tenant_id"] = v
                elif section == "ferrosa" and k == "keyspace":
                    d["keyspace"] = v
                elif section == "embeddings" and k == "dimensions":
                    d["dimensions"] = int(v)
    return d


CONF = parse_toml_minimal(CONFIG_PATH)
KEYSPACE = CONF["keyspace"]
TENANT_ID = CONF["tenant_id"]
DIMS = CONF["dimensions"]

if not TENANT_ID:
    sys.exit(f"no tenant_id in [server] section of {CONFIG_PATH}")

# cassandra-driver wants a real UUID object for `uuid` column binds.
TENANT_ID_UUID = uuid.UUID(TENANT_ID)


# --- Ollama embed ----------------------------------------------------


def embed(text, timeout=30):
    """Returns list[float] of length DIMS, or raises."""
    req = urllib.request.Request(
        f"{OLLAMA_URL}/api/embeddings",
        data=json.dumps({"model": TARGET_MODEL, "prompt": text}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = json.loads(resp.read().decode())
    v = body.get("embedding") or body.get("embeddings")
    if not isinstance(v, list) or len(v) != DIMS:
        raise RuntimeError(
            f"embed({text[:40]!r}): expected {DIMS}-d vector, got {type(v).__name__} len={len(v) if isinstance(v, list) else '?'}"
        )
    return v


# --- CQL session -----------------------------------------------------


def connect_cql():
    last_err = None
    for port in CQL_PORTS:
        try:
            c = Cluster(
                ["127.0.0.1"],
                port=port,
                load_balancing_policy=RoundRobinPolicy(),
                protocol_version=4,
                connect_timeout=10,
            )
            s = c.connect(KEYSPACE)
            s.default_timeout = 30
            print(f"  CQL connected on port {port}", file=sys.stderr)
            return c, s
        except Exception as e:
            last_err = f"port {port}: {type(e).__name__}: {e}"
            print(f"  CQL connect failed: {last_err}", file=sys.stderr)
    raise RuntimeError(f"no CQL port reachable; last_err={last_err}")


# --- Per-table re-embed -----------------------------------------------


def reembed_entities(session):
    """entity_store: entity_embedding (from entity_name) + description_embedding (from description)."""
    print(f"\n== entities ({KEYSPACE}.entity_store) ==", file=sys.stderr)
    select = session.prepare(
        f"SELECT tenant_id, session_id, entity_id, entity_name, description "
        f"FROM {KEYSPACE}.entity_store WHERE tenant_id = ? ALLOW FILTERING"
    )
    update_name = session.prepare(
        f"UPDATE {KEYSPACE}.entity_store SET entity_embedding = ? "
        f"WHERE tenant_id = ? AND session_id = ? AND entity_id = ?"
    )
    update_desc = session.prepare(
        f"UPDATE {KEYSPACE}.entity_store SET description_embedding = ? "
        f"WHERE tenant_id = ? AND session_id = ? AND entity_id = ?"
    )
    name_ok = name_failed = desc_ok = desc_failed = total = 0
    t0 = time.time()
    rows = list(session.execute(select, [TENANT_ID_UUID]))
    print(f"  {len(rows)} rows loaded", file=sys.stderr)
    for r in rows:
        total += 1
        # entity_name embed
        try:
            v = embed(r.entity_name)
            if not DRY_RUN:
                session.execute(update_name, [v, r.tenant_id, r.session_id, r.entity_id])
            name_ok += 1
        except Exception as e:
            name_failed += 1
            print(f"  FAIL name {r.entity_id}: {type(e).__name__}: {str(e)[:120]}", file=sys.stderr)
        # description embed (optional)
        if r.description:
            try:
                v = embed(r.description)
                if not DRY_RUN:
                    session.execute(
                        update_desc, [v, r.tenant_id, r.session_id, r.entity_id]
                    )
                desc_ok += 1
            except Exception as e:
                desc_failed += 1
                print(
                    f"  FAIL desc {r.entity_id}: {type(e).__name__}: {str(e)[:120]}",
                    file=sys.stderr,
                )
        if total % PROGRESS_EVERY == 0:
            elapsed = time.time() - t0
            rate = total / max(elapsed, 0.001)
            eta = (len(rows) - total) / max(rate, 0.001)
            print(
                f"  progress: {total}/{len(rows)} rows ({name_ok} names, {desc_ok} descs; "
                f"{name_failed + desc_failed} fails) {rate:.1f} rows/s ETA {eta:.0f}s",
                file=sys.stderr,
            )
    return {
        "rows": total,
        "name_ok": name_ok,
        "name_failed": name_failed,
        "desc_ok": desc_ok,
        "desc_failed": desc_failed,
    }


def reembed_folds(session):
    """trajectory_folds: fold_embedding (from fold_summary)."""
    print(f"\n== folds ({KEYSPACE}.trajectory_folds) ==", file=sys.stderr)
    select = session.prepare(
        f"SELECT tenant_id, session_id, fold_id, fold_summary "
        f"FROM {KEYSPACE}.trajectory_folds WHERE tenant_id = ? ALLOW FILTERING"
    )
    update = session.prepare(
        f"UPDATE {KEYSPACE}.trajectory_folds SET fold_embedding = ? "
        f"WHERE tenant_id = ? AND session_id = ? AND fold_id = ?"
    )
    ok = failed = skipped = total = 0
    t0 = time.time()
    rows = list(session.execute(select, [TENANT_ID_UUID]))
    print(f"  {len(rows)} rows loaded", file=sys.stderr)
    for r in rows:
        total += 1
        if not r.fold_summary:
            skipped += 1
            continue
        try:
            v = embed(r.fold_summary)
            if not DRY_RUN:
                session.execute(update, [v, r.tenant_id, r.session_id, r.fold_id])
            ok += 1
        except Exception as e:
            failed += 1
            print(f"  FAIL fold {r.fold_id}: {type(e).__name__}: {str(e)[:120]}", file=sys.stderr)
        if total % PROGRESS_EVERY == 0:
            elapsed = time.time() - t0
            rate = total / max(elapsed, 0.001)
            eta = (len(rows) - total) / max(rate, 0.001)
            print(
                f"  progress: {total}/{len(rows)} rows ({ok} ok, {failed} fails, {skipped} skipped) "
                f"{rate:.1f} rows/s ETA {eta:.0f}s",
                file=sys.stderr,
            )
    return {"rows": total, "ok": ok, "failed": failed, "skipped": skipped}


def truncate_memos(session):
    """memo_cache is a time-expiring cache; TRUNCATE is simpler than re-embedding."""
    print(f"\n== memos ({KEYSPACE}.memo_cache) ==", file=sys.stderr)
    if DRY_RUN:
        print("  dry-run: would TRUNCATE", file=sys.stderr)
        return {"truncated": False, "dry_run": True}
    try:
        session.execute(SimpleStatement(f"TRUNCATE {KEYSPACE}.memo_cache"))
        print("  memo_cache truncated (cache rebuilds on demand)", file=sys.stderr)
        return {"truncated": True}
    except Exception as e:
        print(f"  TRUNCATE failed: {type(e).__name__}: {str(e)[:200]}", file=sys.stderr)
        return {"truncated": False, "error": str(e)}


# --- Main ------------------------------------------------------------


def main():
    print(
        f"backfill-embeddings-v2: target_model={TARGET_MODEL} ollama={OLLAMA_URL} "
        f"tenant={TENANT_ID} dims={DIMS} dry_run={DRY_RUN}",
        file=sys.stderr,
    )

    # Pre-flight: reach Ollama + embed a probe.
    try:
        v = embed("probe")
        print(
            f"  probe embed OK: {len(v)}-d vector, first 3 = {v[:3]}",
            file=sys.stderr,
        )
    except Exception as e:
        sys.exit(f"ABORT: probe embed failed — is {TARGET_MODEL} loaded in ollama? {e}")

    cluster, session = connect_cql()
    try:
        e_stats = reembed_entities(session)
        f_stats = reembed_folds(session)
        m_stats = truncate_memos(session)
    finally:
        cluster.shutdown()

    print("\n== summary ==")
    print(json.dumps({"entities": e_stats, "folds": f_stats, "memos": m_stats}, indent=2))

    total_fails = (
        e_stats.get("name_failed", 0)
        + e_stats.get("desc_failed", 0)
        + f_stats.get("failed", 0)
    )
    if total_fails > 0 and not DRY_RUN:
        sys.exit(
            f"ABORT: {total_fails} failures — re-run after fixing the embedding provider"
        )


if __name__ == "__main__":
    main()
