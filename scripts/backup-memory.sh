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

# --- Bounds ----------------------------------------------------------------
# Observed runs take 2.5-10h for ~5.5M rows (~180 rows/sec). With a 12h
# StartInterval, a 10h run leaves under 2h of headroom, so an unbounded run can
# still be going when launchd fires the next one. Every one of these is a hard
# ceiling on WORK, not a cap on results: exceeding one aborts loud and marks the
# run degraded rather than silently truncating the dump.
MAX_RUNTIME_SECS="${FMEM_BACKUP_MAX_RUNTIME_SECS:-28800}"   # 8h for the whole run
MAX_TABLE_SECS="${FMEM_BACKUP_MAX_TABLE_SECS:-5400}"        # 90m for any one table
REQUEST_TIMEOUT="${FMEM_BACKUP_REQUEST_TIMEOUT:-30}"        # per CQL page fetch
PROGRESS_SECS="${FMEM_BACKUP_PROGRESS_SECS:-60}"            # intra-table heartbeat
MAX_TOTAL_RETRIES="${FMEM_BACKUP_MAX_TOTAL_RETRIES:-25}"    # retry budget for the run

mkdir -p "$BACKUP_ROOT"

# --- Single-instance lock --------------------------------------------------
# A run that overruns the 12h interval must not be joined by a second one:
# two concurrent full-keyspace scans against the same cluster is how the host
# gets flattened. Stale locks (holder died) are reclaimed rather than wedging
# backups forever.
LOCK_FILE="$BACKUP_ROOT/.backup.lock"
if [ -f "$LOCK_FILE" ]; then
    LOCK_PID=$(head -1 "$LOCK_FILE" 2>/dev/null || echo "")
    if [ -n "$LOCK_PID" ] && kill -0 "$LOCK_PID" 2>/dev/null; then
        echo "$(date): ABORT — backup already running (pid $LOCK_PID, started $(sed -n 2p "$LOCK_FILE" 2>/dev/null)). Not starting a second scan." >&2
        exit 75   # EX_TEMPFAIL
    fi
    echo "$(date): reclaiming stale lock from dead pid ${LOCK_PID:-unknown}" >&2
    rm -f "$LOCK_FILE"
fi
printf '%s\n%s\n' "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCK_FILE"

# Release the lock on every exit path, including SIGTERM from launchctl kill.
cleanup_lock() { rm -f "$LOCK_FILE"; }
trap cleanup_lock EXIT
trap 'echo "$(date): received SIGTERM — aborting backup" >&2; exit 143' TERM
trap 'echo "$(date): received SIGINT — aborting backup" >&2; exit 130' INT

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
import json, uuid, datetime, sys, os, base64, traceback, time, signal, re

from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy
from cassandra.query import SimpleStatement
from cassandra import ConsistencyLevel

host = '$CQL_HOST'
port = int('$WORKING_PORT')
backup_dir = '$BACKUP_DIR'
backup_root = '$BACKUP_ROOT'
keyspace = 'agent_memory'

# --- Work bounds -----------------------------------------------------------
run_deadline = time.monotonic() + float('$MAX_RUNTIME_SECS')
max_table_secs = float('$MAX_TABLE_SECS')
request_timeout = float('$REQUEST_TIMEOUT')
progress_secs = float('$PROGRESS_SECS')
retry_budget = int('$MAX_TOTAL_RETRIES')

class Deadline(Exception):
    '''Raised when a bound on WORK is hit. Never swallowed — it degrades the run.'''

class Aborted(Exception):
    '''Raised on SIGTERM/SIGINT so the manifest still gets written.'''

def _on_signal(signum, _frame):
    raise Aborted(f'signal {signal.Signals(signum).name}')

signal.signal(signal.SIGTERM, _on_signal)
signal.signal(signal.SIGINT, _on_signal)
# A table may legitimately shrink (forget/retract, TTL, consolidation). It may
# not shrink a LOT. Anything past this fraction is treated as an under-capture
# until a human says otherwise.
max_shrink = float('$MAX_SHRINK_FRACTION')

# Reuse the port the pre-flight verified as answering. If that port breaks
# between pre-flight and now, we'll fail loud rather than masking — backups
# against a partially-broken cluster are worse than no backup.
def connect():
    c = Cluster([host], port=port, load_balancing_policy=RoundRobinPolicy(),
                protocol_version=4, connect_timeout=15)
    return c, c.connect(keyspace)

