//! Bar-item detail overlays: the popup/modal a masthead or statusbar item opens
//! when activated (Enter on the focused item, or a click). Mirrors the
//! [`crate::menu::MenuOverlay`] lifecycle — an `Option<DetailOverlay>` slot in
//! the loop, fed `handle_key`, painted last via `render` over the composed
//! frame, dismissed on Esc.
//!
//! Content comes in three shapes: a time-series [`GraphDetail`] (CPU/mem/temp/
//! load/net history, via [`crate::telemetry::TelemetryHistory`] + the braille
//! primitives in [`thegn_core::viz`]), a scrollable [`ListDetail`]
//! (notifications, tests), and a static [`KeyValDetail`] block (disk, battery,
//! swap/gpu/freq/uptime, PR, date/clock). [`open_detail_for`] maps an item id to
//! its content, size, and placement; the data is snapshotted at open time so the
//! overlay owns it (no borrow of the model held across frames).

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::{self, BarBadge, BarItemId, FrameModel, S};
use crate::compositor::Rect;
use crate::layer::{self, Anchor, LayerSpec};
use crate::seg::{self, Line, Tok, seg};
use crate::telemetry::TelemetryHistory;
use thegn_core::log_view::{LogLevel, LogLine};
use thegn_core::theme::Hue;
use thegn_core::viz;

/// A time-series graph (one or two normalized 0..=1 series). `series2`, when
/// present, splits the plot area in half (e.g. net rx over tx).
pub struct GraphDetail {
    pub label: String,
    pub cur: String,
    pub footer: String,
    pub series: Vec<f32>,
    pub tone: Tok,
    pub series2: Option<(Vec<f32>, Tok, String)>, // (values, tone, half-label)
}

/// A side effect a detail-overlay row can fire (drilldown / open / mutation).
/// The overlay snapshots its data at open time, so an action carries everything
/// the loop needs to execute it without re-borrowing the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailAction {
    /// Open a URL in the system browser.
    OpenUrl(String),
    /// Drill a CI run's detail *in place* in the modal. Carries the cached run so
    /// the header (state/conclusion/title/branch/…) paints instantly; the loop
    /// then kicks an off-loop fetch of jobs/steps + failing-log tail and delivers
    /// the fill via [`apply_ci_detail`]. Unlike every other action, this one keeps
    /// the overlay open (see [`DetailAction::keeps_overlay`]). Boxed so a large
    /// `CiRun` doesn't bloat every `DetailAction`/`DetailOutcome`.
    DrillCiRun { run: Box<thegn_core::ci::CiRun> },
    /// Re-run a CI run (all jobs, or `failed` only), off the loop.
    CiRerun { run_id: String, failed: bool },
    /// Cancel an in-flight CI run, off the loop.
    CiCancel { run_id: String },
    /// Force a CI run-history refetch (bypasses the `[ci] ttl_secs` guard).
    CiRefresh,
    /// Drill the notification overlay into the log viewer *in place*, carrying a
    /// snapshot of the log tail so no model re-borrow / loop round-trip is needed
    /// (handled inside [`DetailOverlay::handle_key`]).
    ShowLog(Vec<LogLine>),
    /// Switch to the worktree at this path (a notification's `worktree_path`).
    /// Only reaches an *open* tab; the "Needs you" list uses [`Self::ActivateTarget`]
    /// instead so a dormant-workspace worktree opens rather than erroring.
    FocusWorktree(String),
    /// Activate a resolved sidebar row target (live tab or dormant-workspace
    /// switch) — the "Needs you" Enter action. Intercepted by the loop's Act arm
    /// (it owns the workspace-pool / drawer locals `CiActionCtx` lacks), the same
    /// machinery `Alt a` (jump-attention) uses.
    ActivateTarget(crate::sidebar::RowTarget),
    /// Acknowledge (quiet) one worktree's live "Needs you" signal, carrying the
    /// exact `(reason, since)` showing so the ack only covers that episode.
    /// Fired on highlight; keeps the overlay open.
    AckAttention {
        path: String,
        reason: thegn_core::attention::AttentionReason,
        since: Option<i64>,
    },
    /// Acknowledge every current needs-you worktree (the "Needs you" mark-all).
    AckAllAttention,
    /// Mark a single notification read *in place* (highlight) — like
    /// [`Self::DismissNotification`] but keeps the overlay open and doesn't post
    /// the "Dismissed" status.
    MarkNotificationRead { id: i64 },
    /// Mark a single notification read (dismiss from the inbox + badge), off the
    /// loop, then refresh.
    DismissNotification { id: i64 },
    /// Mark every notification read (empty the inbox + red flag), off the loop.
    ClearNotifications,
    /// Open the raw thegn.log in a pager pane (fuller scrollback than the tail).
    OpenLogPager,
    /// Copy a single log line's raw text to the system clipboard.
    CopyLine(String),
    /// Open the right panel to Work ▸ Merge queue (the MQ badge's `m` key —
    /// intercepted by the loop, which owns the panel state).
    OpenMergeQueueSection,
}

impl DetailAction {
    /// True for actions that mutate the overlay *in place* and must NOT close it
    /// when they fire (the CI in-place drill). Every other action closes the
    /// overlay. Read by the loop's Act dispatch.
    pub fn keeps_overlay(&self) -> bool {
        matches!(
            self,
            DetailAction::DrillCiRun { .. }
                | DetailAction::AckAttention { .. }
                | DetailAction::MarkNotificationRead { .. }
        )
    }
}

/// The CI-badge modal's in-place run drill lives in a child module (it reaches
/// `DetailOverlay`'s private fields); re-exported so callers keep using
/// `crate::detail::{apply_ci_detail, CiDetailPayload}`.
mod ci_drill;
pub use ci_drill::{CiDetailPayload, apply_ci_detail};
mod proxy_dash;
use ci_drill::{ci_fmt_secs, ci_glyph_marker, ci_state_word};
pub use proxy_dash::{ProxyDashPayload, apply_proxy_dash, proxy_dash_loading};

/// One scrollable list row: a colored marker glyph, the body text, and an
/// optional dim right-aligned note (relative time, count, …). Rows may carry an
/// `enter` action (fired by Enter/Return) and extra char-keyed `actions`; a list
/// with any actionable row becomes navigable (a selection cursor, not just
/// scroll).
pub struct DetailRow {
    pub marker: Tok,
    pub glyph: String,
    pub text: String,
    pub note: Option<String>,
    pub enter: Option<DetailAction>,
    pub actions: Vec<(char, DetailAction)>,
    /// Fired when the row-cursor lands on this row (mark-read-on-highlight).
    /// The action keeps the overlay open (see [`DetailAction::keeps_overlay`]);
    /// the loop dims the row in place after applying it.
    pub on_highlight: Option<DetailAction>,
    /// A non-selectable dim group-heading row (used to split one list into
    /// labelled sections). The row cursor skips these; they carry no actions.
    pub header: bool,
}

impl DetailRow {
    /// A plain (non-actionable) row.
    pub fn new(marker: Tok, glyph: impl Into<String>, text: impl Into<String>) -> DetailRow {
        DetailRow {
            marker,
            glyph: glyph.into(),
            text: text.into(),
            note: None,
            enter: None,
            actions: Vec::new(),
            on_highlight: None,
            header: false,
        }
    }
    /// A dim, non-selectable group heading — a section label inside a list. The
    /// row cursor skips it and it fires no actions.
    pub fn header(label: impl Into<String>) -> DetailRow {
        DetailRow {
            header: true,
            ..DetailRow::new(Tok::Slot(S::Dim), "", label)
        }
    }
    pub fn note(mut self, note: impl Into<String>) -> DetailRow {
        self.note = Some(note.into());
        self
    }
    pub fn on_enter(mut self, action: DetailAction) -> DetailRow {
        self.enter = Some(action);
        self
    }
    /// Fire `action` when the cursor highlights this row (mark-read-on-highlight).
    pub fn on_highlight(mut self, action: DetailAction) -> DetailRow {
        self.on_highlight = Some(action);
        self
    }
    pub fn action(mut self, key: char, action: DetailAction) -> DetailRow {
        self.actions.push((key, action));
        self
    }
}

/// A scrollable list (notifications, test failures, …).
pub struct ListDetail {
    pub rows: Vec<DetailRow>,
    pub empty_hint: String,
}

/// A static key/value block (one `key … value` row per pair).
pub struct KeyValDetail {
    pub pairs: Vec<(String, String, Tok)>,
}

/// A tokei-style aligned table: a dim header row, right-aligned numeric body
/// rows (scrollable), and a bold `Total` footer. Column 0 is left-aligned
/// (labels); every other column is right-aligned (counts). Non-actionable —
/// scroll only. Cells are pre-formatted strings; widths are computed by display
/// width at render time.
pub struct TableDetail {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: Vec<String>,
}

/// A scrollable, filterable log viewer (the notification → log drilldown). Holds
/// a snapshot of parsed lines plus live view state (level gate, text filter,
/// tail-follow); the overlay's `sel`/`scroll` cursor indexes the *filtered* view.
pub struct LogDetail {
    pub lines: Vec<LogLine>,
    /// Level gate: `Some(lvl)` shows entries `<= lvl` (Error is most severe);
    /// `None` shows all levels.
    pub level: Option<LogLevel>,
    /// Case-insensitive substring filter over each line's raw text.
    pub filter: String,
    /// True while typing into the filter (keys edit the query, not navigate).
    pub filter_edit: bool,
    /// Keep the cursor pinned to the most-recent matching line on view changes.
    pub tail: bool,
}

