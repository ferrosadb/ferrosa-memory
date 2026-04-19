# LSP-Based Code Indexing

## Problem

The forge `ingest` pipeline extracts code entities at module/crate granularity only. Function signatures, struct definitions, trait impls, and TODO comments are not indexed. Agents fall back to expensive grep+read cycles that burn tokens.

See: `/Users/bkearns/src/ferrosa/ferrosa-memory-indexing-gap.md`

## Solution

Use the Language Server Protocol (LSP) to extract fine-grained code symbols during `frg ingest`. LSP is language-agnostic — the same client code works for Rust, Python, Go, TypeScript, Elixir, etc.

## Architecture

```
frg ingest --cql localhost:19042 /path/to/project
    |
    v
[1. Detect language(s)]  -- Cargo.toml? mix.exs? go.mod? package.json?
    |
    v
[2. Find/start LSP]      -- rust-analyzer, pyright, gopls, etc.
    |                        (prompt user to install if missing)
    v
[3. Walk source files]
    |
    v
[4. textDocument/documentSymbol]  -- returns symbol tree per file
    |                                fn, struct, enum, trait, class, method, etc.
    v
[5. Emit entities + edges]  -- entity per symbol, contains edges from module
    |
    v
[6. Load into ferrosa-memory via CQL]
```

## Entity Types Produced

| Entity Type | Source | Example Name | Context Snippet |
|-------------|--------|-------------|-----------------|
| `function` | LSP SymbolKind::Function | `read_one_replica` | `pub async fn read_one_replica(coordinator: &Node, key: &[u8], cl: CL) -> Result<Row>` @ `ferrosa-cluster/src/coordinator/read.rs:552` |
| `struct` | LSP SymbolKind::Struct | `ClusterInvite` | `pub struct ClusterInvite { host_id: Uuid, seeds: Vec<SocketAddr>, epoch: u64 }` @ `ferrosa-cluster/src/types.rs:45` |
| `trait` | LSP SymbolKind::Interface | `Storage` | `pub trait Storage: Send + Sync { ... }` @ `ferrosa-memory-core/src/storage.rs:14` |
| `enum` | LSP SymbolKind::Enum | `MemoryState` | `pub enum MemoryState { Active, Dormant, Silent, Unavailable }` @ `types.rs:124` |
| `method` | LSP SymbolKind::Method | `IntentionStore::check` | `pub fn check(&mut self, context: &str, repo: &str) -> Vec<&Intention>` @ `intention.rs:111` |
| `todo` | Regex (post-LSP) | `TODO: decode mutation.row` | `// TODO: decode mutation.row as a ferrosa_sstable::types::Row` @ `streaming/receiver.rs:88` |

## Edge Types

| Edge Type | Meaning | Source |
|-----------|---------|--------|
| `contains` | Module contains function/struct | LSP symbol hierarchy |
| `implements` | Struct implements trait | LSP (if available) or regex `impl Trait for Struct` |
| `calls` | Function calls function | Existing `use` analysis + LSP references (optional) |

## LSP Client Implementation

### Minimal LSP session

Skilltools needs a lightweight LSP client that:
1. Spawns the LSP server as a subprocess (stdio transport)
2. Sends `initialize` with the project root
3. Sends `initialized` notification
4. For each source file: `textDocument/didOpen` + `textDocument/documentSymbol`
5. Sends `shutdown` + `exit`

No need for full LSP — just the symbol extraction calls. This is ~200 lines of JSON-RPC over stdin/stdout.

### LSP Binary Detection

For each language, check PATH for the standard LSP binary:

| Language | LSP Binary | Install Command |
|----------|-----------|-----------------|
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` |
| Python | `pyright` | `pip install pyright` or `npm install -g pyright` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| TypeScript/JS | `typescript-language-server` | `npm install -g typescript-language-server` |
| Elixir | `elixir-ls` | `mix escript.install hex elixir_ls` |
| C/C++ | `clangd` | `brew install llvm` / system package manager |

### User Prompt for Installation

When `op-init` or `ingest` detects a language but can't find its LSP:

```
[forge] Detected Rust project (Cargo.toml found)
[forge] rust-analyzer not found in PATH
[forge] Install it for function-level code indexing:
[forge]   rustup component add rust-analyzer
[forge] Without it, ingest will use module-level extraction only.
```

This message goes to stdout so the user sees it in their terminal. The ingest continues with degraded (module-level) extraction rather than failing.

## op-init Integration

The `op-init` skill already detects project type. Add LSP detection to its output:

1. Detect language from project files
2. Check if LSP binary exists in PATH
3. If missing, include install instruction in the op-init output
4. If present, note it in the project config so `ingest` knows to use it

## Implementation Plan

### Phase 1: LSP client library (in forge)
- New crate: `forge/crates/lsp-client/`
- Minimal JSON-RPC stdio client
- `initialize` / `documentSymbol` / `shutdown` lifecycle
- LSP binary detection and install prompting

### Phase 2: Integrate with ingest extractor
- After module-level extraction, run LSP symbol pass
- For each `.rs` file, call `documentSymbol` and emit function/struct/trait entities
- Add `contains` edges from module entity to symbol entities
- Extract TODO/FIXME via regex (LSP doesn't cover comments)

### Phase 3: op-init integration
- Add language detection → LSP check → install prompt to op-init output
- Save detected LSP config to `.claude/project.json` or similar

### Phase 4: Multi-language
- Add pyright, gopls, typescript-language-server support
- Same client code, different binary + initialization params

## Token Savings Estimate

Current: Agent does ~5-10 grep+read cycles to find a function (est. 2000-5000 tokens per lookup).
With LSP indexing: Agent does 1 `retrieve_entities` or `hybrid_search` call (est. 200-500 tokens).

For a session that looks up 20 code locations, that's ~40k-90k tokens saved.
