//! The rollback / discard window (item 604): a dedicated modal listing every
//! working-tree change with a checkbox, a per-row preview of the cursor's
//! change, and a bulk-discard that partitions tracked files (restored to HEAD)
//! from untracked ones (deleted). A destructive op gets its own confirm
//! boundary, away from the interaction-dense changes panel.
//!
//! The state machine is pure and unit-tested; `render` paints a centered layer
//! and takes the cursor row's preview lines as data so the module stays
//! decoupled from the panel's hunk cache.

use std::collections::BTreeSet;

use termwiz::input::{KeyCode, Modifiers};

use crate::chrome::S;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::panel::{ChangeRow, Stage};
use crate::seg::{self, Line, Tok, seg, sp};
use termwiz::surface::Surface;

use crate::compositor::Rect;

/// One selectable change in the rollback window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackRow {
    pub path: String,
    /// Porcelain status glyph ("M", "A", "?", …) for display.
    pub status: String,
    /// Untracked files are *deleted* on discard (`clean -f`); tracked ones are
    /// restored (`checkout --`).
    pub untracked: bool,
}

/// What a key meant to the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackOutcome {
    Pending,
    Cancel,
    /// Discard the marked rows.
    Confirm,
}

/// The rollback window's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackModal {
    pub rows: Vec<RollbackRow>,
    pub marked: BTreeSet<usize>,
    pub cursor: usize,
}

