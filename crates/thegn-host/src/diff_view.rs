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
use crate::review_rows::{
    ReviewRow, expanded_file_rows, feedback_rows, file_stat, render_review_row, sel_marker,
    top_level_feedback_lines,
};
use crate::seg::{Line, Tok, Under, seg};
use thegn_core::ansi_cells::StyledLine;
use thegn_core::forge::model::{DiffLine, PrDiff};

/// A structural render result: the styled lines difft produced, or a one-line
/// notice to show above the internal view when difft could not be used.
pub type StructuralResult = Result<Vec<StyledLine>, String>;

/// Async-loaded diff delivered over `diff_view_tx` after the view opens. Stale
/// generations are dropped by the loop.
#[derive(Debug, Clone)]
pub struct DiffViewData {
    pub generation: u64,
    pub diff: Option<PrDiff>,
    /// Structural (difftastic) render, when structural mode was requested:
    /// `Some(Ok(lines))` renders structurally, `Some(Err(notice))` falls back to
    /// the internal view with the notice, `None` = structural was not attempted.
    pub structural: Option<StructuralResult>,
    pub review: Option<thegn_core::review::PrReviewSnapshot>,
    pub review_status: Option<String>,
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
    /// Whether structural rendering was requested for this view (governs whether
    /// the toggle key does anything and whether "Loading…" mentions difft).
    want_structural: bool,
    /// The delivered structural render (or fallback notice); `None` until loaded.
    structural: Option<StructuralResult>,
    /// Toggle: show the structural render vs the internal unified view.
    show_structural: bool,
    /// Independent scroll for the flat structural pane.
    structural_scroll: std::cell::Cell<usize>,
    review: Option<thegn_core::review::PrReviewSnapshot>,
    review_status: Option<String>,
    source: DiffSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffSource {
    Worktree,
    PrReview,
}

impl DiffView {
    /// Open a view, declaring whether structural rendering was requested (so the
    /// toggle is live and the loading hint is accurate). Structural content
    /// arrives later over `apply_data`, exactly like the internal diff.
    pub fn with_structural(title: String, generation: u64, want_structural: bool) -> Self {
        Self {
            generation,
            title,
            diff: None,
            sel: 0,
            scroll: std::cell::Cell::new(0),
            open_file: None,
            want_structural,
            structural: None,
            review: None,
            review_status: None,
            // Prefer structural when it was requested; a failure flips this off.
            show_structural: want_structural,
            structural_scroll: std::cell::Cell::new(0),
            source: DiffSource::Worktree,
        }
    }

    /// Fold a delivered diff in (the loop guards `generation` first).
    pub fn apply_data(&mut self, data: DiffViewData) {
        self.diff = data.diff;
        if let Some(structural) = data.structural {
            // A structural failure falls back to the internal view; a success
            // shows structurally (respecting a user's prior toggle-off).
            if structural.is_err() {
                self.show_structural = false;
            }
            self.structural = Some(structural);
        }
        if let Some(review) = data.review {
            self.set_review(Some(review), data.review_status.clone());
        }
        self.review_status = data.review_status;
        let n = self.row_count();
        if self.sel >= n {
            self.sel = n.saturating_sub(1);
        }
    }

    pub fn set_review(
        &mut self,
        review: Option<thegn_core::review::PrReviewSnapshot>,
        status: Option<String>,
    ) -> bool {
        let source_changed = review.is_none() && self.source == DiffSource::PrReview;
        let changed = self.review != review || self.review_status != status || source_changed;
        if source_changed {
            self.source = DiffSource::Worktree;
            self.open_file = None;
            self.sel = 0;
            self.scroll.set(0);
        }
        self.review = review;
        self.review_status = status;
        changed
    }

    /// Whether the structural pane is currently the active render.
    fn structural_active(&self) -> bool {
        self.show_structural && matches!(self.structural, Some(Ok(_)))
    }

    // --- navigation model --------------------------------------------------

    fn row_count(&self) -> usize {
        match self.open_file {
            None => {
                self.active_diff().map_or(0, |d| d.files.len())
                    + if self.source == DiffSource::PrReview {
                        self.anchored_review()
                            .map_or(0, |review| feedback_rows(&review, false).len())
                    } else {
                        0
                    }
            }
            Some(i) => {
                if self.source == DiffSource::PrReview {
                    self.active_diff()
                        .and_then(|d| d.files.get(i))
                        .map(|file| {
                            expanded_file_rows(file, self.anchored_review().as_ref(), false).len()
                        })
                        .unwrap_or(0)
                } else {
                    self.open_file_lines(i).len()
                }
            }
        }
    }

