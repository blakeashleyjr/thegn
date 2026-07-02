# Add workspace create & delete workflow

## Summary

Give the native host a full workspace lifecycle in-shell: a **create** flow
(discover repos under the configured roots, or type a path / git URL → validate
or clone → register → switch), and a non-destructive-capable **delete** flow
(confirm → prune the DB registration → switch to the next workspace or an empty
home). This reconciles the OpenSpec change with the shipped implementation and
closes two gaps: surfacing auto-discovered repos in the create picker, and
reporting orphaned worktrees when a workspace is removed but its files are kept.

Source plan: `docs/superpowers/plans/workspace-create-delete-workflow.md` (kept
for history; the shipped design diverges from it — see design.md).

## Impact

- **C** (Workspaces) — items 29 (add repo as workspace), 30 (remove workspace,
  non-destructive), 31 (auto-discover under root dir).
- Extends the **workspace** capability. Reuses `session::switch_to_workspace`,
  `Db::put_workspace`/`workspaces`/`del_workspace`/`del_worktrees_for_repo`,
  `repo::discover_repos`, and the host `InputOverlay`/`MenuOverlay` components.

## Rationale

The DB layer, the `Action::NewWorkspace`/`DeleteWorkspace` actions, the
`InputOverlay` typed-path/URL create flow (with clone + `git init` offer), and
the `delete_workspace_menu` confirm all already ship. The remaining work is
reachability + UX + reporting: `Action::DeleteWorkspace` had no palette entry and
its `Alt Shift X` / `Space X` binds were dead (shadowed by close-worktree), so it
was only reachable from the sidebar; the create action does not yet surface repos
auto-discovered under `repo_roots`; and the keep-files delete path does not tell
the user how many worktrees remain on disk. Deletion of the DB registration is
non-destructive; the destructive "delete worktrees from disk" option is retained
as an explicit, confirmed choice.

## Non-goals

- Non-git-directory workspaces (item 37) and workspace icon/color labels (item 39) — out of scope here.
- A full widget system — the flow reuses the existing `InputOverlay` (typed
  input) and `MenuOverlay` (discovery picker + delete confirm) components.
