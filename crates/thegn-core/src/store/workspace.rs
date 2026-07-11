//! The **workspace/session** seam: registered repos & workspaces,
//! thegn-managed worktrees (positions, sandbox/env/agent bindings), folders,
//! saved layouts, tab groups + the persisted session (active tab/workspace),
//! sidebar UI state, pins, palette frecency, and named terminals. git is the
//! source of truth for worktrees on disk; this is the cache + resurrection layer.

use crate::models::{WorkspaceRow, WorktreeRow};
use anyhow::Result;

/// Object-safe (`&self` + concrete args), so `&dyn WorkspaceStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait WorkspaceStore {
    fn touch_repo(&self, path: &str, name: &str) -> Result<()>;

    fn recent_repos(&self, limit: i64) -> Result<Vec<String>>;

    fn known_repos(&self) -> Result<Vec<String>>;

    /// Whether thegn already knows this repo (registered, or in recents).
    fn is_known_repo(&self, repo_path: &str) -> Result<bool>;

    /// Record (or refresh) a registered workspace. Keyed by path — all
    /// workspaces share the one UI session. `kind` is `"repo"` (a git repo) or
    /// `"dir"` (a plain non-git directory); it is set only on first insert, so a
    /// later refresh never downgrades a known workspace's kind.
    fn put_workspace(&self, repo_path: &str, name: &str, kind: &str) -> Result<()>;

    /// A stable, globally-unique slug for a repo (the prefix of all its tabs).
    /// Reuses the previously-assigned slug; otherwise takes `base`, suffixing
    /// `-2`, `-3`, … on collision with a *different* repo, then persists it.
    /// Two repos with the same basename therefore get distinct tab namespaces.
    fn slug_for_repo(&self, repo_path: &str, base: &str) -> Result<String>;

    /// Drop a repo's stable sidebar slug so a removed workspace can't reclaim a
    /// stale slug if it is reopened later.
    fn del_repo_slug(&self, repo_path: &str) -> Result<()>;

    /// Forget a whole workspace (no disk side effects). Removes the
    /// `workspaces` row so the sidebar stops listing it. The worktree files on
    /// disk are intentionally left untouched.
    fn del_workspace(&self, repo_path: &str) -> Result<()>;

    /// All registered repos (for the sidebar / `list`), in manual `position`
    /// order (seeded from recency at the v16 migration; reorderable via
    /// `swap_workspace_positions`). The `last_active DESC` tie-break keeps the
    /// order deterministic if any row's position is somehow NULL.
    fn workspaces(&self) -> Result<Vec<WorkspaceRow>>;

    /// Swap the persisted sort positions of two workspaces (by repo_path). The
    /// workspace analogue of `swap_worktree_positions`: the sidebar's manual
    /// workspace reorder (Ctrl+Alt+↑/↓) picks two adjacent workspaces and this
    /// exchanges their `position` so the new order survives restart.
    fn swap_workspace_positions(&self, a: &str, b: &str) -> Result<()>;

    /// Set one workspace's persisted sort position (repo_path key).
    fn set_workspace_position(&self, repo_path: &str, position: i64) -> Result<()>;

    /// Record a worktree. `location` is the remote descriptor (JSON) for a remote
    /// worktree, or `None`/empty for an ordinary on-host one.
    fn put_worktree(
        &self,
        tab: &str,
        root: &str,
        wt: &str,
        branch: &str,
        location: Option<&str>,
        folder_id: Option<i64>,
    ) -> Result<()>;

    /// The remote-location descriptor for a worktree (None/empty = local).
    fn location_for(&self, wt: &str) -> Result<Option<String>>;

    /// The (local) repo root recorded for a worktree — needed for the per-repo
    /// `.thegn` overlay when the worktree itself lives remote.
    fn repo_root_for(&self, wt: &str) -> Result<Option<String>>;

    /// The recorded agent for a worktree (for `pick-agent --resume` on restart).
    fn worktree_agent(&self, worktree: &str) -> Result<Option<String>>;

    fn set_worktree_agent(&self, wt: &str, agent: &str) -> Result<()>;

    fn del_worktree(&self, wt: &str) -> Result<()>;

    /// Forget every registry worktree row owned by a repo (no disk side
    /// effects). Pairs with [`Self::del_workspace`] so a removed workspace's
    /// cross-workspace rows neither re-render nor resurrect on the next launch.
    fn del_worktrees_for_repo(&self, repo_path: &str) -> Result<()>;

    /// Re-key a worktree registry row after a rename (`git branch -m` +
    /// `git worktree move`): the primary key `worktree` (path) moves to
    /// `new_path`, and the `tab_name`/`branch` follow the new branch. `position`,
    /// `agent`, and `sandbox_backend` are preserved. No-op if the old row is gone.
    fn rename_worktree(
        &self,
        old_path: &str,
        new_path: &str,
        new_tab: &str,
        new_branch: &str,
    ) -> Result<()>;

    /// Forget the registry row for a worktree group by its owning repo and tab
    /// name. This is intentionally independent of the worktree path so close /
    /// delete operations cannot be undone by a stale row whose path was moved or
    /// normalized differently than the live session group.
    fn del_worktree_for_tab(&self, repo_root: &str, tab: &str) -> Result<()>;

    fn set_worktree_sandbox(&self, wt: &str, backend: &str) -> Result<()>;

    fn worktree_sandbox(&self, wt: &str) -> Result<Option<String>>;

    /// The worktree path for a (session, tab) pair — how the panel plugin maps
    /// the focused tab to a worktree (PaneInfo carries no cwd).
    fn worktree_for_tab(&self, session: &str, tab: &str) -> Result<Option<String>>;

    /// All recorded worktrees (metadata only; git supplies live status).
    fn worktrees(&self) -> Result<Vec<WorktreeRow>>;

    /// Swap the persisted sort positions of two worktrees (by path). Used by
    /// the sidebar's manual reorder (Shift+Alt+↑/↓): the caller picks the two
    /// adjacent siblings, this exchanges their `position` so the new order
    /// survives restart. Positions are globally unique (migration + MAX+1
    /// inserts), so a swap can never create a collision.
    fn swap_worktree_positions(&self, a: &str, b: &str) -> Result<()>;

    /// Set one worktree's persisted sort position (path key). The session-layout
    /// persist path uses this to keep `position` in step with the live group
    /// order after a manual move.
    fn set_worktree_position(&self, wt: &str, position: i64) -> Result<()>;

    fn folders_for_workspace(&self, repo_path: &str) -> Result<Vec<crate::models::FolderRow>>;

    fn create_folder(&self, repo_path: &str, name: &str) -> Result<i64>;

    fn rename_folder(&self, folder_id: i64, new_name: &str) -> Result<()>;

    fn del_folder(&self, folder_id: i64) -> Result<()>;

    /// Find a folder in `repo_path` whose name matches `name`
    /// (case-insensitive, trimmed) and return its id, creating it if absent.
    /// This is the find-or-create primitive behind the "file worktree into
    /// folder" actions, so repeated firing never spawns duplicate folders.
    fn ensure_folder(&self, repo_path: &str, name: &str) -> Result<i64>;

    /// File (or unfile, with `None`) a single worktree into a folder.
    fn set_worktree_folder(&self, worktree: &str, folder_id: Option<i64>) -> Result<()>;

    /// Select the named execution environment for a worktree (`[env.<name>]`).
    /// `""` clears it (inherit the workspace/repo/global layer).
    fn set_worktree_env(&self, wt: &str, env: &str) -> Result<()>;

    /// The worktree's selected env name, if any (NULL/empty ⇒ inherit).
    fn worktree_env(&self, wt: &str) -> Result<Option<String>>;

    /// Select the default execution environment for a whole workspace. `""`
    /// clears it.
    fn set_workspace_env(&self, repo_path: &str, env: &str) -> Result<()>;

    /// The workspace's default env name, if any (NULL/empty ⇒ inherit).
    fn workspace_env(&self, repo_path: &str) -> Result<Option<String>>;

    /// The effective selected env for a worktree: its own `env_name`, else its
    /// workspace's `env_name`. (`None` ⇒ fall through to repo `.thegn.*` /
    /// global default in [`crate::config::Config::resolve_env`].)
    fn effective_env(&self, wt: &str, repo_path: &str) -> Option<String>;

    /// Save (or replace) a named layout snapshot. `spec` is a serialized
    /// `LayoutSpec` JSON string.
    fn put_layout(&self, name: &str, spec: &str) -> Result<()>;

    /// The serialized spec for a named layout, if present.
    fn get_layout(&self, name: &str) -> Result<Option<String>>;

    /// All saved layout names, alphabetical.
    fn list_layouts(&self) -> Result<Vec<String>>;

    /// Delete a named layout (no-op if absent).
    fn delete_layout(&self, name: &str) -> Result<()>;

    /// Insert or replace a worktree group's persisted row.
    fn put_tab_group(&self, session: &str, row: &crate::models::TabGroupRow) -> Result<()>;

    /// Insert or replace one tab inside a worktree group.
    fn put_group_tab(&self, session: &str, row: &crate::models::GroupTabRow) -> Result<()>;

    /// All persisted worktree groups for a session, in display order.
    fn groups_for_session(&self, session: &str) -> Result<Vec<crate::models::TabGroupRow>>;

    /// All persisted tabs for every group in a session, ordered (group, tab).
    fn group_tabs_for_session(&self, session: &str) -> Result<Vec<crate::models::GroupTabRow>>;

    /// Forget one worktree group and its tabs (on worktree close).
    fn delete_tab_group(&self, session: &str, name: &str) -> Result<()>;

    /// Forget every group (and its tabs) of `session` whose `worktree` column
    /// equals `worktree` — the headless-removal path (`wt rm`), which knows the
    /// worktree path but not the display group name. A stale `tab_groups` row
    /// resurrects the worktree at next launch, so removal must key on the path.
    fn delete_tab_groups_for_worktree(&self, session: &str, worktree: &str) -> Result<()>;

    /// Wipe a session's whole persisted layout (groups + tabs). The host
    /// persists snapshots as clear-then-insert inside one transaction so
    /// closed/renamed entries can't linger.
    fn clear_session_layout(&self, session: &str) -> Result<()>;

    /// Record which worktree group is active (for restoring focus on resurrect).
    fn set_active_tab(&self, session: &str, tab: &str, now: i64) -> Result<()>;

    /// Record the workspace (repo path) that was focused at the last switch.
    /// Stored as a global `ui_state` pointer ("" scope) so startup can reopen
    /// the workspace the user was actually in — independent of the
    /// `workspaces.last_active` column, which also orders the sidebar tree and
    /// must not reshuffle on every switch.
    fn set_active_workspace(&self, repo_path: &str) -> Result<()>;

    /// The workspace recorded by [`Self::set_active_workspace`], if any.
    fn active_workspace(&self) -> Result<Option<String>>;

    /// The tab that was active at exit, if recorded.
    fn active_tab(&self, session: &str) -> Result<Option<String>>;

    /// Read a persisted UI-state value, `None` if unset.
    fn get_ui_state(&self, scope: &str, key: &str) -> Result<Option<String>>;

    /// Upsert a persisted UI-state value for `(scope, key)`.
    fn set_ui_state(&self, scope: &str, key: &str, value: &str) -> Result<()>;

    /// Delete a persisted UI-state value (e.g. unpinning). No-op if absent.
    fn del_ui_state(&self, scope: &str, key: &str) -> Result<()>;

    /// Delete every persisted UI-state key in `scope` starting with `prefix` —
    /// the orphan-pruning hook for entity removal (a deleted workspace/worktree/
    /// folder takes its `collapse:`/`pin:` keys with it). No-op if none match.
    fn del_ui_state_prefix(&self, scope: &str, prefix: &str) -> Result<()>;

    /// All `(key, value)` pairs in a scope — used to load every collapse/pin
    /// entry at once on sidebar build.
    fn ui_state_in_scope(&self, scope: &str) -> Result<Vec<(String, String)>>;

    /// Record the running-pin set (an opaque JSON string) for a session without
    /// disturbing `active_tab`. Used by the native host to resurrect pins.
    fn set_pin_state(&self, session: &str, json: &str, now: i64) -> Result<()>;

    /// The running-pin JSON recorded for a session, if any.
    fn pin_state(&self, session: &str) -> Result<Option<String>>;

    /// Record that `key` was just chosen (increment count, stamp last_used).
    fn bump_palette_usage(&self, key: &str) -> Result<()>;

    /// All usage rows as (key, count, last_used), for frecency ranking.
    fn palette_usage(&self) -> Result<Vec<(String, i64, i64)>>;

    fn terminals(&self) -> Result<Vec<crate::models::TerminalRow>>;

    fn put_terminal(
        &self,
        name: &str,
        kind: &str,
        connection_string: &str,
        folder_id: Option<i64>,
    ) -> Result<i64>;

    /// Record the sandbox backend a local terminal launches under (keyed by the
    /// terminal's unique name). Mirrors [`Self::set_worktree_sandbox`].
    fn set_terminal_sandbox(&self, name: &str, backend: &str) -> Result<()>;

    /// Record the named execution environment a terminal launches under.
    fn set_terminal_env(&self, name: &str, env: &str) -> Result<()>;

    fn del_terminal(&self, id: i64) -> Result<()>;

    fn rename_terminal(&self, id: i64, new_name: &str) -> Result<()>;
}
