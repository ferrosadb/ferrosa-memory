# Ferrosa + Ferrosa Memory Onboarding Harness

This file is designed to be pasted or loaded into an LLM with an **"onboard me"** command. It gives Hermes, Claude, Codex, or another coding-agent harness a structured setup flow for a new user who wants to run Ferrosa Database and Ferrosa Memory locally, configure agent integrations, and verify that memory tools work.

## Onboard-me command

Use this prompt with an LLM agent:

```text
onboard me using ONBOARDING.md
```

The agent must read this file, ask the questions in Phase 0, then execute only the steps that match the user's machine, preferred runtime, and agent harness. The agent must not assume credentials, ports, data paths, or whether Docker/Podman is available.

---

## Agent rules for this onboarding

1. **Ask first, then act.** Ask the Phase 0 questions before running setup commands.
2. **Do not delete data.** Never run `docker compose down -v`, `podman compose down -v`, `rm -rf`, `git clean`, or volume cleanup unless the user explicitly asks for destructive reset.
3. **Prefer public repos.** Clone from the public repositories unless the user gives alternate remotes.
4. **Keep secrets out of chat.** Ask users to set API keys and passwords in environment/config files; do not ask them to paste secrets into the conversation.
5. **Verify each layer.** Source checkout, build, containers, HTTP health, MCP tools, and agent harness integration each need separate verification.
6. **Record decisions.** If the harness supports memory/notes, save only stable decisions: install directory, container runtime, exposed ports, and selected agent integrations.
7. **Verify persistent mounts before diagnosing missing data.** For the full local compose stack, node data must be mounted from `~/data/ferrosa-memory/node1`, `~/data/ferrosa-memory/node2`, and `~/data/ferrosa-memory/node3`. If `19042-19044` are served by CI/test containers or `.runtime/` data directories, stop and fix the runtime wiring before searching for or restoring data.
8. **Preserve schema migration order and data.** Follow `AGENTS.md`: every schema change needs an ordered version bump and an automatic migration. A database at version `N` must upgrade to version `M` by applying every migration in sequence, and migrations must preserve or transform old rows rather than dropping, damaging, or orphaning them.

---

## Phase 0 — Questions to ask the user

Ask these before setup:

1. **Operating system:** macOS, Linux, or WSL2?
2. **Deployment mode:** native binaries, Docker Compose, Podman Compose, or no preference? For a minimal local setup, native binaries with local filesystem storage are enough; S3/MinIO is optional.
3. **Container runtime, if using containers:** Docker, Podman, neither, or no preference?
4. **Install directory:** where should the repos live? Default: `~/src/ferrosa-suite`.
5. **Data directory:** where should persistent Ferrosa data live? Default: `~/data/ferrosa-memory`.
6. **Embedding layer:** should semantic/vector search use local Nomic embeddings via Ollama? If skipped, lexical/phonetic search still works but semantic recall will be degraded.
7. **Ports:** are these available?
   - MCP/workbench: `18765`
   - Viz: `18766`
   - CQL nodes: `19042`, `19043`, `19044`
   - Graph HTTP: `17474`, `17475`, `17476`
   - Bolt: `17687`, `17688`, `17689`
   - MinIO, full stack only: `19000`, `19001`
8. **Agent harnesses to configure:** Hermes, Claude Desktop/Claude Code, Codex, or all three?
9. **Should the setup use local default credentials for a single-user dev stack?** If no, pause and ask the user to provide their preferred auth policy out-of-band.
10. **Should the agent build from source now, or only write setup commands for the user to run?**

If the user is unsure, recommend:

```text
Native binaries + local filesystem storage for the quickest single-user setup, Docker Compose for the full 3-node/S3-like dev stack, Nomic embeddings enabled when semantic search quality matters, Hermes first, then Claude/Codex MCP configs.
```

---

## Phase 1 — Hosted quick install or source checkout

Choose one setup path. Do not mix the hosted bootstrap command with a source checkout unless the user explicitly wants both.

### Hosted quick install

