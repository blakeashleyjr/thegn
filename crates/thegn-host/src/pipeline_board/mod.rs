//! The agent-pipeline board: a standalone overlay that reads **left to right**.
//!
//! Lifecycle mirrors [`crate::monitor::MonitorOverlay`] minus the tab
//! machinery: an `Option<PipelineBoard>` slot on the loop, fed `handle_key`,
//! painted late over the composed frame, dismissed on `Esc`/`q`. It was a tab
//! of the monitor once; a stage pipeline is not a hardware metric, it needs the
//! whole box to lay stage columns out side by side, and it was unreachable by
//! digit on a machine that showed every metric family.
//!
//! # Live, but not a wake source
//!
//! [`PipelineBoard::refresh`] rides the loop's existing roster sample, which is
//! itself gated on [`PipelineBoard::wants_dispatches`] — so a shut board costs
//! no timer, no thread and no DB read at all. Freezing (`Space`) freezes the
//! **view** only: the roster keeps moving underneath, exactly as the monitor's
//! pause leaves its history rings filling.
//!
//! # Doctrine
//!
//! The board is a **view**, not a controller. Nothing here starts, advances or
//! stops a stage — `[[pipeline.stages]]` is structure a supervising agent
//! executes, and `concurrency`/`timeout_secs` are displayed (the stall cue) and
//! never enforced. The only action it raises is "take me to this row's work".

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::detail::StatusCtx;
use crate::layer::{self, Anchor, LayerSpec};
use crate::monitor_pipeline::DispatchRoster;
use crate::seg::{Line, Tok};
use thegn_core::config_pipeline::PipelineStage;

pub(crate) mod action;
pub(crate) mod layout;
pub(crate) mod view;

pub use action::{pipeline_target, spawn_dispatch_sample};

/// "Take me to this stage's work" — raised by `↵`/click on a board row.
///
/// `session` is carried but not yet consumed: focusing the *pane* running the
/// stage (rather than its worktree) is phase 2, and the request shape is fixed
/// now so that lands without re-plumbing the escalation channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineJump {
    /// Worktree path of the dispatch row.
    pub worktree: String,
    /// The daemon session running it, when the row records one.
    pub session: Option<String>,
}

/// An action the loop must perform on the board's behalf, because it needs the
/// session/sidebar the overlay cannot reach. Drained with
/// [`PipelineBoard::take_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardAction {
    Jump(PipelineJump),
}

/// What a key delivered to the board meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOutcome {
    Pending,
    Close,
    /// A row was activated; the loop pulls it with [`PipelineBoard::take_action`].
    Action,
    /// **Not ours** — the loop should let the global keymap have this key
    /// instead of treating it as consumed. Without it the modal would eat every
    /// chord it does not implement, so the chord that OPENS the board could
    /// never toggle it shut and `Ctrl-g` (key lock) would close it instead of
    /// locking anything.
    Passthrough,
}

/// Rows the chrome reserves inside the box: the two-row stage rail on top
/// ([`view::RAIL_ROWS`]) and the key-hint footer on the bottom. Cached into
/// `body_rows` so the scroll clamp and the renderer can never disagree about
/// the viewport — get this wrong and the tail of a long board is unreachable
/// with no visible symptom.
const CHROME_ROWS: usize = view::RAIL_ROWS + 1;

