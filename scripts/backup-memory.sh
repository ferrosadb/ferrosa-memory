#!/bin/bash
# Backup fmem data to disk via CQL dump.
# Intended for low-frequency launchd use (com.ferrosa-memory.backup.plist).
# This is a full keyspace dump, so keep it off the interactive CQL node and
# run it gently enough that MCP search stays responsive.
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
# Default to the non-interactive nodes first. MCP/forge clients normally use
# node1 (:19042) as their first contact point; full backup scans on that node
# make semantic recall and task-board reads timeout under normal agent work.
CQL_PORTS="${FMEM_CQL_PORTS:-19044,19043,19042}"
MAX_BACKUPS=10
MAX_DEGRADED=3    # degraded runs are evidence; keep a few, but bound the growth
MIN_ENTITIES=100  # refuse to backup if fewer entities than this
# Fraction a table's row count may fall vs the previous good backup before the
# run is flagged degraded. 0.10 = a 10% drop is tolerated, 11% is not.
MAX_SHRINK_FRACTION="${FMEM_BACKUP_MAX_SHRINK:-0.10}"
# Optional comma-separated table allowlist. Empty = full keyspace dump.
# A restricted run is always marked degraded (it is not a restorable snapshot).
BACKUP_TABLES="${FMEM_BACKUP_TABLES:-}"
FETCH_SIZE="${FMEM_BACKUP_FETCH_SIZE:-500}"
THROTTLE_ROWS="${FMEM_BACKUP_THROTTLE_ROWS:-5000}"
THROTTLE_SECS="${FMEM_BACKUP_THROTTLE_SECS:-0.05}"

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
from cassandra import ConsistencyLevel
from cassandra.query import SimpleStatement

ports = [int(p) for p in '$CQL_PORTS'.split(',') if p.strip()]
last_err = None
for port in ports:
    try:
        cluster = Cluster(['$CQL_HOST'], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4, connect_timeout=5)
        session = cluster.connect('agent_memory')
        # QUORUM, not the driver default LOCAL_ONE. A single replica can be
        # arbitrarily stale — on 2026-08-07 node2 was 22 days behind and still
        # answered /readyz true (t_891840e7). A LOCAL_ONE pre-flight against
        # that node reports a plausible-looking count and green-lights a
        # backup that is missing 41% of the data.
        rows = session.execute(SimpleStatement(
            'SELECT COUNT(*) FROM agent_memory.entity_store',
            consistency_level=ConsistencyLevel.QUORUM))
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
from cassandra.query import SimpleStatement
from cassandra import ConsistencyLevel

host = '$CQL_HOST'
port = int('$WORKING_PORT')
backup_dir = '$BACKUP_DIR'
backup_root = '$BACKUP_ROOT'
keyspace = 'agent_memory'
# A table may legitimately shrink (forget/retract, TTL, consolidation). It may
# not shrink a LOT. Anything past this fraction is treated as an under-capture
# until a human says otherwise.
max_shrink = float('$MAX_SHRINK_FRACTION')

# Reuse the port the pre-flight verified as answering. If that port breaks
# between pre-flight and now, we'll fail loud rather than masking — backups
# against a partially-broken cluster are worse than no backup.
cluster = Cluster([host], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4)
session = cluster.connect(keyspace)
fetch_size = int('$FETCH_SIZE')
throttle_rows = int('$THROTTLE_ROWS')
throttle_secs = float('$THROTTLE_SECS')

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

# Optional allowlist. A full dump takes hours, which makes the retention and
# regression logic effectively untestable; this makes a seconds-long run
# possible. Also useful for a targeted re-dump of one table.
# A partial run is NOT a valid baseline, so it is marked degraded below.
table_filter = [t for t in '$BACKUP_TABLES'.split(',') if t.strip()]
partial_run = bool(table_filter)
if partial_run:
    missing = sorted(set(table_filter) - set(tables))
    if missing:
        print(f'ABORT — requested tables not in {keyspace}: {missing}', file=sys.stderr)
        sys.exit(3)
    tables = sorted(table_filter)
    print(f'  PARTIAL RUN — restricted to {tables}; will be marked degraded', file=sys.stderr)