impl LogDetail {
    /// Indices into `lines` that pass the level gate + text filter, oldest first.
    fn matches(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                self.level.is_none_or(|lvl| l.level <= lvl)
                    && (needle.is_empty() || l.raw.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn match_count(&self) -> usize {
        self.matches().len()
    }
}

/// A stack of heterogeneous blocks (timeline graph + breakdown table/keyval),
/// drawn top → bottom. The richer disk/mem/net/gpu/power popups are built from
/// these so a glance shows both trend and composition. Scrolls (by row) when the
/// stacked height exceeds the box.
pub struct SectionsDetail {
    pub sections: Vec<Section>,
}

/// One block within a [`SectionsDetail`].
pub enum Section {
    /// A one-row dim label with an optional right-aligned note (a group header).
    Heading { label: String, note: Option<String> },
    /// A timeline graph block (header + `height`-row plot + optional footer).
    Graph(GraphSection),
    /// A columnar breakdown (optional dim header row + body rows).
    Table(TableSection),
    /// A `key … value` block (same shape as [`KeyValDetail`]).
    KeyVal(Vec<(String, String, Tok)>),
    /// A one-row `label … sparkline value` (a compact inline trend).
    Sparkrow {
        label: String,
        spark: Vec<f32>,
        cur: String,
        tone: Tok,
    },
}

/// A graph block inside a [`SectionsDetail`]: like [`GraphDetail`] but with an
/// explicit plot `height` (so the section knows its own row count) and an
/// optional footer.
pub struct GraphSection {
    pub label: String,
    pub cur: String,
    pub footer: Option<String>,
    pub series: Vec<f32>,
    pub tone: Tok,
    pub height: usize,
    pub series2: Option<(Vec<f32>, Tok)>,
}

/// A table cell: left-aligned text, or a filled bar (`frac` of `width` cells,
/// drawn with [`viz::bar_track`]).
pub enum Cell {
    Text(String, Tok),
    Bar(f32, usize, Tok),
}

impl Cell {
    /// Display width the cell occupies in its column.
    fn width(&self) -> usize {
        match self {
            Cell::Text(s, _) => s.chars().count(),
            Cell::Bar(_, w, _) => *w,
        }
    }
}

/// A columnar breakdown: an optional header row plus body rows of [`Cell`]s.
pub struct TableSection {
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Section {
    /// Row count this section occupies when stacked.
    fn height(&self) -> usize {
        match self {
            Section::Heading { .. } | Section::Sparkrow { .. } => 1,
            Section::Graph(g) => 1 + g.height + g.footer.is_some() as usize,
            Section::Table(t) => (!t.header.is_empty()) as usize + t.rows.len(),
            Section::KeyVal(rows) => rows.len(),
        }
    }
}

/// What a detail overlay shows.
pub enum DetailContent {
    Graph(GraphDetail),
    List(ListDetail),
    KeyVal(KeyValDetail),
    Table(TableDetail),
    Log(LogDetail),
    Sections(SectionsDetail),
}

/// Where the box sits relative to the originating bar item.
#[derive(Debug, Clone, Copy)]
enum Placement {
    Center,
    NearBelow(Rect),
    NearAbove(Rect),
}

impl Placement {
    /// Pick a near-the-item placement: drop below an item in the screen's top
    /// half, float above one in the bottom half (so masthead items open
    /// downward, statusbar items upward), each clamped on-screen.
    fn near(anchor: Rect, screen: Rect) -> Placement {
        if anchor.y < screen.y + screen.rows / 2 {
            Placement::NearBelow(anchor)
        } else {
            Placement::NearAbove(anchor)
        }
    }

    /// Resolve to a layer [`Anchor`] for a `spec` on `screen`.
    fn anchor(self, spec: &LayerSpec, screen: Rect) -> Anchor {
        match self {
            Placement::Center => Anchor::Center,
            Placement::NearBelow(r) => {
                let (bw, bh) = layer::box_dims(spec, screen);
                let (x, y) = layer::clamp_origin(r.x, r.y + r.rows, bw, bh, screen);
                Anchor::At { x, y }
            }
            Placement::NearAbove(r) => {
                let (bw, bh) = layer::box_dims(spec, screen);
                let want_y = r.y.saturating_sub(bh);
                let (x, y) = layer::clamp_origin(r.x, want_y, bw, bh, screen);
                Anchor::At { x, y }
            }
        }
    }
}

/// The summoned bar-item detail overlay.
pub struct DetailOverlay {
    title: String,
    content: DetailContent,
    cols: usize,
    rows: usize,
    placement: Placement,
    /// List scroll offset (rows scrolled off the top); ignored by Graph/KeyVal.
    scroll: usize,
    /// Selected row (row-cursor), only meaningful for an actionable list.
    sel: usize,
    /// A dim key-hint footer line for actionable lists (drawn on the last row).
    hint: Option<String>,
    /// While a CI-run drill's async fetch is in flight, the run id being fetched;
    /// [`apply_ci_detail`] only fills a result whose id still matches (the user
    /// may have navigated away). `None` outside a CI drill.
    pending_ci: Option<String>,
    /// A drilled run that was still in flight at its last fill — the loop
    /// re-polls it on the CI tick ([`DetailOverlay::live_ci_repoll`]) so the
    /// drill updates in place. `None` outside a CI drill / once terminal.
    live_ci: Option<thegn_core::ci::CiRun>,
}

/// What a key delivered to the detail overlay meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailOutcome {
    Pending,
    Close,
    /// The overlay fired a row action; the loop executes it and closes.
    Act(DetailAction),
}

impl DetailOverlay {
    /// The overlay's title bar text — lets the loop recognise an already-open
    /// surface (e.g. to toggle the unified notifications overlay shut).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Total scrollable rows (list rows, table body rows, or filtered log lines;
    /// 0 otherwise), for scroll clamping.
    fn content_len(&self) -> usize {
        match &self.content {
            DetailContent::List(l) => l.rows.len(),
            DetailContent::Table(t) => t.rows.len(),
            DetailContent::Log(lg) => lg.match_count(),
            _ => 0,
        }
    }

    /// Total stacked row height of a Sections popup (0 otherwise).
    fn content_rows(&self) -> usize {
        match &self.content {
            DetailContent::Sections(d) => d.sections.iter().map(Section::height).sum(),
            _ => 0,
        }
    }

    /// Largest valid scroll offset: a list/table/log scrolls to its last row; a
    /// Sections popup scrolls until its final row is visible (only when it
    /// overflows the box).
    fn scroll_max(&self) -> usize {
        match &self.content {
            DetailContent::Sections(_) => self.content_rows().saturating_sub(self.rows),
            _ => self.content_len().saturating_sub(1),
        }
    }

    /// A list is actionable (row-cursor, not just scroll) when any row carries an
    /// `enter` or char-keyed action. Non-actionable lists keep pure scroll.
    fn actionable(&self) -> bool {
        matches!(&self.content, DetailContent::List(l)
            if l.rows.iter().any(|r| r.enter.is_some() || !r.actions.is_empty()))
    }

    /// Row indices the cursor may land on — every list row except dim group
    /// [`DetailRow::header`]s. Empty for non-list content.
    fn selectable_indices(&self) -> Vec<usize> {
        match &self.content {
            DetailContent::List(l) => l
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.header)
                .map(|(i, _)| i)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Move the row cursor by `delta` *selectable* rows (skipping headers),
    /// clamped to the selectable range, then keep it in the scroll window.
    fn move_sel(&mut self, delta: i32) {
        let idx = self.selectable_indices();
        if idx.is_empty() {
            return;
        }
        let cur = idx.iter().position(|&i| i == self.sel).unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, idx.len() as i32 - 1) as usize;
        self.sel = idx[next];
        self.scroll_to_sel();
    }

    /// Visible body rows for a list (one row is reserved for the hint footer
    /// when present; the log viewer always reserves its footer row).
    fn visible_rows(&self) -> usize {
        let reserve = self.hint.is_some() || matches!(self.content, DetailContent::Log(_));
        self.rows.saturating_sub(reserve as usize)
    }

    /// Keep the selected row inside the scroll window.
    fn scroll_to_sel(&mut self) {
        let vis = self.visible_rows().max(1);
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + vis {
            self.scroll = self.sel + 1 - vis;
        }
    }

    /// The action bound to `key` on the selected row (if any).
    fn action_for(&self, key: char) -> Option<DetailAction> {
        let DetailContent::List(l) = &self.content else {
            return None;
        };
        l.rows.get(self.sel).and_then(|r| {
            r.actions
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, a)| a.clone())
        })
    }

