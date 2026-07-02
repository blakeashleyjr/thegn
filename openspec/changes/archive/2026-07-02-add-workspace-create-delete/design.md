# Design

This documents the **shipped** design (the original source plan proposed an
invented `PromptMode`/`Db::delete_workspace->u32`/`util::is_url` shape that was
superseded during implementation).

## Create

`Action::NewWorkspace` (bind `Alt W`, vim `Space W`, palette `new-workspace`) is
discovery-first:

- It calls `repo::discover_repos(cfg)` (honors `[general] repo_roots` +
  `repo_scan_depth`; `repo_roots` defaults to `[workspaces_dir]`). If any repos
  are found it opens `menu::new_workspace_menu` — a `MenuOverlay`
  (`MenuKindTag::NewWorkspace`) listing each discovered repo (capped, with a
  "N found" note when truncated) plus an **"enter a path or URL…"** item.
- Picking a repo resolves to `MenuChoice::CreateWorkspaceFromPath(path)`; the
  "enter a path or URL…" item resolves to `MenuChoice::NewWorkspacePrompt`,
  which opens the typed-input `InputOverlay` (`HostInputKind::NewWorkspace`).
- With no repos discovered, the action opens the typed-input overlay directly.

Both the picked-path and typed-input routes funnel through
`create_workspace_from_input_with_config`:

- Git URL (`looks_like_git_url`: `http(s)://`, `ssh://`, `git://`, `git@`) →
  clone into `workspaces_dir/<repo-name>` via the scrubbed `util::git_cmd` (skip
  if already present); path → `expand_tilde`, resolve, validate it is a dir.
- If the resolved root is a git repo → `put_workspace` + `touch_repo` +
  `session.switch_to_workspace`, returning `WorkspaceResolution::Repo`.
- If it is a directory but not a git repo → `WorkspaceResolution::NotARepo`,
  which offers `menu::init_git_menu` (`git init`, then create).
- Empty / non-existent path / failed clone → a status-bar error, no registration.

## Delete

`Action::DeleteWorkspace` (palette `delete-workspace`; sidebar `RemoveWorkspace`
via the row menu / `D` key) opens `menu::delete_workspace_menu` when `[ui]
confirm_delete_workspace` is set. It has no default keybind — `Alt X` / `Space X`
belong to close-worktree — so it is palette-driven + user-bindable (an
`ActionSpec` was added so it appears in the palette; the previously dead
`Alt Shift X` / `Space X` → DeleteWorkspace binds, shadowed by close-worktree,
were removed). The
confirm menu offers three choices resolving to `MenuChoice::ConfirmDeleteWorkspace
{ keep_files }` or `Dismiss`:

- **delete worktrees from disk** (default, destructive) — removes each branch
  worktree dir (the home checkout is never deleted).
- **keep files on disk** (non-destructive) — leaves every worktree dir intact.
- **cancel**.

`remove_workspace` reads the workspace's branch-worktree dirs
(`workspace_worktree_dirs`) _before_ pruning, then `remove_workspace_with_db`
prunes the DB (`del_worktrees_for_repo` + `del_workspace` + slug + active-pointer)
and closes the live groups (never touching disk). Removing the active workspace
switches to the first remaining workspace, or empties the session
(`session.id`/`worktrees` cleared) when none remain. The **keep-files** status
reports the count of worktrees that survive on disk (the orphan warning); the
DB layer keeps the simple `del_workspace`/`del_worktrees_for_repo` methods (no
count-returning variant is needed).

## State & rendering

Typed input reuses `menu::InputOverlay` (keys accumulate, Enter submits, Esc
cancels; rendered as a centered layer). The discovery picker and delete confirm
reuse `menu::MenuOverlay` (j/k nav, hotkeys, Enter picks, Esc/q cancels). No new
prompt state machine is introduced. `Enter` on a workspace sidebar row still
switches (never deletes).

## Persistence / compat

`put_workspace` is an idempotent upsert; `del_workspace` is non-destructive at
the disk level; no schema migration (no new columns). The legacy CLI
`new-workspace` path is unaffected.

## Invariants

Core logic (`discover_repos`, `del_workspace`, the switch/empty fallback) is
unit-tested; host-side orphan counting and the discovery menu are unit-tested in
the host crate. No loop blocking; no polling added.
