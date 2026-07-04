//! The Alt+W "new workspace" fuzzy picker: type-to-filter over every git repo
//! superzej can see (DB-known + recents instantly, plus an off-loop
//! `discover_repos` scan of the configured `repo_roots` streamed in as it
//! finishes), with Tab toggling a manual-entry mode for paths, clone URLs and
//! brand-new projects. Replaces the static `new_workspace_menu` list.
//!
//! Loop wiring mirrors the worktree wizard: an `Option<WorkspacePicker>` modal
//! whose `handle_key` returns a [`PickerOutcome`] the loop maps onto the
//! shared `workspace_create` flows. The discovery channel is owned here (like
//! `search_everywhere::PaletteSession`) so the loop only adds a one-line drain.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatcherConfig, Matcher, Utf32Str};
use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use termwiz::terminal::TerminalWaker;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};

/// Maximum visible repo rows at one time (the list scrolls past this).
const MAX_ITEMS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerMode {
    /// Type-to-filter over the known/discovered repo list (the default).
    Fuzzy,
    /// Free-text entry: existing path, clone URL, or a new-project leaf.
    Manual,
}

/// One selectable repo row. Fuzzy matching runs over `name` + `path` so a
/// path fragment ("code/acme") narrows just like a basename does.
#[derive(Debug, Clone)]
pub(crate) struct RepoEntry {
    pub(crate) path: String,
    pub(crate) name: String,
}

/// What a key delivered to the picker meant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    Pending,
    Cancel,
    /// Fuzzy Enter: open this repo path as a workspace.
    OpenRepo(String),
    /// Manual Enter: raw typed input, classified by `plan_new_workspace_input`.
    Manual(String),
}

/// The picker's instant seed: most-recent first (frecency for the empty
/// query), then every other repo the DB knows, deduped in order and
/// stat-filtered to dirs that still exist.
pub(crate) fn seed_repos(db: &superzej_core::db::Db) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !out.contains(&p) && std::path::Path::new(&p).is_dir() {
            out.push(p);
        }
    };
    for p in db.recent_repos(50).unwrap_or_default() {
        push(p);
    }
    for p in db.known_repos().unwrap_or_default() {
        push(p);
    }
    out
}

/// A tiny single-line editor: a text buffer plus a char-aware cursor. Enough
/// for the picker's query and manual-entry fields — insert/delete AT the cursor
/// and horizontal movement, so a mistyped URL can be fixed mid-string (not just
/// backspaced from the end), plus paste. `cursor` is a byte offset into `buf`,
/// always kept on a char boundary.
#[derive(Debug, Default, Clone)]
pub(crate) struct TextField {
    buf: String,
    cursor: usize,
}

