#!/bin/bash
# Backfill embeddings for entities that don't have them.
# Calls Ollama nomic-embed-text for each entity's context_snippet,
# then writes the embedding to Ferrosa via CQL literal.

set -euo pipefail

CQL_HOST="${FMEM_CQL_HOST:-localhost}"
CQL_PORT="${FMEM_CQL_PORT:-19042}"
OLLAMA_URL="${OLLAMA_URL:-http://localhost:11434}"
BATCH_SIZE=50

echo "$(date): backfilling embeddings (host=$CQL_HOST:$CQL_PORT, ollama=$OLLAMA_URL)"

python3 -c "
import json, uuid, sys, time, urllib.request
from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy

cluster = Cluster(['$CQL_HOST'], port=int('$CQL_PORT'), load_balancing_policy=RoundRobinPolicy(), protocol_version=4)
session = cluster.connect('agent_memory')

# Find entities without embeddings
rows = list(session.execute('SELECT tenant_id, session_id, entity_id, entity_name, context_snippet, entity_embedding FROM agent_memory.entity_store'))
missing = [r for r in rows if r.entity_embedding is None and r.entity_name and r.context_snippet]
print(f'Entities without embeddings: {len(missing)}/{len(rows)}', file=sys.stderr)

if not missing:
    print('All entities have embeddings.', file=sys.stderr)
    sys.exit(0)

def embed(text):
    text = text[:2000]  # truncate to avoid Ollama timeouts
    req = urllib.request.Request(
        '$OLLAMA_URL/api/embed',
        data=json.dumps({'model': 'nomic-embed-text', 'input': text}).encode(),
        headers={'Content-Type': 'application/json'},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read())
            if data.get('embeddings') and len(data['embeddings']) > 0:
                return data['embeddings'][0]
    except Exception as e:
        print(f'  embed failed: {e}', file=sys.stderr)
    return None

count = 0
errors = 0
for r in missing:
    text = r.context_snippet or r.entity_name
    vec = embed(text)
    if vec is None:
        errors += 1
        continue

    vec_literal = '[' + ','.join(str(v) for v in vec) + ']'
    try:
        session.execute(
            f'UPDATE agent_memory.entity_store SET entity_embedding = {vec_literal} '
            f'WHERE tenant_id = %s AND session_id = %s AND entity_id = %s',
            (r.tenant_id, r.session_id, r.entity_id)
        )
        count += 1
    except Exception as e:
        errors += 1
        if errors <= 3:
            print(f'  write failed for {r.entity_name}: {e}', file=sys.stderr)

    if count % $BATCH_SIZE == 0:
        print(f'  progress: {count}/{len(missing)} embedded ({errors} errors)', file=sys.stderr)

cluster.shutdown()
print(f'Done: {count}/{len(missing)} embeddings written ({errors} errors)', file=sys.stderr)
" 2>&1