    fn active_diff(&self) -> Option<&PrDiff> {
        match self.source {
            DiffSource::Worktree => self.diff.as_ref(),
            DiffSource::PrReview => self.review.as_ref().map(|r| &r.diff),
        }
    }

    fn anchored_review(&self) -> Option<thegn_core::review::AnchoredReview> {
        self.review.as_ref().map(|snapshot| {
            thegn_core::review::anchor_threads(&snapshot.diff, &snapshot.conversation.threads)
        })
    }

    /// The flattened diff lines of file `i` (the open-file selectable rows).
    fn open_file_lines(&self, i: usize) -> Vec<&DiffLine> {
        self.active_diff()
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

    /// Scroll the flat structural pane (upper bound is clamped again at render
    /// time against the visible height).
    fn scroll_structural(&self, delta: isize) -> DiffViewOutcome {
        let cur = self.structural_scroll.get() as isize;
        self.structural_scroll.set((cur + delta).max(0) as usize);
        DiffViewOutcome::Pending
    }

    // --- input -------------------------------------------------------------

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> DiffViewOutcome {
        if mods.contains(Modifiers::CTRL) && matches!(key, KeyCode::Char('c' | 'C' | 'g' | 'G')) {
            return DiffViewOutcome::Close;
        }
        // `t` toggles between the structural (difftastic) render and the internal
        // unified view — live only when a structural render actually loaded.
        if matches!(key, KeyCode::Char('t' | 'T')) && matches!(self.structural, Some(Ok(_))) {
            self.show_structural = !self.show_structural;
            return DiffViewOutcome::Pending;
        }
        // Structural output is a complete rendered view of the worktree. Do
        // not let the source label switch underneath it; return to the
        // internal view first, then Tab can select the PR projection.
        if matches!(key, KeyCode::Tab) && self.review.is_some() && !self.structural_active() {
            self.source = match self.source {
                DiffSource::Worktree => DiffSource::PrReview,
                DiffSource::PrReview => DiffSource::Worktree,
            };
            self.open_file = None;
            self.sel = 0;
            self.scroll.set(0);
            return DiffViewOutcome::Pending;
        }
        // The structural pane is a flat scrollable blob: movement scrolls it, and
        // there is no file to open.
        if self.structural_active() {
            return match key {
                KeyCode::Char('q') | KeyCode::Escape => DiffViewOutcome::Close,
                KeyCode::Char('j') | KeyCode::DownArrow => self.scroll_structural(1),
                KeyCode::Char('k') | KeyCode::UpArrow => self.scroll_structural(-1),
                KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_structural(10),
                KeyCode::PageUp => self.scroll_structural(-10),
                KeyCode::Char('g') => {
                    self.structural_scroll.set(0);
                    DiffViewOutcome::Pending
                }
                _ => DiffViewOutcome::Pending,
            };
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
                    && self.active_diff().is_some_and(|d| self.sel < d.files.len())
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
        // Structural pane: a flat, independently-scrolled styled blob.
        if self.structural_active() {
            self.render_structural(surface, inner, body_rows, pad);
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

    /// Render the flat structural (difftastic) pane: styled lines scrolled by
    /// `structural_scroll`, clamped against the visible height.
    fn render_structural(&self, surface: &mut Surface, inner: Rect, body_rows: usize, pad: Tok) {
        let Some(Ok(lines)) = &self.structural else {
            return;
        };
        let total = lines.len();
        let max = total.saturating_sub(body_rows);
        let scroll = self.structural_scroll.get().min(max);
        self.structural_scroll.set(scroll);
        for row in 0..body_rows {
            let y = inner.y + row;
            match lines.get(scroll + row) {
                Some(styled) => {
                    let line = structural_line(styled);
                    crate::seg::draw_line(surface, inner.x, y, inner.cols, &line, pad);
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
        // Offer the toggle only when a structural render actually loaded.
        let toggle = matches!(self.structural, Some(Ok(_)));
        let glyphs = crate::caps::active_glyphs();
        let movement = format!("{}{}", glyphs.arrow_up, glyphs.arrow_down);
        let separator = format!(" {} ", glyphs.middot);
        let hint = if self.structural_active() {
            if toggle {
                [
                    format!("{movement} scroll"),
                    "t internal".into(),
                    "q/esc close".into(),
                ]
                .join(&separator)
            } else {
                [format!("{movement} scroll"), "q/esc close".into()].join(&separator)
            }
        } else if self.open_file.is_some() {
            [
                format!("{movement} move"),
                "Left back".into(),
                "q/esc close".into(),
            ]
            .join(&separator)
        } else if toggle {
            [
                format!("{movement} move"),
                "Enter open".into(),
                "Tab source".into(),
                "t structural".into(),
                "q/esc close".into(),
            ]
            .join(&separator)
        } else {
            [
                format!("{movement} move"),
                "Enter open file".into(),
                "Tab source".into(),
                "q/esc close".into(),
            ]
            .join(&separator)
        };
        let source = match self.source {
            DiffSource::Worktree => "Worktree",
            DiffSource::PrReview => "PR review",
        };
        let stale = self.review_status.as_deref().unwrap_or("");
        Line::segs(vec![seg(
            Tok::Slot(S::Dim),
            format!("{source}{separator}{hint} {stale}"),
        )])
    }

    fn body_lines(&self, cols: usize) -> Vec<(Line, bool)> {
        let mut out = vec![(Line::Blank, false)];
        // A structural failure shows the internal view under a one-line notice.
        if let Some(Err(notice)) = &self.structural {
            out.push((
                Line::segs(vec![seg(
                    Tok::Hue(thegn_core::theme::Hue::Amber),
                    notice.clone(),
                )]),
                false,
            ));
            out.push((Line::Blank, false));
        }
        let Some(diff) = self.active_diff() else {
            let msg = if self.want_structural && self.structural.is_none() {
                "Loading structural diff…"
            } else {
                "Loading diff…"
            };
            out.push((Line::segs(vec![seg(Tok::Slot(S::Dim), msg)]), false));
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
            if self.source == DiffSource::Worktree {
                return out;
            }
        } else {
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
                                    seg(
                                        Tok::Hue(thegn_core::theme::Hue::Green),
                                        format!("+{adds} "),
                                    ),
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
                        if self.source == DiffSource::PrReview {
                            let review = self.anchored_review();
                            for (ri, row) in expanded_file_rows(f, review.as_ref(), false)
                                .into_iter()
                                .enumerate()
                            {
                                let selected = ri == self.sel;
                                out.extend(crate::review_rows::render_review_row(
                                    &row, selected, cols,
                                ));
                            }
                        } else {
                            // Preserve the original Worktree selection model:
                            // hunk headers render, but only diff lines consume
                            // cursor indices. PR review rows use their separate
                            // shared selectable projection above.
                            let mut line_index = 0usize;
                            for hunk in &f.hunks {
                                out.extend(crate::review_rows::render_review_row(
                                    &ReviewRow::Hunk(hunk.header.clone()),
                                    false,
                                    cols,
                                ));
                                for line in &hunk.lines {
                                    let selected = line_index == self.sel;
                                    out.extend(crate::review_rows::render_review_row(
                                        &ReviewRow::Diff(line.clone()),
                                        selected,
                                        cols,
                                    ));
                                    line_index += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        if self.source == DiffSource::PrReview {
            let snapshot = self.review.as_ref();
            if let Some(snapshot) = snapshot {
                out.extend(top_level_feedback_lines(&snapshot.conversation, cols));
            }
            if self.open_file.is_none() {
                for (i, row) in self
                    .anchored_review()
                    .map_or_else(Vec::new, |review| feedback_rows(&review, false))
                    .into_iter()
                    .enumerate()
                {
                    let selected = self
                        .active_diff()
                        .is_some_and(|diff| diff.files.len() + i == self.sel);
                    out.extend(render_review_row(&row, selected, cols));
                }
            }
        }
        out
    }
}

/// Convert one parsed structural line (SGR runs resolved to RGB) into a
/// compositor [`Line`]. Colours ride as `Tok::Rgb` — composed truecolor and
/// quantized once at the `wire.rs` chokepoint, so no colour literal at a draw
/// site. A run with no fg inherits the surface text colour.
fn structural_line(styled: &StyledLine) -> Line {
    if styled.is_empty() {
        return Line::Blank;
    }
    let segs: Vec<crate::seg::Seg> = styled
        .iter()
        .map(|run| {
            let fg = run
                .style
                .fg
                .map(|c| Tok::Rgb(c.r, c.g, c.b))
                .unwrap_or(Tok::Slot(S::Text));
            let mut s = seg(fg, run.text.clone());
            if let Some(bg) = run.style.bg {
                s = s.bg(Tok::Rgb(bg.r, bg.g, bg.b));
            }
            if run.style.bold {
                s = s.bold();
            }
            if run.style.italic {
                s = s.italic();
            }
            if run.style.underline {
                s = s.under(Under::Single);
            }
            s
        })
        .collect();
    Line::segs(segs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::forge::model::{
        DiffFile, DiffHunk, DiffLineKind, PrComment, PrConversation, PrReview, ReviewThread,
    };

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
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: None,
            review_status: None,
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
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: None,
            review_status: None,
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

    use thegn_core::ansi_cells::{CellStyle, Rgb, StyledRun};

    fn styled(text: &str) -> StyledLine {
        vec![StyledRun {
            text: text.into(),
            style: CellStyle {
                fg: Some(Rgb::new(1, 2, 3)),
                ..CellStyle::default()
            },
        }]
    }

    #[test]
    fn structural_render_and_toggle() {
        let mut v = DiffView::with_structural("t".into(), 1, true);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: Some(Ok(vec![styled("fn add"), styled("fn sub")])),
            review: None,
            review_status: None,
        });
        // Requested + loaded ⇒ structural is the active render.
        assert!(v.structural_active());
        // `t` toggles to the internal view and back.
        v.handle_key(&KeyCode::Char('t'), Modifiers::NONE);
        assert!(!v.structural_active(), "toggled to internal");
        v.handle_key(&KeyCode::Char('t'), Modifiers::NONE);
        assert!(v.structural_active(), "toggled back to structural");
        // In the flat pane, `j` scrolls rather than moving a file selection.
        assert_eq!(v.sel, 0);
        v.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
        assert_eq!(v.sel, 0, "structural pane scrolls, not selects");
    }

    #[test]
    fn structural_failure_falls_back_with_notice() {
        let mut v = DiffView::with_structural("t".into(), 1, true);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: Some(Err("difft timed out".into())),
            review: None,
            review_status: None,
        });
        // A failure never leaves the view structural — the internal view renders.
        assert!(!v.structural_active());
        // The notice is present in the body.
        let body = v.body_lines(40);
        let has_notice = body
            .iter()
            .any(|(l, _)| format!("{l:?}").contains("difft timed out"));
        assert!(has_notice, "fallback notice should render");
        // The toggle is inert (no structural render to switch to).
        v.handle_key(&KeyCode::Char('t'), Modifiers::NONE);
        assert!(!v.structural_active());
    }

    #[test]
    fn structural_mode_keeps_the_worktree_source_label_pair() {
        let mut v = DiffView::with_structural("t".into(), 1, true);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: Some(Ok(vec![styled("fn add")])),
            review: Some(thegn_core::review::PrReviewSnapshot {
                diff: sample(),
                ..Default::default()
            }),
            review_status: None,
        });

        assert!(v.structural_active());
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);
        assert_eq!(v.source, DiffSource::Worktree);
        assert!(v.structural_active());
        assert!(format!("{:?}", v.footer()).contains("Worktree"));

        v.handle_key(&KeyCode::Char('t'), Modifiers::NONE);
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);
        assert_eq!(v.source, DiffSource::PrReview);
        assert!(!v.structural_active());
        assert!(format!("{:?}", v.footer()).contains("PR review"));
    }

    #[test]
    fn late_review_delivery_can_switch_an_already_open_view() {
        let review_diff = sample();
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: None,
            review_status: None,
        });
        assert_eq!(v.row_count(), 2, "the worktree diff is available first");

        assert!(v.set_review(
            Some(thegn_core::review::PrReviewSnapshot {
                diff: review_diff,
                ..Default::default()
            }),
            None,
        ));
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);

        assert_eq!(v.source, DiffSource::PrReview);
        assert_eq!(
            v.row_count(),
            2,
            "the PR diff arrived without losing the view"
        );
    }

    #[test]
    fn stale_review_delivery_clears_the_pr_source_and_keeps_worktree_diff() {
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: Some(thegn_core::review::PrReviewSnapshot {
                diff: sample(),
                ..Default::default()
            }),
            review_status: None,
        });
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);
        assert_eq!(v.source, DiffSource::PrReview);

