//! The host's session model: the ordered tab list and which is active, plus the
//! bridge to SQLite for resurrect/persist. The native host owns layout (zellij
//! did before); this is what rebuilds the workspace on cold start.
//!
//! Tab ↔ row conversion is pure (and unit-tested); the DB calls are a thin shell
//! over `superzej_core::db`'s v4 `tab_layout`/`session_state`.

use anyhow::Result;
use superzej_core::db::Db;
use superzej_core::models::TabLayoutRow;

use crate::center::CenterTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Home,
    Worktree,
    Extra,
    Pinned,
}

impl TabKind {
    fn as_str(self) -> &'static str {
        match self {
            TabKind::Home => "home",
            TabKind::Worktree => "worktree",
            TabKind::Extra => "extra",
            TabKind::Pinned => "pinned",
        }
    }
    fn parse(s: &str) -> TabKind {
        match s {
            "home" => TabKind::Home,
            "extra" => TabKind::Extra,
            "pinned" => TabKind::Pinned,
            _ => TabKind::Worktree,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub name: String,
    pub kind: TabKind,
    /// Owning worktree path (empty for home/pinned).
    pub worktree: String,
    pub center: CenterTree,
    pub focused_pane: u32,
}

impl Tab {
    /// Reconstruct a tab from its persisted row. A malformed `pane_tree` falls
    /// back to a single pane rather than failing the whole resurrect.
    pub fn from_row(row: &TabLayoutRow) -> Tab {
        let center =
            serde_json::from_str::<CenterTree>(&row.pane_tree).unwrap_or(CenterTree::Leaf(0));
        Tab {
            name: row.tab_name.clone(),
            kind: TabKind::parse(&row.kind),
            worktree: row.worktree.clone(),
            center,
            focused_pane: row.focused_pane.max(0) as u32,
        }
    }

    /// Serialize this tab to a persistable row at the given display order.
    pub fn to_row(&self, ordinal: i64) -> TabLayoutRow {
        TabLayoutRow {
            tab_name: self.name.clone(),
            kind: self.kind.as_str().to_string(),
            worktree: self.worktree.clone(),
            pane_tree: serde_json::to_string(&self.center).unwrap_or_else(|_| "0".into()),
            ordinal,
            focused_pane: self.focused_pane as i64,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl Session {
    /// Rebuild the session from the DB (cold-start resurrect). Tabs come back in
    /// persisted order; the active tab is restored from `session_state`.
    pub fn resurrect(db: &Db, session: &str) -> Result<Session> {
        let mut tabs: Vec<Tab> = db
            .tabs_for_session(session)?
            .iter()
            .map(Tab::from_row)
            .collect();

        // Also fetch any worktrees recorded for this session that aren't in tab_layout yet.
        // This handles migrating/restoring state from older sessions where tab_layout wasn't used.
        if let Ok(wts) = db.worktrees() {
            let session_path = std::path::Path::new(session);
            let slug = if session_path.is_absolute() {
                superzej_core::repo::repo_slug(session_path)
            } else {
                session.to_string()
            };
            for wt in wts {
                if wt.session_name == session && !tabs.iter().any(|t| t.name == wt.tab_name) {
                    tabs.push(Tab {
                        name: wt.tab_name.clone(),
                        kind: TabKind::Worktree,
                        worktree: wt.worktree.clone(),
                        center: CenterTree::Leaf(0),
                        focused_pane: 0,
                    });
                } else if wt.session_name.is_empty()
                    && wt.repo_root == session
                    && wt.tab_name.starts_with(&slug)
                    && !tabs.iter().any(|t| t.name == wt.tab_name)
                {
                    // Pre-v3 or missing session_name fallback
                    tabs.push(Tab {
                        name: wt.tab_name.clone(),
                        kind: TabKind::Worktree,
                        worktree: wt.worktree.clone(),
                        center: CenterTree::Leaf(0),
                        focused_pane: 0,
                    });
                }
            }
        }

        let active = db
            .active_tab(session)?
            .and_then(|name| tabs.iter().position(|t| t.name == name))
            .unwrap_or(0);
        Ok(Session {
            id: session.to_string(),
            tabs,
            active,
        })
    }

    /// Persist the full tab set + active tab (debounced by the caller on layout
    /// changes — not per keystroke).
    pub fn persist(&self, db: &Db, session: &str, now: i64) -> Result<()> {
        for (i, tab) in self.tabs.iter().enumerate() {
            db.put_tab_layout(session, &tab.to_row(i as i64))?;
        }
        if let Some(active) = self.tabs.get(self.active) {
            db.set_active_tab(session, &active.name, now)?;
        }
        Ok(())
    }

    /// The active tab, if any. (Convenience used by tests; the loop indexes
    /// `tabs[active]` directly for mutable access.)
    #[allow(dead_code)]
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Append a tab and focus it; returns its index.
    pub fn add_tab(&mut self, tab: Tab) -> usize {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.active
    }

    /// Focus the tab at `idx` (clamped); no-op if empty. (Wired to the palette's
    /// tab-nav dispatch.)
    pub fn switch_to(&mut self, idx: usize) {
        if !self.tabs.is_empty() {
            self.active = idx.min(self.tabs.len() - 1);
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
        let mut tabs = new_session.tabs;
        let active = new_session.active;

        if tabs.is_empty() {
            let base = std::path::Path::new(repo_path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into());
            tabs.push(Tab {
                name: format!("{base}/home"),
                kind: TabKind::Home,
                worktree: repo_path.to_string(),
                center: CenterTree::Leaf(0),
                focused_pane: 0,
            });
            let _ = db.put_workspace(repo_path, &base);
            let _ = db.touch_repo(repo_path, &base);
        }

        self.id = repo_path.to_string();
        self.tabs = tabs;
        self.active = active;
        self.persist(db, &self.id, now)?;

        Ok(())
    }

    /// Cycle focus to the next tab (wraps).
    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// Cycle focus to the previous tab (wraps).
    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    /// Close the active tab; focus the previous one. Returns the removed tab.
    pub fn close_active(&mut self) -> Option<Tab> {
        if self.tabs.is_empty() {
            return None;
        }
        let removed = self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        Some(removed)
    }
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
    fn tab_row_roundtrip_preserves_tree_and_kind() {
        let tab = Tab {
            name: "app/feat".into(),
            kind: TabKind::Worktree,
            worktree: "/wt/feat".into(),
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
        };
        let back = Tab::from_row(&tab.to_row(3));
        assert_eq!(tab, back);
    }

    #[test]
    fn malformed_pane_tree_degrades_to_single_pane() {
        let row = TabLayoutRow {
            tab_name: "x/home".into(),
            kind: "home".into(),
            worktree: String::new(),
            pane_tree: "this is not json".into(),
            ordinal: 0,
            focused_pane: 0,
        };
        let tab = Tab::from_row(&row);
        assert_eq!(tab.center, CenterTree::Leaf(0));
        assert_eq!(tab.kind, TabKind::Home);
    }

    fn tab(name: &str) -> Tab {
        Tab {
            name: name.into(),
            kind: TabKind::Worktree,
            worktree: format!("/wt/{name}"),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        }
    }

    #[test]
    fn tab_ops_add_switch_cycle_and_close() {
        let mut s = Session::default();
        s.add_tab(tab("a"));
        s.add_tab(tab("b"));
        s.add_tab(tab("c"));
        assert_eq!(s.active_tab().unwrap().name, "c"); // add focuses

        s.next_tab(); // wraps c -> a
        assert_eq!(s.active_tab().unwrap().name, "a");
        s.prev_tab(); // wraps a -> c
        assert_eq!(s.active_tab().unwrap().name, "c");
        s.switch_to(1);
        assert_eq!(s.active_tab().unwrap().name, "b");

        let removed = s.close_active().unwrap();
        assert_eq!(removed.name, "b");
        assert_eq!(s.tabs.len(), 2);
        // Focus stays valid (now at index 1 -> "c").
        assert_eq!(s.active_tab().unwrap().name, "c");

        s.close_active();
        s.close_active();
        assert!(s.active_tab().is_none(), "empty session has no active tab");
        assert_eq!(s.close_active(), None);
    }

    #[test]
    fn persist_then_resurrect_reproduces_the_session() {
        let db = temp_db();
        let sess = "s";
        let make = |name: &str, kind: TabKind, pane: u32| Tab {
            name: name.into(),
            kind,
            worktree: format!("/wt/{name}"),
            center: CenterTree::Leaf(pane),
            focused_pane: pane,
        };
        let session = Session {
            id: sess.to_string(),
            tabs: vec![
                make("app/home", TabKind::Home, 0),
                make("app/feat", TabKind::Worktree, 2),
            ],
            active: 1,
        };
        session.persist(&db, sess, 1234).unwrap();

        let back = Session::resurrect(&db, sess).unwrap();
        assert_eq!(back.tabs, session.tabs);
        assert_eq!(back.active, 1, "active tab restored from session_state");
    }
}
