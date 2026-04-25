#!/bin/bash
# Backup fmem data to disk via CQL dump.
# Run every 30 minutes via launchd (com.ferrosa-memory.backup.plist).
#
# Dumps every fmem table that exists and holds data to per-table JSON
# files under a timestamped directory. Skips tables that don't exist
# (schema drift between deploys is tolerated). Prunes to MAX_BACKUPS
# successful runs.
#
# Restore: python3 scripts/restore-memory.sh <backup-dir>

set -euo pipefail

# When run under launchd, PATH is sparse. Explicitly add homebrew + user
# tool paths so `python3` and friends resolve the same way they do
# interactively. Without this, the script exited with "python3: command
# not found" and the error never surfaced because the script writes
# nothing before the pre-flight check.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

BACKUP_ROOT="${FMEM_BACKUP_DIR:-$HOME/data/ferrosa-memory/backups}"
CQL_HOST="${FMEM_CQL_HOST:-localhost}"
# Default to querying ANY of the three nodes — cassandra-driver will
# pick the first one that responds. If node1 is in a bad state (raft
# init failure or similar) the driver falls through to node2/node3.
# Reads work even when Raft writes are blocked since SSTable reads are
# a non-consensus path.
CQL_PORTS="${FMEM_CQL_PORTS:-19042,19043,19044}"
MAX_BACKUPS=10
MIN_ENTITIES=100  # refuse to backup if fewer entities than this

mkdir -p "$BACKUP_ROOT"

