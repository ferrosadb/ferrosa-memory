# Ferrosa Memory Skills

Portable agent **skills** (slash-command playbooks) for getting the most out of Ferrosa
Memory. Each skill is a directory containing a `SKILL.md` with YAML frontmatter
(`name`, `description`) and a step-by-step body. They work with Claude Code (and any
agent that loads `SKILL.md`-style skills, e.g. Pi).

## Installation

The [`install-memory.sh`](../docs/install-memory.sh) installer copies these into your
agent skill directory (`~/.claude/skills/` by default) automatically:

```sh
curl -fsSL https://www.ferrosa.ai/install-memory.sh | bash
```

Skip skill installation with `--no-skills`, or point it elsewhere with
`--skills-dir <path>`.

To install manually from a source checkout:

```sh
cp -R skills/* ~/.claude/skills/
```

Then invoke a skill in your agent with `/<name>` (e.g. `/set-foresight`).

## Skills

### Using the memory (work with just the Ferrosa Memory MCP)

| Skill | What it does |
|-------|--------------|
| [`memory-session-start`](memory-session-start/SKILL.md) | Restore working context (`check_intentions` + `hybrid_search`) at session start, before reading files. |
| [`set-foresight`](set-foresight/SKILL.md) | Declare a time-bounded fact / temporary constraint (`valid_from` / `valid_until`) so search surfaces it only while valid. |
| [`consolidate-wrapup`](consolidate-wrapup/SKILL.md) | Force a consolidation pass (edges, scenes, profiles) at the end of a session. |

### Managing follow-up work (need the [`forge`](https://github.com/ferrosadb/forge) companion task board)

| Skill | What it does |
|-------|--------------|
| [`defer`](defer/SKILL.md) | Capture surfaced-but-not-done work so it isn't lost. |
| [`whats-next`](whats-next/SKILL.md) | Answer "what's next?" by ranking open deferred work for the repo. |
| [`roadmap`](roadmap/SKILL.md) | Synthesize a Now / Next / Later roadmap from deferred work + live code TODOs + unshipped branches. |

The `defer` / `whats-next` / `roadmap` trio writes to the `forge` task board; install
[`forge`](https://github.com/ferrosadb/forge) (the open-source companion CLI/MCP tool) to
use them. The memory-usage skills need only the Ferrosa Memory MCP server.

## Writing your own

A skill is just a directory with a `SKILL.md`:

```markdown
---
name: my-skill
description: One line describing when to use it (used for relevance matching).
---

# Title

Steps the agent should follow...
```

Reference supporting files with a relative path (see `defer/references/`). Keep the
`description` action-oriented — it's what the agent matches against to decide relevance.
