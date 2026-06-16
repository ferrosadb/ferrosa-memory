---
executive_summary: >
  Ferrosa Memory needs a native-feeling local installer that makes the default
  path private, reversible, and low-clutter for agent context. The installer
  should place binaries, configuration, data, logs, and runtime state under
  ~/.ferrosa by default, expose an idempotent setup wizard, and configure
  first-class MCP clients only after showing exact changes and receiving user
  approval.
---

# Native Installer And Onboarding

## Goals

- Fully local by default: no cloud service, no Docker requirement, no sudo.
- Single-user first: install under `~/.ferrosa` with optional system-wide mode.
- Native operations:
  - macOS: user LaunchAgent.
  - Linux: user systemd service.
- Private network exposure only: bind to localhost.
- Idempotent setup: running `ferrosa-memory setup` multiple times reconciles
  the running installation with the user's new choices.
- Low context clutter: configure tiered MCP tool exposure so normal agent
  sessions see only the recommended memory surface.
- Explicit consent before client config or hook writes.
- Reversible uninstall that preserves user data by default.

## Default Layout

```text
~/.ferrosa/
  bin/
    ferrosa
    ferrosa-memory
    ferrosa-memory-mcp
  config/
    ferrosa-memory.toml
    clients/
  data/
    ferrosa/
    memory/
  logs/
  run/
```

The default ports are:

- `18765`: local Ferrosa database.
- `18766`: Ferrosa Memory MCP/control/health endpoint.

If either port is already active during setup, prompt:

```text
Port 18765 is already in use.

Choose:
  [x] Auto-select next available port
  [ ] Enter port manually
  [ ] Abort setup
```

The selected ports are persisted in `~/.ferrosa/config/ferrosa-memory.toml`.

## System-Wide Mode

System-wide mode is opt-in:

```bash
ferrosa-memory setup --system
```

The wizard must ask where data should live:

```text
Data location:
  [x] Per-user: ~/.ferrosa/data
  [ ] System-wide: /var/lib/ferrosa-memory
```

System-wide installation may require elevated privileges for service and binary
placement, but client configuration remains per-user unless explicitly selected.

## Setup Reconciliation Model

`setup` is a reconciler, not a one-shot installer.

Each run should:

1. Discover current install state:
   - binary versions and paths
   - config paths and port assignments
   - service status
   - schema/migration status
   - client config entries
   - shell PATH state
   - hook state
2. Ask for desired state:
   - install scope
   - data location
   - port choices
   - encryption-at-rest option
   - global/project/both client scope
   - selected clients
   - tool profile per client
   - PATH update
   - hook installation
3. Build an apply plan with exact file/service changes.
4. Print the plan and config diffs.
5. Apply only after user confirmation.
6. Write backups for edited user config files.
7. Start or restart services only when necessary.
8. Run `doctor`-equivalent validation.

Repeated runs must update existing managed blocks rather than append duplicate
entries.

## Client Configuration

First-class clients:

- Claude Code
- Codex
- Claude Desktop
- Zed
- Hermes

The setup wizard asks scope:

```text
Configure MCP for:
  [x] Global user config
  [ ] Current project only
  [ ] Both
```

For every selected client, setup prints the exact planned change and asks:

```text
Apply MCP config changes? [y/N]
```

Backups are timestamped:

```text
~/.hermes/config.yaml.20260615-112233.bak
```

### Hermes

Hermes reads MCP config from `~/.hermes/config.yaml` under `mcp_servers`.
Ferrosa Memory should configure a local stdio server:

```yaml
mcp_servers:
  ferrosa-memory:
    command: "/Users/example/.ferrosa/bin/ferrosa-memory-mcp"
    args:
      - "--config"
      - "/Users/example/.ferrosa/config/ferrosa-memory.toml"
    enabled: true
    tools:
      include:
        - all_tools
        - ingest
        - search
        - chunk_ctx
        - check
        - feedback
        - stats
        - find
        - list
        - forget
```

Hermes supports per-server tool filtering with `tools.include`, so setup should
write a recommended include list by default and allow a custom checklist.

## Tool Tiering

