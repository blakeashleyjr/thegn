//! The pane terminal-emulator seam.
//!
//! A `PaneEmulator` turns a PTY byte stream into a readable grid of styled
//! cells. The compositor reads that grid to paint the focused pane; background
//! panes still `advance()` (drain-without-render) so a backgrounded agent keeps
//! progressing.
//!
//! The only impl is [`AlacrittyEmulator`] (`alacritty_terminal`). It is
//! intentionally behind a trait: image-protocol support (sixel/kitty) via an
//! escape-interception passthrough layer, or a `wezterm-term` git dep (unpublished
//! on crates.io), can swap in without touching the compositor.

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// One styled cell, renderer-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridCell {
    /// Cell contents (usually one grapheme; empty == blank).
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A borrowing view of a cell — same fields as [`GridCell`] but the glyph is a
/// `&str` into the emulator's own grid instead of an owned `String`. The render
/// hot path ([`crate::compositor::compose_pane`]) reads every visible cell every
/// frame and only ever appends the glyph to a run buffer, so borrowing here
/// avoids a heap allocation per cell per frame (vt100 stores glyphs inline, so
/// `Cell::contents()` is already a cheap borrow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRef<'a> {
    pub text: &'a str,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// A color in terminal terms, normalized away from any one library's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellColor {
    #[default]
    Default,
    /// One of the 256 indexed colors.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// A `Copy` cell for the reusable-buffer snapshot path: the glyph is a plain
/// `char`, exactly what [`conv_cell`] reduces alacritty's cell to before
/// calling `.to_string()` on it. The `String` per cell was ~10k mallocs per
/// 200×50 pane per composed frame — pure overhead, since the compositor only
/// ever appends the glyph to a run buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapCell {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for SnapCell {
    fn default() -> Self {
        SnapCell {
            ch: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

/// A reusable, flat (row-major, `rows × cols`) visible-grid snapshot filled by
/// [`PaneEmulator::grid_snapshot_into`]. The compositor keeps one per thread
/// and refills it each compose — zero steady-state allocation, one grid lock.
#[derive(Debug, Default)]
pub struct GridSnapshot {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<SnapCell>,
}

impl GridSnapshot {
    /// The cell at `(row, col)`, or `None` outside the snapshot's dimensions.
    #[inline]
    pub fn get(&self, row: usize, col: usize) -> Option<SnapCell> {
        (row < self.rows && col < self.cols).then(|| self.cells[row * self.cols + col])
    }
}

/// A sink that can feed a pane's grid from OFF the event loop (the pane's
/// reader thread). Owns its own escape parser; exactly one feeder OR the
/// loop-side [`PaneEmulator::advance`] drives a given pane's stream — never
/// both (two parsers on one stream would split escape state).
pub trait FeedSink: Send {
    /// Parse + apply PTY output bytes to the shared grid.
    fn advance(&mut self, bytes: &[u8]);
}

/// A terminal emulator for a single pane.
pub trait PaneEmulator: Send {
    /// Feed PTY output bytes (advances the screen; never renders).
    fn advance(&mut self, bytes: &[u8]);
    /// A handle that feeds this emulator's grid from another thread (the pane
    /// reader), or `None` when the emulator can't share its grid — the pane
    /// then stays loop-fed. See `pane_feed` in the module docs of `pane.rs`.
    fn feeder(&self) -> Option<Box<dyn FeedSink>> {
        None
    }
    /// Resize the screen to `rows` x `cols`.
    fn resize(&mut self, rows: u16, cols: u16);
    /// Current grid size as `(rows, cols)`.
    fn size(&self) -> (u16, u16);
    /// Cell at `(row, col)`, or `None` if out of range.
    fn cell(&self, row: u16, col: u16) -> Option<GridCell>;
    /// Read a cell by absolute grid line (alacritty `Line`: 0 = top of the live
    /// screen, negative = scrollback history), independent of the current
    /// viewport. Copy-mode uses this so a selection can span rows that have
    /// scrolled off screen. Default `None` for emulators without scrollback.
    fn cell_abs(&self, _line: i32, _col: u16) -> Option<GridCell> {
        None
    }
    /// Borrowing view of the cell at `(row, col)` — the allocation-free path the
    /// compositor uses. Defaults to `None`; emulators that can expose the glyph
    /// as a borrow override it (the compositor falls back to [`Self::cell`] when
    /// this returns `None`).
    fn cell_ref(&self, _row: u16, _col: u16) -> Option<CellRef<'_>> {
        None
    }
    /// One-lock bulk read of the visible grid (row-major, `rows × cols`) for
    /// composition. With the feed running on the pane's READER thread, the
    /// per-cell accessors would take (and contend) the grid lock once per cell
    /// — ~10k lock round-trips per pane compose racing a flood's parser. The
    /// snapshot pays one lock + one pass. Default `None` → per-cell path.
    fn grid_snapshot(&self) -> Option<Vec<Vec<GridCell>>> {
        None
    }
    /// Zero-alloc variant of [`Self::grid_snapshot`]: refill the caller's
    /// reusable [`GridSnapshot`] buffer and return `true`, or return `false`
    /// (buffer untouched) to send the caller down the allocating fallback
    /// chain. Same one-lock/one-pass contract as `grid_snapshot`, and the
    /// implementation must also latch [`Self::snapshot_cursor`] under that
    /// lock.
    fn grid_snapshot_into(&self, _out: &mut GridSnapshot) -> bool {
        false
    }
    /// The OSC window title (OSC 0/2) the app last set, if any. `None` when the
    /// app has set no title, so callers can fall back to a derived name.
    fn title(&self) -> Option<String> {
        None
    }
    /// Cursor position as `(row, col)`.
    fn cursor(&self) -> (u16, u16);
    /// The cursor as of the last [`Self::grid_snapshot`] — the position that
    /// belongs to the cells currently composed into the frame.
    ///
    /// The grid is parsed on the pane's READER thread (`pane_pty::open_pty`),
    /// so it advances between any two lock acquisitions the loop makes. Reading
    /// the live cursor at flush time therefore paints a frame whose content is
    /// from the compose-time snapshot but whose caret is from some later state
    /// — visibly, the caret teleports to where the app is mid-redraw (typically
    /// the start of the next line) and snaps back a frame later. Taking the
    /// cursor inside the snapshot's lock keeps caret and cells consistent.
    ///
    /// Emulators without a snapshot read the grid per cell anyway, so they fall
    /// back to the live cursor.
    fn snapshot_cursor(&self) -> (u16, u16) {
        self.cursor()
    }
    /// Scroll the viewport up into history by `n` rows (copy-mode / scrollback).
    fn scroll_up(&mut self, _n: usize) {}
    /// Scroll the viewport back down toward the live tail by `n` rows.
    fn scroll_down(&mut self, _n: usize) {}
    /// Jump back to the live tail (offset 0).
    fn scroll_reset(&mut self) {}
    /// Current scrollback offset in rows (0 == live tail).
    fn scrollback(&self) -> usize {
        0
    }
    /// Borrow a visible row as a single plain string when it carries no
    /// styling, else `None`. The compositor composes cell-by-cell (so this is
    /// no longer on its hot path), but it stays as a cheap accessor that tests
    /// use to assert PTY output landed in the grid.
    #[allow(dead_code)]
    fn row_text(&self, _row: u16) -> Option<String> {
        None
    }
    /// DECCKM: when set, arrows/Home/End must be sent SS3-encoded (`ESC O A`).
    fn application_cursor(&self) -> bool {
        false
    }
    /// Alternate-screen active (a full-screen TUI). Predictive echo never fires
    /// here. Default `false` for non-alacritty emulators.
    fn alt_screen(&self) -> bool {
        false
    }
    /// Bracketed paste: when set, pastes are wrapped in `ESC[200~ … ESC[201~`.
    fn bracketed_paste(&self) -> bool {
        false
    }
    /// The mouse reporting the app requested: `(mode, SGR encoding?)`. The
    /// host forwards matching mouse events into the pane instead of using
    /// them for its own selection (hold Shift to force host selection).
    fn mouse_mode(&self) -> (MouseMode, bool) {
        (MouseMode::None, false)
    }
}

/// Mouse reporting level an app can request (DECSET 9/1000/1002/1003),
/// normalized away from any one library's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

/// The Term's event sink. The only event we care about is the OSC-0/2 window
/// title (`AlacrittyEvent::Title`); everything else the alacritty core emits
/// (bell, clipboard, PTY writes, etc.) the compositor drives itself, so we drop
/// it. The title lands in a shared `Mutex<Option<String>>` that
/// [`AlacrittyEmulator::title`] reads back. The cell is owned by the single
/// `EventProxy` stored inside the shared `Term`, so both the loop-side
/// `advance` and the reader-thread [`AlacrittyFeeder`] update the same title.
#[derive(Clone)]
pub struct EventProxy {
    title: Arc<Mutex<Option<String>>>,
}

impl EventProxy {
    fn new() -> Self {
        Self {
            title: Arc::new(Mutex::new(None)),
        }
    }
}

/// Bumped on every OSC title set/reset across ALL panes. The loop's
/// `collect_window_titles` used to lock every pane's title mutex (active
/// session + every parked workspace) and rebuild a BTreeMap on EVERY rendered
/// frame; titles change rarely, so it now skips the sweep entirely while this
/// generation is unchanged. Process-global on purpose: one u64 load per frame
/// versus per-pane bookkeeping.
static TITLE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current title generation (see [`TITLE_GEN`]).
pub fn title_generation() -> u64 {
    TITLE_GEN.load(Ordering::Relaxed)
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        match event {
            AlacrittyEvent::Title(t) => {
                if let Ok(mut g) = self.title.lock() {
                    *g = Some(t);
                }
                TITLE_GEN.fetch_add(1, Ordering::Relaxed);
            }
            AlacrittyEvent::ResetTitle => {
                if let Ok(mut g) = self.title.lock() {
                    *g = None;
                }
                TITLE_GEN.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct AlacrittyEmulator {
    term: Arc<FairMutex<Term<EventProxy>>>,
    parser: Processor,
    /// Shared with the [`EventProxy`] inside `term`; holds the last OSC title.
    title: Arc<Mutex<Option<String>>>,
    /// The cursor read inside the last [`PaneEmulator::grid_snapshot`] lock,
    /// packed `row << 16 | col` ([`NO_SNAP_CURSOR`] until the first snapshot).
    /// An atomic because the compositor holds only `&dyn PaneEmulator`, and it
    /// keeps the render hot path lock-free — same pattern as the undercurl
    /// atomic in `seg.rs` and the caps atomics in `caps.rs`.
    snap_cursor: AtomicU32,
}

/// `snap_cursor` sentinel: no snapshot has been taken yet, so
/// [`PaneEmulator::snapshot_cursor`] falls back to the live cursor.
const NO_SNAP_CURSOR: u32 = u32::MAX;

fn pack_cursor(row: u16, col: u16) -> u32 {
    ((row as u32) << 16) | col as u32
}

fn unpack_cursor(v: u32) -> (u16, u16) {
    ((v >> 16) as u16, (v & 0xffff) as u16)
}

#[derive(Clone, Copy)]
struct PaneSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for PaneSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl AlacrittyEmulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let size = PaneSize {
            cols: cols as usize,
            rows: rows as usize,
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Default::default()
        };

        let proxy = EventProxy::new();
        let title = Arc::clone(&proxy.title);
        let term = Term::new(config, &size, proxy);
        Self {
            term: Arc::new(FairMutex::new(term)),
            parser: Processor::new(),
            title,
            snap_cursor: AtomicU32::new(NO_SNAP_CURSOR),
        }
    }
}

fn conv_cell(cell: &alacritty_terminal::term::cell::Cell) -> GridCell {
    use alacritty_terminal::term::cell::Flags;
    GridCell {
        text: cell.c.to_string(),
        fg: conv_color(cell.fg),
        bg: conv_color(cell.bg),
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        underline: cell.flags.contains(Flags::UNDERLINE),
        inverse: cell.flags.contains(Flags::INVERSE),
    }
}

fn conv_color(c: alacritty_terminal::vte::ansi::Color) -> CellColor {
    use alacritty_terminal::vte::ansi::Color;
    use alacritty_terminal::vte::ansi::NamedColor;
    match c {
        Color::Indexed(i) => CellColor::Indexed(i),
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => {
            CellColor::Default
        }
        Color::Named(n) => CellColor::Indexed(n as u8),
    }
}

/// The off-thread feed handle for [`AlacrittyEmulator`]: a clone of the
/// `FairMutex<Term>` plus its own `Processor`. This is alacritty's shipped
/// architecture — its reader thread parses into the shared Term while the
/// render thread reads it; the FairMutex prevents either side starving the
/// other. Feeds lock once per (≤64KB) chunk, so loop-side reads (compose,
/// copymode, cursor) never wait long.
pub struct AlacrittyFeeder {
    term: Arc<FairMutex<Term<EventProxy>>>,
    parser: Processor,
}

impl FeedSink for AlacrittyFeeder {
    fn advance(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.parser.advance(&mut *term, bytes);
    }
}

impl PaneEmulator for AlacrittyEmulator {
    fn advance(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.parser.advance(&mut *term, bytes);
    }

    fn feeder(&self) -> Option<Box<dyn FeedSink>> {
        Some(Box::new(AlacrittyFeeder {
            term: Arc::clone(&self.term),
            parser: Processor::new(),
        }))
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let mut term = self.term.lock();
        let size = PaneSize {
            cols: cols as usize,
            rows: rows as usize,
        };
        term.resize(size);
    }

    fn size(&self) -> (u16, u16) {
        let term = self.term.lock();
        (term.screen_lines() as u16, term.columns() as u16)
    }

    fn cell(&self, row: u16, col: u16) -> Option<GridCell> {
        let term = self.term.lock();
        if row >= term.screen_lines() as u16 || col >= term.columns() as u16 {
            return None;
        }
        let display_offset = term.grid().display_offset();
        let point = alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(row as i32 - display_offset as i32),
            alacritty_terminal::index::Column(col as usize),
        );
        Some(conv_cell(&term.grid()[point]))
    }

    fn cell_abs(&self, line: i32, col: u16) -> Option<GridCell> {
        let term = self.term.lock();
        let grid = term.grid();
        let (cols, screen, total) = (grid.columns(), grid.screen_lines(), grid.total_lines());
        let history = total.saturating_sub(screen); // == grid.history_size()
        // Valid Line range is -(history) ..= screen-1. Out-of-range indexing into
        // alacritty's ring storage silently returns the WRONG row, so guard first.
        if col as usize >= cols || line < -(history as i32) || line > screen as i32 - 1 {
            return None;
        }
        let point = alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(line),
            alacritty_terminal::index::Column(col as usize),
        );
        Some(conv_cell(&grid[point]))
    }

    fn grid_snapshot(&self) -> Option<Vec<Vec<GridCell>>> {
        let term = self.term.lock();
        let (rows, cols) = (term.screen_lines(), term.columns());
        let display_offset = term.grid().display_offset() as i32;
        let grid = term.grid();
        // Same lock, same instant as the cells: the caret this frame paints
        // must belong to the grid this frame paints. See `snapshot_cursor`.
        let cursor = grid.cursor.point;
        self.snap_cursor.store(
            pack_cursor(cursor.line.0 as u16, cursor.column.0 as u16),
            Ordering::Relaxed,
        );
        let mut out = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = Vec::with_capacity(cols);
            for col in 0..cols {
                let point = alacritty_terminal::index::Point::new(
                    alacritty_terminal::index::Line(row as i32 - display_offset),
                    alacritty_terminal::index::Column(col),
                );
                line.push(conv_cell(&grid[point]));
            }
            out.push(line);
        }
        Some(out)
    }

    fn grid_snapshot_into(&self, out: &mut GridSnapshot) -> bool {
        let term = self.term.lock();
        let (rows, cols) = (term.screen_lines(), term.columns());
        let display_offset = term.grid().display_offset() as i32;
        let grid = term.grid();
        // Same lock, same instant as the cells: the caret this frame paints
        // must belong to the grid this frame paints. See `snapshot_cursor`.
        let cursor = grid.cursor.point;
        self.snap_cursor.store(
            pack_cursor(cursor.line.0 as u16, cursor.column.0 as u16),
            Ordering::Relaxed,
        );
        out.rows = rows;
        out.cols = cols;
        out.cells.clear();
        out.cells.reserve(rows * cols);
        for row in 0..rows {
            for col in 0..cols {
                use alacritty_terminal::term::cell::Flags;
                let point = alacritty_terminal::index::Point::new(
                    alacritty_terminal::index::Line(row as i32 - display_offset),
                    alacritty_terminal::index::Column(col),
                );
                let cell = &grid[point];
                out.cells.push(SnapCell {
                    ch: cell.c,
                    fg: conv_color(cell.fg),
                    bg: conv_color(cell.bg),
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.contains(Flags::UNDERLINE),
                    inverse: cell.flags.contains(Flags::INVERSE),
                });
            }
        }
        true
    }

    fn title(&self) -> Option<String> {
        // OSC 0/2 titles arrive via the shared `EventProxy` (see its docs); read
        // the last one back here. A blank title is treated as "none" so callers
        // fall back to a derived pane name instead of showing an empty label.
        self.title
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|t| !t.is_empty())
    }

    fn cursor(&self) -> (u16, u16) {
        let term = self.term.lock();
        let point = term.grid().cursor.point;
        (point.line.0 as u16, point.column.0 as u16)
    }

    fn snapshot_cursor(&self) -> (u16, u16) {
        match self.snap_cursor.load(Ordering::Relaxed) {
            NO_SNAP_CURSOR => self.cursor(),
            packed => unpack_cursor(packed),
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let n_i32 = (n as isize).try_into().unwrap_or(i32::MAX);
        self.term.lock().scroll_display(Scroll::Delta(n_i32));
    }

    fn scroll_down(&mut self, n: usize) {
        let n_i32 = (-(n as isize)).try_into().unwrap_or(i32::MIN);
        self.term.lock().scroll_display(Scroll::Delta(n_i32));
    }

    fn scroll_reset(&mut self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    fn scrollback(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    fn row_text(&self, row: u16) -> Option<String> {
        let (_, cols) = self.size();
        let mut s = String::new();
        for col in 0..cols {
            match self.cell(row, col) {
                Some(c) => {
                    if c.bold
                        || c.italic
                        || c.underline
                        || c.inverse
                        || c.fg != CellColor::Default
                        || c.bg != CellColor::Default
                    {
                        return None;
                    }
                    if c.text.is_empty() {
                        s.push(' ');
                    } else {
                        s.push_str(&c.text);
                    }
                }
                _ => s.push(' '),
            }
        }
        Some(s)
    }

    fn application_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    fn alt_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    fn bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn mouse_mode(&self) -> (MouseMode, bool) {
        let term = self.term.lock();
        let mode = term.mode();
        let mm = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::ButtonMotion
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::PressRelease
        } else {
            MouseMode::None
        };
        let sgr = mode.contains(TermMode::SGR_MOUSE);
        (mm, sgr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"hello world");
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("hello world".to_string())
        );
        assert_eq!(e.cursor(), (0, 11));
    }

    #[test]
    fn snapshot_cursor_belongs_to_the_snapshotted_grid() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"hello");
        // Before any snapshot there is nothing cached, so fall back to live.
        assert_eq!(e.snapshot_cursor(), e.cursor());

        let snap = e.grid_snapshot().expect("alacritty exposes a snapshot");
        assert_eq!(snap[0][0].text, "h");
        assert_eq!(e.snapshot_cursor(), (0, 5));

        // The pane's reader thread advances the grid between the compose and
        // the flush. The live cursor moves; the snapshot's must not, or the
        // frame paints row 0's cells with row 1's caret.
        e.advance(b"\r\nworld");
        assert_eq!(e.cursor(), (1, 5));
        assert_eq!(
            e.snapshot_cursor(),
            (0, 5),
            "the caret must stay with the grid the frame composed"
        );

        // A fresh compose re-syncs both.
        let _ = e.grid_snapshot();
        assert_eq!(e.snapshot_cursor(), e.cursor());
    }

    #[test]
    // The row/col indices address THREE parallel structures (flat snapshot,
    // legacy Vec<Vec>, per-cell accessor) — indexing is the clear spelling.
    #[expect(clippy::needless_range_loop)]
    fn buffered_snapshot_matches_cell_reads_and_legacy_snapshot() {
        // The zero-alloc `grid_snapshot_into` must be glyph/style/color
        // identical to both the legacy allocating snapshot and per-cell
        // `cell()` reads — including wide glyphs (漢 + its spacer cell),
        // combining-char drops (conv_cell keeps only `cell.c`), and styles.
        let mut e = AlacrittyEmulator::new(4, 20, 0);
        e.advance("ab\u{0301} 漢\r\n\x1b[1;31mstyled\x1b[0m".as_bytes());

        let mut snap = GridSnapshot::default();
        assert!(
            e.grid_snapshot_into(&mut snap),
            "alacritty fills the buffer"
        );
        assert_eq!((snap.rows, snap.cols), (4, 20));
        let legacy = e.grid_snapshot().expect("legacy snapshot");
        for row in 0..snap.rows {
            for col in 0..snap.cols {
                let got = snap.get(row, col).unwrap();
                let want = &legacy[row][col];
                assert_eq!(got.ch.to_string(), want.text, "glyph at {row},{col}");
                let via_cell = e.cell(row as u16, col as u16).unwrap();
                assert_eq!(
                    (
                        got.fg,
                        got.bg,
                        got.bold,
                        got.italic,
                        got.underline,
                        got.inverse
                    ),
                    (
                        via_cell.fg,
                        via_cell.bg,
                        via_cell.bold,
                        via_cell.italic,
                        via_cell.underline,
                        via_cell.inverse
                    ),
                    "style at {row},{col}"
                );
            }
        }
        // It also latches the snapshot cursor, like the legacy path.
        e.advance(b"\r\nmore");
        assert_ne!(e.snapshot_cursor(), e.cursor());

        // Refill reuses the buffer (no growth surprises after a resize down).
        e.resize(2, 10);
        assert!(e.grid_snapshot_into(&mut snap));
        assert_eq!((snap.rows, snap.cols), (2, 10));
        assert_eq!(snap.cells.len(), 20);
    }

    #[test]
    fn cursor_packing_round_trips_to_the_grid_edges() {
        for (row, col) in [(0u16, 0u16), (0, 65534), (65534, 0), (4095, 511)] {
            assert_eq!(unpack_cursor(pack_cursor(row, col)), (row, col));
            assert_ne!(
                pack_cursor(row, col),
                NO_SNAP_CURSOR,
                "a real position must never collide with the sentinel"
            );
        }
    }

    #[test]
    fn styled_rows_refuse_the_fast_path_so_color_survives() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"plain\r\n\x1b[31mred text\x1b[0m\r\n\x1b[1mbold\x1b[0m");
        // Unstyled rows blit fast...
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("plain".to_string())
        );
        assert_eq!(
            e.row_text(0).map(|r| r.chars().count()),
            Some(80),
            "fast-path rows are full width so stale cells get overwritten"
        );
        // ...but any colored/bold row must go cell-by-cell.
        assert_eq!(e.row_text(1), None, "colored row must not fast-path");
        assert_eq!(e.row_text(2), None, "bold row must not fast-path");
        let c = e.cell(1, 0).unwrap();
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn newline_advances_row() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"line1\r\nline2");
        assert_eq!(
            e.row_text(0).map(|r| r.trim_end().to_string()),
            Some("line1".to_string())
        );
        assert_eq!(
            e.row_text(1).map(|r| r.trim_end().to_string()),
            Some("line2".to_string())
        );
    }

    #[test]
    fn sgr_bold_and_color_are_captured() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.advance(b"\x1b[1;31mX\x1b[0m");
        let c = e.cell(0, 0).unwrap();
        assert_eq!(c.text, "X");
        assert!(c.bold);
        assert_eq!(c.fg, CellColor::Indexed(1));
    }

    #[test]
    fn scrollback_view_reveals_history() {
        let mut e = AlacrittyEmulator::new(3, 20, 100);
        for i in 1..=6 {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(e.scrollback(), 0);
        let tail: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            tail.iter().any(|l| l == "line5"),
            "tail shows recent: {tail:?}"
        );
        assert!(!tail.iter().any(|l| l == "line1"));

        e.scroll_up(100);
        assert!(e.scrollback() > 0, "offset advanced into history");
        let hist: Vec<String> = (0..3)
            .map(|r| e.row_text(r).unwrap_or_default().trim_end().to_string())
            .collect();
        assert!(
            hist.iter().any(|l| l == "line1"),
            "history shows line1: {hist:?}"
        );

        e.scroll_reset();
        assert_eq!(e.scrollback(), 0);
    }

    #[test]
    fn resize_changes_reported_size() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        e.resize(40, 100);
        assert_eq!(e.size(), (40, 100));
    }

    #[test]
    fn emulator_positions_via_hvp_like_btop() {
        let mut emu = AlacrittyEmulator::new(10, 40, 0);
        emu.advance(b"\x1b[3;5fBTOP");
        assert_eq!(emu.cell(2, 4).unwrap().text, "B");
        assert_eq!(emu.cell(2, 7).unwrap().text, "P");
    }

    #[test]
    fn osc_title_is_captured_and_reset() {
        let mut e = AlacrittyEmulator::new(24, 80, 0);
        // No title until the app sets one.
        assert_eq!(e.title(), None);
        // OSC 2 (window title). Regression: EventProxy used to drop this and
        // title() was hardcoded None, so OSC-title features were dead.
        e.advance(b"\x1b]2;my session\x07");
        assert_eq!(e.title(), Some("my session".to_string()));
        // OSC 0 sets both icon + window title.
        e.advance(b"\x1b]0;another\x07");
        assert_eq!(e.title(), Some("another".to_string()));
        // A blank title reads as "none" so callers fall back to a derived name.
        e.advance(b"\x1b]2;\x07");
        assert_eq!(e.title(), None);
    }

    #[test]
    fn osc_title_is_shared_with_the_feeder() {
        // Both the loop-side `advance` and the reader-thread feeder drive the
        // same Term, so a title set through the feeder is visible via title().
        let e = AlacrittyEmulator::new(24, 80, 0);
        let mut feeder = e.feeder().expect("alacritty exposes a feeder");
        feeder.advance(b"\x1b]2;from feeder\x07");
        assert_eq!(e.title(), Some("from feeder".to_string()));
    }
}