impl RollbackModal {
    /// Build from the changes section's rows. Conflicted entries are excluded —
    /// discarding mid-conflict is a separate, more dangerous operation.
    pub fn from_changes(changes: &[ChangeRow]) -> Self {
        let rows = changes
            .iter()
            .filter(|c| c.stage != Stage::Conflict)
            .map(|c| RollbackRow {
                path: c.path.clone(),
                status: c.status.clone(),
                untracked: c.stage == Stage::Untracked,
            })
            .collect();
        RollbackModal {
            rows,
            marked: BTreeSet::new(),
            cursor: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    /// Toggle the mark on the cursor row.
    pub fn toggle(&mut self) {
        if self.cursor < self.rows.len() && !self.marked.remove(&self.cursor) {
            self.marked.insert(self.cursor);
        }
    }

    pub fn select_all(&mut self) {
        self.marked = (0..self.rows.len()).collect();
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// The `(path, untracked)` pairs to discard — the marked rows, or (when
    /// nothing is marked) the cursor row as a convenience.
    pub fn marked_paths(&self) -> Vec<(String, bool)> {
        let idxs: Vec<usize> = if !self.marked.is_empty() {
            self.marked.iter().copied().collect()
        } else if self.rows.is_empty() {
            Vec::new()
        } else {
            vec![self.cursor.min(self.rows.len() - 1)]
        };
        idxs.into_iter()
            .filter_map(|i| self.rows.get(i))
            .map(|r| (r.path.clone(), r.untracked))
            .collect()
    }

    /// How many untracked files the current selection would delete.
    pub fn untracked_in_selection(&self) -> usize {
        self.marked_paths().iter().filter(|(_, u)| *u).count()
    }

    /// Build the cursor row's preview lines from the panel hunk cache. Untracked
    /// rows have no diff; tracked rows show their cached hunks (header + a few
    /// signed lines), or a "no cached diff" note when the cache is cold.
    pub fn preview_for(
        &self,
        hunks: &std::collections::HashMap<String, Vec<superzej_svc::git::Hunk>>,
    ) -> Vec<Line> {
        let Some(row) = self.rows.get(self.cursor) else {
            return Vec::new();
        };
        if row.untracked {
            return vec![Line::segs(vec![
                seg(Tok::Slot(S::Ghost2), "untracked"),
                seg(Tok::Slot(S::Ghost), " · whole file deleted"),
            ])];
        }
        match hunks.get(&row.path) {
            Some(hs) if !hs.is_empty() => {
                let mut lines = Vec::new();
                for h in hs.iter().take(2) {
                    lines.push(Line::segs(vec![seg(
                        Tok::Slot(S::Ghost2),
                        h.header.clone(),
                    )]));
                    for (origin, text) in h.lines.iter().take(4) {
                        let (tok, mark) = match origin {
                            '+' => (Tok::Hue(superzej_core::theme::Hue::Green), "+ "),
                            '-' => (Tok::Hue(superzej_core::theme::Hue::Red), "− "),
                            _ => (Tok::Slot(S::Ghost), "  "),
                        };
                        lines.push(Line::segs(vec![
                            seg(Tok::Slot(S::Ghost3), mark),
                            seg(tok, text.clone()),
                        ]));
                    }
                }
                lines
            }
            _ => vec![Line::segs(vec![seg(
                Tok::Slot(S::Ghost2),
                "no cached diff",
            )])],
        }
    }

    pub fn handle_key(&mut self, key: &KeyCode, _mods: Modifiers) -> RollbackOutcome {
        if crate::input::is_escape_key(key) {
            return RollbackOutcome::Cancel;
        }
        match key {
            KeyCode::Char('j') | KeyCode::DownArrow => {
                self.move_down();
                RollbackOutcome::Pending
            }
            KeyCode::Char('k') | KeyCode::UpArrow => {
                self.move_up();
                RollbackOutcome::Pending
            }
            KeyCode::Char(' ') => {
                self.toggle();
                RollbackOutcome::Pending
            }
            KeyCode::Char('a') => {
                self.select_all();
                RollbackOutcome::Pending
            }
            KeyCode::Char('c') => {
                self.clear_marks();
                RollbackOutcome::Pending
            }
            KeyCode::Enter => RollbackOutcome::Confirm,
            _ => RollbackOutcome::Pending,
        }
    }

    /// Paint the modal. `preview` is the pre-rendered diff preview for the
    /// cursor row (supplied by the caller from the hunk cache); empty when none.
    pub fn render(&self, surface: &mut Surface, screen: Rect, preview: &[Line]) {
        const COLS: usize = 76;
        let list_rows = self.rows.len().min(10);
        let preview_rows = preview.len().min(8);
        // Surface the irreversible part: untracked files are *deleted*, not
        // restorable from HEAD, so call out their count in the badge.
        let deletes = self.untracked_in_selection();
        let badge = if deletes > 0 {
            format!(" {} marked · {deletes} delete ", self.marked.len())
        } else {
            format!(" {} marked ", self.marked.len())
        };
        let spec = LayerSpec {
            title: "rollback changes".into(),
            badge: Some(badge),
            cols: COLS,
            // prompt? no — header + list + rule + preview + rule + footer
            rows: list_rows + preview_rows + 4,
            anchor: Anchor::Center,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        let mut y = inner.y;

        // Rows: checkbox + status + path; cursor row tinted.
        let offset = self.cursor.saturating_sub(list_rows.saturating_sub(1));
        for (row, r) in self.rows.iter().enumerate().skip(offset).take(list_rows) {
            let checked = self.marked.contains(&row);
            let box_glyph = if checked { "☑ " } else { "☐ " };
            let status_tok = if r.untracked {
                Tok::Slot(S::Ghost2)
            } else {
                Tok::Slot(S::Text)
            };
            let mut segs = vec![
                sp(1),
                seg(Tok::Slot(S::Accent), box_glyph),
                seg(status_tok, format!("{:<2} ", r.status)),
                seg(Tok::Slot(S::Text), r.path.clone()),
            ];
            if r.untracked {
                segs.push(seg(Tok::Slot(S::Ghost2), "  (delete)"));
            }
            let bg = if row == self.cursor {
                Tok::SelAccent
            } else {
                panel
            };
            seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(segs), bg);
            y += 1;
        }

        if preview_rows > 0 && y < inner.y + inner.rows {
            seg::draw_line(surface, inner.x, y, inner.cols, &rule, panel);
            y += 1;
            for pl in preview.iter().take(preview_rows.saturating_sub(1)) {
                seg::draw_line(surface, inner.x, y, inner.cols, pl, panel);
                y += 1;
            }
        }

        // Footer.
        let fy = inner.y + inner.rows - 1;
        let footer = Line::segs(vec![
            seg(Tok::Slot(S::Ghost2), "space"),
            seg(Tok::Slot(S::Ghost), " mark  "),
            seg(Tok::Slot(S::Ghost2), "a"),
            seg(Tok::Slot(S::Ghost), " all  "),
            seg(Tok::Slot(S::Ghost2), "c"),
            seg(Tok::Slot(S::Ghost), " clear  "),
            seg(Tok::Slot(S::Ghost2), "↵"),
            seg(Tok::Slot(S::Ghost), " discard  "),
            seg(Tok::Slot(S::Ghost2), "esc"),
            seg(Tok::Slot(S::Ghost), " cancel"),
        ]);
        seg::draw_line(surface, inner.x, fy, inner.cols, &footer, panel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, stage: Stage) -> ChangeRow {
        ChangeRow {
            status: "M".into(),
            stage,
            dir: String::new(),
            name: path.into(),
            path: path.into(),
            added: 0,
            deleted: 0,
        }
    }

    fn modal() -> RollbackModal {
        RollbackModal::from_changes(&[
            change("a.rs", Stage::Unstaged),
            change("b.rs", Stage::Staged),
            change("new.txt", Stage::Untracked),
            change("conflict.rs", Stage::Conflict),
        ])
    }

    #[test]
    fn from_changes_excludes_conflicts_and_flags_untracked() {
        let m = modal();
        assert_eq!(m.rows.len(), 3, "conflict row dropped");
        assert!(!m.rows[0].untracked);
        assert!(m.rows[2].untracked, "new.txt is untracked");
    }

    #[test]
    fn toggle_and_select_all_and_clear() {
        let mut m = modal();
        m.toggle(); // mark row 0
        assert!(m.marked.contains(&0));
        m.toggle(); // unmark row 0
        assert!(m.marked.is_empty());
        m.select_all();
        assert_eq!(m.marked.len(), 3);
        m.clear_marks();
        assert!(m.marked.is_empty());
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let mut m = modal();
        m.move_up();
        assert_eq!(m.cursor, 0, "clamps at top");
        m.move_down();
        m.move_down();
        m.move_down(); // only 3 rows → clamps at 2
        assert_eq!(m.cursor, 2);
    }

    #[test]
    fn marked_paths_partitions_tracked_and_untracked() {
        let mut m = modal();
        m.select_all();
        let paths = m.marked_paths();
        assert_eq!(paths.len(), 3);
        // a.rs/b.rs tracked (false), new.txt untracked (true).
        assert!(paths.contains(&("a.rs".to_string(), false)));
        assert!(paths.contains(&("new.txt".to_string(), true)));
        assert_eq!(m.untracked_in_selection(), 1);
    }

    #[test]
    fn marked_paths_falls_back_to_cursor_when_nothing_marked() {
        let mut m = modal();
        m.move_down(); // cursor on b.rs
        assert_eq!(m.marked_paths(), vec![("b.rs".to_string(), false)]);
    }

    #[test]
    fn keys_drive_the_state_machine() {
        let mut m = modal();
        assert_eq!(
            m.handle_key(&KeyCode::Char(' '), Modifiers::NONE),
            RollbackOutcome::Pending
        );
        assert!(m.marked.contains(&0));
        assert_eq!(
            m.handle_key(&KeyCode::Enter, Modifiers::NONE),
            RollbackOutcome::Confirm
        );
        assert_eq!(
            m.handle_key(&KeyCode::Escape, Modifiers::NONE),
            RollbackOutcome::Cancel
        );
    }
}