Use the hosted bootstrap for the fastest single-user setup on a machine without an existing checkout. The script is published from `ferrosadb.com`; it is **not** a file that should already exist in this repository checkout. It installs the prebuilt binaries, downloads this onboarding file, **installs the LLM-harness hooks** (session-start / recall / turn-finalization), optionally clones or updates the public repos, offers to pull the Nomic embedding model, and then hands the user to the selected LLM harness for the `onboard me` flow:

```bash
curl -fsSL https://ferrosadb.com/setup-memory.sh | bash
```

The hosted `setup-memory.sh` uses this `ONBOARDING.md` as its source of truth for skills, hints, hooks, prompts, runtime choices, credentials, and ports. It does **not** require cloning either repo: hook installation works without a checkout by fetching the self-contained hook installer pinned to the release tag. Useful flags: `--no-clone` (skip the source checkout), `--harness auto|all|codex|claude|hermes|generic`, `--mcp-url <url>`, `--no-hooks` (skip hook install), `--no-nomic`, `--no-hermes`.

> Note: there are **two** scripts named `setup.sh`. The hosted `https://ferrosadb.com/setup.sh` installs only the **Ferrosa database** binary. The repo-local `./setup.sh` (below) is the **contributor** harness installer and exists only inside a source checkout. The hosted `setup-memory.sh` is what end users run; it does not depend on the repo-local `./setup.sh`.

### Source checkout setup

If the user already cloned `ferrosa-memory`, use the repo-local setup script from that checkout — the right path for contributors, local development, and anyone following instructions from inside the repository. It builds the MCP binary by default, installs/restarts the macOS LaunchAgent when available, writes Codex/Claude/Hermes hook wrappers, patches supported harness config files, and verifies the default MCP tool list includes `ingest`:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
./setup.sh --harness auto
```

The compact tool list should also include `edge` and `turn_chain`; use `all_tools` when a harness needs the full catalog.

Common variants:

```bash
./setup.sh --harness all --skip-service
./setup.sh --harness codex --skip-build --skip-service
./setup.sh --harness auto --no-apply-config
```

Manual clone/update remains available for contributors:

```bash
mkdir -p ~/src/ferrosa-suite
cd ~/src/ferrosa-suite

git clone https://github.com/ferrosadb/ferrosa.git ferrosa
# If it already exists:
git -C ferrosa fetch --all --prune

