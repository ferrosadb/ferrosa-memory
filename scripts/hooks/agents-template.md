# AGENTS.md — global agent instructions for Ferrosa Memory users

## Forge-first: prefer MCP tools over raw shell output

Forge and ferrosa-memory MCP servers are configured and approved by the
ferrosa-memory installer. Use them before raw shell for repo inspection,
build/test summaries, and code understanding. Raw shell output (git logs,
cargo test output, grep/find results) is the single biggest token waster
in this environment.

### Decision table — try the Forge tool FIRST

| Task | Use this Forge tool | Instead of |
|------|---------------------|------------|
| Repo overview / dependency inventory | `project_summary` | `cargo metadata`, `ls` |
| Understand module structure | `module_outline` | `rg "fn \|struct \|impl "` |
| Extract one symbol's source | `excerpt` | `cat file.rs`, `sed -n` |
| Dependency graph / coupling | `dependency_tree` | `cargo tree` |
| Git status / recent commits | `git_summary` | `git log --oneline -50` |
| Diff between branches/files | `diff_filter` | `git diff \| head -200` |
| Run tests and get compact result | `test_summary` | `cargo test 2>&1 \| tail` |
| Run cargo build/check/clippy | `cargo` | `cargo build 2>&1 \| head` |
| Lint warnings deduped | `lint_dedup` | `cargo clippy 2>&1 \| grep` |
| Format check / fix | `format_fix` | `cargo fmt --check` |
| Architecture / coupling analysis | `dsm` | manual dependency tracing |
| Elixir mix compile/test/format | `mix_compile` / `mix_test` / `mix_format_check` | `mix test 2>&1` |
| Build log distillation | `log_distill` | `cat build.log`, `grep ERROR` |
| Find dead code | `dead_code` | manual code tracing |
| API contract diff | `api_contract_diff` | manual diff inspection |
| Secret scan | `secret_scan` | `grep -r "AKIA\|ghp_"` |
| TODO/FIXME inventory | `todo_extract` | `grep -rn "TODO"` |

### Rules

1. Before running raw shell for repo inspection, check the table above.
   If a Forge tool covers the task, call it first. Fall back to raw shell
   only if the Forge tool's output is insufficient.
2. Never pipe large build/test output through `head`/`tail`/`grep` to
   "summarize" it. Use `test_summary`, `log_distill`, or `cargo` instead.
3. For single targeted test runs (e.g. `cargo test -p crate module::test`),
   raw shell is fine — Forge's `test_summary` is for full-suite runs.
4. For actual deployment/infra commands (`docker`, `fly`, `curl`, `ps`),
   use raw shell — Forge doesn't cover those.
5. ferrosa-memory: search for prior context with `hybrid_search` before
   starting a task that may have been discussed before. If results are
   empty, ingest what you learn with `smart_ingest`.

### Why this matters

Audit of agent sessions showed the majority of sessions with 8+ shell-heavy
commands had zero Forge calls. Forge distillation can replace most
repo-inspection shell calls with compact structured JSON, cutting token
usage dramatically.