/// The standalone pipeline board.
pub struct PipelineBoard {
    /// The laid-out board, rebuilt from the roster on every refresh.
    board: layout::Board,
    /// The rendered body lines, in view order. Cached with the board so a click
    /// resolves against exactly what was drawn, never against a re-derivation.
    lines: Vec<view::BoardLine>,
    /// The active stage column (`←`/`→`).
    col: usize,
    /// The selected row, by roster **id**. Never an index: the roster is
    /// re-sampled under the user and an index cursor would silently point at a
    /// different agent after every change.
    cursor: Option<i64>,
    /// Where the cursor last sat inside its column, so a vanished row lands the
    /// cursor on its neighbour rather than back at the top.
    cursor_ix: usize,
    scroll: usize,
    /// Frozen view. The roster keeps moving underneath.
    frozen: bool,
    /// Frozen "now", so a frozen board's ages and stall cues don't creep.
    frozen_now_ms: Option<i64>,
    last_now_ms: i64,
    /// Hide rows whose status is terminal.
    ///
    /// Neither this nor `frozen` is persisted, and neither writes to the DB.
    /// Deliberate: both are *reading* postures for the session you are in, not
    /// preferences — a board that reopened with finished rows hidden would hide
    /// the thing you came back to check.
    hide_finished: bool,
    /// A transient footer notice (a jump that found nowhere to land). Cleared
    /// by the next keystroke.
    notice: Option<String>,
    pending_action: Option<BoardAction>,
    /// The screen the box was last sized against — what `handle_click` hit-tests
    /// with, so the click target can never drift from what was drawn.
    screen: Rect,
    cols: usize,
    rows: usize,
    /// `rows - CHROME_ROWS`. What the scroll clamp measures against.
    body_rows: usize,
}

impl PipelineBoard {
    /// Open sized against `ctx.screen`, showing `roster` grouped by `stages`.
    pub fn open(
        roster: &DispatchRoster,
        stages: &[PipelineStage],
        ctx: &StatusCtx,
    ) -> PipelineBoard {
        let (cols, rows) = Self::dims(ctx.screen);
        let mut b = PipelineBoard {
            board: layout::board(&[], stages, cols, ctx.now_ms, false),
            lines: Vec::new(),
            col: 0,
            cursor: None,
            cursor_ix: 0,
            scroll: 0,
            frozen: false,
            frozen_now_ms: None,
            last_now_ms: ctx.now_ms,
            hide_finished: false,
            notice: None,
            pending_action: None,
            screen: ctx.screen,
            cols,
            rows,
            body_rows: rows.saturating_sub(CHROME_ROWS),
        };
        b.rebuild(roster, stages, ctx);
        b
    }

    /// Box interior size, clamped exactly the way [`layer::box_dims`] will clamp
    /// it. The two must agree: `rows` is what the scroll clamp measures content
    /// against, so leaving it at a size the layer then shrinks would strand the
    /// tail of a long board out of reach.
    fn dims(screen: Rect) -> (usize, usize) {
        let cols = (screen.cols * 9 / 10)
            .max(56)
            .min(screen.cols.saturating_sub(6))
            .max(1);
        let rows = (screen.rows * 4 / 5)
            .max(16)
            .min(screen.rows.saturating_sub(3))
            .max(1);
        (cols, rows)
    }

    /// True while the board wants the off-loop roster sample — open and not
    /// frozen. A shut board (or a frozen one) ⇒ false ⇒ no periodic DB read at
    /// all, which is what keeps the board free when nobody is looking at it.
    pub fn wants_dispatches(&self) -> bool {
        !self.frozen
    }

    #[allow(dead_code)] // read by tests
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Drain a pending loop-side action. Called after every key so the overlay
    /// never touches the session or the sidebar itself.
    pub fn take_action(&mut self) -> Option<BoardAction> {
        self.pending_action.take()
    }

    /// The loop pushes an outcome (a jump that found nowhere to land) here; it
    /// shows in the footer until the next keystroke.
    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// Rebuild in place from a fresh roster. Returns `true` when it repainted.
    /// A frozen board returns immediately and touches nothing — which is also
    /// what keeps a frozen picture from re-dirtying the frame at the sample
    /// rate.
    pub fn refresh(
        &mut self,
        roster: &DispatchRoster,
        stages: &[PipelineStage],
        ctx: &StatusCtx,
    ) -> bool {
        if self.frozen {
            return false;
        }
        self.rebuild(roster, stages, ctx);
        true
    }

