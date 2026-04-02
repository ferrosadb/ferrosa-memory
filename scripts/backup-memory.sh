#!/bin/bash
# Backup fmem data to disk via CQL dump.
# Run every 30 minutes via cron or launchd.
#
# Dumps entity_store, typed_edges, entity_types, and edge_types to JSON.
# Keeps last 48 backups (24 hours at 30-min interval).
#
# Restore: python3 scripts/restore-memory.sh <backup-dir>

set -euo pipefail

BACKUP_ROOT="${FMEM_BACKUP_DIR:-$HOME/data/ferrosa-memory/backups}"
CQL_HOST="${FMEM_CQL_HOST:-localhost}"
CQL_PORT="${FMEM_CQL_PORT:-19042}"
MAX_BACKUPS=10

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
BACKUP_DIR="$BACKUP_ROOT/$TIMESTAMP"
mkdir -p "$BACKUP_DIR"

echo "$(date): backing up fmem to $BACKUP_DIR"

python3 -c "
import json, uuid, datetime, sys, os

from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy

host = '$CQL_HOST'
port = int('$CQL_PORT')
backup_dir = '$BACKUP_DIR'

cluster = Cluster([host], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4)
session = cluster.connect('agent_memory')

def uuid_serializer(obj):
    if isinstance(obj, uuid.UUID):
        return str(obj)
    if isinstance(obj, (datetime.datetime, datetime.date)):
        return obj.isoformat()
    if isinstance(obj, bytes):
        import base64
        return base64.b64encode(obj).decode('ascii')
    raise TypeError(f'Not serializable: {type(obj)}')

# Dump entity_store
print('  dumping entity_store...', file=sys.stderr)
rows = list(session.execute('SELECT * FROM agent_memory.entity_store'))
entities = []
for r in rows:
    entities.append({c: getattr(r, c) for c in r._fields})
with open(os.path.join(backup_dir, 'entity_store.json'), 'w') as f:
    json.dump(entities, f, default=uuid_serializer)
print(f'  entity_store: {len(entities)} rows', file=sys.stderr)

# Dump typed_edges
print('  dumping typed_edges...', file=sys.stderr)
rows = list(session.execute('SELECT * FROM agent_memory.typed_edges'))
edges = []
for r in rows:
    edges.append({c: getattr(r, c) for c in r._fields})
with open(os.path.join(backup_dir, 'typed_edges.json'), 'w') as f:
    json.dump(edges, f, default=uuid_serializer)
print(f'  typed_edges: {len(edges)} rows', file=sys.stderr)

# Dump entity_types registry
print('  dumping entity_types...', file=sys.stderr)
rows = list(session.execute('SELECT * FROM agent_memory.entity_types'))
types = []
for r in rows:
    types.append({c: getattr(r, c) for c in r._fields})
with open(os.path.join(backup_dir, 'entity_types.json'), 'w') as f:
    json.dump(types, f, default=uuid_serializer)
print(f'  entity_types: {len(types)} rows', file=sys.stderr)

# Dump edge_types registry
print('  dumping edge_types...', file=sys.stderr)
rows = list(session.execute('SELECT * FROM agent_memory.edge_types'))
etypes = []
for r in rows:
    etypes.append({c: getattr(r, c) for c in r._fields})
with open(os.path.join(backup_dir, 'edge_types.json'), 'w') as f:
    json.dump(etypes, f, default=uuid_serializer)
print(f'  edge_types: {len(etypes)} rows', file=sys.stderr)

# Dump intentions
print('  dumping intentions...', file=sys.stderr)
rows = list(session.execute('SELECT * FROM agent_memory.intentions'))
intents = []
for r in rows:
    intents.append({c: getattr(r, c) for c in r._fields})
with open(os.path.join(backup_dir, 'intentions.json'), 'w') as f:
    json.dump(intents, f, default=uuid_serializer)
print(f'  intentions: {len(intents)} rows', file=sys.stderr)

cluster.shutdown()

total = len(entities) + len(edges) + len(types) + len(etypes) + len(intents)
print(f'  total: {total} rows backed up', file=sys.stderr)
"

# Prune old backups, keep last MAX_BACKUPS
cd "$BACKUP_ROOT"
BACKUP_COUNT=$(ls -1d 2* 2>/dev/null | wc -l | tr -d ' ')
if [ "$BACKUP_COUNT" -gt "$MAX_BACKUPS" ]; then
    PRUNE_COUNT=$((BACKUP_COUNT - MAX_BACKUPS))
    ls -1d 2* | head -n "$PRUNE_COUNT" | while read dir; do
        rm -rf "$dir"
        echo "  pruned old backup: $dir"
    done
fi

echo "$(date): backup complete"