cluster, session = connect()

def reconnect(why):
    '''Rebuild the session after the connection is torn down.

    A single-column decode failure (entity_warmth.last_accessed_at holds an
    out-of-range timestamp) does not fail just that query — the driver marks the
    whole connection defunct. Every SUBSEQUENT table then fails NoHostAvailable,
    which IS classified retriable, so one bad cell cascades into a retry storm
    across unrelated tables. Rebuilding the session contains the blast radius to
    the table that actually has bad data.
    '''
    global cluster, session
    print(f'    reconnecting after {why}', file=sys.stderr, flush=True)
    try:
        cluster.shutdown()
    except Exception:
        pass
    cluster, session = connect()

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
retries_used = 0
aborted_reason = None

def columns_for(table):
    '''Column names for a table. The WHERE predicate is unreliable on Ferrosa
    (see the system_schema note above), so filter client-side.'''
    rows = session.execute(
        SimpleStatement('SELECT keyspace_name, table_name, column_name FROM system_schema.columns',
                        consistency_level=ConsistencyLevel.QUORUM),
        timeout=request_timeout)
    return sorted(r.column_name for r in rows
                  if getattr(r, 'keyspace_name', None) == keyspace
                  and getattr(r, 'table_name', None) == table)

# chr(34) rather than a literal double quote: this whole program is the argument
# to python3 -c inside a double-quoted shell string, so an unescaped one here
# would silently truncate the script.
BAD_COL_RE = re.compile('decoding result column ' + chr(34) + r'([A-Za-z0-9_]+)' + chr(34))

def bad_column(err_text):
    m = BAD_COL_RE.search(err_text)
    return m.group(1) if m else None

def dump_table(table, out_path, exclude_cols=()):
    '''Stream one table to JSON. Returns the row count, or raises.

    Bounds WORK (a per-table and an overall wall-clock deadline), not the
    result: hitting either raises Deadline, which marks the run degraded. It
    never returns a short row set as if it were the whole table.

    exclude_cols drops columns the driver cannot decode. That is a real loss of
    data and is recorded in the manifest as dropped_columns -- capturing the rest
    of the table beats losing all of it, but only because the gap is disclosed.
    '''
    started = time.monotonic()
    table_deadline = started + max_table_secs
    last_beat = started
    if exclude_cols:
        keep = [c for c in columns_for(table) if c not in exclude_cols]
        if not keep:
            raise RuntimeError(f'every column of {table} is undecodable')
        projection = ', '.join(keep)
    else:
        projection = '*'
    stmt = SimpleStatement(f'SELECT {projection} FROM {keyspace}.{table}',
                           fetch_size=fetch_size,
                           consistency_level=ConsistencyLevel.QUORUM)
    rows = session.execute(stmt, timeout=request_timeout)
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
            now = time.monotonic()
            # Heartbeat. document_terms is 2.25M rows and the cluster scans at
            # roughly 180 rows/sec, so one table can legitimately run for hours.
            # Without a heartbeat a slow scan and a hung one are indistinguishable
            # -- on 2026-08-25 a run sat 2h with no output and looked wedged.
            if now - last_beat >= progress_secs:
                rate = row_count / max(now - started, 1e-9)
                print(f'      {table}: {row_count:,} rows, {rate:.0f} rows/s, '
                      f'{now - started:.0f}s elapsed', file=sys.stderr, flush=True)
                last_beat = now
            if now > table_deadline:
                raise Deadline(f'per-table budget of {max_table_secs:.0f}s exceeded '
                               f'at {row_count:,} rows')
            if now > run_deadline:
                raise Deadline(f'overall run budget exceeded during {table} '
                               f'at {row_count:,} rows')
            if throttle_rows > 0 and throttle_secs > 0 and row_count % throttle_rows == 0:
                time.sleep(throttle_secs)
        f.write(']')
    return row_count

def discard_partial(out_path):
    '''A truncated dump is invalid JSON with no closing bracket, but it is still
    a plausible-looking <table>.json sitting next to the good ones. Remove it so
    a failed table leaves no artifact a restore could pick up.'''
    try:
        if os.path.exists(out_path):
            os.remove(out_path)
    except OSError as e:
        print(f'    warning: could not remove partial {out_path}: {e}', file=sys.stderr)