impl TextField {
    fn new(s: impl Into<String>) -> Self {
        let buf = s.into();
        let cursor = buf.len();
        Self { buf, cursor }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.buf
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The cursor as a byte offset (always a char boundary) — used by the
    /// renderer to split the buffer around the cursor bar.
    fn cursor(&self) -> usize {
        self.cursor
    }

    fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert pasted text at the cursor. Newlines are stripped — a pasted URL
    /// must not submit or split the single line.
    fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !matches!(c, '\n' | '\r')) {
            self.insert_char(c);
        }
    }

    /// Delete the char before the cursor (Backspace). Returns whether anything
    /// was removed.
    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.buf[..self.cursor]
            .chars()
            .next_back()
            .expect("cursor > 0 ⇒ a preceding char");
        self.cursor -= prev.len_utf8();
        self.buf.remove(self.cursor);
        true
    }

    /// Delete the char at the cursor (Delete/^D). Returns whether anything was
    /// removed.
    fn delete(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.buf.remove(self.cursor);
        true
    }

    fn left(&mut self) {
        if let Some(prev) = self.buf[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    fn right(&mut self) {
        if let Some(next) = self.buf[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.buf.len();
    }
}

/// The picker's lifecycle. It no longer vanishes the instant you submit: a
/// clone keeps it on screen showing live `git clone` progress, and a bad
/// input / failed clone parks it in `Error` (visible until dismissed) instead
/// of flashing a status line behind a dismissed modal.
#[derive(Debug)]
pub(crate) enum Phase {
    /// Browsing the fuzzy list or typing in the manual field.
    Browse,
    /// A URL clone is running off-loop; `line` is the latest git progress line.
    Cloning { url: String, line: Option<String> },
    /// A validation or clone error — shown inline, input stays editable.
    Error(String),
}

pub(crate) struct WorkspacePicker {
    mode: PickerMode,
    /// Lifecycle phase (Browse / Cloning / Error).
    phase: Phase,
    /// Fuzzy-filter field (kept across Tab toggles).
    query: TextField,
    /// Manual-entry field (kept across Tab toggles).
    manual: TextField,
    items: Vec<RepoEntry>,
    matcher: Matcher,
    /// Indices into `items`, best match first (seed order when query empty).
    matches: Vec<usize>,
    selected: usize,
    scroll_offset: usize,
    /// True while the off-loop `discover_repos` scan is in flight.
    scanning: bool,
    result_tx: tokio::sync::mpsc::UnboundedSender<Vec<String>>,
    result_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<String>>,
}

fn entry(path: String) -> RepoEntry {
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    RepoEntry { path, name }
}

impl WorkspacePicker {
    pub(crate) fn new(seed: Vec<String>) -> Self {
        let (result_tx, result_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = Self {
            mode: PickerMode::Fuzzy,
            phase: Phase::Browse,
            query: TextField::default(),
            manual: TextField::default(),
            items: seed.into_iter().map(entry).collect(),
            matcher: Matcher::new(MatcherConfig::DEFAULT),
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            scanning: false,
            result_tx,
            result_rx,
        };
        p.recompute();
        p
    }

    /// Open directly in manual-entry mode, optionally pre-filled — the
    /// clone-and-open palette action (paste a URL) and connect-to-root's
    /// "add this path as a workspace" offer. Tab still flips back to fuzzy.
    pub(crate) fn start_manual(&mut self, prefill: impl Into<String>) {
        self.mode = PickerMode::Manual;
        self.manual = TextField::new(prefill);
    }

    /// Enter the clone-in-flight phase (the manual `Clone` submit): the picker
    /// stays on screen showing progress instead of being dropped.
    pub(crate) fn begin_clone(&mut self, url: impl Into<String>) {
        self.phase = Phase::Cloning {
            url: url.into(),
            line: None,
        };
    }

    /// Feed the latest `git clone --progress` line into the cloning panel.
    pub(crate) fn set_clone_progress(&mut self, line: impl Into<String>) {
        if let Phase::Cloning { line: slot, .. } = &mut self.phase {
            *slot = Some(line.into());
        }
    }

    /// The URL currently being cloned, if any — the loop's clone-drain arm uses
    /// this to ignore a result the user has since cancelled or moved past.
    pub(crate) fn cloning_url(&self) -> Option<&str> {
        match &self.phase {
            Phase::Cloning { url, .. } => Some(url.as_str()),
            _ => None,
        }
    }

    /// Park the picker in a visible, editable error state (bad input / failed
    /// clone). Forces manual mode so the offending input is on screen to fix.
    pub(crate) fn set_error(&mut self, msg: impl Into<String>) {
        self.mode = PickerMode::Manual;
        self.phase = Phase::Error(msg.into());
    }

    /// Kick the (potentially slow — it walks every `repo_roots` tree) repo
    /// scan on `spawn_blocking`; results land via [`drain_discovery`].
    ///
    /// [`drain_discovery`]: WorkspacePicker::drain_discovery
    pub(crate) fn spawn_discovery(
        &mut self,
        cfg: superzej_core::config::Config,
        waker: TerminalWaker,
    ) {
        self.scanning = true;
        let tx = self.result_tx.clone();
        tokio::task::spawn_blocking(move || {
            let found = superzej_core::repo::discover_repos(&cfg);
            // best-effort: the picker may already be closed
            let _ = tx.send(found);
            let _ = waker.wake();
        });
    }

    /// Merge any finished discovery results into the list (seed entries keep
    /// their frecency positions; new paths append in scan order). Returns
    /// whether anything changed (→ the caller marks the frame dirty).
    pub(crate) fn drain_discovery(&mut self) -> bool {
        let mut changed = false;
        while let Ok(found) = self.result_rx.try_recv() {
            self.scanning = false;
            changed = true;
            for p in found {
                if !self.items.iter().any(|e| e.path == p) {
                    self.items.push(entry(p));
                }
            }
        }
        if changed {
            self.recompute();
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> PickerMode {
        self.mode
    }

    #[cfg(test)]
    pub(crate) fn matches(&self) -> Vec<&RepoEntry> {
        self.matches
            .iter()
            .filter_map(|&i| self.items.get(i))
            .collect()
    }

    fn selected_path(&self) -> Option<&str> {
        self.matches
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
            .map(|e| e.path.as_str())
    }

    fn recompute(&mut self) {
        if self.query.as_str().trim().is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let pattern = Pattern::parse(
                self.query.as_str(),
                CaseMatching::Smart,
                Normalization::Smart,
            );
            let mut buf = Vec::new();
            let mut scored: Vec<(usize, u32)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| {
                    let hay = format!("{} {}", it.name, it.path);
                    pattern
                        .score(Utf32Str::new(&hay, &mut buf), &mut self.matcher)
                        .map(|s| (i, s))
                })
                .collect();
            scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
            self.matches = scored.into_iter().map(|(i, _)| i).collect();
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
            self.clamp_scroll();
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + MAX_ITEMS {
            self.scroll_offset = self.selected + 1 - MAX_ITEMS;
        }
    }

    /// The text field the current mode is editing.
    fn field_mut(&mut self) -> &mut TextField {
        match self.mode {
            PickerMode::Fuzzy => &mut self.query,
            PickerMode::Manual => &mut self.manual,
        }
    }

    /// Re-filter after a query edit (fuzzy mode) — resets the cursor row.
    fn on_query_edit(&mut self) {
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute();
    }

    /// Insert bracketed-paste text into the active field. Cloning ignores
    /// paste; any other phase clears a stale error and edits.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        if matches!(self.phase, Phase::Cloning { .. }) {
            return;
        }
        self.phase = Phase::Browse;
        match self.mode {
            PickerMode::Manual => self.manual.insert_str(text),
            PickerMode::Fuzzy => {
                self.query.insert_str(text);
                self.on_query_edit();
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> PickerOutcome {
        // While a clone is in flight the only meaningful key is cancel.
        if matches!(self.phase, Phase::Cloning { .. }) {
            let cancel = crate::input::is_escape_key(key)
                || (mods.contains(Modifiers::CTRL)
                    && matches!(key, KeyCode::Char('c' | 'C' | 'g' | 'G')));
            return if cancel {
                PickerOutcome::Cancel
            } else {
                PickerOutcome::Pending
            };
        }
        if mods.contains(Modifiers::CTRL) {
            match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => return PickerOutcome::Cancel,
                KeyCode::Char('j' | 'J' | 'n' | 'N') if self.mode == PickerMode::Fuzzy => {
                    self.move_down();
                }
                KeyCode::Char('k' | 'K' | 'p' | 'P') if self.mode == PickerMode::Fuzzy => {
                    self.move_up();
                }
                // readline home/end within the active field
                KeyCode::Char('a' | 'A') => self.field_mut().home(),
                KeyCode::Char('e' | 'E') => self.field_mut().end(),
                _ => {}
            }
            return PickerOutcome::Pending;
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return PickerOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return PickerOutcome::Cancel;
        }
        // Any interaction clears a stale inline error.
        self.phase = Phase::Browse;
        // Tab flips fuzzy ⇄ manual; both buffers survive the round-trip.
        if matches!(key, KeyCode::Tab) {
            self.mode = match self.mode {
                PickerMode::Fuzzy => PickerMode::Manual,
                PickerMode::Manual => PickerMode::Fuzzy,
            };
            return PickerOutcome::Pending;
        }
        match self.mode {
            PickerMode::Fuzzy => match key {
                KeyCode::Enter => self
                    .selected_path()
                    .map(|p| PickerOutcome::OpenRepo(p.to_string()))
                    .unwrap_or(PickerOutcome::Pending),
                KeyCode::UpArrow => {
                    self.move_up();
                    PickerOutcome::Pending
                }
                KeyCode::DownArrow => {
                    self.move_down();
                    PickerOutcome::Pending
                }
                KeyCode::LeftArrow => {
                    self.query.left();
                    PickerOutcome::Pending
                }
                KeyCode::RightArrow => {
                    self.query.right();
                    PickerOutcome::Pending
                }
                KeyCode::Home => {
                    self.query.home();
                    PickerOutcome::Pending
                }
                KeyCode::End => {
                    self.query.end();
                    PickerOutcome::Pending
                }
                KeyCode::Backspace => {
                    if self.query.backspace() {
                        self.on_query_edit();
                    }
                    PickerOutcome::Pending
                }
                KeyCode::Delete => {
                    if self.query.delete() {
                        self.on_query_edit();
                    }
                    PickerOutcome::Pending
                }
                KeyCode::Char(c) => {
                    self.query.insert_char(*c);
                    self.on_query_edit();
                    PickerOutcome::Pending
                }
                _ => PickerOutcome::Pending,
            },
            PickerMode::Manual => match key {
                KeyCode::Enter => {
                    let v = self.manual.as_str().trim().to_string();
                    if v.is_empty() {
                        PickerOutcome::Pending
                    } else {
                        PickerOutcome::Manual(v)
                    }
                }
                KeyCode::LeftArrow => {
                    self.manual.left();
                    PickerOutcome::Pending
                }
                KeyCode::RightArrow => {
                    self.manual.right();
                    PickerOutcome::Pending
                }
                KeyCode::Home => {
                    self.manual.home();
                    PickerOutcome::Pending
                }
                KeyCode::End => {
                    self.manual.end();
                    PickerOutcome::Pending
                }
                KeyCode::Backspace => {
                    self.manual.backspace();
                    PickerOutcome::Pending
                }
                KeyCode::Delete => {
                    self.manual.delete();
                    PickerOutcome::Pending
                }
                KeyCode::Char(c) => {
                    self.manual.insert_char(*c);
                    PickerOutcome::Pending
                }
                _ => PickerOutcome::Pending,
            },
        }
    }

    /// Draw as the same boxed layer the palette/menus use: prompt row, rule,
    /// repo rows (name + dim path), rule, footer hints. Manual mode is a
    /// single input row with its own footer.
    pub(crate) fn render(&self, surface: &mut Surface, screen: Rect) {
        if let Phase::Cloning { url, line } = &self.phase {
            self.render_cloning(surface, screen, url, line.as_deref());
            return;
        }
        let err = match &self.phase {
            Phase::Error(m) => Some(m.as_str()),
            _ => None,
        };
        match self.mode {
            PickerMode::Manual => self.render_manual(surface, screen, err),
            PickerMode::Fuzzy => self.render_fuzzy(surface, screen),
        }
    }

    /// The single-row manual entry (cursor-aware), with an optional inline
    /// error line above the footer.
    fn render_manual(&self, surface: &mut Surface, screen: Rect, err: Option<&str>) {
        const COLS: usize = 72;
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        let rows = if err.is_some() { 4 } else { 3 }; // input (+err) + rule + footer
        let spec = LayerSpec {
            title: "new workspace".into(),
            badge: Some(" tab: fuzzy ".into()),
            cols: COLS,
            rows,
            anchor: Anchor::TopThird,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(input_segs(&self.manual, "path, URL, or new dir…")),
            panel,
        );
        let mut y = inner.y + 1;
        if let Some(msg) = err {
            let line = Line::segs(vec![
                seg(danger(), "✗ ").bold(),
                seg(danger(), msg.to_string()),
            ]);
            seg::draw_line(surface, inner.x, y, inner.cols, &line, panel);
            y += 1;
        }
        if y < inner.y + inner.rows.saturating_sub(1) {
            seg::draw_line(surface, inner.x, y, inner.cols, &rule, panel);
            let footer = Line::segs(vec![
                seg(Tok::Slot(S::Ghost2), "↵"),
                seg(Tok::Slot(S::Ghost), " open   "),
                seg(Tok::Slot(S::Ghost2), "tab"),
                seg(Tok::Slot(S::Ghost), " fuzzy   "),
                seg(
                    Tok::Slot(S::Ghost3),
                    "repo · dir · URL · new dir (parent must exist)",
                ),
            ]);
            seg::draw_line(surface, inner.x, y + 1, inner.cols, &footer, panel);
        }
    }

    /// The cloning progress panel: the URL, the latest `git clone --progress`
    /// line, and a percentage bar derived from it.
    fn render_cloning(&self, surface: &mut Surface, screen: Rect, url: &str, line: Option<&str>) {
        const COLS: usize = 72;
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        let spec = LayerSpec {
            title: "new workspace".into(),
            badge: Some(" cloning ".into()),
            cols: COLS,
            rows: 4, // title + rule + progress + footer
            anchor: Anchor::TopThird,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let title = Line::segs(vec![
            seg(Tok::Slot(S::Accent), "⟳ ").bold(),
            seg(Tok::Slot(S::Text), "Cloning "),
            seg(Tok::Slot(S::Dim), url.to_string()),
            seg(Tok::Slot(S::Ghost3), "…"),
        ]);
        seg::draw_line(surface, inner.x, inner.y, inner.cols, &title, panel);
        if inner.rows < 4 {
            return;
        }
        seg::draw_line(surface, inner.x, inner.y + 1, inner.cols, &rule, panel);
        let pct = line.and_then(parse_percent);
        let status = line.unwrap_or("starting…").to_string();
        let progress = Line::split(vec![seg(Tok::Slot(S::Ghost), status)], bar_segs(pct));
        seg::draw_line(surface, inner.x, inner.y + 2, inner.cols, &progress, panel);
        let footer = Line::segs(vec![
            seg(Tok::Slot(S::Ghost2), "esc"),
            seg(Tok::Slot(S::Ghost), " cancel"),
        ]);
        seg::draw_line(surface, inner.x, inner.y + 3, inner.cols, &footer, panel);
    }

    fn render_fuzzy(&self, surface: &mut Surface, screen: Rect) {
        const COLS: usize = 72;
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        {
            let shown = self.matches.len().min(MAX_ITEMS);
            let spec = LayerSpec {
                title: "new workspace".into(),
                badge: Some(" tab: manual ".into()),
                cols: COLS,
                rows: shown + 4, // prompt + rule + items + rule + footer
                anchor: Anchor::TopThird,
                ..LayerSpec::default()
            };
            let Some(inner) = open_layer(surface, screen, &spec) else {
                return;
            };

            seg::draw_line(
                surface,
                inner.x,
                inner.y,
                inner.cols,
                &Line::segs(input_segs(&self.query, "type to filter repos…")),
                panel,
            );
            if inner.rows < 2 {
                return;
            }
            seg::draw_line(surface, inner.x, inner.y + 1, inner.cols, &rule, panel);

            let rows_avail = inner.rows.saturating_sub(4);
            for row in 0..rows_avail {
                let match_idx = self.scroll_offset + row;
                let Some(&item_idx) = self.matches.get(match_idx) else {
                    break;
                };
                let Some(item) = self.items.get(item_idx) else {
                    continue;
                };
                let selected = match_idx == self.selected;
                let pad = if selected { Tok::SelAccent } else { panel };
                let name = if selected {
                    seg(Tok::Slot(S::Text), item.name.clone()).bold()
                } else {
                    seg(Tok::Slot(S::Dim), item.name.clone())
                };
                let line = Line::split(
                    vec![sp(1), name],
                    vec![seg(Tok::Slot(S::Ghost3), item.path.clone()), sp(1)],
                );
                seg::draw_line(surface, inner.x, inner.y + 2 + row, inner.cols, &line, pad);
            }

            if inner.rows >= 4 {
                let fy = inner.y + inner.rows - 2;
                seg::draw_line(surface, inner.x, fy, inner.cols, &rule, panel);
                let total = self.matches.len();
                let count_str = if self.scanning {
                    format!("{total} · scanning repo roots…")
                } else if total > MAX_ITEMS {
                    let end = (self.scroll_offset + MAX_ITEMS).min(total);
                    format!("{}-{}/{}", self.scroll_offset + 1, end, total)
                } else {
                    format!("{total} repos")
                };
                let footer = Line::split(
                    vec![
                        seg(Tok::Slot(S::Ghost2), "↑↓"),
                        seg(Tok::Slot(S::Ghost), " move   "),
                        seg(Tok::Slot(S::Ghost2), "↵"),
                        seg(Tok::Slot(S::Ghost), " open   "),
                        seg(Tok::Slot(S::Ghost2), "tab"),
                        seg(Tok::Slot(S::Ghost), " manual   "),
                        seg(Tok::Slot(S::Ghost2), "esc"),
                        seg(Tok::Slot(S::Ghost), " dismiss"),
                    ],
                    vec![seg(Tok::Slot(S::Ghost3), count_str)],
                );
                seg::draw_line(surface, inner.x, fy + 1, inner.cols, &footer, panel);
            }
        }
    }
}

/// theme::RED as a seg color — the S palette has no dedicated danger slot yet.
fn danger() -> Tok {
    let mut it = superzej_core::theme::RED
        .split(';')
        .filter_map(|s| s.trim().parse::<u8>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(g), Some(b)) => Tok::Rgb(r, g, b),
        _ => Tok::Slot(S::Text),
    }
}

/// The `❯ ` prompt for a text field, splitting the buffer around a cursor bar
/// so mid-string edits are visible (not just an end-of-line caret).
fn input_segs(field: &TextField, placeholder: &str) -> Vec<Seg> {
    let mut out = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
    if field.is_empty() {
        out.push(seg(Tok::Slot(S::Accent), "▏"));
        out.push(seg(Tok::Slot(S::Ghost3), placeholder.to_string()));
        return out;
    }
    let s = field.as_str();
    let (before, after) = s.split_at(field.cursor());
    out.push(seg(Tok::Slot(S::Text), before.to_string()));
    out.push(seg(Tok::Slot(S::Accent), "▏"));
    if !after.is_empty() {
        out.push(seg(Tok::Slot(S::Text), after.to_string()));
    }
    out
}

/// Pull a whole-number percentage out of a git progress line, e.g.
/// `"Receiving objects:  47% (470/1000)"` → `47`.
fn parse_percent(line: &str) -> Option<u8> {
    let idx = line.find('%')?;
    let digits: String = line[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse::<u8>().ok().map(|p| p.min(100))
}

/// A fixed-width block bar for the clone panel (indeterminate when `pct` is
/// unknown — git hasn't emitted a percentage yet).
fn bar_segs(pct: Option<u8>) -> Vec<Seg> {
    const W: usize = 12;
    match pct {
        Some(p) => {
            let filled = (p as usize * W / 100).min(W);
            vec![
                seg(Tok::Slot(S::Accent), "▓".repeat(filled)),
                seg(Tok::Slot(S::Ghost3), "░".repeat(W - filled)),
            ]
        }
        None => vec![seg(Tok::Slot(S::Ghost3), "░".repeat(W))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(paths: &[&str]) -> WorkspacePicker {
        WorkspacePicker::new(paths.iter().map(|s| s.to_string()).collect())
    }

    fn key(p: &mut WorkspacePicker, k: KeyCode) -> PickerOutcome {
        p.handle_key(&k, Modifiers::NONE)
    }

    fn type_str(p: &mut WorkspacePicker, s: &str) {
        for c in s.chars() {
            key(p, KeyCode::Char(c));
        }
    }

    #[test]
    fn empty_query_keeps_seed_frecency_order() {
        let p = picker(&["/code/zeta", "/code/alpha", "/code/mid"]);
        let names: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, vec!["/code/zeta", "/code/alpha", "/code/mid"]);
    }

    #[test]
    fn typing_fuzzy_filters_by_name_and_path() {
        let mut p = picker(&["/code/superzej", "/code/other", "/work/zej-tools"]);
        type_str(&mut p, "zej");
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/code/superzej"), "{paths:?}");
        assert!(paths.contains(&"/work/zej-tools"), "{paths:?}");
        assert!(!paths.contains(&"/code/other"), "{paths:?}");
        // A path fragment narrows too.
        let mut p = picker(&["/code/app", "/work/app"]);
        type_str(&mut p, "work");
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/work/app"]);
    }

    #[test]
    fn enter_in_fuzzy_opens_the_selected_repo() {
        let mut p = picker(&["/code/a", "/code/b"]);
        key(&mut p, KeyCode::DownArrow);
        assert_eq!(
            key(&mut p, KeyCode::Enter),
            PickerOutcome::OpenRepo("/code/b".into())
        );
        // No matches → Enter is inert.
        let mut p = picker(&[]);
        assert_eq!(key(&mut p, KeyCode::Enter), PickerOutcome::Pending);
    }

    #[test]
    fn tab_toggles_mode_and_preserves_both_buffers() {
        let mut p = picker(&["/code/a"]);
        type_str(&mut p, "abc");
        key(&mut p, KeyCode::Tab);
        assert_eq!(p.mode(), PickerMode::Manual);
        type_str(&mut p, "/tmp/x");
        key(&mut p, KeyCode::Tab);
        assert_eq!(p.mode(), PickerMode::Fuzzy);
        assert_eq!(p.query.as_str(), "abc");
        key(&mut p, KeyCode::Tab);
        assert_eq!(p.manual.as_str(), "/tmp/x");
        assert_eq!(
            key(&mut p, KeyCode::Enter),
            PickerOutcome::Manual("/tmp/x".into())
        );
    }

    #[test]
    fn manual_enter_requires_nonempty_input() {
        let mut p = picker(&[]);
        key(&mut p, KeyCode::Tab);
        assert_eq!(key(&mut p, KeyCode::Enter), PickerOutcome::Pending);
        type_str(&mut p, "  ");
        assert_eq!(key(&mut p, KeyCode::Enter), PickerOutcome::Pending);
    }

    #[test]
    fn escape_and_ctrl_c_cancel_in_both_modes() {
        let mut p = picker(&["/code/a"]);
        assert_eq!(key(&mut p, KeyCode::Escape), PickerOutcome::Cancel);
        key(&mut p, KeyCode::Tab);
        assert_eq!(
            p.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            PickerOutcome::Cancel
        );
    }

    #[test]
    fn ctrl_j_k_navigate_and_scroll_clamps() {
        let paths: Vec<String> = (0..20).map(|i| format!("/code/r{i:02}")).collect();
        let mut p = WorkspacePicker::new(paths);
        for _ in 0..15 {
            p.handle_key(&KeyCode::Char('j'), Modifiers::CTRL);
        }
        assert_eq!(p.selected, 15);
        assert_eq!(p.scroll_offset, 6, "cursor stays within the window");
        p.handle_key(&KeyCode::Char('k'), Modifiers::CTRL);
        assert_eq!(p.selected, 14);
        // Ctrl+j while a query is live must navigate, not type.
        type_str(&mut p, "r0");
        assert!(p.query.as_str().contains("r0"));
    }

    #[test]
    fn drain_discovery_merges_without_duplicating_seeds() {
        let mut p = picker(&["/code/seeded"]);
        p.scanning = true;
        p.result_tx
            .send(vec!["/code/seeded".into(), "/code/found".into()])
            .unwrap();
        assert!(p.drain_discovery());
        assert!(!p.scanning);
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/code/seeded", "/code/found"]);
        // Idempotent: nothing new → no change.
        assert!(!p.drain_discovery());
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut p = picker(&["/code/alpha", "/code/beta"]);
        type_str(&mut p, "alpha");
        assert_eq!(p.matches().len(), 1);
        for _ in 0.."alpha".len() {
            key(&mut p, KeyCode::Backspace);
        }
        assert_eq!(p.matches().len(), 2);
    }

    #[test]
    fn cursor_edits_the_manual_field_mid_string() {
        let mut p = picker(&[]);
        key(&mut p, KeyCode::Tab); // → manual
        type_str(&mut p, "htps://x"); // typo: missing 't'
        // Move the cursor back to just after "ht" and insert the missing 't'.
        for _ in 0.."ps://x".len() {
            key(&mut p, KeyCode::LeftArrow);
        }
        key(&mut p, KeyCode::Char('t'));
        assert_eq!(p.manual.as_str(), "https://x");
        // Home + Delete removes the first char; End appends.
        key(&mut p, KeyCode::Home);
        key(&mut p, KeyCode::Delete);
        assert_eq!(p.manual.as_str(), "ttps://x");
        key(&mut p, KeyCode::End);
        type_str(&mut p, "y");
        assert_eq!(p.manual.as_str(), "ttps://xy");
    }

    #[test]
    fn backspace_and_delete_respect_the_cursor() {
        let mut p = picker(&[]);
        key(&mut p, KeyCode::Tab);
        type_str(&mut p, "abc");
        key(&mut p, KeyCode::LeftArrow); // cursor between b and c
        key(&mut p, KeyCode::Backspace); // deletes 'b'
        assert_eq!(p.manual.as_str(), "ac");
        key(&mut p, KeyCode::Delete); // deletes 'c'
        assert_eq!(p.manual.as_str(), "a");
    }

    #[test]
    fn paste_inserts_at_cursor_and_strips_newlines() {
        let mut p = picker(&[]);
        key(&mut p, KeyCode::Tab);
        type_str(&mut p, "ab");
        key(&mut p, KeyCode::LeftArrow); // between a and b
        p.handle_paste("XY\nZ");
        assert_eq!(p.manual.as_str(), "aXYZb");
    }

    #[test]
    fn paste_into_fuzzy_refilters() {
        let mut p = picker(&["/code/superzej", "/code/other"]);
        p.handle_paste("zej");
        assert_eq!(p.query.as_str(), "zej");
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/code/superzej"]);
    }

    #[test]
    fn clone_phase_shows_progress_and_ignores_typing() {
        let mut p = picker(&[]);
        p.begin_clone("https://github.com/acme/x.git");
        assert_eq!(p.cloning_url(), Some("https://github.com/acme/x.git"));
        p.set_clone_progress("Receiving objects:  42%");
        // Typing during a clone is inert (only esc cancels).
        assert_eq!(key(&mut p, KeyCode::Char('z')), PickerOutcome::Pending);
        assert_eq!(key(&mut p, KeyCode::Escape), PickerOutcome::Cancel);
    }

    #[test]
    fn error_phase_clears_on_next_edit() {
        let mut p = picker(&[]);
        p.set_error("path does not exist: /nope");
        assert!(matches!(p.phase, Phase::Error(_)));
        // The manual input stays editable; the first keystroke clears the error.
        key(&mut p, KeyCode::Char('x'));
        assert!(matches!(p.phase, Phase::Browse));
    }

    #[test]
    fn parse_percent_extracts_trailing_number() {
        assert_eq!(
            parse_percent("Receiving objects:  47% (470/1000)"),
            Some(47)
        );
        assert_eq!(
            parse_percent("Resolving deltas: 100% (5/5), done."),
            Some(100)
        );
        assert_eq!(parse_percent("remote: Counting objects"), None);
        // Guards against a >100 reading.
        assert_eq!(parse_percent("bogus 250%"), Some(100));
    }

    #[test]
    fn seed_repos_orders_recents_first_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("sj-picker-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let db = superzej_core::db::Db::open_at(&dir.join("db.sqlite")).unwrap();
        let (a_s, b_s) = (
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        );
        // `b` is known via a workspace row; `a` is a (newer) recent repo.
        db.put_workspace(&b_s, "b", "repo").unwrap();
        db.touch_repo(&a_s, "a").unwrap();
        let seed = seed_repos(&db);
        assert_eq!(seed, vec![a_s.clone(), b_s.clone()]);
        // A vanished dir is filtered out.
        db.touch_repo("/no/such/dir", "gone").unwrap();
        assert_eq!(seed_repos(&db), vec![a_s, b_s]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
