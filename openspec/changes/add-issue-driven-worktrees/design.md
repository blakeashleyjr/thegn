# Design

## The action (host)

The My Work panel (`panel/sections/my_work.rs`, over `WorkRow`) gains a "start"
action on a selected issue that runs a pipeline:

1. **Branch/worktree** — derive a branch from the issue via
   `worktree::branch_name` (honoring `session_name_template`, e.g.
   `{identifier}-{slug}`), create it with `add_checked`, and add+focus a
   `WorktreeGroup` via `session::add_group`.
2. **Bind** — record the issue↔worktree link in the existing `issue_links` table
   so the panel's linked-worktree marker (`◈`) lights up and the binding survives
   restart.
3. **Agent (optional)** — if `auto_launch_agent` is on, build a `launch_spec` and
   spawn the agent as a visible pane, seeding the issue's title/body into the
   agent's initial context (via the pane env / initial prompt path).

`auto_create_worktree` gates whether the action creates a worktree or just binds
to an existing one; `auto_launch_agent` gates step 3.

## Context seeding (agent, additive)

The issue title/body/URL are passed to the agent as initial context (env vars the
pane firewall already exports, plus the launch spec's initial prompt). This is the
only AI-touching part and is skipped entirely when no agent runs.

## Invariants

- **Event loop**: worktree creation + clone-like git work runs off-loop
  (spawn_blocking), handed back over the channel + `TerminalWaker`; agent spawn is
  the existing pane-spawn path. No polling timer, no blocking git on the loop.
- **Render**: adding/focusing the worktree tab is a chrome `dirty` repaint + normal
  tab activation. render_plan invariants unchanged.
- **State**: no `user_version` bump — reuse `issue_links`.
- **Additivity**: issue→worktree is shell-level and works with no agent; the
  agent-launch + seeding is the additive AI layer, gated by config.

## Alternatives considered

- **A new issue↔worktree table** — rejected; `issue_links` already models exactly
  this worktree-scoped binding.
- **Always launching an agent** — rejected; the worktree-creation half must stand
  alone for the AI-free shell, so the agent step is gated.
- **Seeding context by scraping the issue into a file** — the env/initial-prompt
  path is cleaner and matches how the pane firewall already injects context.
