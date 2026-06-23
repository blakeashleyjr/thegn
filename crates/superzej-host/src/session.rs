//! The host's session model: worktree groups (one per git worktree, shown in
//! the sidebar) each owning an ordered set of tabs, plus the bridge to SQLite
//! for resurrect/persist. Tabs live *within* a worktree — the tabbar shows the
//! active group's tabs only; Alt+←/→ cycles tabs, Alt+↑/↓ moves between groups.
//!
//! Group/Tab ↔ row conversion is pure (and unit-tested); the DB calls are a
//! thin shell over `superzej_core::db`'s v6 `tab_groups`/`group_tabs`.

use anyhow::Result;
use superzej_core::db::Db;
use superzej_core::models::{GroupTabRow, TabGroupRow};

use crate::center::CenterTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// The repo's main checkout.
    Home,
    /// A branch worktree.
    Branch,
}

impl GroupKind {
    fn as_str(self) -> &'static str {
        match self {
            GroupKind::Home => "home",
            GroupKind::Branch => "branch",
        }
    }
    fn parse(s: &str) -> GroupKind {
        match s {
            "home" => GroupKind::Home,
            _ => GroupKind::Branch,
        }
    }
}

/// The last foreground command observed in a pane, captured from `/proc` at
/// persist time so a resurrected (or crashed) pane can offer to relaunch the
/// program that was running — never the bare interactive shell.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PaneCmd {
    /// The foreground process's argv (e.g. `["nvim", "src/main.rs"]`).
    pub argv: Vec<String>,
    /// Its working directory, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl PaneCmd {
    /// A shell-ready command line for the overlay + relaunch (naive join; argv
    /// elements with spaces are rare for the editor/REPL programs this targets).
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// One tab inside a worktree group: a pane tree and which leaf has focus.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    /// Short chip title ("1", "2", … by default; renamable later).
    pub title: String,
    pub center: CenterTree,
    pub focused_pane: u32,
    /// Last-known working directory of each leaf pane (`pane id → cwd`),
    /// captured at persist time so resurrected panes respawn where they were
    /// rather than at the worktree root. Only host (non-sandbox) panes whose
    /// dir still exists are honored on respawn; missing entries fall back to
    /// the worktree root.
    pub pane_cwds: std::collections::BTreeMap<u32, String>,
    /// Last-known foreground command of each leaf pane (`pane id → PaneCmd`),
    /// captured at persist time so a resurrected pane can offer to relaunch the
    /// program that was running. Only set for non-shell foreground programs.
    pub pane_cmds: std::collections::BTreeMap<u32, PaneCmd>,
}

impl Tab {
    pub fn new(title: impl Into<String>) -> Tab {
        Tab {
            title: title.into(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
            pane_cwds: std::collections::BTreeMap::new(),
            pane_cmds: std::collections::BTreeMap::new(),
        }
    }

    /// Reconstruct a tab from its persisted row. A malformed `pane_tree` falls
    /// back to a single pane rather than failing the whole resurrect; malformed
    /// or absent `pane_cwds` simply yields no cwd hints.
    pub fn from_row(row: &GroupTabRow) -> Tab {
        let center = serde_json::from_str::<CenterTree>(&row.pane_tree)
            .inspect_err(|e| tracing::warn!("malformed pane_tree in tab '{}': {e}", row.group_name))
            .unwrap_or(CenterTree::Leaf(0));
        let pane_cwds = if row.pane_cwds.is_empty() {
            std::collections::BTreeMap::new()
        } else {
            serde_json::from_str(&row.pane_cwds).unwrap_or_default()
        };
        let pane_cmds = if row.pane_cmds.is_empty() {
            std::collections::BTreeMap::new()
        } else {
            serde_json::from_str(&row.pane_cmds).unwrap_or_default()
        };
        Tab {
            title: if row.title.is_empty() {
                (row.ordinal + 1).to_string()
            } else {
                row.title.clone()
            },
            center,
            focused_pane: row.focused_pane.max(0) as u32,
            pane_cwds,
            pane_cmds,
        }
    }

    /// Serialize this tab to a persistable row at the given position. Stale cwd
    /// entries for leaves no longer in the tree are pruned so the map can't grow
    /// unbounded across splits/closes.
    pub fn to_row(&self, group: &str, ordinal: i64) -> GroupTabRow {
        let ids = self.center.pane_ids();
        let live_cwds: std::collections::BTreeMap<&u32, &String> = self
            .pane_cwds
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .collect();
        let live_cmds: std::collections::BTreeMap<&u32, &PaneCmd> = self
            .pane_cmds
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .collect();
        GroupTabRow {
            group_name: group.to_string(),
            ordinal,
            title: self.title.clone(),
            pane_tree: serde_json::to_string(&self.center).unwrap_or_else(|_| "0".into()),
            focused_pane: self.focused_pane as i64,
            pane_cwds: if live_cwds.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&live_cwds).unwrap_or_default()
            },
            pane_cmds: if live_cmds.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&live_cmds).unwrap_or_default()
            },
        }
    }
}