git clone https://github.com/ferrosadb/ferrosa-memory.git ferrosa-memory
# If it already exists:
git -C ferrosa-memory fetch --all --prune
```

Verification:

```bash
git -C ~/src/ferrosa-suite/ferrosa remote -v
git -C ~/src/ferrosa-suite/ferrosa-memory remote -v
git -C ~/src/ferrosa-suite/ferrosa status --short --branch
git -C ~/src/ferrosa-suite/ferrosa-memory status --short --branch
```

Expected remotes:

```text
https://github.com/ferrosadb/ferrosa.git
https://github.com/ferrosadb/ferrosa-memory.git
```

---

## Phase 2 — Prerequisites

Check tools:

```bash
rustc --version
cargo --version
git --version
docker --version || podman --version
docker compose version || podman compose version
curl --version
python3 --version
```

Install missing prerequisites with the OS package manager. Do not invent one-size-fits-all install commands; ask the user which package manager they use.

---

## Phase 3 — Choose runtime mode

### Option A: minimal native binaries

Use this for the smallest single-user setup. It avoids Compose and S3-compatible object storage. Run Ferrosa Database and Ferrosa Memory directly from the prebuilt binaries installed by `setup.sh` / `setup-memory.sh`.

```bash
~/.ferrosa/bin/ferrosa --version
~/.ferrosa/bin/ferrosa-memory-mcp --help
```

If the hosted setup scripts were not used, install both directly. The memory
script installs the binary **and** the harness hooks without a checkout
(`--no-clone` skips only the optional source clone, not the hooks):

```bash
curl -fsSL https://ferrosadb.com/setup.sh | bash -s -- --no-service --no-password
curl -fsSL https://ferrosadb.com/setup-memory.sh | bash -s -- --no-clone --no-nomic --no-hermes
```

To (re)install the harness hooks by themselves — e.g. after switching harnesses
— the simplest path is to re-run `setup-memory.sh` (it is idempotent). To run
the installer directly, fetch **both** of its self-contained files (the wrappers
reference the turn hook by path, so it must live somewhere stable) pinned to
your installed version `$VER`:

```bash
VER=$(curl -fsSL https://ferrosadb.com/LATEST | tr -d '[:space:]')
RAW="https://raw.githubusercontent.com/ferrosadb/ferrosa-memory/${VER}"
DEST="$HOME/.ferrosa/share/ferrosa-memory"
mkdir -p "$DEST/scripts/hooks"
curl -fsSL "$RAW/scripts/install-agent-hooks.py"            -o "$DEST/scripts/install-agent-hooks.py"
curl -fsSL "$RAW/scripts/hooks/ferrosa-memory-turn-hook.py" -o "$DEST/scripts/hooks/ferrosa-memory-turn-hook.py"
( cd "$DEST" && python3 scripts/install-agent-hooks.py --harness auto --mcp-url http://127.0.0.1:18765/mcp )
```

A native setup still needs a Ferrosa node listening on CQL and graph ports, plus a Ferrosa Memory config pointing at those ports. Both setup scripts drop example configs at `~/.ferrosa/config/{ferrosa,ferrosa-memory}.toml`. Keep data under the user-selected data directory, for example:

```text
~/data/ferrosa-memory/native/node1
~/data/ferrosa-memory/native/memory
```

Native mode is enough for local onboarding, MCP tool testing, and agent harness integration. Use the Compose stack when you need the full three-node cluster shape, MinIO/S3-like object storage, or operational testing.

Contributors who need a from-source build can clone the repositories (Phase 1 `git clone` block) and run `cargo build --release` inside each, but the onboarding flow does not require it.

### Option B: full Compose development stack

Use this when you want the three-node Ferrosa cluster plus MinIO and the same port layout used by development docs.

---

## Phase 4 — Build Ferrosa Database image

From the Ferrosa repo:

```bash
cd ~/src/ferrosa-suite/ferrosa
docker build -t ferrosa-memory-node:latest .
```

For Podman:

```bash
cd ~/src/ferrosa-suite/ferrosa
podman build -t ferrosa-memory-node:latest .
```

Verification:

```bash
docker image inspect ferrosa-memory-node:latest --format 'image={{.Id}} created={{.Created}}' \
  || podman image inspect ferrosa-memory-node:latest --format 'image={{.Id}} created={{.Created}}'
```

---

## Phase 5 — Configure Ferrosa Memory runtime files

The development compose file is in:

```text
~/src/ferrosa-suite/ferrosa-memory/docker-compose.yml
```

It expects runtime config under:

```text
~/src/ferrosa-suite/ferrosa-memory/.runtime/
```

Generate the local runtime files before starting Compose:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
scripts/init-runtime.sh
```

The generated `.runtime/ferrosa-memory-http-podman.toml` is designed for the repository compose file's `network_mode: host` MCP service:

- MCP/workbench bound to loopback on `127.0.0.1:18765`
- CQL contact points `127.0.0.1:19042`, `127.0.0.1:19043`, `127.0.0.1:19044`
- `agent_memory` keyspace
- file-backed local HTTP auth via `.runtime/http-auth.toml`
- viz disabled by default for HTTP mode

Do not commit `.runtime/` secrets.

---

## Phase 6 — Start Ferrosa Memory stack

Docker:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
scripts/init-runtime.sh
make build-podman-binary
docker compose up -d
```

Podman:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
scripts/init-runtime.sh
make build-podman-binary
podman compose up -d
```

Expected local endpoints:

```text
Ferrosa Memory MCP/workbench: http://127.0.0.1:18765/
Ferrosa Memory MCP JSON-RPC: http://127.0.0.1:18765/mcp
Ferrosa Memory viz:          http://127.0.0.1:18766/viz
Ferrosa CQL node 1:          127.0.0.1:19042
Ferrosa CQL node 2:          127.0.0.1:19043
Ferrosa CQL node 3:          127.0.0.1:19044
```

Health checks:

```bash
curl -fsS http://127.0.0.1:18765/healthz/live && echo
curl -fsS http://127.0.0.1:18765/healthz/ready && echo
curl -fsS http://127.0.0.1:18766/viz | head -c 64 && echo
```

Container checks:

```bash
docker compose ps || podman compose ps
```

Expected DB services:

```text
node1 healthy
node2 healthy
node3 healthy
minio healthy
ferrosa-memory-mcp running/healthy
```

If MCP is run as a standalone host-network container instead of compose, preserve the existing topology and verify with container inspect before changing it.

---

## Phase 7 — Configure optional Nomic embeddings

Ferrosa Memory can still operate without an embedding model, but semantic/vector search quality will be degraded. Lexical, phonetic, direct ID lookup, and graph traversal remain useful; ANN-style semantic ranking and context-segment embedding search need embeddings.

Recommended local option:

```bash
ollama pull nomic-embed-text-v2-moe
ollama list | grep nomic-embed-text-v2-moe
```

Then configure Ferrosa Memory's embedding provider according to the repository config examples. Verify with a semantic query that should match paraphrased evidence, not only exact keywords.

If the user declines embeddings, record this clearly in the onboarding summary:

```text
Nomic embeddings disabled; semantic search degraded. Use lexical/phonetic examples for initial verification.
```

---

## Phase 8 — Smoke-test MCP tools

Use HTTP JSON-RPC if the agent harness has no native MCP client yet:

```bash
curl -sS -u ferrosa_user:ferrosa_user \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  http://127.0.0.1:18765/mcp
```

Then call a read-only tool such as stats if exposed:

```bash
curl -sS -u ferrosa_user:ferrosa_user \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_stats","arguments":{}}}' \
  http://127.0.0.1:18765/mcp
```

Success criteria:

- `/healthz/live` returns `ok`.
- `/healthz/ready` returns `ready`.
- `/viz` returns HTML.
- `tools/list` includes memory tools.
- Compact `tools/list` includes `edge` for typed edge creation and `turn_chain` for captured-turn traversal.
- `get_stats` returns JSON, not a timeout.

---

## Phase 9 — Configure Hermes

If Hermes is selected, configure Ferrosa Memory as an MCP server. Prefer non-interactive config edits for repeatable onboarding; use `hermes mcp add` only when an interactive wizard is acceptable.

Ask the user where Hermes config lives if not default. Default:

```text
~/.hermes/config.yaml
```

MCP server shape:

```yaml
mcp_servers:
  ferrosa-memory:
    url: "http://127.0.0.1:18765/mcp"
    transport: "http"
    headers:
      Authorization: "Basic <base64 ferrosa_user:ferrosa_user>"
```

Safer approach: put credentials in the harness' secret store or environment and reference them according to the harness' supported config format.

Verify:

```bash
hermes mcp list
hermes mcp test ferrosa-memory
```

In a new Hermes session:

```text
/reload-mcp
Use the ferrosa-memory get_stats tool and tell me whether memory is healthy.
```

Recommended Hermes skills to install or load for this repo family:

```text
ferrosa-memory-ops
llm-context-evaluation
repo-workflow
test-driven-development
subagent-driven-development
systematic-debugging
```

If using fmem-backed skills, verify the skill list is non-empty after MCP is healthy. Do not cache an empty skill list during an outage.

---

## Phase 10 — Configure Claude

For Claude Desktop or Claude Code, use the harness' MCP configuration mechanism to add the same HTTP MCP endpoint:

```json
{
  "mcpServers": {
    "ferrosa-memory": {
      "type": "http",
      "url": "http://127.0.0.1:18765/mcp",
      "headers": {
        "Authorization": "Basic <base64 ferrosa_user:ferrosa_user>"
      }
    }
  }
}
```

If the Claude harness does not support HTTP MCP directly, run a local stdio bridge only if the repository provides one or the user approves installing one.

Verify in Claude:

```text
List available MCP tools for ferrosa-memory, then call get_stats.
```

---

## Phase 11 — Configure Codex

For Codex or Codex-like agent harnesses, add Ferrosa Memory as an MCP HTTP server using the harness-specific MCP config file.

Generic server entry:

```json
{
  "name": "ferrosa-memory",
  "transport": "http",
  "url": "http://127.0.0.1:18765/mcp",
  "headers": {
    "Authorization": "Basic <base64 ferrosa_user:ferrosa_user>"
  }
}
```

Verify in Codex:

```text
Use the ferrosa-memory MCP server to call get_stats, then retrieve entities for query "Ferrosa Memory".
```

If the harness cannot attach MCP servers, keep the HTTP JSON-RPC smoke commands as the fallback.

---

## Phase 12 — Install agent hooks

Run the hook installer after MCP health checks pass. A Claude or Hermes agent can run this step directly from the repository checkout:

```bash
python3 scripts/install-agent-hooks.py --harness auto --verify
```

For a full source-checkout setup, prefer the top-level wrapper:

```bash
./setup.sh --harness auto
```

The installer detects local Codex, Claude, and Hermes harnesses. It writes wrapper scripts and a manifest under:

```text
~/.config/ferrosa-memory/hooks/
```

Generated wrappers:

```text
<harness>-session-start.sh
<harness>-recall.sh
<harness>-ingest-turn.sh
```

The session-start wrapper calls `configure` once with harness session metadata so the MCP server creates and stores the active Ferrosa Memory `session_id`. Agents should not generate or remember Ferrosa Memory UUIDs in prompts. The recall wrapper calls `check_intentions` and `hybrid_search` with the current working directory. The ingest wrapper asks Ferrosa Memory for the active session and stores:

- a durable `turn` entity with `cwd`, `workspace`, and `working_directory` attributes;
- automatic temporal `next_turn` and `previous_turn` links between successive captured turns in the same session, queryable through `turn_chain`;
- deterministic context segments through `ctx_ingest`, including user, assistant, and tool artifacts when the harness payload exposes them;
- session, turn, harness, and cwd metadata so later retrieval and reranking can prefer knowledge learned in the same repo.

This is the intended session-memory loop:

1. Session-start hook establishes the active Ferrosa Memory session mechanically.
2. Pre-turn recall injects relevant memories for the active working directory.
3. Turn-end capture stores the trajectory and surrounding context.
4. Search/rerank uses cwd/workspace metadata and later `feedback`/`outcome` signals to adjust future rankings.
5. The agent should call `feedback` with `+1`/`-1` item feedback after retrieval, and call `outcome` for broader task success/failure when it can identify the relevant entity IDs.

Default endpoint:

```text
http://127.0.0.1:18765/mcp
```

Override it when needed:

```bash
python3 scripts/install-agent-hooks.py \
  --harness auto \
  --mcp-url http://127.0.0.1:18765/mcp \
  --verify
```

If the MCP endpoint requires auth, edit `~/.config/ferrosa-memory/hooks/env` or export one of these before the harness starts:

```bash
export FERROSA_MEMORY_MCP_USER='ferrosa_user'
export FERROSA_MEMORY_MCP_PASSWORD='ferrosa_user'
# or:
export FERROSA_MEMORY_AUTH_HEADER='Basic <base64 user:password>'
```

Harness config behavior:

- Claude Code: patches `~/.claude/settings.json` with `SessionStart`, `UserPromptSubmit`, `Stop`, `SubagentStop`, and `PreCompact` hooks, with a timestamped backup.
- Hermes: patches `~/.hermes/config.yaml` only when the existing `hooks` block is empty, adding session-start, recall, and turn-finalization hooks with a timestamped backup.
- Codex: patches `~/.codex/hooks.json` with `SessionStart`, `UserPromptSubmit`, `Stop`, `SubagentStop`, and `PreCompact` hooks when that file is available or can be created, with a timestamped backup when modifying an existing file.

For a config-only dry run that still refreshes wrapper scripts but does not patch harness config files:

```bash
python3 scripts/install-agent-hooks.py --harness auto --dry-run --no-apply-config --verify
```

Manual merge snippets are always written:

```text
~/.config/ferrosa-memory/hooks/claude-settings-snippet.json
~/.config/ferrosa-memory/hooks/hermes-hooks-snippet.yaml
~/.config/ferrosa-memory/hooks/codex-hooks-snippet.json
```

Hook environment knobs live in:

```text
~/.config/ferrosa-memory/hooks/env
```

Important defaults:

```bash
export FERROSA_MEMORY_HOOK_TIMEOUT=${FERROSA_MEMORY_HOOK_TIMEOUT:-8}
export FERROSA_MEMORY_HOOK_SEARCH_LIMIT=${FERROSA_MEMORY_HOOK_SEARCH_LIMIT:-5}
export FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS=${FERROSA_MEMORY_HOOK_CAPTURE_SEGMENTS:-true}
export FERROSA_MEMORY_HOOK_EMBED_MISSING=${FERROSA_MEMORY_HOOK_EMBED_MISSING:-false}
```

The hook installer configures supported harness hook timeouts at 10 seconds by default. The hook script remains best-effort and exits zero on recoverable failures, but a local override can still lower or raise `FERROSA_MEMORY_HOOK_TIMEOUT`.

Hook safety rules:

- Hooks are best-effort and exit zero after logging failures to stderr.
- Hooks must not block session shutdown on long consolidation jobs.
- Hooks must not ingest secrets or full environment dumps.
- Hooks must be idempotent and safe to retry.
- Keep `FERROSA_MEMORY_HOOK_EMBED_MISSING=false` until an embedding provider is configured. Set it to `true` for higher semantic recall once Nomic/Ollama or another provider is healthy.

---

## Phase 13 — First useful memory examples

Once the stack and MCP client work, run these examples.

### Create an entity

```json
{
  "tool": "ingest",
  "arguments": {
    "entity_name": "Ferrosa Memory onboarding",
    "entity_type": "concept",
    "content": "Ferrosa Memory is configured as a local MCP memory service for this agent harness."
  }
}
```

### Retrieve it

```json
{
  "tool": "hybrid_search",
  "arguments": {
    "query": "Ferrosa Memory onboarding"
  }
}
```

Retrieval tools use `[retrieval] default_limit` when `limit`/`k` is omitted.
To reduce token usage at runtime, call:

```json
{
  "tool": "config",
  "arguments": {
    "retrieval_limit": 5
  }
}
```

### Record an evolving fact

```json
{
  "tool": "write_temporal_fact",
  "arguments": {
    "entity_id": "<entity UUID from ingest>",
    "fact_text": "Initial local onboarding completed successfully."
  }
}
```

### Check health/statistics

```json
{
  "tool": "get_stats",
  "arguments": {}
}
```

### Connect graph entities

The compact `edge` tool is the default way to insert typed graph edges from an agent. It writes through Ferrosa's graph API in the serving path and is readable through CQL-backed typed-edge APIs, graph queries, `explore_connections`, and `find_memory_chain`.

```json
{
  "tool": "edge",
  "arguments": {
    "session_id": "<session UUID>",
    "src_entity_id": "<source entity UUID>",
    "dst_entity_id": "<destination entity UUID>",
    "edge_type": "references",
    "weight": 0.75
  }
}
```

To verify graph and MCP traversal in one command against the default local stack:

```bash
bash scripts/smoke-18765.sh
```

---

## Phase 14 — Long-recall defaults and eval profile

For local 0.13 preview installs, verify the runtime config uses:

```toml
[retrieval]
default_limit = 10

[embeddings]
provider = "ollama"
model = "nomic-embed-text-v2-moe"
dimensions = 768

[eval]
retrieval_k = 25
```

`default_limit = 10` is the live agent default. It keeps normal memory turns compact. If a user reports token churn, lower it with the `config` MCP tool; if a benchmark needs wider recall, set the eval runner's candidate parameters instead of raising the live default.

The current best-known BRIGHT-Pro support-doc-closed MCP slice settings are:

```text
candidate_limit=50
fusion_profile=all
query_decomposition=llm
query_task=bright_pro
query_variant_limit=5
query_embed_variants=true
chunk_expansion=none
rerank=false
```

That profile measured alpha_nDCG `0.816`, NDCG `0.799`, aspect_recall `0.940`, and recall `0.796` on the 200-document biology support-closed slice. Treat these as preview slice numbers, not a full-corpus paper comparison.

Local judge/reranker settings:

```toml
[judge]
enabled = true
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen2.5-coder:7b"
timeout_seconds = 60
```

Live judge reranking is on by default when the configured local or remote model endpoint is available. If the model is absent or cold, retrieval still succeeds with fused baseline ranking and reports the failure in rerank diagnostics; set `enabled = false` on hosts where live judge calls are not desired. Judge failures or no-decisions should be recorded as abstentions (`"-"`), while agent/user feedback should use `+1` and `-1` scores so the reranker can learn by workspace and retrieval channel.

Useful eval commands:

```bash
scripts/run-official-evals.sh --self-test

FMEM_EVAL_QUERY_DECOMPOSITION=llm \
FMEM_EVAL_QUERY_EMBED_VARIANTS=true \
scripts/run-fusion-ablations.sh

scripts/start-bright-pro-full-load.sh
```

The full-corpus loader writes `heartbeat.json`, `progress.json`, and `load.log` under `diagnostics/eval-runs/...`. It is intentionally resumable and observable because full BRIGHT-Pro ingestion is large.

---

## Phase 15 — Troubleshooting checklist

| Symptom | First checks |
|---|---|
| `curl :18765` fails | Is the MCP container running? Is it host-networked or port-mapped? Is the config HTTP or HTTPS? |
| `ready` fails but `live` works | Check CQL contact points, auth, and node health. |
| `get_stats` times out | Check all three Ferrosa nodes, recent replay/OOM logs, and CQL read timeouts. |
| All tools return 504 immediately after restart | ANN index cold-load: Ferrosa rebuilds its vector index in memory before serving queries. With a large entity store this takes several minutes. Wait and retry. |
| `ingest` reports success but entities are missing on retrieval | Ferrosa drops role GRANTs on restart. Re-run `GRANT ALL PERMISSIONS ON KEYSPACE agent_memory TO ferrosa_user` before starting ferrosa-memory-mcp. |
| `/health` fails | `/health` is an alias of `/healthz/live`; check whether the listener is HTTP or HTTPS, host-networked or port-mapped, and whether a proxy/tunnel is intercepting the host/port. Prefer `127.0.0.1` for bind-specific local stacks. |
| Claude/Codex cannot see tools | Verify the harness MCP config path and restart the harness. |
| Hermes tools list is stale | Run `/reload-mcp` in a new session after fmem health is green. |
| Bridge container cannot reach `localhost:19042` | Use host networking for MCP or change CQL contact points to container-routable names. |
| Viz blank or unhelpful | Open `http://127.0.0.1:18766/viz`, wait for the query to finish, then use the detail view. |

Log commands:

```bash
docker compose logs --tail=100 node1 node2 node3 ferrosa-memory-mcp
# or
podman compose logs --tail=100 node1 node2 node3 ferrosa-memory-mcp
```

Never run destructive cleanup as part of troubleshooting unless the user explicitly asks for a reset.

---

## Completion criteria

Onboarding is complete when:

- Repos are cloned or updated from public remotes.
- Ferrosa Database node image exists.
- Ferrosa Memory stack is running.
- `/healthz/live`, `/healthz/ready`, and `/viz` work.
- At least one selected harness can list Ferrosa Memory MCP tools.
- `get_stats` succeeds.
- A small ingest + retrieval example succeeds.
- The user knows where data is persisted and how to stop/start without deleting volumes.

Safe stop/start commands:

```bash
cd ~/src/ferrosa-suite/ferrosa-memory
docker compose stop
docker compose start
# or
podman compose stop
podman compose start
```

Do **not** use `down -v` unless intentionally deleting data.
