//! The full-screen in-app worktree diff viewer. `Alt /` (the `Diff` action)
//! opens it when no external `diff` tool is configured; a configured tool still
//! wins so power users keep their delta/difftastic setup.
//!
//! It is a read-only slice of [`crate::pr_view`]'s Files tab: the same
//! file-list ↔ expanded-file navigation, the same `diff_line` renderer, the
//! same async load pattern (the diff lands over `diff_view_tx` after the modal
//! opens, so the loop never blocks on git). It shows the worktree's full delta
//! against its branch point — the exact range `thegn diff` prints — including
//! uncommitted work.
//!
//! Lifecycle mirrors [`crate::pr_view::PrView`]: an `Option<DiffView>` slot in
//! the loop, fed `handle_key`, painted last via `render`, dismissed on Esc/q.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::pr_view::{diff_line, file_stat, sel_marker, trunc};
use crate::seg::{Line, Tok, seg};
use thegn_core::github::{DiffLine, PrDiff};

/// Async-loaded diff delivered over `diff_view_tx` after the view opens. Stale
/// generations are dropped by the loop.
#[derive(Debug, Clone)]
pub struct DiffViewData {
    pub generation: u64,
    pub diff: Option<PrDiff>,
}

/// What a key delivered to the view meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffViewOutcome {
    /// Consumed; nothing else to do.
    Pending,
    /// Close the view.
    Close,
}

/// The read-only worktree diff modal.
pub struct DiffView {
    pub generation: u64,
    title: String,
    /// Async-loaded; `None` while the git diff is still being read.
    diff: Option<PrDiff>,
    /// Cursor row (index into files, or into the open file's diff lines).
    sel: usize,
    scroll: std::cell::Cell<usize>,
    /// `None` = file list; `Some(i)` = expanded file `i`.
    open_file: Option<usize>,
}

impl DiffView {
    pub fn new(title: String, generation: u64) -> Self {
        Self {
            generation,
            title,
            diff: None,
            sel: 0,
            scroll: std::cell::Cell::new(0),
            open_file: None,
        }
    }

    /// Fold a delivered diff in (the loop guards `generation` first).
    pub fn apply_data(&mut self, data: DiffViewData) {
        self.diff = data.diff;
        let n = self.row_count();
        if self.sel >= n {
            self.sel = n.saturating_sub(1);
        }
    }

    // --- navigation model --------------------------------------------------

    fn row_count(&self) -> usize {
        match self.open_file {
            None => self.diff.as_ref().map_or(0, |d| d.files.len()),
            Some(i) => self.open_file_lines(i).len(),
        }
    }

    /// The flattened diff lines of file `i` (the open-file selectable rows).
    fn open_file_lines(&self, i: usize) -> Vec<&DiffLine> {
        self.diff
            .as_ref()
            .and_then(|d| d.files.get(i))
            .map(|f| f.hunks.iter().flat_map(|h| &h.lines).collect())
            .unwrap_or_default()
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let cur = self.sel as isize;
        self.sel = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    // --- input -------------------------------------------------------------

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> DiffViewOutcome {
        if mods.contains(Modifiers::CTRL) && matches!(key, KeyCode::Char('c' | 'C' | 'g' | 'G')) {
            return DiffViewOutcome::Close;
        }
        match key {
            KeyCode::Char('q') => DiffViewOutcome::Close,
            KeyCode::Escape => {
                // In an expanded file, Esc collapses back to the file list first.
                if self.open_file.take().is_some() {
                    self.sel = 0;
                    self.scroll.set(0);
                    DiffViewOutcome::Pending
                } else {
                    DiffViewOutcome::Close
                }
            }
            KeyCode::Char('j') | KeyCode::DownArrow => {
                self.move_sel(1);
                DiffViewOutcome::Pending
            }
            KeyCode::Char('k') | KeyCode::UpArrow => {
                self.move_sel(-1);
                DiffViewOutcome::Pending
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.move_sel(10);
                DiffViewOutcome::Pending
            }
            KeyCode::PageUp => {
                self.move_sel(-10);
                DiffViewOutcome::Pending
            }
            KeyCode::Char('g') => {
                self.sel = 0;
                self.scroll.set(0);
                DiffViewOutcome::Pending
            }
            KeyCode::Char('G') => {
                self.sel = self.row_count().saturating_sub(1);
                DiffViewOutcome::Pending
            }
            KeyCode::Enter | KeyCode::RightArrow => {
                if self.open_file.is_none()
                    && self.diff.as_ref().is_some_and(|d| self.sel < d.files.len())
                {
                    self.open_file = Some(self.sel);
                    self.sel = 0;
                    self.scroll.set(0);
                }
                DiffViewOutcome::Pending
            }
            KeyCode::LeftArrow => {
                if self.open_file.take().is_some() {
                    self.sel = 0;
                    self.scroll.set(0);
                }
                DiffViewOutcome::Pending
            }
            _ => DiffViewOutcome::Pending,
        }
    }

    // --- rendering ---------------------------------------------------------

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let cols = screen.cols.saturating_sub(8).max(24);
        let rows = screen.rows.saturating_sub(4).max(10);
        let spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(" esc ".into()),
            cols,
            rows,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Accent),
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let pad = Tok::Slot(S::Panel);
        // Footer (last row).
        let footer_y = inner.y + inner.rows.saturating_sub(1);
        crate::seg::draw_line(surface, inner.x, footer_y, inner.cols, &self.footer(), pad);