print(f'  discovered {len(tables)} tables: {tables}', file=sys.stderr)

# Load the most recent NON-DEGRADED manifest to compare against. Without a
# baseline the guard cannot fire, so the first ever run is unguarded by
# construction — that is stated in the manifest rather than hidden.
def load_baseline():
    try:
        names = sorted(d for d in os.listdir(backup_root)
                       if os.path.isdir(os.path.join(backup_root, d)))
    except FileNotFoundError:
        return None, None
    for name in reversed(names):
        if os.path.abspath(os.path.join(backup_root, name)) == os.path.abspath(backup_dir):
            continue
        path = os.path.join(backup_root, name, '_manifest.json')
        if not os.path.isfile(path):
            continue
        try:
            with open(path) as f:
                m = json.load(f)
        except Exception:
            continue
        if m.get('degraded'):
            continue
        return name, m
    return None, None

baseline_name, baseline = load_baseline()
if baseline is None:
    print('  no prior good backup — regression guard has no baseline this run', file=sys.stderr)
else:
    print(f'  regression baseline: {baseline_name}', file=sys.stderr)

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
            out_path = os.path.join(backup_dir, f'{table}.json')
            rows = session.execute(
                SimpleStatement(f'SELECT * FROM {keyspace}.{table}',
                                fetch_size=fetch_size,
                                consistency_level=ConsistencyLevel.QUORUM)
            )
            row_count = 0
            with open(out_path, 'w') as f:
                f.write('[')
                first = True
                for r in rows:
                    row_dict = {}
                    for c in r._fields:
                        row_dict[c] = getattr(r, c)
                    if not first:
                        f.write(',')
                    json.dump(row_dict, f, default=json_default)
                    first = False
                    row_count += 1
                    if throttle_rows > 0 and throttle_secs > 0 and row_count % throttle_rows == 0:
                        time.sleep(throttle_secs)
                f.write(']')
            print(f'    {table}: {row_count} rows → {os.path.basename(out_path)}', file=sys.stderr)
            manifest['tables'][table] = {'rows': row_count, 'file': f'{table}.json', 'attempts': attempt + 1}
            grand_total += row_count
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

# --- Regression guard -------------------------------------------------------
# A backup that silently captures less than the last one is worse than no
# backup, because it looks like success. Compare this run against the last
# good one and refuse to call a shrunken dump healthy.
regressions = []
manifest['baseline'] = baseline_name
manifest['max_shrink_fraction'] = max_shrink
if baseline is not None:
    prev = baseline.get('tables', {})
    prev_ok = {t: e['rows'] for t, e in prev.items() if isinstance(e, dict) and 'rows' in e}

    # A table that existed before and is gone now means schema loss, not data
    # loss — exactly the 2026-08-06 incident (t_cd44f3eb), where a node served
    # 69 of 73 tables and every query against it succeeded.
    for t in sorted(set(prev_ok) - set(tables)):
        regressions.append({'table': t, 'kind': 'table_missing',
                            'previous_rows': prev_ok[t], 'current_rows': None})

    for t in sorted(set(prev_ok) & set(manifest['tables'])):
        entry = manifest['tables'][t]
        if 'rows' not in entry:
            continue  # already recorded as a failure
        was, now = prev_ok[t], entry['rows']
        if was > 0 and now < was * (1.0 - max_shrink):
            regressions.append({'table': t, 'kind': 'row_count_regression',
                                'previous_rows': was, 'current_rows': now,
                                'lost_fraction': round((was - now) / was, 4)})
        elif now < was:
            # Under threshold: report it, do not fail on it.
            print(f'    note: {t} shrank {was} -> {now} (within {max_shrink:.0%} tolerance)',
                  file=sys.stderr)

