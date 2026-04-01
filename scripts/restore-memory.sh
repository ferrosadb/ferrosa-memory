#!/bin/bash
# Restore fmem data from a backup directory.
#
# Usage: ./scripts/restore-memory.sh [backup-dir]
#        ./scripts/restore-memory.sh              # restore latest
#        ./scripts/restore-memory.sh 20260401-120000  # restore specific
#
# This INSERTS data — it does not delete existing rows first.
# Duplicate primary keys are overwritten (CQL upsert semantics).

set -euo pipefail

BACKUP_ROOT="${FMEM_BACKUP_DIR:-$HOME/data/ferrosa-memory/backups}"
CQL_HOST="${FMEM_CQL_HOST:-localhost}"
CQL_PORT="${FMEM_CQL_PORT:-19042}"

if [ -n "${1:-}" ]; then
    if [ -d "$1" ]; then
        BACKUP_DIR="$1"
    elif [ -d "$BACKUP_ROOT/$1" ]; then
        BACKUP_DIR="$BACKUP_ROOT/$1"
    else
        echo "ERROR: backup not found: $1"
        echo "Available backups:"
        ls -1d "$BACKUP_ROOT"/2* 2>/dev/null | tail -10
        exit 1
    fi
else
    BACKUP_DIR=$(ls -1d "$BACKUP_ROOT"/2* 2>/dev/null | tail -1)
    if [ -z "$BACKUP_DIR" ]; then
        echo "ERROR: no backups found in $BACKUP_ROOT"
        exit 1
    fi
fi

echo "Restoring from: $BACKUP_DIR"

# Count rows in backup
for f in entity_store typed_edges entity_types edge_types; do
    if [ -f "$BACKUP_DIR/$f.json" ]; then
        COUNT=$(python3 -c "import json; print(len(json.load(open('$BACKUP_DIR/$f.json'))))")
        echo "  $f: $COUNT rows"
    fi
done

read -p "Proceed with restore? [y/N] " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
fi

python3 -c "
import json, uuid, datetime, sys, os

from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy

host = '$CQL_HOST'
port = int('$CQL_PORT')
backup_dir = '$BACKUP_DIR'

cluster = Cluster([host], port=port, load_balancing_policy=RoundRobinPolicy(), protocol_version=4)
session = cluster.connect('agent_memory')

def parse_uuid(s):
    return uuid.UUID(s) if s else None

def parse_dt(s):
    if not s: return None
    return datetime.datetime.fromisoformat(s.replace('Z', '+00:00'))

# Restore entity_store
path = os.path.join(backup_dir, 'entity_store.json')
if os.path.exists(path):
    entities = json.load(open(path))
    q = 'INSERT INTO agent_memory.entity_store (tenant_id, session_id, entity_id, entity_name, entity_type, context_snippet, confidence, state, created_at) VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s)'
    count = 0
    for e in entities:
        if not e.get('entity_name') or not e.get('entity_id'):
            continue  # skip ghost rows
        try:
            session.execute(q, (
                parse_uuid(e['tenant_id']),
                parse_uuid(e['session_id']),
                parse_uuid(e['entity_id']),
                e['entity_name'],
                e.get('entity_type', 'concept'),
                e.get('context_snippet', ''),
                float(e.get('confidence', 1.0)),
                e.get('state', 'active'),
                parse_dt(e.get('created_at')) or datetime.datetime.now(datetime.UTC),
            ))
            count += 1
        except Exception as ex:
            print(f'  WARN: entity {e.get(\"entity_name\")}: {ex}', file=sys.stderr)
        if count % 500 == 0 and count > 0:
            print(f'  entity_store: {count}/{len(entities)}', file=sys.stderr)
    print(f'  entity_store: {count}/{len(entities)} restored', file=sys.stderr)

# Restore typed_edges
path = os.path.join(backup_dir, 'typed_edges.json')
if os.path.exists(path):
    edges = json.load(open(path))
    q = 'INSERT INTO agent_memory.typed_edges (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) VALUES (%s,%s,%s,%s,%s,%s,%s,%s)'
    count = 0
    for e in edges:
        if not e.get('edge_type') or not e.get('src_id'):
            continue  # skip ghost rows
        try:
            session.execute(q, (
                parse_uuid(e['tenant_id']),
                parse_uuid(e['session_id']),
                parse_uuid(e['src_id']),
                e['edge_type'],
                parse_uuid(e['dst_id']),
                float(e.get('weight', 1.0)),
                e.get('metadata', ''),
                parse_dt(e.get('created_at')) or datetime.datetime.now(datetime.UTC),
            ))
            count += 1
        except Exception as ex:
            pass
        if count % 1000 == 0 and count > 0:
            print(f'  typed_edges: {count}/{len(edges)}', file=sys.stderr)
    print(f'  typed_edges: {count}/{len(edges)} restored', file=sys.stderr)

# Restore entity_types
path = os.path.join(backup_dir, 'entity_types.json')
if os.path.exists(path):
    types = json.load(open(path))
    for t in types:
        if not t.get('type_name'): continue
        session.execute(
            'INSERT INTO agent_memory.entity_types (type_name, description, created_at) VALUES (%s,%s,%s)',
            (t['type_name'], t.get('description', ''), parse_dt(t.get('created_at')) or datetime.datetime.now(datetime.UTC))
        )
    print(f'  entity_types: {len(types)} restored', file=sys.stderr)

# Restore edge_types
path = os.path.join(backup_dir, 'edge_types.json')
if os.path.exists(path):
    etypes = json.load(open(path))
    for t in etypes:
        if not t.get('type_name'): continue
        session.execute(
            'INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at) VALUES (%s,%s,%s,%s,%s)',
            (t['type_name'], t.get('description', ''), t.get('src_types', ''), t.get('dst_types', ''), parse_dt(t.get('created_at')) or datetime.datetime.now(datetime.UTC))
        )
    print(f'  edge_types: {len(etypes)} restored', file=sys.stderr)

cluster.shutdown()
print('Restore complete.', file=sys.stderr)
"
