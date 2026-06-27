# Add workspace create & delete workflow

## Summary

Give the native host a real workspace **create** flow (enter a path or git URL →
validate/clone → register → switch) and a **delete** flow (confirm → remove only
the DB registration, warn on orphaned worktrees → switch to the next workspace).
Today `NewWorkspace` just opens the switch palette and there is no delete at all.

Source plan: `docs/superpowers/plans/workspace-create-delete-workflow.md`.

## Impact

- **C** (Workspaces) — items 29 (add repo as workspace), 30 (remove workspace,
  non-destructive), 31 (auto-discover under root dir, deferred).
- Extends the **workspace** capability and uses existing `session::switch_to_workspace`
  - `Db::put_workspace`/`workspaces`.

## Rationale

The DB layer (`put_workspace`, `workspaces`, `slug_for_repo`) and switch flow
already exist; the gaps are the native-host UX: an inline create prompt, a
`delete_workspace` DB method + `DeleteWorkspace` action, and confirmation +
orphaned-worktree handling. Deletion must be non-destructive — worktrees on disk
survive.

## Non-goals

- Auto-discovering repos under configured root dirs (item 31, deferred).
- A full widget system — the prompt uses the status-bar prompt pattern.
