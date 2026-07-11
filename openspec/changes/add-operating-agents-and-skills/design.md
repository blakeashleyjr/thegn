# Design

## Operating-agent definitions (core, declarative)

A definition is a markdown file with front-matter, resolved from
`.thegn/agents/*.md` (repo) then `$XDG_CONFIG_HOME/thegn/agents/*.md`
(global), repo winning — the same layering as other config:

```markdown
---
id: "researcher"
title: "Read-only research"
tools: ["fs_read", "fs_search", "fetch"] # the ALLOWED set (bound)
model: "opus" # optional per-agent model
---

You are a read-only research agent. You never modify files…
```

Parsed into `OperatingAgent { id, title, tools: BTreeSet<ToolId>, model,
system_prompt }` in `thegn-core::operating_agent` — **pure + unit-tested**:
front-matter parse, tool-set parse, missing-fields defaults, precedence
(repo overrides global by `id`). Ship three built-in defaults:

- `executor` — `{fs_read, fs_write, fs_patch, shell, fetch}`
- `researcher` — `{fs_read, fs_search, fetch}` (no mutation)
- `planner` — `{fs_read, fs_search, plan_write}` (writes only plan files)

Users override or add by dropping files in the agents dir.

## Tool restriction (enforcement at the AR/ACP seam)

The agent's declared `tools` set is the **upper bound** on what it may call. At
the gateway/ACP tool-filtering seam (AR 541–551 / R 695), the advertised tool
list handed to the agent is intersected with the operating agent's set, and any
tool call outside the set is refused. This composes with the sandbox policy
engine (which decides allow/deny/ask _within_ an allowed tool) and the bouncer
(the hard boundary) — three concentric gates: **tool-set bound → policy decision
→ container seal**.

## Skills (declarative workflows, injected by the gateway)

A skill is a named, reusable workflow resolved from `.thegn/skills/<name>/`
(and the global dir): a `SKILL.md` describing when/how to use it plus optional
parameters. Skills are **orthogonal to agents** — any agent may invoke a skill.
They are surfaced to harnesses by the gateway's capability injection (AR
541–543): the skill becomes an advertised tool/command, **translated per harness**
(AR 570) so one definition works across Claude Code / Codex / OpenCode.
`thegn-core::skill` parses and validates a skill definition (pure, tested);
the injection/translation itself is AR's mechanism (a dependency, not built here).

## Cache-aware injection (constraint, not mechanism)

Because injected blocks can bust prompt caching (the known AR tension), skill/
agent system-prompt blocks must be injectable with a **stable prefix ordering** and
a cache breakpoint after the injected block. This change only _requires_ that its
definitions carry stable, ordering-friendly content; the cache-aware placement is
AR's job.

## Invariants

- **Event loop**: definitions load off-loop (config resolution / fs), reported
  over the existing channel; no blocking read on the render loop, no new timer.
- **Render**: an operating-agent picker (which agent is active for a worktree)
  reuses the palette/statusbar chip — chrome `dirty` repaint, not a pane
  recompose. render_plan invariants unchanged.
- **State**: no `user_version` bump — agents/skills are declarative files.
- **Additivity**: no agent configured / no skills present → the AI-free shell is
  unaffected; core parsing has no proxy/tokio dependency.

## Alternatives considered

- **One general agent with all tools + prompting** — rejected: prompting alone
  doesn't _prevent_ a research agent from writing; a hard tool-set bound does.
- **Skills as sub-agents** — rejected: Forge's insight is that skills are
  orthogonal _workflows_, invokable by any agent, not agents themselves.
- **Baking the three roles in as code** — rejected: declarative files let users
  add/override roles without a rebuild (the low-barrier customization win).