    /// Esc / q / Ctrl-C / Ctrl-G close. A non-actionable list scrolls with
    /// j/k/arrows/PageUp/Down and closes on Enter. An actionable list moves a
    /// row cursor with j/k, fires the selected row's `enter` action on Enter,
    /// and its char-keyed actions on the bound key (all as `Act`).
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> DetailOutcome {
        // The log viewer owns every key (it has a text-filter edit sub-mode where
        // Esc/letters must not close/navigate), so it dispatches before the shared
        // guards below.
        if matches!(self.content, DetailContent::Log(_)) {
            return self.handle_log_key(key, mods);
        }
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => DetailOutcome::Close,
                _ => DetailOutcome::Pending,
            };
        }
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return DetailOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return DetailOutcome::Close;
        }
        let actionable = self.actionable();
        // Row-cursor max for actionable lists; scroll-offset max otherwise
        // (a plain list/table/log clamps to its last row, a Sections popup to
        // its overflow).
        let max = if actionable {
            self.content_len().saturating_sub(1)
        } else {
            self.scroll_max()
        };
        match key {
            KeyCode::Char('q') => DetailOutcome::Close,
            KeyCode::Enter => {
                if actionable {
                    match self.selected_enter() {
                        // ShowLog drills in place: swap this overlay's content for
                        // the log viewer (no loop round-trip), keeping the snapshot.
                        Some(DetailAction::ShowLog(lines)) => {
                            self.enter_log_view(lines);
                            DetailOutcome::Pending
                        }
                        // DrillCiRun swaps to the run's header in place *and* asks
                        // the loop to kick the off-loop jobs/steps/log fetch. It's
                        // the one action that keeps the overlay open (Act, not
                        // Pending — the loop needs to spawn the fetch).
                        Some(DetailAction::DrillCiRun { run }) => {
                            self.enter_ci_view(&run);
                            DetailOutcome::Act(DetailAction::DrillCiRun { run })
                        }
                        Some(a) => DetailOutcome::Act(a),
                        None => DetailOutcome::Pending,
                    }
                } else {
                    DetailOutcome::Close
                }
            }
            KeyCode::DownArrow | KeyCode::Char('j') => {
                if actionable {
                    self.move_sel(1);
                    self.highlight_outcome()
                } else {
                    self.scroll = (self.scroll + 1).min(max);
                    DetailOutcome::Pending
                }
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                if actionable {
                    self.move_sel(-1);
                    self.highlight_outcome()
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                    DetailOutcome::Pending
                }
            }
            KeyCode::PageDown => {
                if actionable {
                    self.move_sel(8);
                    self.highlight_outcome()
                } else {
                    self.scroll = (self.scroll + 8).min(max);
                    DetailOutcome::Pending
                }
            }
            KeyCode::PageUp => {
                if actionable {
                    self.move_sel(-8);
                    self.highlight_outcome()
                } else {
                    self.scroll = self.scroll.saturating_sub(8);
                    DetailOutcome::Pending
                }
            }
            KeyCode::Char(c) if actionable => match self.action_for(*c) {
                Some(a) => DetailOutcome::Act(a),
                None => DetailOutcome::Pending,
            },
            _ => DetailOutcome::Pending,
        }
    }

    /// The Enter action for the selected row (if any).
    fn selected_enter(&self) -> Option<DetailAction> {
        let DetailContent::List(l) = &self.content else {
            return None;
        };
        l.rows.get(self.sel).and_then(|r| r.enter.clone())
    }

    /// Outcome after a cursor move: fire the selected row's `on_highlight`
    /// (mark-read-on-highlight) if it has one, else just stay open. The action
    /// keeps the overlay open, so navigation continues normally.
    fn highlight_outcome(&self) -> DetailOutcome {
        if let DetailContent::List(l) = &self.content
            && let Some(a) = l.rows.get(self.sel).and_then(|r| r.on_highlight.clone())
        {
            DetailOutcome::Act(a)
        } else {
            DetailOutcome::Pending
        }
    }

    /// Mark the selected row read *in place*: swap its marker to the ghost tone
    /// (the same read-tone the inbox uses) and clear its `on_highlight` so
    /// re-landing on it in this static snapshot doesn't re-fire the (idempotent)
    /// DB write. Called by the loop right after applying a highlight action.
    pub fn dim_selected(&mut self) {
        if let DetailContent::List(l) = &mut self.content
            && let Some(row) = l.rows.get_mut(self.sel)
        {
            row.marker = Tok::Slot(S::Ghost);
            row.on_highlight = None;
        }
    }

    /// Key handling for the log viewer (`DetailContent::Log`). `l` cycles the
    /// level gate, `/` toggles a text-filter edit sub-mode, `a` re-pins to the
    /// tail, `y`/Enter copies the selected line, `F` expands into the side Logs
    /// panel; j/k/arrows/PageUp/Down move the cursor. Esc/q/Ctrl-C close.
    fn handle_log_key(&mut self, key: &KeyCode, mods: Modifiers) -> DetailOutcome {
        // Filter-edit sub-mode: keys build the query rather than navigate.
        if matches!(&self.content, DetailContent::Log(l) if l.filter_edit) {
            match key {
                KeyCode::Enter | KeyCode::Escape => {
                    if let DetailContent::Log(l) = &mut self.content {
                        l.filter_edit = false;
                    }
                }
                KeyCode::Backspace => {
                    if let DetailContent::Log(l) = &mut self.content {
                        l.filter.pop();
                    }
                    self.log_reclamp();
                }
                KeyCode::Char(c)
                    if !mods.intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER) =>
                {
                    if let DetailContent::Log(l) = &mut self.content {
                        l.filter.push(*c);
                    }
                    self.log_reclamp();
                }
                _ => {}
            }
            return DetailOutcome::Pending;
        }
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => DetailOutcome::Close,
                _ => DetailOutcome::Pending,
            };
        }
        if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
            return DetailOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return DetailOutcome::Close;
        }
        let max = self.content_len().saturating_sub(1);
        match key {
            KeyCode::Char('q') => DetailOutcome::Close,
            KeyCode::Char('l') => {
                self.log_cycle_level();
                DetailOutcome::Pending
            }
            KeyCode::Char('/') => {
                if let DetailContent::Log(l) = &mut self.content {
                    l.filter_edit = true;
                }
                DetailOutcome::Pending
            }
            KeyCode::Char('a') => {
                if let DetailContent::Log(l) = &mut self.content {
                    l.tail = true;
                }
                self.log_reclamp();
                DetailOutcome::Pending
            }
            KeyCode::Char('F') => DetailOutcome::Act(DetailAction::OpenLogPager),
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => match self.log_selected_raw() {
                Some(raw) => DetailOutcome::Act(DetailAction::CopyLine(raw)),
                None => DetailOutcome::Pending,
            },
            KeyCode::DownArrow | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(max);
                self.log_untail();
                self.scroll_to_sel();
                DetailOutcome::Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                self.log_untail();
                self.scroll_to_sel();
                DetailOutcome::Pending
            }
            KeyCode::PageDown => {
                self.sel = (self.sel + 8).min(max);
                self.log_untail();
                self.scroll_to_sel();
                DetailOutcome::Pending
            }
            KeyCode::PageUp => {
                self.sel = self.sel.saturating_sub(8);
                self.log_untail();
                self.scroll_to_sel();
                DetailOutcome::Pending
            }
            _ => DetailOutcome::Pending,
        }
    }

    /// A manual move breaks tail-follow (the cursor stops sticking to the end).
    fn log_untail(&mut self) {
        if let DetailContent::Log(l) = &mut self.content {
            l.tail = false;
        }
    }

    /// Cycle the level gate (Error→Warn→…→Trace→all→Error), retitle, and reclamp.
    fn log_cycle_level(&mut self) {
        let next = match &self.content {
            DetailContent::Log(l) => match l.level {
                Some(lvl) => lvl.next_cycle(),
                None => Some(LogLevel::Error),
            },
            _ => return,
        };
        if let DetailContent::Log(l) = &mut self.content {
            l.level = next;
        }
        self.title = log_title(next);
        self.log_reclamp();
    }

    /// Keep the cursor valid after the filtered set changes; tail-follow pins it
    /// to the most-recent matching line.
    fn log_reclamp(&mut self) {
        let n = self.content_len();
        if n == 0 {
            self.sel = 0;
            self.scroll = 0;
            return;
        }
        let tail = matches!(&self.content, DetailContent::Log(l) if l.tail);
        self.sel = if tail { n - 1 } else { self.sel.min(n - 1) };
        self.scroll_to_sel();
    }

    /// The raw text of the line at the cursor (over the filtered view).
    fn log_selected_raw(&self) -> Option<String> {
        let DetailContent::Log(l) = &self.content else {
            return None;
        };
        let idx = *l.matches().get(self.sel)?;
        l.lines.get(idx).map(|ln| ln.raw.clone())
    }

    /// Swap this overlay's content in place for the log viewer over `lines`
    /// (the notification → log drilldown). Opens error-gated and pinned to the
    /// most-recent matching line; grows the box to the log size.
    fn enter_log_view(&mut self, lines: Vec<LogLine>) {
        let level = Some(LogLevel::Error);
        self.content = DetailContent::Log(LogDetail {
            lines,
            level,
            filter: String::new(),
            filter_edit: false,
            tail: true,
        });
        self.title = log_title(level);
        self.cols = 72;
        self.rows = 18;
        self.hint = None;
        self.scroll = 0;
        self.sel = self.content_len().saturating_sub(1);
        self.scroll_to_sel();
    }

    /// The outer box rect this overlay will draw, for outside-click
    /// hit-testing. Mirrors `render`'s spec geometry exactly (title/badge don't
    /// affect layout — only cols/rows/placement do).
    pub fn box_rect(&self, screen: Rect) -> Option<Rect> {
        let mut spec = LayerSpec {
            cols: self.cols,
            rows: self.rows,
            ..LayerSpec::default()
        };
        spec.anchor = self.placement.anchor(&spec, screen);
        layer::box_rect(&spec, screen)
    }

    /// Paint the overlay as a summoned layer over the composed frame.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let mut spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(" esc ".into()),
            cols: self.cols,
            rows: self.rows,
            ..LayerSpec::default()
        };
        spec.anchor = self.placement.anchor(&spec, screen);
        let Some(inner) = layer::open_layer(surface, screen, &spec) else {
            return;
        };
        match &self.content {
            DetailContent::Graph(g) => render_graph(surface, inner, g),
            DetailContent::List(l) => {
                let sel = self.actionable().then_some(self.sel);
                render_list(surface, inner, l, self.scroll, sel, self.hint.as_deref());
            }
            DetailContent::KeyVal(kv) => render_keyval(surface, inner, kv),
            DetailContent::Table(t) => render_table(surface, inner, t, self.scroll),
            DetailContent::Log(lg) => render_log(surface, inner, lg, self.scroll, self.sel),
            DetailContent::Sections(d) => render_sections(surface, inner, self.scroll, d),
        }
    }
}

fn panel() -> Tok {
    Tok::Slot(S::Panel)
}

/// Draw `line` at row `y` only when it falls inside the clip rect's rows — the
/// bounds check that makes a stacked/scrolled Sections popup clip cleanly at its
/// top and bottom edges (rows above/below the box are simply skipped).
fn put_line(surface: &mut Surface, clip: Rect, x: usize, y: i64, w: usize, line: &Line, pad: Tok) {
    if y < clip.y as i64 || y >= (clip.y + clip.rows) as i64 {
        return;
    }
    seg::draw_line(surface, x, y as usize, w, line, pad);
}

/// The standalone graph popup: fill the whole box (header, plot, footer).
fn render_graph(surface: &mut Surface, inner: Rect, g: &GraphDetail) {
    let sec = GraphSection {
        label: g.label.clone(),
        cur: g.cur.clone(),
        footer: Some(g.footer.clone()),
        series: g.series.clone(),
        tone: g.tone,
        // Plot fills the box between the header (row 0) and footer (last row).
        height: inner.rows.saturating_sub(2),
        series2: g.series2.as_ref().map(|(s, t, _)| (s.clone(), *t)),
    };
    draw_graph_block(surface, inner, inner.x, inner.y as i64, inner.cols, &sec);
}