for table in tables:
    if aborted_reason:
        break
    last_err = None
    success = False
    dropped_cols = []
    out_path = os.path.join(backup_dir, f'{table}.json')
    for attempt in range(MAX_ATTEMPTS):
        if time.monotonic() > run_deadline:
            aborted_reason = 'overall runtime budget exhausted'
            last_err = last_err or 'run deadline reached before this table started'
            break
        if attempt > 0:
            if retries_used >= retry_budget:
                last_err = f'{last_err} (run retry budget of {retry_budget} exhausted)'
                print(f'    {table}: retry budget exhausted for this run; giving up',
                      file=sys.stderr, flush=True)
                break
            retries_used += 1
            time.sleep(BACKOFF_SECS[attempt])
            print(f'  retrying {table} (attempt {attempt + 1}/{MAX_ATTEMPTS}, '
                  f'retry {retries_used}/{retry_budget})...', file=sys.stderr, flush=True)
        else:
            print(f'  dumping {table}...', file=sys.stderr, flush=True)
        try:
            row_count = dump_table(table, out_path, exclude_cols=tuple(dropped_cols))
            note = f' (dropped undecodable columns: {dropped_cols})' if dropped_cols else ''
            print(f'    {table}: {row_count} rows → {os.path.basename(out_path)}{note}', file=sys.stderr)
            manifest['tables'][table] = {'rows': row_count, 'file': f'{table}.json', 'attempts': attempt + 1}
            if dropped_cols:
                manifest['tables'][table]['dropped_columns'] = list(dropped_cols)
            grand_total += row_count
            success = True
            break
        except Aborted as e:
            discard_partial(out_path)
            aborted_reason = str(e)
            last_err = f'aborted: {e}'
            break
        except Deadline as e:
            discard_partial(out_path)
            last_err = f'Deadline: {e}'
            print(f'    {table}: {last_err}', file=sys.stderr, flush=True)
            # A deadline is a statement about how much work we will do, not a
            # transient fault. Retrying would just burn the same budget again.
            if 'overall run budget' in str(e):
                aborted_reason = 'overall runtime budget exhausted'
            break
        except Exception as e:
            discard_partial(out_path)
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
            # A decode failure kills the connection, so everything after it fails
            # NoHostAvailable until the session is rebuilt. Reconnect on any
            # connection-level error so one bad cell cannot cascade.
            if any(m in probe for m in ('ConnectionShutdown', 'defunct',
                                        'NoHostAvailable', 'decoding')):
                try:
                    reconnect(f'{type(e).__name__} on {table}')
                except Exception as rerr:
                    last_err = f'{last_err}; reconnect failed: {rerr}'
                    aborted_reason = 'cluster unreachable after connection loss'
                    break
            # A column the driver cannot decode (entity_warmth.last_accessed_at
            # holds an out-of-range timestamp) fails the table forever otherwise
            # -- it has failed every run since 2026-08-05. Drop just that column
            # and retry: a table minus one disclosed column beats no table at all.
            col = bad_column(probe)
            if col and col not in dropped_cols:
                dropped_cols.append(col)
                retriable = True
                print(f'    {table}: column {col} is undecodable; retrying without it',
                      file=sys.stderr, flush=True)
            print(f'    {table}: attempt {attempt + 1} failed ({last_err}); retriable={retriable}', file=sys.stderr)
            if not retriable:
                break
    if not success:
        msg = last_err or 'unknown error'
        failures.append((table, msg))
        manifest['tables'][table] = {'error': msg, 'attempts': attempt + 1}

# Tables we never reached because the run was cut short are recorded explicitly.
# Staying silent would make them look absent from the schema on the next run,
# which the regression guard would then report as table_missing rather than as
# work this run simply did not get to.
if aborted_reason:
    for t in tables:
        if t not in manifest['tables']:
            manifest['tables'][t] = {'error': f'not attempted: {aborted_reason}', 'attempts': 0}
            failures.append((t, f'not attempted: {aborted_reason}'))

