//! Shared data types.

use serde::Serialize;

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
    /// An LLM-proxy request (model traffic): tokens + cost.
    Proxy,
}

impl TimelineSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TimelineSource::Sandbox => "sandbox",
            TimelineSource::Proxy => "proxy",
        }
    }
}

/// One normalized entry in the **unified per-worktree activity timeline** — the
/// sandbox audit log and the proxy request log merged into a single, time-ordered
/// view (the cross-backend "what is this worktree doing" surface). Timestamps are
/// **milliseconds** since the epoch so the two sources (container `ts` is seconds,
/// proxy `ts_ms` is millis) sort together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub ts_ms: i64,
    pub source: TimelineSource,
    pub kind: String,
    pub detail: String,
}

/// Merge the sandbox audit events and proxy request rows for one worktree into a
/// single newest-first timeline, capped at `limit`. Pure (no I/O) so the host's
/// hydration thread just calls it on the two DB reads.
pub fn merge_timeline(
    sandbox: &[ContainerEvent],
    proxy: &[crate::db::ProxyRequestRow],
    limit: usize,
) -> Vec<TimelineEvent> {
    let mut out: Vec<TimelineEvent> = Vec::with_capacity(sandbox.len() + proxy.len());
    for e in sandbox {
        out.push(TimelineEvent {
            // container `ts` is in seconds (see `util::now`); normalize to ms.
            ts_ms: e.ts.saturating_mul(1000),
            source: TimelineSource::Sandbox,
            kind: e.kind.clone(),
            detail: e.detail.clone().unwrap_or_default(),
        });
    }
    for r in proxy {
        let model = if r.backend_model.is_empty() {
            r.client_model.clone()
        } else {
            r.backend_model.clone()
        };
        let mut detail = format!(
            "{model} · {}+{} tok · ${:.4}",
            r.input_tokens, r.output_tokens, r.cost_usd
        );
        if !r.outcome.is_empty() {
            detail.push_str(&format!(" · {}", r.outcome));
        }
        out.push(TimelineEvent {
            ts_ms: r.ts_ms,
            source: TimelineSource::Proxy,
            kind: "request".to_string(),
            detail,
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
    use crate::db::ProxyRequestRow;

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
    fn pr(ts_ms: i64, cost: f64) -> ProxyRequestRow {
        ProxyRequestRow {
            ts_ms,
            backend_model: "claude-opus-4-8".into(),
            input_tokens: 100,
            output_tokens: 20,
            cost_usd: cost,
            outcome: "ok".into(),
            ..Default::default()
        }
    }

    #[test]
    fn merges_and_sorts_newest_first_across_sources() {
        // container ts is seconds; proxy is ms. 5s = 5000ms should sort after 6000ms.
        let sandbox = [ce(5, "exec", Some("sh")), ce(2, "die", None)];
        let proxy = [pr(6000, 0.0123), pr(1000, 0.5)];
        let tl = merge_timeline(&sandbox, &proxy, 10);
        assert_eq!(tl.len(), 4);
        // Order by ms: 6000(proxy), 5000(sandbox exec), 2000(sandbox die), 1000(proxy)
        assert_eq!(tl[0].source, TimelineSource::Proxy);
        assert_eq!(tl[0].ts_ms, 6000);
        assert_eq!(tl[1].source, TimelineSource::Sandbox);
        assert_eq!(tl[1].kind, "exec");
        assert_eq!(tl[3].ts_ms, 1000);
    }

    #[test]
    fn proxy_detail_carries_model_tokens_cost() {
        let tl = merge_timeline(&[], &[pr(1000, 0.0123)], 10);
        assert_eq!(tl[0].kind, "request");
        assert!(tl[0].detail.contains("claude-opus-4-8"));
        assert!(tl[0].detail.contains("100+20 tok"));
        assert!(tl[0].detail.contains("$0.0123"));
        assert!(tl[0].detail.contains("ok"));
    }

    #[test]
    fn respects_limit() {
        let sandbox: Vec<_> = (0..20).map(|i| ce(i, "exec", None)).collect();
        let tl = merge_timeline(&sandbox, &[], 5);
        assert_eq!(tl.len(), 5);
        // Newest (highest ts) retained.
        assert_eq!(tl[0].ts_ms, 19_000);
    }

    #[test]
    fn empty_inputs_yield_empty() {
        assert!(merge_timeline(&[], &[], 10).is_empty());
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

/// A superzej-managed worktree (one per tab) as recorded in the DB. Some fields
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
    pub sandbox_backend: Option<String>,
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
#[derive(Debug, Clone)]
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
            branch: "sz/x".into(),
            agent: "claude".into(),
            dirty: 1,
            ahead: 2,
            behind: 0,
            created_at: 3,
            exists: true,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(j.contains("\"branch\":\"sz/x\"") && j.contains("\"exists\":true"));

        // WorktreeRow has no Serialize; just exercise construction + Clone/Debug.
        let row = WorktreeRow {
            worktree: "/wt".into(),
            branch: "sz/x".into(),
            agent: String::new(),
            created_at: 0,
            repo_root: "/r".into(),
            tab_name: "r/x".into(),
            session_name: "default".into(),
            location: String::new(),
            position: 0,
            sandbox_backend: None,
            folder_id: None,
            env_name: None,
        };
        let _ = format!("{:?}", row.clone());
    }
}