/// Draw a graph block (header + `g.height`-row plot + optional footer) at row
/// `y0`, clipped to `clip`. Shared by the standalone graph popup and the graph
/// section of a stacked popup.
fn draw_graph_block(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y0: i64,
    w: usize,
    g: &GraphSection,
) {
    // Header: label (dim) … current value (toned).
    put_line(
        surface,
        clip,
        x,
        y0,
        w,
        &Line::split(
            vec![seg(Tok::Slot(S::Dim), g.label.clone())],
            vec![seg(g.tone, g.cur.clone()).bold()],
        ),
        panel(),
    );
    let plot_top = y0 + 1;
    if g.height > 0 && w > 0 {
        match &g.series2 {
            None => draw_series(surface, clip, plot_top, g.height, &g.series, g.tone),
            Some((s2, tone2)) => {
                let top_h = g.height.div_ceil(2);
                let bot_h = g.height - top_h;
                draw_series(surface, clip, plot_top, top_h, &g.series, g.tone);
                if bot_h > 0 {
                    draw_series(surface, clip, plot_top + top_h as i64, bot_h, s2, *tone2);
                }
            }
        }
    }
    if let Some(f) = &g.footer {
        put_line(
            surface,
            clip,
            x,
            y0 + 1 + g.height as i64,
            w,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), f.clone())]),
            panel(),
        );
    }
}

/// Draw an `h`-row braille plot at row `y`, spanning the clip's full width.
fn draw_series(surface: &mut Surface, clip: Rect, y: i64, h: usize, vals: &[f32], tone: Tok) {
    for (i, row) in viz::braille_graph(vals, clip.cols, h)
        .into_iter()
        .enumerate()
    {
        put_line(
            surface,
            clip,
            clip.x,
            y + i as i64,
            clip.cols,
            &Line::segs(vec![seg(tone, row)]),
            panel(),
        );
    }
}

/// Paint a stacked Sections popup: walk sections top → bottom from a `scroll`-
/// shifted origin; each block bounds-checks its own rows via [`put_line`], so
/// rows scrolled above the box or spilling past its bottom are simply dropped.
fn render_sections(surface: &mut Surface, inner: Rect, scroll: usize, d: &SectionsDetail) {
    let mut y = inner.y as i64 - scroll as i64;
    for sec in &d.sections {
        draw_section(surface, inner, inner.x, y, inner.cols, sec);
        y += sec.height() as i64;
    }
}

fn draw_section(surface: &mut Surface, clip: Rect, x: usize, y0: i64, w: usize, sec: &Section) {
    match sec {
        Section::Heading { label, note } => {
            let line = match note {
                Some(n) => Line::split(
                    vec![seg(Tok::Slot(S::Dim), label.clone())],
                    vec![seg(Tok::Slot(S::Ghost), n.clone())],
                ),
                None => Line::segs(vec![seg(Tok::Slot(S::Dim), label.clone())]),
            };
            put_line(surface, clip, x, y0, w, &line, panel());
        }
        Section::Graph(g) => draw_graph_block(surface, clip, x, y0, w, g),
        Section::Table(t) => draw_table(surface, clip, x, y0, w, t),
        Section::KeyVal(rows) => {
            for (i, (k, v, tone)) in rows.iter().enumerate() {
                put_line(
                    surface,
                    clip,
                    x,
                    y0 + i as i64,
                    w,
                    &Line::split(
                        vec![seg(Tok::Slot(S::Dim), k.clone())],
                        vec![seg(*tone, v.clone())],
                    ),
                    panel(),
                );
            }
        }
        Section::Sparkrow {
            label,
            spark,
            cur,
            tone,
        } => {
            put_line(
                surface,
                clip,
                x,
                y0,
                w,
                &Line::split(
                    vec![seg(Tok::Slot(S::Dim), label.clone())],
                    vec![
                        seg(*tone, viz::sparkline(spark)),
                        seg(*tone, format!(" {cur}")).bold(),
                    ],
                ),
                panel(),
            );
        }
    }
}

/// Draw a table: per-column widths sized to the widest cell (a `Bar` counts as
/// its cell width), a dim header row when present, then body rows. Columns are
/// packed left → right with a one-space gap; a `Cell::Bar` renders as a filled
/// bar plus its `░` track.
fn draw_table(surface: &mut Surface, clip: Rect, x: usize, y0: i64, w: usize, t: &TableSection) {
    let ncol = t
        .rows
        .iter()
        .map(|r| r.len())
        .chain(std::iter::once(t.header.len()))
        .max()
        .unwrap_or(0);
    let mut colw = vec![0usize; ncol];
    for (i, h) in t.header.iter().enumerate() {
        colw[i] = colw[i].max(h.chars().count());
    }
    for row in &t.rows {
        for (i, c) in row.iter().enumerate() {
            colw[i] = colw[i].max(c.width());
        }
    }
    let mut y = y0;
    if !t.header.is_empty() {
        let mut segs = Vec::new();
        for (i, h) in t.header.iter().enumerate() {
            segs.push(seg(Tok::Slot(S::Ghost), format!("{:<w$} ", h, w = colw[i])));
        }
        put_line(surface, clip, x, y, w, &Line::segs(segs), panel());
        y += 1;
    }
    for row in &t.rows {
        let mut segs = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            let cw = colw[i];
            match cell {
                Cell::Text(s, tone) => {
                    segs.push(seg(*tone, format!("{s:<cw$} ")));
                }
                Cell::Bar(frac, bw, tone) => {
                    let (bar, track) = viz::bar_track(*frac, *bw);
                    segs.push(seg(*tone, bar));
                    segs.push(seg(Tok::Slot(S::Ghost), format!("{track} ")));
                }
            }
        }
        put_line(surface, clip, x, y, w, &Line::segs(segs), panel());
        y += 1;
    }
}

fn render_list(
    surface: &mut Surface,
    inner: Rect,
    l: &ListDetail,
    scroll: usize,
    sel: Option<usize>,
    hint: Option<&str>,
) {
    if l.rows.is_empty() {
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), l.empty_hint.clone())]),
            panel(),
        );
        return;
    }
    // Reserve the last inner row for the key-hint footer, when present.
    let body_rows = inner.rows.saturating_sub(hint.is_some() as usize);
    let scroll = scroll.min(l.rows.len().saturating_sub(1));
    // Actionable lists reserve a leading `❯`/space column for the row cursor;
    // plain lists keep their original layout (no extra indent).
    let cursored = sel.is_some();
    for (row, item) in l.rows.iter().skip(scroll).take(body_rows).enumerate() {
        // A dim group heading: no cursor column, no marker — just the label.
        if item.header {
            seg::draw_line(
                surface,
                inner.x,
                inner.y + row,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Dim), item.text.clone())]),
                panel(),
            );
            continue;
        }
        let selected = sel == Some(scroll + row);
        let pad = if selected { Tok::SelAccent } else { panel() };
        let mut left = Vec::new();
        if cursored {
            // A `❯` marks the selected row (the same affordance the menu uses).
            left.push(if selected {
                seg(Tok::Slot(S::Accent), "❯").bold()
            } else {
                seg(item.marker, " ")
            });
        }
        left.push(seg(item.marker, format!("{} ", item.glyph)));
        left.push(seg(Tok::Slot(S::Text), item.text.clone()));
        let line = match &item.note {
            Some(n) => Line::split(left, vec![seg(Tok::Slot(S::Ghost), n.clone())]),
            None => {
                left.push(seg(Tok::Slot(S::Text), String::new()));
                Line::segs(left)
            }
        };
        seg::draw_line(surface, inner.x, inner.y + row, inner.cols, &line, pad);
    }
    if let Some(h) = hint {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Faint), h.to_string())]),
            panel(),
        );
    }
}

fn render_keyval(surface: &mut Surface, inner: Rect, kv: &KeyValDetail) {
    for (row, (k, v, tone)) in kv.pairs.iter().take(inner.rows).enumerate() {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + row,
            inner.cols,
            &Line::split(
                vec![seg(Tok::Slot(S::Dim), k.clone())],
                vec![seg(*tone, v.clone())],
            ),
            panel(),
        );
    }
}

/// Per-column max display width over the header, every body row, and the total.
fn table_widths(t: &TableDetail) -> Vec<usize> {
    use unicode_width::UnicodeWidthStr;
    let mut w = vec![0usize; t.headers.len()];
    let all = std::iter::once(&t.headers)
        .chain(t.rows.iter())
        .chain(std::iter::once(&t.total));
    for cells in all {
        for (i, c) in cells.iter().enumerate() {
            if let Some(slot) = w.get_mut(i) {
                *slot = (*slot).max(c.width());
            }
        }
    }
    w
}

/// Join a row's cells into a single line: column 0 left-aligned (labels), the
/// rest right-aligned (counts), a two-space gutter between columns. Padding is
/// display-width aware.
fn table_line(cells: &[String], widths: &[usize]) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or_else(|| cell.width());
        let pad = w.saturating_sub(cell.width());
        if i > 0 {
            out.push_str("  ");
        }
        if i == 0 {
            out.push_str(cell);
            out.push_str(&" ".repeat(pad));
        } else {
            out.push_str(&" ".repeat(pad));
            out.push_str(cell);
        }
    }
    out
}

/// A tokei-style table: dim header, scrollable body, bold `Total` footer.
fn render_table(surface: &mut Surface, inner: Rect, t: &TableDetail, scroll: usize) {
    let widths = table_widths(t);
    // Header (row 0) and Total footer (last row) are fixed; the body scrolls
    // between them.
    seg::draw_line(
        surface,
        inner.x,
        inner.y,
        inner.cols,
        &Line::segs(vec![seg(
            Tok::Slot(S::Dim),
            table_line(&t.headers, &widths),
        )]),
        panel(),
    );
    let body_rows = inner.rows.saturating_sub(2);
    let scroll = scroll.min(t.rows.len().saturating_sub(1));
    for (row, cells) in t.rows.iter().skip(scroll).take(body_rows).enumerate() {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + 1 + row,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Text), table_line(cells, &widths))]),
            panel(),
        );
    }
    if inner.rows >= 2 {
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(vec![
                seg(Tok::Slot(S::Text), table_line(&t.total, &widths)).bold(),
            ]),
            panel(),
        );
    }
}

