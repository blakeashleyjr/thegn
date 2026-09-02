//! The **workspace/session** seam: registered repos & workspaces,
//! thegn-managed worktrees (positions, sandbox/env/agent bindings), folders,
//! saved layouts, tab groups + the persisted session (active tab/workspace),
//! sidebar UI state, pins, palette frecency, and named terminals. git is the
//! source of truth for worktrees on disk; this is the cache + resurrection layer.

use crate::models::{WorkspaceRow, WorktreeRow};
use anyhow::Result;

/// `ui_state` scope holding workspace removal tombstones (keyed by `repo_path`).
/// See [`WorkspaceStore::tombstone_workspace`].
pub const WORKSPACE_TOMBSTONE_SCOPE: &str = "workspace_tombstone";

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

    /// Mark a workspace as *explicitly removed by the user* so the resurrection
    /// paths never silently re-add it. thegn keeps the home checkout on disk
    /// when a workspace is removed (git is the source of truth), so without a
    /// tombstone a cold start whose cwd — or a stale active-workspace pointer —
    /// resolves to that directory would `put_workspace` it straight back into
    /// the sidebar. Recorded in `ui_state` (no schema bump), keyed by
    /// `repo_path`; cleared by [`Self::clear_workspace_tombstone`] on an
    /// explicit reopen. Default impl over the `ui_state` primitives.
    fn tombstone_workspace(&self, repo_path: &str) -> Result<()> {
        self.set_ui_state(
            WORKSPACE_TOMBSTONE_SCOPE,
            repo_path,
            &crate::util::now().to_string(),
        )
    }

    /// Whether [`Self::tombstone_workspace`] marked this workspace removed —
    /// the guard the resurrection paths consult before re-registering a
    /// workspace resolved from disk.
    fn workspace_tombstoned(&self, repo_path: &str) -> Result<bool> {
        Ok(self
            .get_ui_state(WORKSPACE_TOMBSTONE_SCOPE, repo_path)?
            .is_some())
    }

    /// Clear a workspace tombstone — called when the user explicitly reopens a
    /// workspace (create / switch-to), which is unambiguous intent to keep it.
    fn clear_workspace_tombstone(&self, repo_path: &str) -> Result<()> {
        self.del_ui_state(WORKSPACE_TOMBSTONE_SCOPE, repo_path)
    }

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

    /// Persist the full sidebar workspace order: write `position = index` for
    /// each `repo_path` in `order`, in one transaction. Unlike the swap +
    /// normalize path, this encodes the *exact* on-screen order the caller
    /// passes, so a hydration reload via [`Self::workspaces`] reproduces it
    /// verbatim — no tiebreak drift between the persisted order and what the
    /// user arranged. Paths not in `order` (none, in practice) keep their
    /// current position.
    fn set_workspace_order(&self, order: &[String]) -> Result<()>;

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

    /// Rebind a worktree's persisted remote-location blob to the freshly
    /// RESOLVED placement — a stale create-time blob must not keep routing
    /// chrome reads at a dead provider. Plain UPDATE: a missing row is a no-op
    /// (a location without a registered worktree is meaningless), and callers
    /// only pass a remote blob, so a provider location can never be wiped by a
    /// local/failover resolution (mirrors [`Self::put_worktree`]'s COALESCE).
    fn set_worktree_location(&self, wt: &str, location: &str) -> Result<()>;

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

    /// Record a worktree's sandbox **pick** — a deliberate override that drives
    /// re-resolution on later opens. This is intent, NOT what a launch achieved;
    /// never display it (see [`Self::set_worktree_observed`]).
    fn set_worktree_sandbox(&self, wt: &str, backend: &str) -> Result<()>;

    fn worktree_sandbox(&self, wt: &str) -> Result<Option<String>>;

    /// Record the containment a worktree's launch ACTUALLY entered, derived from
    /// its argv by `sandbox_truth`. This is the value surfaces display, so a pick
    /// that could not be honoured shows as `host` while the pick itself survives
    /// for the next resolution.
    fn set_worktree_observed(&self, wt: &str, backend: &str) -> Result<()>;

    /// The last observed containment for a worktree; `None` when it has never
    /// been launched (displays as nothing rather than as a guess).
    fn worktree_observed(&self, wt: &str) -> Result<Option<String>>;

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

    /// Persist the full sidebar worktree order for a workspace — the worktree
    /// analogue of [`Self::set_workspace_order`], and for the same reason: a
    /// two-position swap leans on `normalize_worktree_positions` to heal
    /// NULL/tied values and can seed a different order than the tree shows,
    /// while writing `position = index` over the caller's sequence makes the
    /// reload via [`Self::worktrees`] reproduce exactly what was on screen.
    ///
    /// The sidebar's manual reorder passes one workspace's on-screen order
    /// (loose run first, then each folder's run). `position` is a *table-wide*
    /// sequence, so a per-workspace rewrite can tie with another workspace's
    /// values — harmless, because order is only ever compared within a
    /// workspace once `worktrees()` has been grouped by `repo_path`.
    fn set_worktree_order(&self, order: &[String]) -> Result<()>;

    fn folders_for_workspace(&self, repo_path: &str) -> Result<Vec<crate::models::FolderRow>>;

    fn create_folder(&self, repo_path: &str, name: &str) -> Result<i64>;

    fn rename_folder(&self, folder_id: i64, new_name: &str) -> Result<()>;

    /// Persist the folder order within a workspace: `position = index` over
    /// `order` (folder ids, top to bottom as the sidebar shows them). Folder
    /// contents follow their header, so reordering folders never touches
    /// `worktrees.position`. Ids not in `order` keep their current position.
    fn set_folder_order(&self, repo_path: &str, order: &[i64]) -> Result<()>;

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

    /// The persisted group row and tabs of the TERMINAL group `name`, plus the
    /// session that last held them — `None` when no session ever persisted it.
    ///
    /// Terminals are workspace-independent (the `terminals` registry is global
    /// and its sidebar row renders in every workspace) but their layout rows are
    /// keyed by whichever session was active when they were last persisted, so
    /// re-opening one from a different workspace has to find that row wherever
    /// it landed — otherwise the restore has no `pane_sessions` to warm-reattach
    /// with and silently forks a fresh shell. The session name comes back so the
    /// caller can delete the donor rows once the group has moved.
    #[allow(clippy::type_complexity)]
    fn terminal_group_tabs(
        &self,
        name: &str,
    ) -> Result<
        Option<(
            String,
            crate::models::TabGroupRow,
            Vec<crate::models::GroupTabRow>,
        )>,
    >;

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

    /// Record a local terminal's sandbox **pick** (keyed by the terminal's unique
    /// name). Mirrors [`Self::set_worktree_sandbox`]: intent, not achievement —
    /// never display it.
    fn set_terminal_sandbox(&self, name: &str, backend: &str) -> Result<()>;

    /// Record the containment a terminal's launch ACTUALLY entered. Mirrors
    /// [`Self::set_worktree_observed`] and is what the tab chip and sidebar show.
    fn set_terminal_observed(&self, name: &str, backend: &str) -> Result<()>;

    /// Record the named execution environment a terminal launches under.
    fn set_terminal_env(&self, name: &str, env: &str) -> Result<()>;

    fn del_terminal(&self, id: i64) -> Result<()>;

    fn rename_terminal(&self, id: i64, new_name: &str) -> Result<()>;

    /// Swap the persisted sort positions of two terminals (by name). Used by the
    /// sidebar's manual reorder (Ctrl+Alt+↑/↓) within a host group, mirroring
    /// [`Self::swap_worktree_positions`]. Heals NULL/tied positions (older rows
    /// whose `position` was never re-assigned) before swapping so a reorder never
    /// silently no-ops.
    fn swap_terminal_positions(&self, a_name: &str, b_name: &str) -> Result<()>;

    /// Set one terminal's persisted sort position (name key). Mirrors
    /// [`Self::set_worktree_position`].
    fn set_terminal_position(&self, name: &str, position: i64) -> Result<()>;
}