    /// Rebuild after a key that changed what should be on screen.
    ///
    /// Distinct from [`Self::refresh`] because it must run **even while frozen**:
    /// freezing the data must not freeze the navigation. It rebuilds against
    /// the frozen clock, so a frozen board still shows the instant it was
    /// frozen at.
    pub fn rebuild_after_key(
        &mut self,
        roster: &DispatchRoster,
        stages: &[PipelineStage],
        ctx: &StatusCtx,
    ) {
        self.rebuild(roster, stages, ctx);
    }

    fn rebuild(&mut self, roster: &DispatchRoster, stages: &[PipelineStage], ctx: &StatusCtx) {
        self.resize(ctx.screen);
        self.last_now_ms = ctx.now_ms;
        let now = self.frozen_now_ms.unwrap_or(ctx.now_ms);
        // The configured stages are the single source for BOTH the row fold's
        // grouping order and the board's columns, so a stale sampled order can
        // never disagree with the live config about where a row belongs.
        let names: Vec<String> = stages
            .iter()
            .filter_map(|s| s.stage_name())
            .map(str::to_string)
            .collect();
        let rows = crate::monitor_pipeline::ordered_rows(&roster.rows, &names, now);
        self.board = layout::board(&rows, stages, self.cols, now, self.hide_finished);
        self.resolve_cursor();
        self.relines();
        self.clamp_scroll();
    }

    /// Re-anchor the cursor after a rebuild: by ROW ID first (the row is still
    /// there, wherever it moved to), then by its last index in the column (the
    /// row finished and vanished — land on its neighbour, not back at the top).
    fn resolve_cursor(&mut self) {
        self.col = self.col.min(self.board.columns.len().saturating_sub(1));
        let Some(col) = self.board.columns.get(self.col) else {
            self.cursor = None;
            self.cursor_ix = 0;
            return;
        };
        if let Some(id) = self.cursor
            && let Some(ix) = col.rows.iter().position(|r| r.row.id == id)
        {
            self.cursor_ix = ix;
            return;
        }
        self.cursor_ix = self.cursor_ix.min(col.rows.len().saturating_sub(1));
        self.cursor = col.rows.get(self.cursor_ix).map(|r| r.row.id);
    }

    /// Re-render the body lines. Cheap enough to redo on every navigation key,
    /// and doing so is what guarantees the hit map is the drawn map.
    fn relines(&mut self) {
        self.lines = view::body(&self.board, self.col, self.cursor, self.cols);
    }