/// Color tone for a log level (error red, warn amber, then a text→ghost ramp).
fn level_hue(l: LogLevel) -> Tok {
    match l {
        LogLevel::Error => Tok::Hue(Hue::Red),
        LogLevel::Warn => Tok::Hue(Hue::Amber),
        LogLevel::Info => Tok::Slot(S::Text),
        LogLevel::Debug => Tok::Slot(S::Dim),
        LogLevel::Trace => Tok::Slot(S::Ghost),
    }
}

/// The overlay title for a level gate (`Log · errors`, `Log · warn+`, …).
fn log_title(level: Option<LogLevel>) -> String {
    match level {
        None => "Log · all".into(),
        Some(LogLevel::Error) => "Log · errors".into(),
        Some(LogLevel::Warn) => "Log · warn+".into(),
        Some(LogLevel::Info) => "Log · info+".into(),
        Some(LogLevel::Debug) => "Log · debug+".into(),
        Some(LogLevel::Trace) => "Log · trace".into(),
    }
}

/// Drop the `YYYY-MM-DDT` date prefix so the right-hand note shows just the time.
fn short_time(ts: &str) -> String {
    ts.split_once('T')
        .map(|(_, t)| t.to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn render_log(surface: &mut Surface, inner: Rect, lg: &LogDetail, scroll: usize, sel: usize) {
    // Last inner row is the footer (key hints, or the filter prompt while editing).
    let body_rows = inner.rows.saturating_sub(1);
    let idxs = lg.matches();
    if idxs.is_empty() {
        let empty = if lg.lines.is_empty() {
            "no log data (set THEGN_LOG to enable)"
        } else if lg.filter.is_empty() && lg.level == Some(LogLevel::Error) {
            // Non-empty payload, error-gated, nothing matched: the log has no
            // errors right now (e.g. rotated/truncated since the notification).
            "no errors in current log (rotated?)"
        } else {
            "no matching log lines"
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), empty.to_string())]),
            panel(),
        );
    } else {
        let scroll = scroll.min(idxs.len().saturating_sub(1));
        for (row, &li) in idxs.iter().skip(scroll).take(body_rows).enumerate() {
            let Some(line) = lg.lines.get(li) else {
                continue;
            };
            let selected = scroll + row == sel;
            // A leading `❯` marks the cursor (like the actionable list) — every
            // tone stays on the panel background it's designed for, so there's no
            // faint-on-accent contrast pitfall.
            let cursor = if selected {
                seg(Tok::Slot(S::Accent), "❯ ").bold()
            } else {
                seg(panel(), "  ")
            };
            let tone = level_hue(line.level);
            let left = vec![
                cursor,
                seg(tone, format!("{} ", line.level.glyph())),
                seg(Tok::Slot(S::Text), line.message.clone()),
            ];
            let l = Line::split(
                left,
                vec![seg(Tok::Slot(S::Dim), short_time(&line.timestamp))],
            );
            seg::draw_line(surface, inner.x, inner.y + row, inner.cols, &l, panel());
        }
    }
    // Footer.
    if inner.rows == 0 {
        return;
    }
    let footer = if lg.filter_edit {
        Line::segs(vec![
            seg(Tok::Slot(S::Accent), "❯ "),
            seg(Tok::Slot(S::Text), lg.filter.clone()),
            seg(Tok::Slot(S::Accent), "▏"),
        ])
    } else {
        let hint = if lg.filter.is_empty() {
            "↵/y copy · l level · / filter · a end · F full log".to_string()
        } else {
            format!("filter: {} · / edit · l level · F full log", lg.filter)
        };
        Line::segs(vec![seg(Tok::Slot(S::Faint), hint)])
    };
    seg::draw_line(
        surface,
        inner.x,
        inner.y + inner.rows - 1,
        inner.cols,
        &footer,
        panel(),
    );
}

// --- content builders -------------------------------------------------------

/// min / avg / max of a 0..=1 series (empty → all zero).
fn stats01(s: &[f32]) -> (f32, f32, f32) {
    if s.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let min = s.iter().copied().fold(f32::INFINITY, f32::min);
    let max = s.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let avg = s.iter().copied().sum::<f32>() / s.len() as f32;
    (min, avg, max)
}

/// Plot width in cells → series length (two braille dot-columns per cell).
fn plot_cols(cols: usize) -> usize {
    cols.saturating_sub(0) * 2
}

/// A trimmed (no fixed-width padding) bytes/sec string for table cells.
fn rate(bps: u64) -> String {
    thegn_metrics::fmt_rate(bps).trim().to_string()
}

/// Free-space tone: red under 5%, amber under 15%, else normal text.
fn free_tone(pct: u8) -> Tok {
    if pct <= 5 {
        Tok::Hue(Hue::Red)
    } else if pct <= 15 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    }
}

/// Short label for a storage medium.
fn kind_str(kind: thegn_metrics::DiskKind) -> &'static str {
    match kind {
        thegn_metrics::DiskKind::Hdd => "HDD",
        thegn_metrics::DiskKind::Ssd => "SSD",
        thegn_metrics::DiskKind::Unknown => "—",
    }
}

/// Truncate `s` to `max` display cells, ellipsizing when it overflows.
fn trunc(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let keep = max.saturating_sub(1);
    format!("{}…", s.chars().take(keep).collect::<String>())
}

