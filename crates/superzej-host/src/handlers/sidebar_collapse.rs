//! Sidebar collapse helpers: fold a leaf's parent group, and auto-reveal the
//! active worktree. Extracted from `run.rs` (pinned by the file-size ratchet).
//!
//! - **`h`/`←` on a leaf** (`collapse_parent`) walks up to the nearest
//!   collapsible ancestor (a filed worktree → its 📂 folder, a loose worktree →
//!   its workspace, a terminal → its host) and folds it — the standard tree
//!   gesture where pressing collapse on a child collapses its parent.
//! - **Alt / Shift+Alt navigation** (`reveal_active_worktree`) un-collapses
//!   whatever group(s) hide the newly-active worktree so the switch is visible.
//!
//! The testable primitives (`parent_collapsible_index`, `active_reveal_keys`)
//! live in `crate::sidebar`; these methods are the thin `SidebarState` wrappers
//! that mutate the collapse set, persist, rebuild, and re-land the cursor.

use crate::chrome::FrameModel;
use crate::run::{SidebarOutcome, SidebarState, visible_index_of_active};

impl SidebarState {
    /// Collapse the nearest collapsible **ancestor** of the cursor row and move
    /// the cursor onto it. If that ancestor is already collapsed, just walk the
    /// cursor up to it (vim-like). No-op at the top of the tree.
    pub(crate) fn collapse_parent(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> SidebarOutcome {
        let (parent, already_collapsed) = {
            let visible: Vec<&crate::sidebar::SidebarRow> =
                model.sidebar_rows.iter().filter(|r| r.visible).collect();
            match crate::sidebar::parent_collapsible_index(&visible, self.cursor) {
                Some(p) => (p, visible[p].collapsed),
                None => return SidebarOutcome::Redraw,
            }
        };
        self.cursor = parent;
        if already_collapsed {
            self.sync(model);
            SidebarOutcome::Redraw
        } else {
            self.toggle_collapse(model, session)
        }
    }

    /// Un-collapse whatever group(s) hide the session's active worktree — its
    /// workspace, and (if the worktree is filed) its 📂 folder — delete the
    /// persisted collapse keys, rebuild rows, and land the cursor on the
    /// now-visible active row. No-op when the active group isn't a worktree
    /// (e.g. a terminal) or nothing was collapsed. Returns whether any collapse
    /// key was cleared.
    pub(crate) fn reveal_active_worktree(
        &mut self,
        model: &mut FrameModel,
        session: &crate::session::Session,
    ) -> bool {
        let Some(g) = session.worktrees.get(session.active) else {
            return false;
        };
        let keys = crate::sidebar::active_reveal_keys(&g.name, &model.sidebar_db_worktrees);
        let mut changed = false;
        for key in keys {
            if self.view.collapsed.remove(&key) {
                // Expanded is the default state: delete the key, don't tombstone.
                self.unpersist(&format!("collapse:{key}"));
                changed = true;
            }
        }
        if changed {
            self.rebuild(model, session);
            self.cursor = visible_index_of_active(model);
            self.sync(model);
        }
        changed
    }
}