# Pre-flight: verify CQL is reachable and has data before creating backup
# dir. Errors are NO LONGER suppressed — if python or the driver fails,
# the script should exit loud so launchd surfaces it in the StandardError
# log.
#
# Tries each port in CQL_PORTS until one answers. If node1 is wedged (raft
# init failure) we fall through to node2/node3 — CQL reads work even when
# Raft writes are blocked since SSTable reads are a non-consensus path.
PREFLIGHT_OUT=$(python3 -c "
import sys
from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy

ports = [int(p) for p in '$CQL_PORTS'.split(',') if p.strip()]
last_err = None
for port in ports:
    try:
        cluster = Cluster(['$CQL_HOST'], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4, connect_timeout=5)
        session = cluster.connect('agent_memory')
        rows = session.execute('SELECT COUNT(*) FROM agent_memory.entity_store')
        # Labeled output so the shell can parse it regardless of stdout/stderr
        # interleaving. An earlier version printed count on stdout + port on
        # stderr and merged with 2>&1, which swapped them in practice.
        print(f'COUNT={rows.one()[0]}')
        print(f'PORT={port}')
        cluster.shutdown()
        sys.exit(0)
    except Exception as e:
        last_err = f'port {port}: {type(e).__name__}: {e}'
        print(f'  pre-flight {last_err}', file=sys.stderr)
print(f'ABORT — no reachable CQL port in {ports}; last_err={last_err}', file=sys.stderr)
sys.exit(1)
")
ENTITY_COUNT=$(echo "$PREFLIGHT_OUT" | sed -n 's/^COUNT=\([0-9][0-9]*\)$/\1/p')
WORKING_PORT=$(echo "$PREFLIGHT_OUT" | sed -n 's/^PORT=\([0-9][0-9]*\)$/\1/p')

if ! [[ "$ENTITY_COUNT" =~ ^[0-9]+$ ]]; then
    echo "$(date): ABORT — pre-flight returned non-numeric entity count '$ENTITY_COUNT'" >&2
    exit 2
fi

if [ "$ENTITY_COUNT" -lt "$MIN_ENTITIES" ]; then
    echo "$(date): SKIPPING backup — cluster returned $ENTITY_COUNT entities (min: $MIN_ENTITIES). Cluster may be down or empty."
    exit 0
fi

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
BACKUP_DIR="$BACKUP_ROOT/$TIMESTAMP"
mkdir -p "$BACKUP_DIR"

echo "$(date): backing up fmem to $BACKUP_DIR ($ENTITY_COUNT entities verified)"

python3 -c "
import json, uuid, datetime, sys, os, base64, traceback, time

from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy

host = '$CQL_HOST'
port = int('$WORKING_PORT')
backup_dir = '$BACKUP_DIR'
keyspace = 'agent_memory'

# Reuse the port the pre-flight verified as answering. If that port breaks
# between pre-flight and now, we'll fail loud rather than masking — backups
# against a partially-broken cluster are worse than no backup.
cluster = Cluster([host], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4)
session = cluster.connect(keyspace)

def json_default(obj):
    if isinstance(obj, uuid.UUID):
        return str(obj)
    if isinstance(obj, (datetime.datetime, datetime.date)):
        return obj.isoformat()
    if isinstance(obj, bytes):
        return {'__bytes_b64': base64.b64encode(obj).decode('ascii')}
    # Tuple/list from vector<float,N> columns — keep as a plain list.
    if isinstance(obj, (tuple, list)):
        return list(obj)
    # Catch anything exotic (OrderedMapSerializedKey, SortedSet, etc.)
    try:
        return list(obj)
    except Exception:
        pass
    raise TypeError(f'Not serializable: {type(obj).__name__}={obj!r}')

# Discover every user table in the keyspace from system_schema.
# system_schema.tables' WHERE predicate isn't reliable on Ferrosa
# (see ../ferrosa/specs/todo/bug-system-schema-where-predicate-not-honored.md),
# so filter client-side.
all_rows = list(session.execute('SELECT keyspace_name, table_name FROM system_schema.tables'))
tables = sorted({
    r.table_name for r in all_rows if getattr(r, 'keyspace_name', None) == keyspace
})
if not tables:
    print(f'ABORT — no user tables discovered in {keyspace}', file=sys.stderr)
    sys.exit(3)
print(f'  discovered {len(tables)} tables: {tables}', file=sys.stderr)

manifest = {'keyspace': keyspace, 'started_at': datetime.datetime.utcnow().isoformat() + 'Z', 'tables': {}}
grand_total = 0
failures = []

MAX_ATTEMPTS = 4
BACKOFF_SECS = [0, 3, 8, 20]  # index into attempt number

for table in tables:
    last_err = None
    success = False
    for attempt in range(MAX_ATTEMPTS):
        if attempt > 0:
            time.sleep(BACKOFF_SECS[attempt])
            print(f'  retrying {table} (attempt {attempt + 1}/{MAX_ATTEMPTS})...', file=sys.stderr, flush=True)
        else:
            print(f'  dumping {table}...', file=sys.stderr, flush=True)
        try:
            rows = list(session.execute(f'SELECT * FROM {keyspace}.{table}'))
            data = []
            for r in rows:
                row_dict = {}
                for c in r._fields:
                    row_dict[c] = getattr(r, c)
                data.append(row_dict)
            out_path = os.path.join(backup_dir, f'{table}.json')
            with open(out_path, 'w') as f:
                json.dump(data, f, default=json_default)
            print(f'    {table}: {len(data)} rows → {os.path.basename(out_path)}', file=sys.stderr)
            manifest['tables'][table] = {'rows': len(data), 'file': f'{table}.json', 'attempts': attempt + 1}
            grand_total += len(data)
            success = True
            break
        except Exception as e:
            last_err = f'{type(e).__name__}: {e}'
            # str(e) drops the class name for some exceptions; check
            # both the exception type and its message. Transient
            # cluster states (NoHostAvailable, OperationTimedOut,
            # Unavailable, lane reconnecting) all warrant a retry.
            probe = f'{type(e).__name__}: {e}'
            retriable = any(
                m in probe
                for m in (
                    'NoHostAvailable',
                    'OperationTimedOut',
                    'Unavailable',
                    'reconnecting',
                    'timeout',
                    'unavailable',
                )
            )
            print(f'    {table}: attempt {attempt + 1} failed ({last_err}); retriable={retriable}', file=sys.stderr)
            if not retriable:
                break
    if not success:
        msg = last_err or 'unknown error'
        failures.append((table, msg))
        manifest['tables'][table] = {'error': msg, 'attempts': MAX_ATTEMPTS}

manifest['completed_at'] = datetime.datetime.utcnow().isoformat() + 'Z'
manifest['grand_total_rows'] = grand_total
manifest['failures'] = failures
with open(os.path.join(backup_dir, '_manifest.json'), 'w') as f:
    json.dump(manifest, f, default=json_default, indent=2)

cluster.shutdown()

if failures:
    print(f'  total: {grand_total} rows across {len(tables) - len(failures)}/{len(tables)} tables; {len(failures)} FAILED', file=sys.stderr)
    sys.exit(4)
print(f'  total: {grand_total} rows backed up across {len(tables)} tables', file=sys.stderr)
"

# Clean up empty backup dirs (from skipped runs or failures).
# "Good" backups are the ones with a _manifest.json — the dump script
# writes it last, so its presence is our commit marker.
cd "$BACKUP_ROOT"
for dir in 2*; do
    [ -d "$dir" ] || continue
    if [ ! -f "$dir/_manifest.json" ]; then
        rm -rf "$dir"
        echo "  removed empty backup dir: $dir"
    fi
done

# Prune old backups, keep last MAX_BACKUPS successful runs.
GOOD_BACKUPS=$(ls -1d 2*/_manifest.json 2>/dev/null | sed 's|/_manifest.json||' | wc -l | tr -d ' ')
if [ "$GOOD_BACKUPS" -gt "$MAX_BACKUPS" ]; then
    PRUNE_COUNT=$((GOOD_BACKUPS - MAX_BACKUPS))
    ls -1d 2*/_manifest.json | sed 's|/_manifest.json||' | head -n "$PRUNE_COUNT" | while read dir; do
        rm -rf "$dir"
        echo "  pruned old backup: $dir"
    done
fi

echo "$(date): backup complete"