/// Format a duration in seconds as `Nh Mm` (or `Mm` under an hour).
fn fmt_eta(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Assumed sample cadence (seconds) for the slope-projected battery ETA. The
/// stats ticker defaults to ~2s; this is only used as a fallback when the OS
/// exposes no native time-to-empty/full, so an approximate base is acceptable.
const EST_SAMPLE_SECS: f32 = 2.0;

/// Estimate time-to-empty (discharging) or time-to-full (charging) from the
/// charge series' slope, as an `~Nh Mm` string. Returns `None` when there is too
/// little history, the charge is flat, or the slope contradicts the AC state
/// (so a noisy reading never contradicts the plainly-shown source). Pure and
/// unit-tested; the native `battery_eta_secs` is preferred when present.
fn battery_eta(series: &[f32], on_ac: bool) -> Option<String> {
    // Drop the leading zero padding that a short history front-loads.
    let vals: Vec<f32> = series.iter().copied().skip_while(|v| *v <= 0.0).collect();
    if vals.len() < 3 {
        return None;
    }
    let first = *vals.first()?;
    let last = *vals.last()?;
    let per = (last - first) / (vals.len() - 1) as f32; // charge fraction per sample
    if per.abs() < 1e-4 {
        return None; // flat — no meaningful projection
    }
    let discharging = per < 0.0;
    if discharging == on_ac {
        return None; // slope disagrees with the source; don't guess
    }
    let remaining = if discharging { last } else { 1.0 - last };
    let samples = remaining / per.abs();
    Some(format!("~{}", fmt_eta((samples * EST_SAMPLE_SECS) as u64)))
}

/// Build the detail overlay for a focused bar item, or `None` when the item has
/// no data to show (so Enter is a no-op rather than an empty modal). `anchor` is
/// the item's on-screen rect (for popup placement); `hist` is the rolling
/// telemetry history (panel-UI docs).
pub fn open_detail_for(
    id: &BarItemId,
    anchor: Rect,
    screen: Rect,
    model: &FrameModel,
    hist: &TelemetryHistory,
) -> Option<DetailOverlay> {
    let near = Placement::near(anchor, screen);
    match id {
        BarItemId::Widget(w) => widget_detail(w, near, model, hist),
        BarItemId::Badge(b) => badge_detail(*b, near, model),
    }
}

fn graph(
    title: &str,
    label: &str,
    cur: String,
    footer: String,
    series: Vec<f32>,
    tone: Tok,
    placement: Placement,
) -> DetailOverlay {
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::Graph(GraphDetail {
            label: label.to_string(),
            cur,
            footer,
            series,
            tone,
            series2: None,
        }),
        cols: 40,
        rows: 12,
        placement,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

fn keyval(
    title: &str,
    pairs: Vec<(String, String, Tok)>,
    cols: usize,
    placement: Placement,
) -> DetailOverlay {
    let rows = pairs.len().max(1);
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::KeyVal(KeyValDetail { pairs }),
        cols,
        rows,
        placement,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

fn table(title: &str, t: TableDetail, cols: usize, height: usize) -> DetailOverlay {
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::Table(t),
        cols,
        rows: height,
        placement: Placement::Center,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

/// A stacked multi-section popup, sized to its content height (clamped on-screen
/// by the layer). Placement is near the originating item like the other widgets.
fn sections(title: &str, cols: usize, secs: Vec<Section>, placement: Placement) -> DetailOverlay {
    let rows = secs.iter().map(Section::height).sum::<usize>().max(1);
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::Sections(SectionsDetail { sections: secs }),
        cols,
        rows,
        placement,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

fn list(
    title: &str,
    rows: Vec<DetailRow>,
    empty_hint: &str,
    cols: usize,
    height: usize,
) -> DetailOverlay {
    // Open the cursor on the first selectable row — a grouped list leads with a
    // dim header the cursor must never sit on (plain lists start at 0).
    let sel = rows.iter().position(|r| !r.header).unwrap_or(0);
    DetailOverlay {
        title: title.to_string(),
        content: DetailContent::List(ListDetail {
            rows,
            empty_hint: empty_hint.to_string(),
        }),
        cols,
        rows: height,
        placement: Placement::Center,
        scroll: 0,
        sel,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

fn widget_detail(
    w: &str,
    near: Placement,
    model: &FrameModel,
    hist: &TelemetryHistory,
) -> Option<DetailOverlay> {
    let s = &model.stats;
    let n = plot_cols(40);
    match w {
        "cpu" => {
            let series = hist.cpu_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s.cpu_pct.map_or("—".into(), |p| format!("{p}%"));
            let footer = format!(
                "min {:.0}%  avg {:.0}%  max {:.0}%",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            Some(graph(
                "CPU history",
                "CPU",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Teal),
                near,
            ))
        }
        "mem" => {
            let series = hist.mem_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s
                .mem_gib
                .map_or("—".into(), |(u, t)| format!("{u:.1}/{t:.0}G"));
            let footer = format!(
                "min {:.0}%  avg {:.0}%  max {:.0}%",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            let mut secs = vec![
                Section::Graph(GraphSection {
                    label: "MEM".into(),
                    cur,
                    footer: Some(footer),
                    series,
                    tone: Tok::Hue(Hue::Purple),
                    height: 5,
                    series2: None,
                }),
                Section::Heading {
                    label: "Breakdown".into(),
                    note: None,
                },
            ];
            if let Some((u, t)) = s.mem_gib {
                secs.push(Section::KeyVal(vec![
                    ("used".into(), format!("{u:.1}G"), Tok::Slot(S::Text)),
                    ("total".into(), format!("{t:.0}G"), Tok::Slot(S::Dim)),
                    (
                        "free".into(),
                        format!("{:.1}G", (t - u).max(0.0)),
                        Tok::Slot(S::Dim),
                    ),
                ]));
            }
            if let Some((u, t)) = s.swap_gib {
                secs.push(Section::Sparkrow {
                    label: "swap".into(),
                    spark: hist.swap_series(16),
                    cur: format!("{u:.1}/{t:.0}G"),
                    tone: Tok::Hue(Hue::Blue),
                });
            }
            Some(sections("Memory", 40, secs, near))
        }
        "temp" => {
            s.cpu_temp_c?;
            let series = hist.temp_series(n);
            let (mn, av, mx) = stats01(&series);
            let cur = s.cpu_temp_c.map_or("—".into(), |c| format!("{c:.0}°C"));
            let footer = format!(
                "min {:.0}°C  avg {:.0}°C  max {:.0}°C",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            Some(graph(
                "Temperature history",
                "TEMP",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Amber),
                near,
            ))
        }
        "load" => {
            let (one, five, fifteen) = s.load_avg?;
            let series = hist.load_series(n);
            let cur = format!("{one:.2}");
            let footer = format!("1m {one:.2} · 5m {five:.2} · 15m {fifteen:.2}");
            Some(graph(
                "Load average",
                "LOAD",
                cur,
                footer,
                series,
                Tok::Hue(Hue::Blue),
                near,
            ))
        }
        "net" => {
            let (rx, tx) = hist.last_rates();
            let mut secs = vec![
                Section::Graph(GraphSection {
                    label: "NET".into(),
                    cur: format!("↓{} ↑{}", rate(rx), rate(tx)),
                    footer: Some("↓ rx (top) · ↑ tx (bottom)".into()),
                    series: hist.rx_series(n),
                    tone: Tok::Hue(Hue::Green),
                    height: 6,
                    series2: Some((hist.tx_series(n), Tok::Hue(Hue::Blue))),
                }),
                Section::Heading {
                    label: "Interfaces".into(),
                    note: None,
                },
            ];
            if s.net_ifaces.is_empty() {
                secs.push(Section::KeyVal(vec![(
                    "interfaces".into(),
                    "idle".into(),
                    Tok::Slot(S::Ghost),
                )]));
            } else {
                let rows: Vec<Vec<Cell>> = s
                    .net_ifaces
                    .iter()
                    .map(|(name, r, t)| {
                        vec![
                            Cell::Text(trunc(name.clone(), 14), Tok::Slot(S::Text)),
                            Cell::Text(format!("↓{}", rate(*r)), Tok::Hue(Hue::Green)),
                            Cell::Text(format!("↑{}", rate(*t)), Tok::Hue(Hue::Blue)),
                        ]
                    })
                    .collect();
                secs.push(Section::Table(TableSection {
                    header: vec!["iface".into(), "rx".into(), "tx".into()],
                    rows,
                }));
            }
            Some(sections("Network", 44, secs, near))
        }
        "swap" => {
            let (u, t) = s.swap_gib?;
            Some(keyval(
                "Swap",
                vec![
                    ("used".into(), format!("{u:.1}G"), Tok::Slot(S::Text)),
                    ("total".into(), format!("{t:.0}G"), Tok::Slot(S::Dim)),
                ],
                28,
                near,
            ))
        }
        "gpu" => {
            let p = s.gpu_pct?;
            let series = hist.gpu_series(n);
            let (mn, av, mx) = stats01(&series);
            let footer = format!(
                "min {:.0}%  avg {:.0}%  max {:.0}%",
                mn * 100.0,
                av * 100.0,
                mx * 100.0
            );
            let mut kv = vec![("utilization".into(), format!("{p}%"), Tok::Hue(Hue::Teal))];
            if let Some((u, t)) = s.gpu_mem_mib {
                kv.push(("vram".into(), format!("{u}/{t} MiB"), Tok::Slot(S::Text)));
            }
            if let Some(c) = s.gpu_temp_c {
                kv.push(("temp".into(), format!("{c:.0}°C"), Tok::Slot(S::Dim)));
            }
            if let Some(w) = s.gpu_power_w {
                kv.push(("power".into(), format!("{w:.0} W"), Tok::Slot(S::Dim)));
            }
            Some(sections(
                "GPU",
                36,
                vec![
                    Section::Graph(GraphSection {
                        label: "GPU".into(),
                        cur: format!("{p}%"),
                        footer: Some(footer),
                        series,
                        tone: Tok::Hue(Hue::Teal),
                        height: 6,
                        series2: None,
                    }),
                    Section::KeyVal(kv),
                ],
                near,
            ))
        }
        "freq" => {
            let mhz = s.cpu_freq_mhz?;
            Some(keyval(
                "CPU frequency",
                vec![
                    (
                        "current".into(),
                        format!("{:.2} GHz", mhz as f32 / 1000.0),
                        Tok::Slot(S::Text),
                    ),
                    (
                        "cores".into(),
                        format!("{}", s.cpu_cores.len()),
                        Tok::Slot(S::Dim),
                    ),
                ],
                30,
                near,
            ))
        }
        "uptime" => {
            let secs = s.uptime_secs?;
            let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
            Some(keyval(
                "Uptime",
                vec![("up".into(), format!("{d}d {h}h {m}m"), Tok::Slot(S::Text))],
                30,
                near,
            ))
        }
        "battery" => {
            let (p, on_ac) = s.battery?;
            let series = hist.battery_series(n);
            let tone = if on_ac {
                Tok::Hue(Hue::Green)
            } else if p <= 15 {
                Tok::Hue(Hue::Amber)
            } else {
                Tok::Hue(Hue::Blue)
            };
            let mut kv = vec![
                ("charge".into(), format!("{p}%"), Tok::Slot(S::Text)),
                (
                    "source".into(),
                    if on_ac { "AC".into() } else { "battery".into() },
                    Tok::Slot(S::Dim),
                ),
            ];
            if let Some(w) = s.battery_power_w {
                kv.push(("power".into(), format!("{w:.1} W"), Tok::Slot(S::Dim)));
            }
            // Native OS estimate wins; else project from the charge slope.
            let eta = s
                .battery_eta_secs
                .map(fmt_eta)
                .or_else(|| battery_eta(&series, on_ac));
            if let Some(e) = eta {
                let label = if on_ac { "to full" } else { "to empty" };
                kv.push((label.into(), e, Tok::Slot(S::Dim)));
            }
            Some(sections(
                "Battery",
                34,
                vec![
                    Section::Graph(GraphSection {
                        label: "BATTERY".into(),
                        cur: format!("{p}%"),
                        footer: Some(if on_ac {
                            "on AC".into()
                        } else {
                            "on battery".into()
                        }),
                        series,
                        tone,
                        height: 5,
                        series2: None,
                    }),
                    Section::KeyVal(kv),
                ],
                near,
            ))
        }
        "disk" => {
            if s.disks.is_empty() {
                return None;
            }
            let series = hist.disk_io_series(n);
            let rows: Vec<Vec<Cell>> = s
                .disks
                .iter()
                .map(|d| {
                    let tone = free_tone(d.free_pct);
                    vec![
                        Cell::Text(trunc(d.mount.clone(), 18), Tok::Slot(S::Text)),
                        Cell::Text(kind_str(d.kind).into(), Tok::Slot(S::Dim)),
                        Cell::Bar(d.free_pct as f32 / 100.0, 8, tone),
                        Cell::Text(format!("{}%", d.free_pct), tone),
                        Cell::Text(format!("↓{}", rate(d.read_bps)), Tok::Slot(S::Dim)),
                        Cell::Text(format!("↑{}", rate(d.write_bps)), Tok::Slot(S::Dim)),
                    ]
                })
                .collect();
            Some(sections(
                "Disks",
                60,
                vec![
                    Section::Graph(GraphSection {
                        label: "DISK IO".into(),
                        cur: rate(hist.last_disk_io()),
                        footer: Some("read + write, window-scaled".into()),
                        series,
                        tone: Tok::Hue(Hue::Blue),
                        height: 5,
                        series2: None,
                    }),
                    Section::Heading {
                        label: "Volumes".into(),
                        note: None,
                    },
                    Section::Table(TableSection {
                        header: vec![
                            "mount".into(),
                            "kind".into(),
                            "free".into(),
                            "".into(),
                            "read".into(),
                            "write".into(),
                        ],
                        rows,
                    }),
                ],
                Placement::Center,
            ))
        }
        "loc" => {
            let r = model.loc.as_ref()?;
            let headers = ["Language", "Files", "Lines", "Code", "Comments", "Blanks"]
                .map(String::from)
                .to_vec();
            let rows: Vec<Vec<String>> = r
                .langs
                .iter()
                .map(|l| {
                    vec![
                        l.name.clone(),
                        l.files.to_string(),
                        l.lines.to_string(),
                        l.code.to_string(),
                        l.comments.to_string(),
                        l.blanks.to_string(),
                    ]
                })
                .collect();
            let total = vec![
                "Total".into(),
                r.total_files.to_string(),
                r.total_lines.to_string(),
                r.total_code.to_string(),
                r.total_comments.to_string(),
                r.total_blanks.to_string(),
            ];
            // header + N body rows + total, capped so the box stays on-screen.
            let height = (r.langs.len() + 2).clamp(4, 18);
            Some(table(
                "Lines of code",
                TableDetail {
                    headers,
                    rows,
                    total,
                },
                58,
                height,
            ))
        }
        "date" | "clock" => {
            let now = chrono::Local::now();
            Some(keyval(
                "Date & time",
                vec![
                    (
                        "date".into(),
                        now.format("%A %B %-d, %Y").to_string(),
                        Tok::Slot(S::Text),
                    ),
                    (
                        "time".into(),
                        now.format("%H:%M:%S %Z").to_string(),
                        Tok::Slot(S::Dim),
                    ),
                ],
                34,
                near,
            ))
        }
        "pr" => {
            let pr = model.panel.pr.as_ref()?;
            Some(keyval(
                "Pull request",
                vec![
                    (
                        "number".into(),
                        format!("#{}", pr.number),
                        Tok::Slot(S::Text),
                    ),
                    ("state".into(), pr.state.clone(), Tok::Slot(S::Dim)),
                    (
                        "draft".into(),
                        if pr.is_draft {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        Tok::Slot(S::Dim),
                    ),
                ],
                50,
                Placement::Center,
            ))
        }
        "tests" => {
            let t = model.panel.tests.as_ref()?;
            Some(keyval(
                "Tests",
                vec![
                    (
                        "passed".into(),
                        format!("{}", t.passed),
                        Tok::Hue(Hue::Green),
                    ),
                    ("failed".into(), format!("{}", t.failed), Tok::Hue(Hue::Red)),
                ],
                40,
                Placement::Center,
            ))
        }
        _ => None,
    }
}

fn badge_detail(b: BarBadge, near: Placement, model: &FrameModel) -> Option<DetailOverlay> {
    match b {
        // Both the inbox chip (⚑ / N unread) and the needs-you chip (✋) open the
        // single unified surface: Needs you · Alerts · Notifications · Logs.
        BarBadge::Notifications | BarBadge::Attention => unified_detail(model),
        BarBadge::Agent => {
            let a = model.agent_activity.as_ref()?;
            let conn = match a.conn {
                chrome::AgentConn::Online => "online",
                chrome::AgentConn::Connecting => "connecting",
                chrome::AgentConn::Exited => "offline",
                chrome::AgentConn::Error => "error",
            };
            let mut pairs = vec![
                ("connection".into(), conn.into(), Tok::Slot(S::Text)),
                (
                    "last tool".into(),
                    a.last_tool.clone().unwrap_or_else(|| "—".into()),
                    Tok::Slot(S::Dim),
                ),
                (
                    "running".into(),
                    if a.running { "yes".into() } else { "no".into() },
                    Tok::Slot(S::Dim),
                ),
            ];
            if a.context_size > 0 {
                let pct = (a.context_used * 100 / a.context_size).clamp(0, 100);
                pairs.push((
                    "context".into(),
                    format!("{pct}% ({}/{})", a.context_used, a.context_size),
                    Tok::Slot(S::Dim),
                ));
            }
            Some(keyval("Agent", pairs, 56, Placement::Center))
        }
        BarBadge::Ci => {
            if model.panel.ci_runs.is_empty() {
                return None;
            }
            use thegn_core::ci::CiState;
            let now = thegn_core::util::now();
            let rows: Vec<DetailRow> = model
                .panel
                .ci_runs
                .iter()
                .map(|r| {
                    let (glyph, marker) = ci_glyph_marker(r.state);
                    // Text: "<name> · <outcome>" plus the commit/PR title when it
                    // adds something the name doesn't. Note: "#run · event · branch
                    // · dur" — the context that used to be hidden behind the glyph.
                    let mut text = format!("{} \u{00b7} {}", r.name, ci_state_word(r.state));
                    if !r.title.is_empty() && r.title != r.name {
                        text.push_str(&format!(" \u{2014} {}", r.title));
                    }
                    let mut note_parts: Vec<String> = Vec::new();
                    if let Some(n) = r.run_number {
                        note_parts.push(format!("#{n}"));
                    }
                    if !r.event.is_empty() {
                        note_parts.push(r.event.clone());
                    }
                    if !r.branch.is_empty() {
                        note_parts.push(r.branch.clone());
                    }
                    if let Some(secs) = r.duration_secs(now) {
                        note_parts.push(ci_fmt_secs(secs));
                    }
                    // Enter drills into the run's jobs/steps *in the modal*; `o`
                    // opens the run page; `r`/`R` re-run; `c` cancels an in-flight
                    // run. Mutations the provider can't perform are declined off-loop.
                    let mut row = DetailRow::new(marker, glyph, text)
                        .on_enter(DetailAction::DrillCiRun {
                            run: Box::new(r.clone()),
                        })
                        .action(
                            'r',
                            DetailAction::CiRerun {
                                run_id: r.id.clone(),
                                failed: false,
                            },
                        )
                        .action(
                            'R',
                            DetailAction::CiRerun {
                                run_id: r.id.clone(),
                                failed: true,
                            },
                        )
                        .action('g', DetailAction::CiRefresh);
                    if !r.url.is_empty() {
                        row = row.action('o', DetailAction::OpenUrl(r.url.clone()));
                    }
                    if r.state == CiState::Running {
                        row = row.action(
                            'c',
                            DetailAction::CiCancel {
                                run_id: r.id.clone(),
                            },
                        );
                    }
                    if !note_parts.is_empty() {
                        row = row.note(note_parts.join(" \u{00b7} "));
                    }
                    row
                })
                .collect();
            let mut ov = list("CI runs", rows, "no CI runs", 60, 14);
            ov.hint = Some("↵ view · o open · r/R rerun · c cancel · g refresh".into());
            Some(ov)
        }
        BarBadge::MergeQueue => {
            if model.panel.merge_queue.is_empty() {
                return None;
            }
            let rows: Vec<DetailRow> = model
                .panel
                .merge_queue
                .iter()
                .map(|r| {
                    let (glyph, marker) = match r.status.as_str() {
                        "landed" => ("✓", Tok::Hue(Hue::Green)),
                        "ready" => ("◆", Tok::Hue(Hue::Green)),
                        "deferred" | "gate_failed" => ("⚑", Tok::Hue(Hue::Red)),
                        "needs_human" => ("✋", Tok::Hue(Hue::Red)),
                        "folding" | "verifying" => ("●", Tok::Hue(Hue::Amber)),
                        "agent_running" => ("◐", Tok::Hue(Hue::Amber)),
                        _ => ("○", Tok::Slot(S::Dim)),
                    };
                    let note = match r.status.as_str() {
                        "deferred" | "gate_failed" | "needs_human" => r
                            .error_detail
                            .as_deref()
                            .or(r.conflict_paths.as_deref())
                            .map(|p| p.replace('\n', ", ")),
                        _ => Some(r.status.clone()),
                    };
                    let mut row = DetailRow::new(marker, glyph, r.branch.clone())
                        .on_enter(DetailAction::FocusWorktree(r.worktree.clone()))
                        .action('m', DetailAction::OpenMergeQueueSection);
                    if let Some(n) = note {
                        row = row.note(n);
                    }
                    row
                })
                .collect();
            let mut ov = list("Merge queue", rows, "merge queue empty", 56, 14);
            ov.hint = Some("↵ focus worktree · m open section".into());
            Some(ov)
        }
        BarBadge::Ingress => {
            if model.shares.is_empty() {
                return None;
            }
            let rows: Vec<DetailRow> = model
                .shares
                .iter()
                .map(|s| {
                    let marker = if s.failed {
                        Tok::Hue(Hue::Red)
                    } else if s.public {
                        Tok::Hue(Hue::Amber)
                    } else {
                        Tok::Hue(Hue::Teal)
                    };
                    let mut row = DetailRow::new(marker, "⇅", format!("port {}", s.port));
                    if let Some(u) = s.url.clone() {
                        row = row.note(u);
                    }
                    row
                })
                .collect();
            Some(list("Ingress shares", rows, "no shares", 60, 12))
        }
        BarBadge::Media => {
            let m = model.panel.media.as_ref()?;
            let text = m.badge()?;
            Some(keyval(
                "Now playing",
                vec![("track".into(), text, Tok::Hue(Hue::Green))],
                50,
                near,
            ))
        }
        BarBadge::AiCost => {
            let m = model.ai_metrics.as_ref()?;
            Some(keyval(
                "Agent spend",
                vec![
                    ("agent".into(), m.agent.clone(), Tok::Slot(S::Text)),
                    (
                        "cost".into(),
                        format!("${:.2}", m.cost),
                        Tok::Hue(Hue::Teal),
                    ),
                    (
                        "tokens".into(),
                        format!("{}", m.tokens.input + m.tokens.output),
                        Tok::Slot(S::Dim),
                    ),
                ],
                44,
                Placement::Center,
            ))
        }
        BarBadge::DiskWarn => {
            use thegn_core::disk::human;
            let ic = &model.stats_icons;
            // Free-% → tone, matching the badge (red ≤ critical, amber ≤ warn).
            let free_tone = |pct: u8| {
                if pct <= ic.disk_free_critical {
                    Tok::Hue(Hue::Red)
                } else if pct <= ic.disk_free_warn {
                    Tok::Hue(Hue::Amber)
                } else {
                    Tok::Slot(S::Text)
                }
            };
            // Worktree usage on this fs + the regenerable `target/` share.
            let (wt_total, wt_target) = model
                .sidebar_status
                .disk_sizes
                .values()
                .fold((0u64, 0u64), |(t, g), &(total, target)| {
                    (t + total.max(0) as u64, g + target.max(0) as u64)
                });
            let mut pairs: Vec<(String, String, Tok)> = Vec::new();
            match (model.stats.disk_bytes, model.stats.disk_free_pct) {
                (Some((total, avail)), pct_opt) => {
                    let pct = pct_opt.unwrap_or(0);
                    pairs.push((
                        "free".into(),
                        format!("{} ({pct}%)", human(avail)),
                        free_tone(pct),
                    ));
                    pairs.push((
                        "used".into(),
                        human(total.saturating_sub(avail)),
                        Tok::Slot(S::Dim),
                    ));
                    pairs.push(("total".into(), human(total), Tok::Slot(S::Dim)));
                }
                (None, Some(pct)) => {
                    pairs.push(("free".into(), format!("{pct}%"), free_tone(pct)));
                }
                (None, None) => {}
            }
            pairs.push(("worktrees".into(), human(wt_total), Tok::Slot(S::Dim)));
            if wt_target > 0 {
                pairs.push(("reclaimable".into(), human(wt_target), Tok::Slot(S::Dim)));
            }
            Some(keyval("Disk space", pairs, 44, near))
        }
        BarBadge::Zoom => Some(keyval(
            "Zoom",
            vec![(
                "state".into(),
                "pane zoomed fullscreen".into(),
                Tok::Hue(Hue::Purple),
            )],
            40,
            near,
        )),
        BarBadge::Lock => Some(keyval(
            "Keybind lock",
            vec![(
                "state".into(),
                "input locked (Ctrl+g toggles)".into(),
                Tok::Hue(Hue::Amber),
            )],
            44,
            near,
        )),
        BarBadge::Sync => Some(keyval(
            "Sync panes",
            vec![(
                "state".into(),
                "broadcasting input to all panes".into(),
                Tok::Hue(Hue::Red),
            )],
            44,
            near,
        )),
        BarBadge::Persist => Some(keyval(
            "Persistent pane",
            vec![(
                "state".into(),
                "daemon-backed: quit keeps it running; relaunch reattaches".into(),
                Tok::Hue(Hue::Teal),
            )],
            52,
            near,
        )),
    }
}

/// The one unified notification surface, opened by both statusbar chips (the
/// inbox `⚑`/unread chip and the needs-you `✋` chip). It folds what used to be
/// two separate popups plus a logs section into a single grouped list:
///
/// - **Needs you** — the live, per-worktree attention rollup (what needs a
///   decision now).
/// - **Alerts** — unread failure-priority notifications *not* already covered by
///   a Needs-you row (host-global / orphan alerts only — no double-show).
/// - **Notifications** — the rest of the inbox (Notice/Info history).
/// - **Logs** — one quiet entry point into thegn.log (never a red alert;
///   self-diagnostics are gated off by default, see `surface_self_log_errors`).
fn unified_detail(model: &FrameModel) -> Option<DetailOverlay> {
    use thegn_core::notification::Priority;
    let mut rows: Vec<DetailRow> = Vec::new();

    // 1. Needs you — the actionable per-worktree rollup. Its paths drive the
    //    de-dup: a worktree failure surfaced here is suppressed in Alerts.
    let (needs_rows, needs_paths) = needs_you_rows(model);
    if !needs_rows.is_empty() {
        rows.push(DetailRow::header("Needs you"));
        rows.extend(needs_rows);
    }

    // The inbox, newest-first. `log:thegn` rows are pulled into the Logs group;
    // everything else splits by severity into Alerts vs Notifications.
    let mut notes: Vec<_> = model.panel.notifications.clone();
    notes.sort_by_key(|n| std::cmp::Reverse(n.created_at_ms));

    // 2. Alerts — failure-priority, not a log row, and not already a Needs-you
    //    worktree (dedup). Priority uses the built-in default so the render layer
    //    needs no config; overrides still drive the badge counts.
    let alerts: Vec<DetailRow> = notes
        .iter()
        .filter(|n| {
            n.source_ref != "log:thegn"
                && n.kind.default_priority() == Priority::Alert
                && (n.worktree_path.is_empty() || !needs_paths.contains(&n.worktree_path))
        })
        .map(notification_row)
        .collect();
    if !alerts.is_empty() {
        rows.push(DetailRow::header("Alerts"));
        rows.extend(alerts);
    }

    // 3. Notifications — the Notice/Info history (never log rows).
    let inbox: Vec<DetailRow> = notes
        .iter()
        .filter(|n| n.source_ref != "log:thegn" && n.kind.default_priority() < Priority::Alert)
        .map(notification_row)
        .collect();
    if !inbox.is_empty() {
        rows.push(DetailRow::header("Notifications"));
        rows.extend(inbox);
    }

    // 4. Logs — always one dim entry point into thegn.log.
    rows.push(DetailRow::header("Logs"));
    rows.push(logs_row(model, &notes));

    let mut ov = list("Notifications", rows, "all clear", 62, 18);
    ov.hint = Some("↵ open · x dismiss · a mark all read · o log · Alt a next".into());
    Some(ov)
}

/// The "Needs you" rows (live attention rollup) plus the set of worktree paths
/// they cover — the caller uses the set to suppress duplicate Alert rows.
fn needs_you_rows(model: &FrameModel) -> (Vec<DetailRow>, std::collections::HashSet<String>) {
    use thegn_core::attention::AttentionTier;
    let g = crate::caps::active_glyphs();
    let mut paths = std::collections::HashSet::new();
    let rows = crate::handlers::attention::needs_user_ordered(model)
        .into_iter()
        .map(|(path, score)| {
            paths.insert(path.clone());
            // Branch label from the tree when the row exists; else the path's
            // basename (registered-but-unlisted edge).
            let label = model
                .sidebar_rows
                .iter()
                .find(|r| r.worktree_path.as_deref() == Some(path.as_str()))
                .map(|r| r.label.clone())
                .unwrap_or_else(|| {
                    std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                });
            let (glyph, marker) = match score.tier {
                AttentionTier::Blocked => (g.attention, Tok::Hue(Hue::Red)),
                AttentionTier::Failure => (g.cross, Tok::Hue(Hue::Red)),
                _ => (g.dot_filled, Tok::Hue(Hue::Amber)),
            };
            let text = format!("{label} \u{2014} {}", score.reason.label());
            // Enter resolves the sidebar row's target (live tab OR a
            // dormant-workspace switch) so activation never dead-ends on
            // "isn't open"; only the no-row edge falls back to the open-tab-only
            // `FocusWorktree`. Same machinery as `Alt a`.
            let enter = model
                .sidebar_rows
                .iter()
                .find(|r| {
                    r.kind == crate::sidebar::RowKind::Worktree
                        && r.worktree_path.as_deref() == Some(path.as_str())
                        && r.tab_target.is_some()
                })
                .and_then(|r| r.tab_target.clone())
                .map(DetailAction::ActivateTarget)
                .unwrap_or_else(|| DetailAction::FocusWorktree(path.clone()));
            let mut row = DetailRow::new(marker, glyph, text)
                .on_enter(enter)
                // `x` quiets this episode explicitly; a/R quiet them all.
                // No quiet-on-highlight: navigating the list must never
                // silently ack an item the user only meant to inspect.
                .action(
                    'x',
                    DetailAction::AckAttention {
                        path: path.clone(),
                        reason: score.reason,
                        since: score.since,
                    },
                )
                .action('a', DetailAction::AckAllAttention)
                .action('R', DetailAction::AckAllAttention);
            if let Some(at) = score.since {
                row = row.note(format!("{} ago", thegn_core::util::age(at)));
            }
            row
        })
        .collect();
    (rows, paths)
}

/// One inbox row for the Alerts / Notifications groups (log rows are handled by
/// [`logs_row`]). Enter jumps to the worktree; `x` dismisses; `X`/`R`/`a` clear
/// all; highlighting an unread row marks it read in place.
fn notification_row(n: &thegn_core::notification::Notification) -> DetailRow {
    let (glyph, marker) = notif_glyph(n.kind);
    let marker = if n.read { Tok::Slot(S::Ghost) } else { marker };
    // `created_at_ms` is epoch *seconds* (legacy misnomer), so `util::age` —
    // not a millisecond clock — gives the real age.
    let mut row = DetailRow::new(marker, glyph, n.message.clone())
        .note(format!("{} ago", thegn_core::util::age(n.created_at_ms)));
    if !n.worktree_path.is_empty() {
        row = row.on_enter(DetailAction::FocusWorktree(n.worktree_path.clone()));
    }
    if n.id != 0 {
        row = row.action('x', DetailAction::DismissNotification { id: n.id });
        if !n.read {
            row = row.on_highlight(DetailAction::MarkNotificationRead { id: n.id });
        }
    }
    row.action('X', DetailAction::ClearNotifications)
        .action('R', DetailAction::ClearNotifications)
        .action('a', DetailAction::ClearNotifications)
}

/// The single dim Logs entry point. In dev mode (`surface_self_log_errors`) a
/// `log:thegn` notification exists and labels the row ("N errors in
/// thegn.log"); by default there is none, so it reads "open thegn.log". Always
/// dim — self-diagnostics are never a red user alert — and only ever one row
/// (the newest), so a flapping log can't stack duplicates.
fn logs_row(model: &FrameModel, notes: &[thegn_core::notification::Notification]) -> DetailRow {
    let log = notes.iter().find(|n| n.source_ref == "log:thegn");
    let text = log
        .map(|n| n.message.clone())
        .unwrap_or_else(|| "open thegn.log".into());
    let mut row = DetailRow::new(Tok::Slot(S::Dim), "\u{2261}", text)
        .on_enter(DetailAction::ShowLog(model.panel.log_tail.clone()))
        .action('o', DetailAction::OpenLogPager);
    if let Some(n) = log {
        row = row.note(format!("{} ago", thegn_core::util::age(n.created_at_ms)));
        if n.id != 0 {
            row = row.action('x', DetailAction::DismissNotification { id: n.id });
            if !n.read {
                row = row.on_highlight(DetailAction::MarkNotificationRead { id: n.id });
            }
        }
    }
    row
}

fn notif_glyph(kind: thegn_core::notification::NotificationKind) -> (&'static str, Tok) {
    use thegn_core::notification::NotificationKind as K;
    match kind {
        K::AgentFailed | K::TestFailed | K::ProcessFailed => ("✗", Tok::Hue(Hue::Red)),
        K::AgentAttention | K::Overdue => ("⚑", Tok::Hue(Hue::Amber)),
        K::AgentDone | K::ProcessExited | K::WorktreeCreated => ("✓", Tok::Hue(Hue::Green)),
        _ => ("•", Tok::Hue(Hue::Blue)),
    }
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