manifest['regressions'] = regressions
manifest['partial'] = partial_run
# A partial run is degraded by definition: it is not a restorable snapshot and
# must never become the baseline a later run is compared against.
manifest['degraded'] = bool(regressions) or bool(failures) or partial_run

with open(os.path.join(backup_dir, '_manifest.json'), 'w') as f:
    json.dump(manifest, f, default=json_default, indent=2)

cluster.shutdown()

if regressions:
    print(f'  REGRESSION — this backup captured less than {baseline_name}:', file=sys.stderr)
    for r in regressions:
        if r['kind'] == 'table_missing':
            print(f\"    {r['table']}: TABLE MISSING (had {r['previous_rows']} rows)\", file=sys.stderr)
        else:
            print(f\"    {r['table']}: {r['previous_rows']} -> {r['current_rows']} \"
                  f\"({r['lost_fraction']:.1%} lost)\", file=sys.stderr)
    print('  Marked degraded; it will NOT be counted as a good backup for retention.', file=sys.stderr)
    print('  Check replica divergence before trusting it (see t_891840e7).', file=sys.stderr)

if failures:
    print(f'  total: {grand_total} rows across {len(tables) - len(failures)}/{len(tables)} tables; {len(failures)} FAILED', file=sys.stderr)
    sys.exit(4)
if regressions:
    sys.exit(5)
print(f'  total: {grand_total} rows backed up across {len(tables)} tables', file=sys.stderr)
" && DUMP_RC=0 || DUMP_RC=$?

# Retention must run even when the dump exited non-zero, otherwise a degraded
# run leaves the directory unpruned forever. The exit code is re-raised at the
# end so launchd still sees the failure.

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

# Partition into good and degraded. A degraded run must NOT count toward the
# good-backup quota — otherwise a week of silently-shrunken dumps evicts the
# last complete one, which is the failure this guard exists to prevent.
is_degraded() {
    python3 -c "
import json,sys
try:
    with open(sys.argv[1]) as f:
        sys.exit(0 if json.load(f).get('degraded') else 1)
except Exception:
    sys.exit(1)
" "$1/_manifest.json"
}

GOOD_LIST=""
DEGRADED_LIST=""
for dir in $(ls -1d 2*/ 2>/dev/null | sed 's|/$||'); do
    [ -f "$dir/_manifest.json" ] || continue
    if is_degraded "$dir"; then
        DEGRADED_LIST="$DEGRADED_LIST$dir"$'\n'
    else
        GOOD_LIST="$GOOD_LIST$dir"$'\n'
    fi
done

prune_to() {  # prune_to <newline-list> <keep> <label>
    local list="$1" keep="$2" label="$3"
    local count
    count=$(printf '%s' "$list" | grep -c . || true)
    if [ "${count:-0}" -gt "$keep" ]; then
        printf '%s' "$list" | grep . | head -n "$((count - keep))" | while IFS= read -r d; do
            rm -rf "$d"
            echo "  pruned old $label backup: $d"
        done
    fi
}

prune_to "$GOOD_LIST" "$MAX_BACKUPS" "good"
prune_to "$DEGRADED_LIST" "$MAX_DEGRADED" "degraded"

GOOD_COUNT=$(printf '%s' "$GOOD_LIST" | grep -c . || true)
DEGRADED_COUNT=$(printf '%s' "$DEGRADED_LIST" | grep -c . || true)
echo "$(date): retention — ${GOOD_COUNT:-0} good, ${DEGRADED_COUNT:-0} degraded (keep $MAX_BACKUPS/$MAX_DEGRADED)"

if [ "$DUMP_RC" -ne 0 ]; then
    echo "$(date): backup FAILED or DEGRADED (dump exit $DUMP_RC) — see errors above" >&2
    exit "$DUMP_RC"
fi

echo "$(date): backup complete"