manifest['completed_at'] = datetime.datetime.utcnow().isoformat() + 'Z'
manifest['grand_total_rows'] = grand_total
manifest['failures'] = failures
manifest['aborted_reason'] = aborted_reason
manifest['retries_used'] = retries_used
manifest['bounds'] = {
    'max_runtime_secs': float('$MAX_RUNTIME_SECS'),
    'max_table_secs': max_table_secs,
    'request_timeout_secs': request_timeout,
    'max_total_retries': retry_budget,
}
if aborted_reason:
    print(f'  ABORTED — {aborted_reason}', file=sys.stderr)

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
# Dropping an undecodable column salvages the table, but the dump is missing
# real data. That is a degraded snapshot, not a clean one -- it must never
# become the baseline a later run is compared against.
dropped = {t: e['dropped_columns'] for t, e in manifest['tables'].items()
           if isinstance(e, dict) and e.get('dropped_columns')}
manifest['dropped_columns'] = dropped
if dropped:
    print('  DEGRADED — columns dropped because the driver could not decode them:',
          file=sys.stderr)
    for t, cols in sorted(dropped.items()):
        print(f'    {t}: {cols}', file=sys.stderr)
    print('  These columns are ABSENT from this backup. Restoring it loses them.',
          file=sys.stderr)
# A partial run is degraded by definition: it is not a restorable snapshot and
# must never become the baseline a later run is compared against.
manifest['degraded'] = (bool(regressions) or bool(failures) or partial_run
                        or bool(aborted_reason) or bool(dropped))

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

if aborted_reason:
    print(f'  total: {grand_total} rows across {len(tables) - len(failures)}/{len(tables)} tables '
          f'before abort ({aborted_reason})', file=sys.stderr)
    sys.exit(6)
if failures:
    print(f'  total: {grand_total} rows across {len(tables) - len(failures)}/{len(tables)} tables; {len(failures)} FAILED', file=sys.stderr)
    sys.exit(4)
if regressions:
    sys.exit(5)
if dropped:
    print(f'  total: {grand_total} rows across {len(tables)} tables, '
          f'but {len(dropped)} table(s) lost columns — NOT a clean backup', file=sys.stderr)
    sys.exit(7)
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

# --- Health marker ----------------------------------------------------------
# Every run from 2026-08-05 to 2026-08-24 failed (entity_warmth in all 13) and
# nobody noticed, because the only evidence was a non-zero exit into a log file
# no one reads. A single status file makes "when did this last actually work?"
# answerable in one cat, and the consecutive-failure count makes a chronic
# failure read differently from a one-off.
STATUS_FILE="$BACKUP_ROOT/.backup-status"
PREV_STREAK=0
if [ -f "$STATUS_FILE" ]; then
    PREV_STREAK=$(sed -n 's/^consecutive_failures=\([0-9][0-9]*\)$/\1/p' "$STATUS_FILE" 2>/dev/null || echo 0)
fi
PREV_STREAK=${PREV_STREAK:-0}

LAST_GOOD=$(sed -n 's/^last_success=\(.*\)$/\1/p' "$STATUS_FILE" 2>/dev/null || echo "never")
LAST_GOOD=${LAST_GOOD:-never}

if [ "$DUMP_RC" -eq 0 ]; then
    STREAK=0
    LAST_GOOD=$(date -u +%Y-%m-%dT%H:%M:%SZ)
else
    STREAK=$((PREV_STREAK + 1))
fi

{
    echo "last_run=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "last_run_exit=$DUMP_RC"
    echo "last_run_dir=$BACKUP_DIR"
    echo "consecutive_failures=$STREAK"
    echo "last_success=$LAST_GOOD"
} > "$STATUS_FILE"

if [ "$DUMP_RC" -ne 0 ]; then
    echo "$(date): backup FAILED or DEGRADED (dump exit $DUMP_RC) — see errors above" >&2
    echo "$(date): consecutive failed runs: $STREAK; last clean backup: $LAST_GOOD" >&2
    if [ "$STREAK" -ge 3 ]; then
        echo "$(date): ALERT — $STREAK consecutive backup failures. There is no recent" >&2
        echo "  restorable snapshot. Investigate before trusting $BACKUP_ROOT." >&2
    fi
    exit "$DUMP_RC"
fi

echo "$(date): backup complete (previous failure streak: $PREV_STREAK, now reset)"
