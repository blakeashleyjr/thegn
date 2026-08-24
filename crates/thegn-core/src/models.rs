//! Shared data types.

use serde::Serialize;

/// A persisted ingress share (`[share]`) — the resurrection record for a tunnel
/// the host respawns on restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShareRow {
    pub worktree: String,
    pub local_port: u16,
    pub provider: String,
    pub public_url: Option<String>,
    pub state: String,
    pub created_at: i64,
}

/// A persisted auto port forward (`[forward]`) — the resurrection record for a
/// forward the host re-detects on restart. Keyed by `(worktree, container_port)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForwardRow {
    pub worktree: String,
    pub container_port: u16,
    pub host_port: u16,
    pub url: String,
    pub created_at: i64,
}

/// Payload of a `focus_workspace` intent (the `thegn open` mailbox row):
/// the CLI writes it, the compositor's model refresh consumes it. One shared
/// type so producer and consumer can't drift.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct FocusIntent {
    /// Canonical repo path of the workspace to focus.
    pub repo: String,
}

/// A sandbox audit event from the `container_events` table.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerEvent {
    pub id: i64,
    pub worktree: String,
    pub ts: i64,
    pub kind: String,
    pub detail: Option<String>,
    pub exit_code: Option<i64>,
}

/// Where a [`TimelineEvent`] originated — drives the row glyph/colour and lets
/// the view filter by source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineSource {
    /// Sandbox container lifecycle (`<engine> events`: exec/die/network) or a
    /// host-synthesized pane exec/exit for non-OCI backends.
    Sandbox,
}

impl TimelineSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TimelineSource::Sandbox => "sandbox",
        }
    }
}

/// One normalized entry in the **unified per-worktree activity timeline** — the
/// cross-backend "what is this worktree doing" surface. Timestamps are
/// **milliseconds** since the epoch so future sources with differing clock
/// granularity sort together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub ts_ms: i64,
    pub source: TimelineSource,
    pub kind: String,
    pub detail: String,
}

/// Normalize the sandbox audit events for one worktree into a single
/// newest-first timeline, capped at `limit`. Pure (no I/O) so the host's
/// hydration thread just calls it on the DB read.
pub fn merge_timeline(sandbox: &[ContainerEvent], limit: usize) -> Vec<TimelineEvent> {
    let mut out: Vec<TimelineEvent> = Vec::with_capacity(sandbox.len());
    for e in sandbox {
        out.push(TimelineEvent {
            // container `ts` is in seconds (see `util::now`); normalize to ms.
            ts_ms: e.ts.saturating_mul(1000),
            source: TimelineSource::Sandbox,
            kind: e.kind.clone(),
            detail: e.detail.clone().unwrap_or_default(),
        });
    }
    // Newest first; ties broken by source for determinism.
    out.sort_by(|a, b| {
        b.ts_ms
            .cmp(&a.ts_ms)
            .then_with(|| a.source.as_str().cmp(b.source.as_str()))
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn ce(ts: i64, kind: &str, detail: Option<&str>) -> ContainerEvent {
        ContainerEvent {
            id: 0,
            worktree: "/w".into(),
            ts,
            kind: kind.into(),
            detail: detail.map(str::to_string),
            exit_code: None,
        }
    }

    #[test]
    fn sorts_newest_first_and_normalizes_seconds_to_ms() {
        let sandbox = [ce(5, "exec", Some("sh")), ce(2, "die", None)];
        let tl = merge_timeline(&sandbox, 10);
        assert_eq!(tl.len(), 2);
        assert_eq!(tl[0].ts_ms, 5000);
        assert_eq!(tl[0].source, TimelineSource::Sandbox);
        assert_eq!(tl[0].kind, "exec");
        assert_eq!(tl[1].ts_ms, 2000);
    }

    #[test]
    fn respects_limit() {
        let sandbox: Vec<_> = (0..20).map(|i| ce(i, "exec", None)).collect();
        let tl = merge_timeline(&sandbox, 5);
        assert_eq!(tl.len(), 5);
        // Newest (highest ts) retained.
        assert_eq!(tl[0].ts_ms, 19_000);
    }

    #[test]
    fn empty_inputs_yield_empty() {
        assert!(merge_timeline(&[], 10).is_empty());
    }
}

/// A registered workspace, as recorded in the DB. Identified by its path — a
/// git repo's main worktree, or a plain directory for a non-repo workspace.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct WorkspaceRow {
    pub repo_path: String,
    pub name: String,
    pub created_at: i64,
    pub last_active: i64,
    /// `"repo"` (a git repo) or `"dir"` (a plain non-git directory). Git-only
    /// actions no-op on `dir` workspaces.
    pub kind: String,
}

/// A thegn-managed worktree (one per tab) as recorded in the DB. Some fields
/// are carried for the sidebar/panel even if `list` ignores them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeRow {
    pub worktree: String,
    pub branch: String,
    pub agent: String,
    pub created_at: i64,
    pub repo_root: String,
    pub tab_name: String,
    pub session_name: String,
    /// Remote-location descriptor (JSON) for a remote worktree; empty = local.
    pub location: String,
    /// Persistent sort key for the sidebar (creation order by default,
    /// user-reorderable via Shift+Alt+↑/↓). Lower sorts first.
    pub position: i64,
    /// The sandbox **pick** for this worktree — a deliberate override that
    /// drives re-resolution. Intent, not achievement: never display it.
    pub sandbox_backend: Option<String>,
    /// What this worktree's last launch ACTUALLY entered (argv-derived). `None`
    /// = never launched. This is the value surfaces display.
    pub observed_backend: Option<String>,
    pub folder_id: Option<i64>,
    /// Selected execution environment (`[env.<name>]`); `None` = inherit the
    /// workspace/repo/global layer. See [`crate::config::Config::resolve_env`].
    pub env_name: Option<String>,
}

