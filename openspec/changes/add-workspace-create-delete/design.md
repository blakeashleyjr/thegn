# Design

## Create

`Action::NewWorkspace` switches from "open the switch palette" to a status-bar
prompt (`PromptMode::NewWorkspace`): keys accumulate into a buffer, Enter commits,
Esc cancels. `create_workspace_from_input`:

- URL (`util::is_url`) → clone into `workspaces_dir()/<repo-name>` (skip if already
  cloned); path → validate it exists and is a git repo.
- `db.put_workspace` + `db.touch_repo`, then `session.switch_to_workspace`.
- Clear errors on empty input / non-existent path / non-repo / clone failure.

## Delete

`Db::delete_workspace(repo_path) -> u32` removes only the `workspaces` row
(worktrees on disk untouched) and returns the count of worktrees still registered
under the repo (orphan warning). `Action::DeleteWorkspace` (bind `Alt Shift X`,
vim `Space X`; sidebar `d`/`x` on a workspace row; palette item) opens a
`DeleteConfirm` prompt; on confirm it deletes, switches to the first remaining
workspace (or creates an empty home when none remain), and reports orphan count.

## State & rendering

The event loop gains `prompt: Option<String>` + `prompt_mode` handled **before**
palette/keymap dispatch; the status bar renders the prompt buffer while active.
`Enter` on a workspace row still switches (never deletes).

## Persistence / compat

`put_workspace` is an idempotent upsert; `delete_workspace` is non-destructive; no
schema migration (no new columns). Legacy CLI `new-workspace` path is unaffected.

## Invariants

Core logic (`delete_workspace`, `is_url`) is unit-tested against the 95% gate;
no loop blocking; no polling added.
