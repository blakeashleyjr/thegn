---
name: thegn-house
description: Read when working inside a thegn worktree. Explains thegn's house tools (budget, fleet status, worktree status, spawning subtasks, requesting human attention) exposed over MCP, and the worktree/model conventions that apply.
---

# Working inside thegn

You are running as an embedded agent inside **thegn**, a terminal-native
git-**worktree** IDE. Each worktree is one task/branch. Your shell and file tools
run in this worktree; your model traffic is routed through thegn's proxy
(budget-capped and metered per worktree).

## House tools (provided by thegn over MCP)

When thegn is connected, these tools are available in addition to your
built-ins — prefer them for the situations below:

- **`check_my_budget`** `{ scope }` — token/cost budget + usage for a scope
  (e.g. `worktree:<path>`, `agent:<name>`, or `global`). Call it before starting
  expensive multi-step work, or if a model call is refused, to see remaining headroom.
- **`spawn_subtask`** `{ worktree, agent }` — ask thegn to start a sibling
  task in another worktree/pane. Use for parallelizable work that deserves its
  own branch, rather than doing everything in one session.
- **`request_human`** `{ worktree, reason }` — raise an attention alert to the
  human (lands in thegn's notification inbox). Use when you're blocked on a
  decision, need credentials/approval, or have finished and want review.

## House resources

- **`fleet://status`** — status of all worktrees/agents in the workspace.
- **`worktree://<id>/status`** — the cached diff + status for a worktree. Read it
  to see what's already changed before editing.

## Conventions

- Stay within this worktree; it maps to one branch/PR. Don't reach across worktrees
  except via `spawn_subtask`.
- Model traffic is metered — if you're about to do something large, check the
  budget first and prefer concise tool output.
- When your task is done or you need a human decision, call `request_human` so the
  user is notified rather than silently waiting.