/// A persisted worktree group (native host, schema v6): one worktree shown in
/// the sidebar, owning an ordered set of tabs (`GroupTabRow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabGroupRow {
    /// Display name, e.g. "app/feat" — unique within a session.
    pub name: String,
    /// "home" (the main checkout) or "branch".
    pub kind: String,
    /// Worktree dir on disk (empty only for legacy rows with no path).
    pub worktree: String,
    pub ordinal: i64,
    /// Index of the group's active tab (restored when switching back).
    pub active_tab: i64,
}

/// A persisted tab inside a worktree group (schema v6). The `pane_tree` is the
/// serialized `CenterTree` (host-owned); core treats it as an opaque blob so the
/// layout model can evolve without touching the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTabRow {
    pub group_name: String,
    pub ordinal: i64,
    /// Short display title for the tab chip ("1", "zsh", …).
    pub title: String,
    /// Serialized pane tree (opaque JSON to core).
    pub pane_tree: String,
    pub focused_pane: i64,
    /// Per-leaf working directories: a JSON map of `pane id → cwd` (opaque to
    /// core). Empty string when unset (pre-v14 rows / no captured cwds).
    pub pane_cwds: String,
    /// Per-leaf last foreground command: a JSON map of `pane id → {argv, cwd}`
    /// (opaque to core). Empty string when unset (pre-v15 rows / idle shell, no
    /// non-shell program was running).
    pub pane_cmds: String,
    /// Per-leaf provider exec session: a JSON map of `pane id → {provider, id,
    /// session}` (opaque to core), so a native-exec pane reattaches to its live
    /// remote session on restart. Empty string when unset (pre-v22 rows / no
    /// native-exec panes).
    pub pane_sessions: String,
    /// Per-leaf captured scrollback tail: a JSON map of `pane id → text` (opaque
    /// to core), repainted into the pane on restore so a resurrected pane shows
    /// its recent history instead of a blank screen. Empty string when unset
    /// (pre-v28 rows / no captured scrollback).
    pub scrollback_snapshot: String,
}

/// A worktree enriched with live git status, for `list` / `dashboard` output.
/// `workspace` holds the owning session name (the workspace) in the v2 model.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeView {
    pub workspace: String,
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub agent: String,
    pub dirty: i64,
    pub ahead: i64,
    pub behind: i64,
    pub created_at: i64,
    pub exists: bool,
}

/// A persistent folder in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderRow {
    pub folder_id: i64,
    pub repo_path: String,
    pub name: String,
    pub position: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub connection_string: String,
    pub folder_id: Option<i64>,
    pub created_at: i64,
    pub last_active: i64,
    pub position: i64,
    /// Sandbox backend label for a local terminal ("bwrap"/"podman"/…), or
    /// "host"/empty for an un-sandboxed shell. Ignored for remote (ssh/mosh)
    /// terminals, whose isolation is owned by the remote end.
    pub sandbox_backend: String,
    /// Named execution environment this terminal launches under, if any.
    pub env_name: String,
    /// What the terminal's last launch ACTUALLY entered, derived from its argv
    /// (`sandbox_truth`). Empty = never launched. This is the display value;
    /// `sandbox_backend` above is the pick, which may not have been honoured.
    pub observed_backend: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_construct_and_serialize() {
        let ws = WorkspaceRow {
            repo_path: "/r".into(),
            name: "r".into(),
            created_at: 1,
            last_active: 2,
            kind: "repo".into(),
        };
        assert!(
            serde_json::to_string(&ws)
                .unwrap()
                .contains("\"repo_path\":\"/r\"")
        );

        let v = WorktreeView {
            workspace: "w".into(),
            repo: "/r".into(),
            path: "/wt".into(),
            branch: "tg/x".into(),
            agent: "claude".into(),
            dirty: 1,
            ahead: 2,
            behind: 0,
            created_at: 3,
            exists: true,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(j.contains("\"branch\":\"tg/x\"") && j.contains("\"exists\":true"));

        // WorktreeRow has no Serialize; just exercise construction + Clone/Debug.
        let row = WorktreeRow {
            worktree: "/wt".into(),
            branch: "tg/x".into(),
            agent: String::new(),
            created_at: 0,
            repo_root: "/r".into(),
            tab_name: "r/x".into(),
            session_name: "default".into(),
            location: String::new(),
            position: 0,
            sandbox_backend: None,
            observed_backend: None,
            folder_id: None,
            env_name: None,
        };
        let _ = format!("{:?}", row.clone());
    }
}