/// One worktree in the session: what the sidebar lists and what the tabbar
/// scopes to. Owns its tabs; always has at least one.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeGroup {
    /// Display name, e.g. "app/feat" — unique within the session.
    pub name: String,
    pub kind: GroupKind,
    /// Worktree dir on disk.
    pub path: String,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

impl WorktreeGroup {
    pub fn new(name: impl Into<String>, kind: GroupKind, path: impl Into<String>) -> Self {
        WorktreeGroup {
            name: name.into(),
            kind,
            path: path.into(),
            tabs: vec![Tab::new("1")],
            active_tab: 0,
        }
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// Append a tab titled with the next free ordinal and focus it.
    pub fn add_tab(&mut self) -> usize {
        self.tabs.push(Tab::new((self.tabs.len() + 1).to_string()));
        self.active_tab = self.tabs.len() - 1;
        self.active_tab
    }

    /// Re-title ordinal-style tabs after a removal so chips read "1 2 3".
    fn renumber(&mut self) {
        for (i, t) in self.tabs.iter_mut().enumerate() {
            if t.title.chars().all(|c| c.is_ascii_digit()) {
                t.title = (i + 1).to_string();
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub worktrees: Vec<WorktreeGroup>,
    /// Index of the active worktree group.
    pub active: usize,
}

impl Session {
    /// Rebuild the session from the DB (cold-start resurrect). Groups come back
    /// in persisted order; the active group is restored from `session_state`.
    pub fn resurrect(db: &Db, session: &str) -> Result<Session> {
        let tab_rows = db.group_tabs_for_session(session)?;
        let mut worktrees: Vec<WorktreeGroup> = db
            .groups_for_session(session)?
            .iter()
            .map(|g| {
                let mut tabs: Vec<Tab> = tab_rows
                    .iter()
                    .filter(|t| t.group_name == g.name)
                    .map(Tab::from_row)
                    .collect();
                if tabs.is_empty() {
                    tabs.push(Tab::new("1"));
                }
                let active_tab = (g.active_tab.max(0) as usize).min(tabs.len() - 1);
                WorktreeGroup {
                    name: g.name.clone(),
                    kind: GroupKind::parse(&g.kind),
                    path: g.worktree.clone(),
                    tabs,
                    active_tab,
                }
            })
            .collect();

        // Canonical workspace slug (DB-assigned, `{slug}/…` tab prefix) — only
        // path-keyed sessions have one. For non-path session names fall back
        // to the name itself for registry matching (legacy behavior).
        let session_path = std::path::Path::new(session);
        let canonical = session_path
            .is_absolute()
            .then(|| superzej_core::repo::repo_slug_with(db, session_path));
        let slug = canonical.clone().unwrap_or_else(|| session.to_string());

        // Also adopt worktrees recorded for this session that aren't in
        // tab_groups yet (state from sessions that predate layout persistence).
        // The registry is the order authority: capture each worktree's
        // persistent `position` so we can sort the final group list by it.
        let mut positions: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        if let Ok(wts) = db.worktrees() {
            for wt in &wts {
                positions.insert(wt.worktree.clone(), wt.position);
            }
            for wt in wts {
                // git is the source of truth: a local registry row whose dir
                // vanished (worktree deleted outside superzej) is stale —
                // never resurrect it. Remote rows (location set) are kept.
                if wt.location.is_empty() && !std::path::Path::new(&wt.worktree).is_dir() {
                    continue;
                }
                let known = |ws: &[WorktreeGroup]| ws.iter().any(|g| g.name == wt.tab_name);
                // Adopt rows registered to this session, plus any row whose
                // repo_root is this workspace and whose tab carries our slug
                // prefix — regardless of the (possibly legacy) session_name,
                // so worktrees never silently vanish from the tree at start.
                let adopt = (wt.session_name == session && !known(&worktrees))
                    || (wt.repo_root == session
                        && wt.tab_name.starts_with(&format!("{slug}/"))
                        && !known(&worktrees));
                if adopt {
                    worktrees.push(WorktreeGroup::new(
                        wt.tab_name.clone(),
                        GroupKind::Branch,
                        wt.worktree.clone(),
                    ));
                }
            }
        }

        // Home groups persisted by older binaries carry the raw dir basename
        // (`WASHU/home`) instead of the canonical slug (`washu/home`) — rename
        // them so the `{slug}/…` prefix invariant holds. Idempotent; the next
        // layout persist makes it stick. Skip a rename that would collide with
        // an existing group (mixed-version leftovers keep their legacy name).
        if let Some(slug) = &canonical {
            let names: std::collections::HashSet<String> =
                worktrees.iter().map(|g| g.name.clone()).collect();
            for g in &mut worktrees {
                if g.kind == GroupKind::Home
                    && let Some((prefix, branch)) = crate::sidebar::split_tab(&g.name)
                    && prefix != *slug
                {
                    let target = format!("{slug}/{branch}");
                    if !names.contains(&target) {
                        g.name = target;
                    }
                }
            }
        }

        // Order worktrees in three tiers so a freshly-created worktree always
        // lands at the bottom. The bug this guards against: a new worktree gets
        // a low registry `position` (e.g. 0 on a fresh registry), while a
        // pre-existing branch that predates position tracking has none — so
        // ordering naively by position (treating "none" as +∞) floated the new
        // one *above* the old one. Tiers, stable within each:
        //   0. `home` — first in the raw vec (the sidebar floats it first at
        //      display time anyway; keeping it first keeps worktree cycling sane).
        //   1. legacy/unregistered branches — predate position tracking, so
        //      they're the oldest; a stable sort preserves their prior order.
        //   2. registered worktrees, by their persistent registry `position`
        //      (creation order by default, user-reorderable via Shift+Alt+↑/↓).
        worktrees.sort_by_key(|g| {
            if g.kind == GroupKind::Home {
                (0, 0)
            } else {
                match positions.get(&g.path).copied() {
                    Some(p) => (2, p),
                    None => (1, 0),
                }
            }
        });

        let active = db
            .active_tab(session)?
            .and_then(|name| {
                worktrees.iter().position(|g| g.name == name).or_else(|| {
                    // The persisted active-tab name may predate the rename.
                    let slug = canonical.as_ref()?;
                    crate::sidebar::split_tab(&name).and_then(|(_, branch)| {
                        let renamed = format!("{slug}/{branch}");
                        worktrees
                            .iter()
                            .position(|g| g.name == renamed && g.kind == GroupKind::Home)
                    })
                })
            })
            .unwrap_or(0);
        Ok(Session {
            id: session.to_string(),
            worktrees,
            active,
        })
    }

    /// Persist the full layout snapshot + active group (debounced by the caller
    /// on layout changes — not per keystroke). Clear-then-insert in one
    /// transaction so closed/renamed groups can't linger.
    pub fn persist(&self, db: &Db, session: &str, now: i64) -> Result<()> {
        db.transaction(|db| {
            db.clear_session_layout(session)?;
            for (gi, g) in self.worktrees.iter().enumerate() {
                db.put_tab_group(
                    session,
                    &TabGroupRow {
                        name: g.name.clone(),
                        kind: g.kind.as_str().to_string(),
                        worktree: g.path.clone(),
                        ordinal: gi as i64,
                        active_tab: g.active_tab as i64,
                    },
                )?;
                for (ti, tab) in g.tabs.iter().enumerate() {
                    db.put_group_tab(session, &tab.to_row(&g.name, ti as i64))?;
                }
            }
            if let Some(active) = self.worktrees.get(self.active) {
                db.set_active_tab(session, &active.name, now)?;
            }
            Ok(())
        })
    }

    pub fn active_group(&self) -> Option<&WorktreeGroup> {
        self.worktrees.get(self.active)
    }

    pub fn active_group_mut(&mut self) -> Option<&mut WorktreeGroup> {
        self.worktrees.get_mut(self.active)
    }

    /// The active tab of the active group, if any.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_group().and_then(|g| g.active_tab())
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.active_group_mut().and_then(|g| g.active_tab_mut())
    }

    /// Every tab in the session with its (group, tab) coordinates.
    pub fn iter_tabs(&self) -> impl Iterator<Item = (usize, usize, &Tab)> {
        self.worktrees
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| g.tabs.iter().enumerate().map(move |(ti, t)| (gi, ti, t)))
    }

    /// Mutable access to a tab by coordinates.
    pub fn tab_mut(&mut self, gi: usize, ti: usize) -> Option<&mut Tab> {
        self.worktrees.get_mut(gi).and_then(|g| g.tabs.get_mut(ti))
    }

    /// Append a worktree group and focus it (its first tab); returns its index.
    pub fn add_group(&mut self, group: WorktreeGroup) -> usize {
        self.worktrees.push(group);
        self.active = self.worktrees.len() - 1;
        self.active
    }

    /// Rewrite every pane id in the session through `f`, keeping each tab's
    /// `center` tree, `focused_pane`, and the `pane_cwds` hint map consistent.
    /// Used to move a cold-resurrected workspace onto a fresh, disjoint id
    /// range so its persisted ids can't collide with the live panes of other
    /// resident workspaces (which are no longer reaped on a switch).
    pub fn remap_pane_ids(&mut self, mut f: impl FnMut(u32) -> u32) {
        for g in &mut self.worktrees {
            for tab in &mut g.tabs {
                tab.center.remap(&mut |id| f(id));
                tab.focused_pane = f(tab.focused_pane);
                tab.pane_cwds = tab
                    .pane_cwds
                    .iter()
                    .map(|(id, cwd)| (f(*id), cwd.clone()))
                    .collect();
                tab.pane_cmds = tab
                    .pane_cmds
                    .iter()
                    .map(|(id, cmd)| (f(*id), cmd.clone()))
                    .collect();
            }
        }
    }

    /// Focus the group at `idx` (clamped); no-op if empty.
    pub fn switch_to(&mut self, idx: usize) {
        if !self.worktrees.is_empty() {
            self.active = idx.min(self.worktrees.len() - 1);
        }
    }

    /// Focus a (group, tab) pair, clamped.
    pub fn switch_to_tab(&mut self, gi: usize, ti: usize) {
        self.switch_to(gi);
        if let Some(g) = self.active_group_mut()
            && !g.tabs.is_empty()
        {
            g.active_tab = ti.min(g.tabs.len() - 1);
        }
    }

    pub fn switch_to_workspace(
        &mut self,
        repo_path: &str,
        db: &superzej_core::db::Db,
    ) -> Result<()> {
        let now = crate::run::now_secs();
        self.persist(db, &self.id, now)?;

        let new_session = Session::resurrect(db, repo_path)?;
        let mut worktrees = new_session.worktrees;
        let active = new_session.active;

        if worktrees.is_empty() {
            let base = std::path::Path::new(repo_path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into());
            // The `{slug}/…` prefix is the canonical workspace key everywhere
            // (sidebar grouping, ui_state, worktree registry); naming the home
            // group by the raw basename here would desync it from the DB slug
            // and duplicate the workspace in the sidebar tree.
            let slug = superzej_core::repo::repo_slug_with(db, std::path::Path::new(repo_path));
            worktrees.push(WorktreeGroup::new(
                superzej_core::repo::home_tab(&slug),
                GroupKind::Home,
                repo_path,
            ));
            // A path that resolves to a git main-worktree is a "repo"
            // workspace; anything else is a plain "dir" workspace.
            let kind =
                if superzej_core::repo::main_worktree(std::path::Path::new(repo_path)).is_some() {
                    "repo"
                } else {
                    "dir"
                };
            let _ = db.put_workspace(repo_path, &base, kind);
            let _ = db.touch_repo(repo_path, &base);
        }

        self.id = repo_path.to_string();
        self.worktrees = worktrees;
        self.active = active;
        self.persist(db, &self.id, now)?;
        // Record the workspace we just entered as the global "last active" so
        // the next cold start reopens it (not whatever sorts first by recency).
        let _ = db.set_active_workspace(repo_path);

        Ok(())
    }

    /// Cycle to the next tab within the active group (wraps).
    pub fn next_tab(&mut self) {
        if let Some(g) = self.active_group_mut()
            && !g.tabs.is_empty()
        {
            g.active_tab = (g.active_tab + 1) % g.tabs.len();
        }
    }

    /// Cycle to the previous tab within the active group (wraps).
    pub fn prev_tab(&mut self) {
        if let Some(g) = self.active_group_mut()
            && !g.tabs.is_empty()
        {
            g.active_tab = (g.active_tab + g.tabs.len() - 1) % g.tabs.len();
        }
    }

    /// Move to the next worktree group (wraps); it restores its own active tab.
    /// Simple session-order wrap; the live `Action::NextWorktree` uses the richer
    /// workspace-confined display-order walk in `run.rs`. Retained as the tested
    /// baseline model.
    #[allow(dead_code)]
    pub fn next_worktree(&mut self) {
        if !self.worktrees.is_empty() {
            self.active = (self.active + 1) % self.worktrees.len();
        }
    }

    /// Move to the previous worktree group (wraps).
    #[allow(dead_code)]
    pub fn prev_worktree(&mut self) {
        if !self.worktrees.is_empty() {
            self.active = (self.active + self.worktrees.len() - 1) % self.worktrees.len();
        }
    }

    /// Close the active tab. The final tab in a worktree is intentionally not
    /// removed here: callers must use `close_active_group` / CloseWorktree for
    /// that explicit destructive transition so CloseTab cannot accidentally
    /// erase a worktree group.
    pub fn close_active_tab(&mut self) -> CloseResult {
        let Some(g) = self.active_group_mut() else {
            return CloseResult::Nothing;
        };
        if g.tabs.len() > 1 {
            let removed = g.tabs.remove(g.active_tab);
            if g.active_tab >= g.tabs.len() {
                g.active_tab = g.tabs.len() - 1;
            }
            g.renumber();
            return CloseResult::Tab(removed);
        }
        CloseResult::Nothing
    }

    /// Remove the active group entirely; focus the nearest remaining one.
    pub fn close_active_group(&mut self) -> Option<WorktreeGroup> {
        if self.worktrees.is_empty() {
            return None;
        }
        let removed = self.worktrees.remove(self.active);
        // If we deleted the last group, clamp down
        if self.active >= self.worktrees.len() {
            self.active = self.worktrees.len().saturating_sub(1);
        }
        // No explicit -1 needed: `remove(self.active)` naturally drops the *next*
        // item into the `self.active` slot, so we automatically land on the
        // group immediately below the one we just deleted.
        Some(removed)
    }
}

/// What `close_active_tab` removed.
#[derive(Debug, PartialEq)]
pub enum CloseResult {
    /// A tab closed; the group lives on.
    Tab(Tab),
    /// There was no non-final tab to close.
    Nothing,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::center::{Branch, Dir};

    fn temp_db() -> Db {
        // Unique-ish path without external rand/time: pid + a process-local counter.
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sj-host-test-{}-{}/db.sqlite",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        Db::open_at(&p).unwrap()
    }

    #[test]
    fn tab_row_roundtrip_preserves_tree() {
        let tab = Tab {
            title: "2".into(),
            center: CenterTree::Split {
                dir: Dir::Row,
                children: vec![
                    Branch {
                        weight: 1.0,
                        child: CenterTree::Leaf(0),
                    },
                    Branch {
                        weight: 1.0,
                        child: CenterTree::Leaf(1),
                    },
                ],
            },
            focused_pane: 1,
            pane_cwds: std::collections::BTreeMap::new(),
            pane_cmds: std::collections::BTreeMap::new(),
        };
        let back = Tab::from_row(&tab.to_row("app/feat", 1));
        assert_eq!(tab, back);
    }

    #[test]
    fn tab_row_roundtrip_preserves_pane_cwds() {
        let mut tab = Tab::new("1");
        tab.center = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(3),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(7),
                },
            ],
        };
        tab.focused_pane = 7;
        tab.pane_cwds.insert(3, "/home/u/repo".into());
        tab.pane_cwds.insert(7, "/home/u/repo/src".into());
        // A stale entry for a leaf no longer in the tree is pruned on serialize.
        tab.pane_cwds.insert(99, "/gone".into());