        // Body above the footer.
        let body_rows = inner.rows.saturating_sub(1);
        if body_rows == 0 {
            return;
        }
        let body = self.body_lines(inner.cols);
        let sel_line = body.iter().position(|(_, s)| *s).unwrap_or(0);
        let scroll = self.clamp_scroll(sel_line, body.len(), body_rows);
        for row in 0..body_rows {
            let y = inner.y + row;
            match body.get(scroll + row) {
                Some((line, selected)) => {
                    let bg = if *selected { Tok::SelAccent } else { pad };
                    crate::seg::draw_line(surface, inner.x, y, inner.cols, line, bg);
                }
                None => crate::seg::draw_line(surface, inner.x, y, inner.cols, &Line::Blank, pad),
            }
        }
    }

    /// Keep the selected line in view; returns the (persisted) scroll offset.
    fn clamp_scroll(&self, sel_line: usize, total: usize, visible: usize) -> usize {
        let mut s = self.scroll.get();
        if visible == 0 {
            return 0;
        }
        if sel_line < s {
            s = sel_line;
        } else if sel_line >= s + visible {
            s = sel_line + 1 - visible;
        }
        let max = total.saturating_sub(visible);
        s = s.min(max);
        self.scroll.set(s);
        s
    }

    fn footer(&self) -> Line {
        let hint = if self.open_file.is_some() {
            "↑↓ move · ← back · q/esc close"
        } else {
            "↑↓ move · Enter open file · q/esc close"
        };
        Line::segs(vec![seg(Tok::Slot(S::Dim), hint)])
    }

    fn body_lines(&self, cols: usize) -> Vec<(Line, bool)> {
        let mut out = vec![(Line::Blank, false)];
        let Some(diff) = &self.diff else {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "Loading diff…")]),
                false,
            ));
            return out;
        };
        if diff.files.is_empty() {
            out.push((
                Line::segs(vec![seg(
                    Tok::Slot(S::Dim),
                    "No changes against the branch point.",
                )]),
                false,
            ));
            return out;
        }
        match self.open_file {
            None => {
                for (i, f) in diff.files.iter().enumerate() {
                    let selected = i == self.sel;
                    let (adds, dels) = file_stat(f);
                    out.push((
                        Line::split(
                            vec![
                                seg(Tok::Slot(S::Faint), sel_marker(selected)),
                                seg(Tok::Slot(S::Text), f.path.clone()),
                            ],
                            vec![
                                seg(Tok::Hue(thegn_core::theme::Hue::Green), format!("+{adds} ")),
                                seg(Tok::Hue(thegn_core::theme::Hue::Red), format!("-{dels}")),
                            ],
                        ),
                        selected,
                    ));
                }
            }
            Some(fi) => {
                if let Some(f) = diff.files.get(fi) {
                    out.push((
                        Line::segs(vec![seg(Tok::Slot(S::Text), f.path.clone()).bold()]),
                        false,
                    ));
                    let mut li = 0usize; // index into flattened selectable lines
                    for h in &f.hunks {
                        out.push((
                            Line::segs(vec![seg(
                                Tok::Hue(thegn_core::theme::Hue::Teal),
                                trunc(&h.header, cols),
                            )]),
                            false,
                        ));
                        for dl in &h.lines {
                            let selected = li == self.sel;
                            out.push((diff_line(dl, selected, cols), selected));
                            li += 1;
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::github::{DiffFile, DiffHunk, DiffLineKind};

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.into(),
            new_lineno: Some(1),
            old_lineno: Some(1),
        }
    }

    fn sample() -> PrDiff {
        PrDiff {
            files: vec![
                DiffFile {
                    path: "a.rs".into(),
                    old_path: None,
                    hunks: vec![DiffHunk {
                        header: "@@ -1 +1 @@".into(),
                        lines: vec![
                            line(DiffLineKind::Context, " ctx"),
                            line(DiffLineKind::Add, "+new"),
                        ],
                    }],
                },
                DiffFile {
                    path: "b.rs".into(),
                    old_path: None,
                    hunks: vec![],
                },
            ],
        }
    }

    #[test]
    fn navigation_enters_and_leaves_a_file() {
        let mut v = DiffView::new("t".into(), 1);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
        });
        // File list: two files.
        assert_eq!(v.row_count(), 2);
        // Enter opens file 0 → its two diff lines become the rows.
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(v.open_file, Some(0));
        assert_eq!(v.row_count(), 2);
        // Left collapses back to the file list.
        v.handle_key(&KeyCode::LeftArrow, Modifiers::NONE);
        assert_eq!(v.open_file, None);
        assert_eq!(v.row_count(), 2);
    }

    #[test]
    fn esc_collapses_then_closes() {
        let mut v = DiffView::new("t".into(), 1);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
        });
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(v.open_file, Some(0));
        // First Esc collapses the open file…
        assert_eq!(
            v.handle_key(&KeyCode::Escape, Modifiers::NONE),
            DiffViewOutcome::Pending
        );
        // …second Esc closes.
        assert_eq!(
            v.handle_key(&KeyCode::Escape, Modifiers::NONE),
            DiffViewOutcome::Close
        );
    }
}