Tool exposure must use progressive disclosure.

```text
Tool profile:
  [x] Recommended
      Search, ingest, recall, memory metrics, forget, safe task/session tools
  [ ] Full
      All tools, including graph/rules/admin/debug surfaces
  [ ] Custom
      Pick individual tools
```

Recommended tools should be enough for normal memory use while avoiding context
clutter. Full/admin surfaces remain reachable through explicit discovery rather
than default injection.

Clients with native filtering should receive an include list. Clients without
filtering rely on the MCP server's compact default tool list and `all_tools` for
progressive discovery.

## Hooks

Hooks are opt-in and independent from MCP client configuration:

```text
Install Ferrosa Memory hooks for prompt/context injection? [y/N]
```

Hook setup must be idempotent:

- update existing managed blocks
- preserve user-owned hook entries
- never duplicate commands
- show diffs before writing
- write backups

Hook output must stay compact and must not inject low-score or cross-session
garbage into unrelated workspaces.

## PATH Management

By default, setup offers to add `~/.ferrosa/bin` to the user's shell PATH.

It should:

- detect the active shell
- edit the right startup file only after preview and confirmation
- use a managed block marker
- avoid duplicate PATH entries
- print the command to activate PATH in the current shell

Example block:

```sh
# >>> ferrosa-memory >>>
export PATH="$HOME/.ferrosa/bin:$PATH"
# <<< ferrosa-memory <<<
```

## Encryption At Rest

Encryption at rest is optional:

```text
Encrypt local memory data at rest? [y/N]
```

The installer should preserve an abstraction for encryption even if the first
release only records the option and validates unsupported combinations.

Expected future key storage:

- macOS: Keychain.
- Linux desktop: Secret Service/libsecret.
- Linux headless: explicit passphrase or key file with a clear warning.

## Doctor

`ferrosa-memory doctor` should be the main support surface.

It verifies:

- `~/.ferrosa` directory layout
- binaries exist and match expected versions
- PATH contains `~/.ferrosa/bin`
- ports are free or bound by Ferrosa-owned services
- local database is running on localhost
- schema migrations are current
- MCP server starts
- selected clients are configured
- hooks are installed or intentionally absent

Output should be concise and actionable:

```text
Ferrosa Memory Doctor

✓ ~/.ferrosa exists
✓ ~/.ferrosa/bin is on PATH
✓ local database healthy on 127.0.0.1:18765
✓ schema version current
✗ Hermes configured but disabled
  Fix: ferrosa-memory clients configure hermes
```

## Uninstall

Default uninstall preserves user data:

```bash
ferrosa-memory uninstall
```

It should:

- stop the service
- remove LaunchAgent/systemd units
- remove managed MCP config entries after preview/confirmation
- remove managed hooks after preview/confirmation
- remove managed PATH blocks
- remove binaries from `~/.ferrosa/bin`
- preserve data, config, and logs by default

Final output:

```text
Ferrosa Memory uninstalled.

Your data was preserved:
  /Users/example/.ferrosa/data

To delete it later:
  rm -rf /Users/example/.ferrosa/data
```

Deleting data requires an explicit destructive flag and typed confirmation:

```bash
ferrosa-memory uninstall --delete-data
```

## Future Menu Bar App

The installer should leave room for a local control API so a later menu-bar app
can provide:

- service status
- start/stop/restart
- log shortcuts
- memory search
- memory edit/review
- questionable-memory approval/delete flows
- client configuration status

Candidate local API:

```text
GET    /health
GET    /status
GET    /metrics
GET    /clients
POST   /start
POST   /stop
GET    /memories/search
PATCH  /memories/:id
DELETE /memories/:id
```

## Implementation Phases

1. CLI command skeleton: `setup`, `doctor`, `uninstall`, `clients configure`,
   `hooks install`.
2. `~/.ferrosa` layout and config reconciliation.
3. Port detection and local service management.
4. Client config preview/apply with backups.
5. Tool profiles and client-specific filtering.
6. PATH reconciliation.
7. Hook reconciliation.
8. Doctor and uninstall validation.
9. Release installer polish and docs.