        let back = Tab::from_row(&tab.to_row("app/feat", 0));
        assert_eq!(
            back.pane_cwds.get(&3).map(String::as_str),
            Some("/home/u/repo")
        );
        assert_eq!(
            back.pane_cwds.get(&7).map(String::as_str),
            Some("/home/u/repo/src")
        );
        assert!(!back.pane_cwds.contains_key(&99), "stale leaf cwd pruned");
    }

    #[test]
    fn tab_row_roundtrip_preserves_pane_cmds() {
        let mut tab = Tab::new("1");
        tab.center = CenterTree::Leaf(3);
        tab.focused_pane = 3;
        tab.pane_cmds.insert(
            3,
            PaneCmd {
                argv: vec!["nvim".into(), "src/main.rs".into()],
                cwd: Some("/home/u/repo/src".into()),
            },
        );
        // A stale entry for a leaf no longer in the tree is pruned on serialize.
        tab.pane_cmds.insert(
            99,
            PaneCmd {
                argv: vec!["gone".into()],
                cwd: None,
            },
        );

        let back = Tab::from_row(&tab.to_row("app/feat", 0));
        assert_eq!(
            back.pane_cmds.get(&3).map(PaneCmd::display),
            Some("nvim src/main.rs".to_string())
        );
        assert_eq!(
            back.pane_cmds.get(&3).and_then(|c| c.cwd.as_deref()),
            Some("/home/u/repo/src")
        );
        assert!(!back.pane_cmds.contains_key(&99), "stale leaf cmd pruned");
    }

    #[test]
    fn malformed_pane_tree_degrades_to_single_pane() {
        let row = GroupTabRow {
            group_name: "x/home".into(),
            ordinal: 0,
            title: String::new(),
            pane_tree: "this is not json".into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
        };
        let tab = Tab::from_row(&row);
        assert_eq!(tab.center, CenterTree::Leaf(0));
        assert_eq!(tab.title, "1", "empty title falls back to ordinal+1");
    }

    fn group(name: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, format!("/wt/{name}"))
    }

    #[test]
    fn group_and_tab_cycling() {
        let mut s = Session::default();
        s.add_group(group("a"));
        s.add_group(group("b"));
        assert_eq!(s.active_group().unwrap().name, "b"); // add focuses

        // Tabs cycle within the group only.
        s.active_group_mut().unwrap().add_tab();
        s.active_group_mut().unwrap().add_tab();
        assert_eq!(s.active_group().unwrap().tabs.len(), 3);
        assert_eq!(s.active_group().unwrap().active_tab, 2);
        s.next_tab(); // wraps 2 -> 0
        assert_eq!(s.active_group().unwrap().active_tab, 0);
        s.prev_tab(); // wraps 0 -> 2
        assert_eq!(s.active_group().unwrap().active_tab, 2);
        assert_eq!(s.active, 1, "tab cycling never leaves the group");

        // Worktree cycling restores each group's own active tab.
        s.next_worktree(); // b -> a (wraps)
        assert_eq!(s.active_group().unwrap().name, "a");
        assert_eq!(s.active_group().unwrap().active_tab, 0);
        s.prev_worktree();
        assert_eq!(s.active_group().unwrap().name, "b");
        assert_eq!(s.active_group().unwrap().active_tab, 2);
    }

    #[test]
    fn close_tab_never_removes_last_tab_or_group() {
        let mut s = Session::default();
        s.add_group(group("a"));
        s.add_group(group("b"));
        s.active_group_mut().unwrap().add_tab();

        // Closing a non-last tab keeps the group and renumbers chips.
        assert!(matches!(s.close_active_tab(), CloseResult::Tab(_)));
        let g = s.active_group().unwrap();
        assert_eq!(g.tabs.len(), 1);
        assert_eq!(g.tabs[0].title, "1");

        // Closing the final tab is a no-op: CloseWorktree is the only action
        // that removes a worktree group from the session.
        assert_eq!(s.close_active_tab(), CloseResult::Nothing);
        assert_eq!(s.worktrees.len(), 2);
        assert_eq!(s.active_group().unwrap().name, "b");
        assert_eq!(s.active_group().unwrap().tabs.len(), 1);

        s.switch_to(0);
        assert_eq!(s.close_active_tab(), CloseResult::Nothing);
        assert_eq!(s.worktrees.len(), 2);
        assert_eq!(s.active_group().unwrap().name, "a");
        assert_eq!(s.active_group().unwrap().tabs.len(), 1);
    }

    #[test]
    fn close_active_group_clamps_focus_when_removing_last_group() {
        let mut s = Session::default();
        s.add_group(group("g1"));
        s.add_group(group("g2"));
        s.add_group(group("g3"));

        // Focus middle group
        s.active = 1;
        assert_eq!(s.active_group().unwrap().name, "g2");

        // Remove middle group -> the old index 1 now points to "g3"
        s.close_active_group();
        assert_eq!(s.worktrees.len(), 2);
        assert_eq!(s.active_group().unwrap().name, "g3");
        assert_eq!(s.active, 1);

        // Remove the last group (now at index 1) -> should clamp focus to index 0 ("g1")
        s.close_active_group();
        assert_eq!(s.worktrees.len(), 1);
        assert_eq!(s.active_group().unwrap().name, "g1");
        assert_eq!(s.active, 0);

        // Remove final group
        s.close_active_group();
        assert_eq!(s.worktrees.len(), 0);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn switch_to_tab_clamps() {
        let mut s = Session::default();
        s.add_group(group("a"));
        s.switch_to_tab(9, 9);
        assert_eq!(s.active, 0);
        assert_eq!(s.active_group().unwrap().active_tab, 0);
    }

    #[test]
    fn remap_pane_ids_rewrites_trees_focus_and_cwds() {
        let mut s = Session::default();
        let mut g = group("a");
        g.tabs[0].center = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(3),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(7),
                },
            ],
        };
        g.tabs[0].focused_pane = 7;
        g.tabs[0].pane_cwds.insert(3, "/a".into());
        g.tabs[0].pane_cwds.insert(7, "/b".into());
        s.add_group(g);

        // Shift every id by 100.
        s.remap_pane_ids(|id| id + 100);

        let tab = &s.worktrees[0].tabs[0];
        assert_eq!(tab.center.pane_ids(), vec![103, 107]);
        assert_eq!(tab.focused_pane, 107);
        assert_eq!(tab.pane_cwds.get(&103).map(String::as_str), Some("/a"));
        assert_eq!(tab.pane_cwds.get(&107).map(String::as_str), Some("/b"));
        assert!(!tab.pane_cwds.contains_key(&3), "old cwd key remapped");
    }

    #[test]
    fn persist_then_resurrect_reproduces_the_session() {
        let db = temp_db();
        let sess = "s";
        let mut home = WorktreeGroup::new("app/home", GroupKind::Home, "/r");
        home.tabs[0].center = CenterTree::Leaf(0);
        let mut feat = WorktreeGroup::new("app/feat", GroupKind::Branch, "/wt/feat");
        feat.add_tab();
        feat.tabs[1].center = CenterTree::Leaf(2);
        feat.tabs[1].focused_pane = 2;
        let session = Session {
            id: sess.to_string(),
            worktrees: vec![home, feat],
            active: 1,
        };
        session.persist(&db, sess, 1234).unwrap();

        let back = Session::resurrect(&db, sess).unwrap();
        assert_eq!(back.worktrees, session.worktrees);
        assert_eq!(back.active, 1, "active group restored from session_state");
        assert_eq!(back.active_group().unwrap().active_tab, 1);

        // Persist again after closing a group: the snapshot replaces (no stale
        // rows from the clear-then-insert).
        let mut s2 = back;
        s2.close_active_group();
        s2.persist(&db, sess, 1235).unwrap();
        let back2 = Session::resurrect(&db, sess).unwrap();
        assert_eq!(back2.worktrees.len(), 1);
        assert_eq!(back2.worktrees[0].name, "app/home");
    }

    #[test]
    fn new_registered_worktree_resurrects_below_positionless_ones() {
        let db = temp_db();
        let sess = "s";
        let home = WorktreeGroup::new("app/home", GroupKind::Home, "/r");
        let old = WorktreeGroup::new("app/old", GroupKind::Branch, "/wt/old");
        let new = WorktreeGroup::new("app/new", GroupKind::Branch, "/wt/new");
        let session = Session {
            id: sess.to_string(),
            worktrees: vec![home, old, new],
            active: 0,
        };
        session.persist(&db, sess, 1234).unwrap();
        // Only the freshly-created worktree gets a registry row (position 0);
        // home + the pre-existing "old" branch were never registered.
        db.put_worktree("app/new", "/r", "/wt/new", "new", None)
            .unwrap();

        let back = Session::resurrect(&db, sess).unwrap();
        let names: Vec<&str> = back.worktrees.iter().map(|g| g.name.as_str()).collect();
        // The new worktree must stay at the BOTTOM, below the pre-existing
        // (positionless) "old" branch — home leads, registered worktrees trail.
        assert_eq!(names, vec!["app/home", "app/old", "app/new"]);
    }

    #[test]
    fn switch_to_workspace_names_home_group_with_canonical_slug() {
        let db = temp_db();
        let mut s = Session {
            id: "/r/old".into(),
            ..Default::default()
        };
        s.switch_to_workspace("/r/WASHU", &db).unwrap();
        assert_eq!(
            s.worktrees[0].name, "washu/home",
            "home group is keyed by the DB slug, not the raw basename"
        );
        assert_eq!(s.worktrees[0].kind, GroupKind::Home);

        // A different path with the same basename gets the -2 suffixed slug.
        let mut s2 = Session {
            id: "/r/old".into(),
            ..Default::default()
        };
        s2.switch_to_workspace("/elsewhere/WASHU", &db).unwrap();
        assert_eq!(s2.worktrees[0].name, "washu-2/home");
    }

    #[test]
    fn resurrect_normalizes_legacy_home_prefix_and_preserves_active() {
        let db = temp_db();
        let repo = "/r/WASHU";
        // Legacy layout: raw-basename home group + canonical branch group,
        // with the active tab persisted under the legacy name.
        let legacy = Session {
            id: repo.into(),
            worktrees: vec![
                WorktreeGroup::new("washu/feat", GroupKind::Branch, "/wt/feat"),
                WorktreeGroup::new("WASHU/home", GroupKind::Home, repo),
            ],
            active: 1,
        };
        legacy.persist(&db, repo, 1).unwrap();

        let s = Session::resurrect(&db, repo).unwrap();
        let names: Vec<_> = s.worktrees.iter().map(|g| g.name.as_str()).collect();
        // home leads the raw vec; the branch trails it.
        assert_eq!(names, vec!["washu/home", "washu/feat"]);
        assert_eq!(
            s.active_group().unwrap().name,
            "washu/home",
            "active group (home) survives the rename"
        );
    }

    #[test]
    fn resurrect_skips_home_rename_that_would_collide() {
        let db = temp_db();
        let repo = "/r/WASHU";
        let legacy = Session {
            id: repo.into(),
            worktrees: vec![
                WorktreeGroup::new("washu/home", GroupKind::Home, repo),
                WorktreeGroup::new("WASHU/home", GroupKind::Home, "/r/other-checkout"),
            ],
            active: 0,
        };
        legacy.persist(&db, repo, 1).unwrap();

        let s = Session::resurrect(&db, repo).unwrap();
        let names: Vec<_> = s.worktrees.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["washu/home", "WASHU/home"],
            "a rename that would duplicate an existing group name is skipped"
        );
    }
}