        assert!(v.set_review(None, Some("stale PR review snapshot".into())));
        assert_eq!(v.source, DiffSource::Worktree);
        assert_eq!(v.row_count(), 2, "the local diff remains available");
        assert!(format!("{:?}", v.footer()).contains("stale PR review snapshot"));
    }

    #[test]
    fn worktree_rows_stay_file_only_and_pr_rows_have_no_invisible_feedback() {
        let diff = sample();
        let snapshot = thegn_core::review::PrReviewSnapshot {
            diff: diff.clone(),
            conversation: PrConversation {
                threads: vec![ReviewThread {
                    id: "general".into(),
                    comments: vec![PrComment {
                        author: "reviewer".into(),
                        body: "general body".into(),
                        ..PrComment::default()
                    }],
                    ..ReviewThread::default()
                }],
                ..PrConversation::default()
            },
            ..Default::default()
        };
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(diff),
            structural: None,
            review: Some(snapshot),
            review_status: None,
        });

        assert_eq!(v.row_count(), 2, "Worktree has only its two file rows");
        let worktree_body = format!("{:?}", v.body_lines(80));
        assert!(!worktree_body.contains("general body"));

        v.handle_key(&KeyCode::Tab, Modifiers::NONE);
        assert_eq!(
            v.row_count(),
            3,
            "PR list has two files plus one feedback row"
        );
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(v.open_file, Some(0));
        assert_eq!(v.row_count(), 4, "expanded PR rows match their renderer");
        let expanded_body = format!("{:?}", v.body_lines(80));
        assert!(expanded_body.contains("general body"));
    }

    #[test]
    fn worktree_hunk_headers_do_not_shift_the_diff_line_selection() {
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: None,
            review_status: None,
        });
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);

        let first = format!(
            "{:?}",
            v.body_lines(80)
                .into_iter()
                .find(|(_, selected)| *selected)
                .map(|(line, _)| line)
        );
        assert!(first.contains("ctx"));
        assert!(!first.contains("@@"));

        v.handle_key(&KeyCode::Char('G'), Modifiers::NONE);
        let last = format!(
            "{:?}",
            v.body_lines(80)
                .into_iter()
                .find(|(_, selected)| *selected)
                .map(|(line, _)| line)
        );
        assert!(last.contains("new"));
    }

    #[test]
    fn empty_pr_diff_still_renders_top_level_and_general_feedback() {
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: Some(thegn_core::review::PrReviewSnapshot {
                diff: PrDiff::default(),
                conversation: PrConversation {
                    comments: vec![PrComment {
                        author: "commenter".into(),
                        body: "top-level body".into(),
                        ..PrComment::default()
                    }],
                    threads: vec![ReviewThread {
                        id: "general".into(),
                        comments: vec![PrComment {
                            author: "reviewer".into(),
                            body: "general body".into(),
                            ..PrComment::default()
                        }],
                        ..ReviewThread::default()
                    }],
                    ..PrConversation::default()
                },
                ..Default::default()
            }),
            review_status: None,
        });
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);

        assert_eq!(v.row_count(), 1);
        let body = format!("{:?}", v.body_lines(80));
        assert!(body.contains("top-level body"));
        assert!(body.contains("general body"));
    }

    #[test]
    fn pr_review_diff_renders_comments_and_submitted_reviews() {
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: Some(sample()),
            structural: None,
            review: Some(thegn_core::review::PrReviewSnapshot {
                diff: sample(),
                conversation: PrConversation {
                    comments: vec![PrComment {
                        author: "commenter".into(),
                        body: "top-level comment".into(),
                        ..PrComment::default()
                    }],
                    reviews: vec![PrReview {
                        author: "approver".into(),
                        state: "APPROVED".into(),
                        body: "submitted review".into(),
                        ..PrReview::default()
                    }],
                    ..PrConversation::default()
                },
                ..Default::default()
            }),
            review_status: None,
        });
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);

        let rendered = format!("{:?}", v.body_lines(80));
        assert!(rendered.contains("top-level comment"));
        assert!(rendered.contains("submitted review"));
    }

    #[test]
    fn pr_review_projection_renders_every_comment_in_a_thread() {
        let thread = ReviewThread {
            id: "thread".into(),
            path: "a.rs".into(),
            line: Some(1),
            comments: vec![
                PrComment {
                    author: "alice".into(),
                    body: "first body".into(),
                    ..PrComment::default()
                },
                PrComment {
                    author: "bob".into(),
                    body: "second body".into(),
                    ..PrComment::default()
                },
            ],
            ..ReviewThread::default()
        };
        let diff = PrDiff {
            files: vec![DiffFile {
                path: "a.rs".into(),
                old_path: None,
                hunks: vec![DiffHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![line(DiffLineKind::Add, "new")],
                }],
            }],
        };
        let mut v = DiffView::with_structural("t".into(), 1, false);
        v.apply_data(DiffViewData {
            generation: 1,
            diff: None,
            structural: None,
            review: Some(thegn_core::review::PrReviewSnapshot {
                diff,
                conversation: PrConversation {
                    threads: vec![thread],
                    ..PrConversation::default()
                },
                ..Default::default()
            }),
            review_status: None,
        });
        v.handle_key(&KeyCode::Tab, Modifiers::NONE);
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);

        let rendered = format!("{:?}", v.body_lines(80));
        assert!(rendered.contains("first body"));
        assert!(rendered.contains("second body"));
    }
}
