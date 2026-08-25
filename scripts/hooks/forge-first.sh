#!/usr/bin/env bash
# forge-first.sh — PreToolUse(Bash) hook for Codex
#
# Nudges Codex toward Forge MCP tools when raw shell commands look like
# repo inspection, build/test summarization, or code search that Forge
# handles with compact structured output instead of large raw text.
#
# Installed by ferrosa-memory setup.sh / install-agent-hooks.py.
# Exit codes: 0 = allow (with optional system message), 2 = block.
# Set CODEX_FORGE_FIRST_BLOCK=1 to hard-block the worst patterns.
set -euo pipefail

payload="$(cat)"

cmd=""
if command -v jq >/dev/null 2>&1; then
  cmd="$(printf '%s' "$payload" | jq -r '.tool_input.command // .command // ""' 2>/dev/null || true)"
fi
if [ -z "$cmd" ]; then
  cmd="$(printf '%s' "$payload" | sed -n 's/.*"command"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' 2>/dev/null || true)"
fi

if [ -z "$cmd" ]; then
  exit 0
fi

norm="$(printf '%s' "$cmd" | tr '\n' ' ' | tr -s ' ')"

block_pattern() {
  local suggestion="$1"
  local pattern="$2"
  if printf '%s' "$norm" | grep -Eq "$pattern"; then
    if [ "${CODEX_FORGE_FIRST_BLOCK:-0}" = "1" ]; then
      echo "BLOCKED: This command produces large raw output that should use a Forge MCP tool instead." >&2
      echo "$suggestion" >&2
      echo "" >&2
      echo "Set CODEX_FORGE_FIRST_BLOCK=0 to allow with a warning, or use the Forge tool directly." >&2
      exit 2
    else
      echo "{\"systemMessage\":\"Forge-first reminder: $suggestion Use the Forge MCP tool instead of piping raw output. Set CODEX_FORGE_FIRST_BLOCK=1 to hard-block this pattern.\"}"
      exit 0
    fi
  fi
}

remind_pattern() {
  local suggestion="$1"
  local pattern="$2"
  if printf '%s' "$norm" | grep -Eq "$pattern"; then
    echo "{\"systemMessage\":\"Forge-first: $suggestion A Forge MCP tool can do this with compact structured output instead of raw shell.\"}"
    exit 0
  fi
}

# 1. cargo test/build/clippy piped through head/tail/grep
block_pattern \
  "Use 'test_summary' for test results or 'cargo' for build/clippy output." \
  'cargo[[:space:]]+(test|build|check|clippy)[[:space:]].*\|[[:space:]]*(head|tail|grep)'

# 2. git log/diff/show piped through head/tail
block_pattern \
  "Use 'git_summary' for repo status/commits or 'diff_filter' for diffs." \
  'git[[:space:]]+(log|diff|show)[[:space:]].*\|[[:space:]]*(head|tail|grep)'

# 3. git log with huge --count limits
remind_pattern \
  "Use 'git_summary' for recent commit history instead of 'git log' with a large limit." \
  'git[[:space:]]+log[[:space:]].*-(n|[0-9]{3,})'

# 4. Full cargo test (not a single filtered test)
remind_pattern \
  "For full test suite runs, prefer 'test_summary' — it runs the tests and returns a compact result instead of raw output." \
  'cargo[[:space:]]+test[[:space:]]+(-p[[:space:]]+[a-z])?[[:space:]]*$'

# 5. cargo test --workspace
remind_pattern \
  "Use 'test_summary' for workspace-wide test runs — it returns compact structured results." \
  'cargo[[:space:]]+test[[:space:]].*--workspace'

# 6. cargo build/check/clippy without piping
remind_pattern \
  "Consider 'cargo' for build/check/clippy — it returns compact structured output." \
  'cargo[[:space:]]+(build|check|clippy)[[:space:]]'

# 7. cargo clippy piped through grep
block_pattern \
  "Use 'lint_dedup' for deduplicated clippy warnings instead of grepping raw output." \
  'cargo[[:space:]]+clippy[[:space:]].*\|[[:space:]]*grep'

# 8. rg/grep for code structure
remind_pattern \
  "For understanding module structure, use 'module_outline' instead of searching for fn/struct/impl." \
  '(rg|grep)[[:space:]].*(fn[[:space:]]|struct[[:space:]]|impl[[:space:]]|enum[[:space:]])'

# 9. cat/sed on source files to read code
remind_pattern \
  "For extracting a specific function/struct, use 'excerpt'. For module shape, use 'module_outline'." \
  '(cat|sed[[:space:]]+-n)[[:space:]].*\.(rs|py|ts|go|ex|ts)$'

# 10. cargo tree / dependency inspection
remind_pattern \
  "Use 'dependency_tree' for dependency graph instead of 'cargo tree'." \
  'cargo[[:space:]]+tree'

# 11. cargo metadata
remind_pattern \
  "Use 'project_summary' for project/dependency inventory instead of 'cargo metadata'." \
  'cargo[[:space:]]+metadata'

# 12. mix test piped through head/tail/grep (Elixir)
block_pattern \
  "Use 'mix_test' for Elixir test results instead of piping raw mix test output." \
  'mix[[:space:]]+test[[:space:]].*\|[[:space:]]*(head|tail|grep)'

# 13. mix test full suite
remind_pattern \
  "Consider 'mix_test' for full Elixir test runs." \
  'mix[[:space:]]+test[[:space:]]*$'

# 14. grep for TODO/FIXME
remind_pattern \
  "Use 'todo_extract' for a structured TODO/FIXME inventory." \
  '(rg|grep)[[:space:]].*(TODO|FIXME|HACK|XXX)'

# 15. cargo fmt --check
remind_pattern \
  "Use 'format_fix' for format checking." \
  'cargo[[:space:]]+fmt[[:space:]].*--check'

exit 0