    fn resize(&mut self, screen: Rect) {
        self.screen = screen;
        let (cols, rows) = Self::dims(screen);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.body_rows = rows.saturating_sub(CHROME_ROWS);
        }
    }

    fn scroll_max(&self) -> usize {
        self.lines.len().saturating_sub(self.body_rows)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.scroll_max());
    }

    /// Keep the cursor's line inside the viewport after a move.
    fn scroll_to_cursor(&mut self) {
        let Some(id) = self.cursor else { return };
        let Some(line) = self.lines.iter().position(|l| l.row_id == Some(id)) else {
            return;
        };
        if line < self.scroll {
            self.scroll = line;
        } else if self.body_rows > 0 && line >= self.scroll + self.body_rows {
            self.scroll = line + 1 - self.body_rows;
        }
        self.clamp_scroll();
    }

    /// Wheel scrolling, for the mouse path.
    pub fn wheel(&mut self, delta: isize) {
        let max = self.scroll_max() as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    /// Move the row cursor within the active column.
    fn nav_row(&mut self, delta: isize) {
        let len = self
            .board
            .columns
            .get(self.col)
            .map(|c| c.rows.len())
            .unwrap_or(0);
        if len == 0 {
            return;
        }
        let max = len.saturating_sub(1) as isize;
        self.cursor_ix = (self.cursor_ix as isize + delta).clamp(0, max) as usize;
        self.cursor = self.board.columns[self.col]
            .rows
            .get(self.cursor_ix)
            .map(|r| r.row.id);
        self.relines();
        self.scroll_to_cursor();
    }

    /// Move to the neighbouring stage column, keeping the reading position.
    fn nav_col(&mut self, delta: isize) {
        if self.board.columns.is_empty() {
            return;
        }
        let max = self.board.columns.len() as isize - 1;
        let next = (self.col as isize + delta).clamp(0, max) as usize;
        if next == self.col {
            return;
        }
        self.col = next;
        self.cursor_ix = self
            .cursor_ix
            .min(self.board.columns[self.col].rows.len().saturating_sub(1));
        self.cursor = self.board.columns[self.col]
            .rows
            .get(self.cursor_ix)
            .map(|r| r.row.id);
        self.relines();
        self.scroll_to_cursor();
    }

    /// The selected row's jump request, or a notice saying why there isn't one.
    fn activate(&mut self) -> BoardOutcome {
        let Some(row) = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.rows.get(self.cursor_ix))
        else {
            return BoardOutcome::Pending;
        };
        if row.row.worktree_path.is_empty() {
            self.notice = Some(format!("{} has no worktree to open", row.row.issue_id));
            return BoardOutcome::Pending;
        }
        self.pending_action = Some(BoardAction::Jump(PipelineJump {
            worktree: row.row.worktree_path.clone(),
            session: row.row.session_id.clone(),
        }));
        BoardOutcome::Action
    }

    /// Dispatch one key.
    ///
    /// Anything the board does not bind is [`BoardOutcome::Passthrough`], not
    /// "consumed" — that is what lets the opening chord toggle it shut and
    /// leaves `Ctrl-g` meaning key-lock.
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> BoardOutcome {
        // A one-keystroke notice clears first: it is pure display, so clearing
        // it can never change what the key then means.
        self.notice = None;
        // Alt/Super chords belong to the compositor. Checked before CTRL so a
        // `Ctrl Alt …` chord passes too.
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return BoardOutcome::Passthrough;
        }
        if mods.contains(Modifiers::CTRL) {
            return match key {
                // Ctrl-C is the universal "get me out of here".
                KeyCode::Char('c' | 'C') => BoardOutcome::Close,
                _ => BoardOutcome::Passthrough,
            };
        }
        if crate::input::is_escape_key(key) {
            return BoardOutcome::Close;
        }
        match key {
            KeyCode::Char('q') => BoardOutcome::Close,
            KeyCode::DownArrow | KeyCode::Char('j') => {
                self.nav_row(1);
                BoardOutcome::Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.nav_row(-1);
                BoardOutcome::Pending
            }
            KeyCode::RightArrow | KeyCode::Char('l') => {
                self.nav_col(1);
                BoardOutcome::Pending
            }
            KeyCode::LeftArrow | KeyCode::Char('h') => {
                self.nav_col(-1);
                BoardOutcome::Pending
            }
            KeyCode::Enter | KeyCode::Char('\r' | '\n') => self.activate(),
            // Freeze the VIEW. The roster keeps moving underneath, so resuming
            // shows the current truth rather than a gap.
            KeyCode::Char(' ') => {
                self.frozen = !self.frozen;
                self.frozen_now_ms = self.frozen.then_some(self.last_now_ms);
                BoardOutcome::Pending
            }
            KeyCode::Char('x') => {
                self.hide_finished = !self.hide_finished;
                BoardOutcome::Pending
            }
            _ => BoardOutcome::Passthrough,
        }
    }

    /// A left-click inside the box: select the row under the pointer, or
    /// activate it when it was already selected.
    ///
    /// Column geometry lives on the [`layout::Board`], which is why this
    /// resolves through it rather than through the flattened line list: a line
    /// in `Columns` mode spans every column at once.
    pub fn handle_click(&mut self, x: usize, y: usize) -> BoardOutcome {
        self.notice = None;
        let Some(inner) = self.inner_rect() else {
            return BoardOutcome::Pending;
        };
        let top = inner.y + view::RAIL_ROWS;
        let bottom = inner.y + inner.rows.saturating_sub(1);
        if y < top || y >= bottom || x < inner.x {
            return BoardOutcome::Pending;
        }
        let line = (y - top) + self.scroll;
        let hit = match self.board.mode {
            layout::Mode::Columns => {
                let cw = self.board.col_w.max(1);
                let ci = ((x - inner.x) / cw).min(self.board.columns.len().saturating_sub(1));
                self.board
                    .columns
                    .get(ci)
                    .and_then(|c| c.rows.get(line))
                    .map(|r| (ci, line, r.row.id))
            }
            layout::Mode::Stacked => self
                .lines
                .get(line)
                .and_then(|l| l.row_id)
                .and_then(|id| self.locate(id).map(|(ci, ix)| (ci, ix, id))),
        };
        let Some((ci, ix, id)) = hit else {
            return BoardOutcome::Pending;
        };
        if self.cursor == Some(id) {
            return self.activate();
        }
        self.col = ci;
        self.cursor_ix = ix;
        self.cursor = Some(id);
        self.relines();
        BoardOutcome::Pending
    }

    /// Where a row id sits on the board, as `(column, index)`.
    fn locate(&self, id: i64) -> Option<(usize, usize)> {
        self.board.columns.iter().enumerate().find_map(|(ci, c)| {
            c.rows
                .iter()
                .position(|r| r.row.id == id)
                .map(|ix| (ci, ix))
        })
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

// --- Rendering -----------------------------------------------------------

impl PipelineBoard {
    fn spec(&self) -> LayerSpec {
        LayerSpec {
            title: "pipeline".into(),
            badge: Some(" esc ".into()),
            cols: self.cols,
            rows: self.rows,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Faint),
        }
    }

    /// The outer box, for mouse hit-testing. Shares `spec` with `render`, so
    /// the click target can never drift from what was drawn.
    pub fn box_rect(&self, screen: Rect) -> Option<Rect> {
        layer::box_rect(&self.spec(), screen)
    }

    /// The content rect inside the box — border + one cell of pad each side
    /// horizontally, border only vertically (see `layer::open_layer`).
    fn inner_rect(&self) -> Option<Rect> {
        let b = self.box_rect(self.screen)?;
        Some(Rect {
            x: b.x + 2,
            y: b.y + 1,
            cols: b.cols.saturating_sub(4),
            rows: b.rows.saturating_sub(2),
        })
    }

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let Some(inner) = layer::open_layer(surface, screen, &self.spec()) else {
            return;
        };
        let pad = crate::sections::panel();
        let rail = view::rail(&self.board, self.col, inner.cols);
        for (i, line) in rail.iter().enumerate() {
            crate::seg::draw_line(surface, inner.x, inner.y + i, inner.cols, line, pad);
        }
        let body_rows = inner.rows.saturating_sub(CHROME_ROWS);
        for (i, bl) in self
            .lines
            .iter()
            .skip(self.scroll)
            .take(body_rows)
            .enumerate()
        {
            crate::seg::draw_line(
                surface,
                inner.x,
                inner.y + view::RAIL_ROWS + i,
                inner.cols,
                &bl.line,
                pad,
            );
        }
        crate::seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows.saturating_sub(1),
            inner.cols,
            &self.footer(),
            pad,
        );
    }

    /// The key-hint legend — or a transient notice while one is set.
    fn footer(&self) -> Line {
        match &self.notice {
            Some(n) => Line::split(
                vec![crate::seg::seg(Tok::Slot(S::Accent), n.clone())],
                vec![crate::seg::seg(
                    Tok::Slot(S::Ghost),
                    "esc close".to_string(),
                )],
            ),
            None => view::legend(self.frozen, self.hide_finished),
        }
    }
}
