//! The Alt+W "new workspace" fuzzy picker: type-to-filter over every git repo
//! thegn can see (DB-known + recents instantly, plus an off-loop
//! `discover_repos` scan of the configured `repo_roots` streamed in as it
//! finishes), with Tab toggling a manual-entry mode for paths, clone URLs and
//! brand-new projects. Replaces the static `new_workspace_menu` list.
//!
//! Loop wiring mirrors the worktree wizard: an `Option<WorkspacePicker>` modal
//! whose `handle_key` returns a [`PickerOutcome`] the loop maps onto the
//! shared `workspace_create` flows. The discovery channel is owned here (like
//! `search_everywhere::PaletteSession`) so the loop only adds a one-line drain.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use termwiz::terminal::TerminalWaker;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use thegn_core::store::WorkspaceStore;

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
    haystack: String,
}

#[derive(Debug)]
enum DiscoveryEvent {
    Batch {
        generation: u64,
        paths: Vec<String>,
    },
    Finished {
        generation: u64,
        report: thegn_core::repo::DiscoveryReport,
    },
}

static DISCOVERY_GENERATION: AtomicU64 = AtomicU64::new(1);

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
/// stat-filtered to dirs that still exist — PLUS the set of already-registered
/// workspace paths. Repos already added as workspaces (they live in the
/// `workspaces` table and show in the sidebar) are excluded from the seed, and
/// the returned set is carried into the picker so streamed-in discovery results
/// are filtered the same way. Plain "recent" repos that were merely opened but
/// never registered are kept.
pub(crate) fn picker_seed(db: &thegn_core::db::Db) -> (Vec<String>, HashSet<String>) {
    let added: HashSet<String> = db
        .workspaces()
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !added.contains(&p) && !out.contains(&p) && std::path::Path::new(&p).is_dir() {
            out.push(p);
        }
    };
    for p in db.recent_repos(50).unwrap_or_default() {
        push(p);
    }
    for p in db.known_repos().unwrap_or_default() {
        push(p);
    }
    (out, added)
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
    /// Paths in `items`, so discovery and seed merges dedup in O(1) on the event loop.
    item_paths: HashSet<String>,
    /// Already-registered workspace paths, kept out of both the seed and any
    /// discovery results streamed in later.
    excluded: HashSet<String>,
    /// Indices into `items`, best match first (seed order when query empty).
    /// Discovery appends are ranked incrementally; query edits and seed reorders
    /// fully recompute this list.
    matches: Vec<usize>,
    match_scores: Vec<u16>,
    #[cfg(test)]
    ranked_haystacks: usize,
    selected: usize,
    scroll_offset: usize,
    /// True while the off-loop `discover_repos` scan is in flight.
    scanning: bool,
    db_pending: bool,
    db_key: Option<String>,
    db_generation: u64,
    truncated: bool,
    generation: u64,
    cancel: Option<Arc<AtomicBool>>,
    scan_roots: Vec<String>,
    scan_depth: usize,
    result_tx: tokio::sync::mpsc::Sender<DiscoveryEvent>,
    result_rx: tokio::sync::mpsc::Receiver<DiscoveryEvent>,
}

impl Drop for WorkspacePicker {
    fn drop(&mut self) {
        self.cancel_discovery();
    }
}

fn entry(path: String) -> RepoEntry {
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let haystack = format!("{} {}", name, path).to_lowercase();
    RepoEntry {
        path,
        name,
        haystack,
    }
}

impl WorkspacePicker {
    pub(crate) fn new(seed: Vec<String>, excluded: HashSet<String>) -> Self {
        let (result_tx, result_rx) = tokio::sync::mpsc::channel(4);
        let items = seed.into_iter().map(entry).collect::<Vec<_>>();
        let mut p = Self {
            mode: PickerMode::Fuzzy,
            phase: Phase::Browse,
            query: TextField::default(),
            manual: TextField::default(),
            item_paths: items.iter().map(|item| item.path.clone()).collect(),
            items,
            excluded,
            matches: Vec::new(),
            match_scores: Vec::new(),
            #[cfg(test)]
            ranked_haystacks: 0,
            selected: 0,
            scroll_offset: 0,
            scanning: false,
            db_pending: false,
            db_key: None,
            db_generation: 0,
            truncated: false,
            generation: 0,
            cancel: None,
            scan_roots: Vec::new(),
            scan_depth: 0,
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

    pub(crate) fn mark_db_pending(&mut self, key: String, generation: u64) {
        self.db_key = Some(key);
        self.db_generation = generation;
        self.db_pending = true;
    }

    pub(crate) fn apply_db_result(
        &mut self,
        key: &str,
        generation: u64,
        seed: Vec<String>,
        excluded: HashSet<String>,
    ) -> bool {
        if !self.db_pending
            || self.db_key.as_deref() != Some(key)
            || self.db_generation != generation
        {
            return false;
        }
        self.apply_db_seed(seed, excluded);
        self.db_pending = false;
        true
    }

    pub(crate) fn clear_db_pending(&mut self, key: &str, generation: u64) {
        if self.db_key.as_deref() == Some(key) && self.db_generation == generation {
            self.db_pending = false;
        }
    }

    pub(crate) fn apply_db_seed(&mut self, seed: Vec<String>, excluded: HashSet<String>) {
        let mut items = seed.into_iter().map(entry).collect::<Vec<_>>();
        let mut paths: HashSet<String> = items.iter().map(|item| item.path.clone()).collect();
        for existing in &self.items {
            if !excluded.contains(&existing.path) && paths.insert(existing.path.clone()) {
                items.push(existing.clone());
            }
        }
        self.items = items;
        self.item_paths = paths;
        self.excluded = excluded;
        self.recompute();
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
        cfg: thegn_core::config::Config,
        waker: TerminalWaker,
    ) {
        self.cancel_discovery();
        let generation = DISCOVERY_GENERATION.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.result_tx = tx.clone();
        self.result_rx = rx;
        self.generation = generation;
        self.cancel = Some(cancel.clone());
        self.scan_roots.clone_from(&cfg.repo_roots);
        self.scan_depth = cfg.repo_scan_depth;
        self.truncated = false;
        self.scanning = true;
        tokio::task::spawn_blocking(move || {
            let report = thegn_core::repo::discover_repos_bounded(
                &cfg,
                thegn_core::repo::DiscoveryBudget::default(),
                &cancel,
                |paths| {
                    if tx
                        .blocking_send(DiscoveryEvent::Batch { generation, paths })
                        .is_err()
                    {
                        return false;
                    }
                    let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                    true
                },
            );
            let _ = tx.blocking_send(DiscoveryEvent::Finished { generation, report });
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        });
    }

    fn cancel_discovery(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.scanning = false;
    }

    pub(crate) fn config_reloaded(
        &mut self,
        cfg: thegn_core::config::Config,
        waker: TerminalWaker,
    ) {
        if self.discovery_config_changed(&cfg) {
            self.spawn_discovery(cfg, waker);
        }
    }

    fn discovery_config_changed(&self, cfg: &thegn_core::config::Config) -> bool {
        cfg.repo_roots != self.scan_roots || cfg.repo_scan_depth != self.scan_depth
    }

    /// Merge any finished discovery results into the list (seed entries keep
    /// their frecency positions; new paths append in scan order). Returns
    /// whether anything changed (→ the caller marks the frame dirty).
    pub(crate) fn drain_discovery(&mut self) -> bool {
        let mut changed = false;
        let old_len = self.items.len();
        while let Ok(event) = self.result_rx.try_recv() {
            match event {
                DiscoveryEvent::Batch { generation, paths } if generation == self.generation => {
                    changed = true;
                    for p in paths {
                        if !self.excluded.contains(&p) && self.item_paths.insert(p.clone()) {
                            self.items.push(entry(p));
                        }
                    }
                }
                DiscoveryEvent::Finished { generation, report }
                    if generation == self.generation =>
                {
                    self.scanning = false;
                    self.truncated = report.truncated || report.traversal_errors > 0;
                    changed = true;
                }
                _ => {}
            }
        }
        if self.items.len() > old_len {
            self.rank_appended(old_len);
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
            self.matches.clear();
            self.matches.extend(0..self.items.len());
            self.match_scores.clear();
            self.match_scores.resize(self.items.len(), 0);
        } else {
            // Fuzzy over "name path" so a path fragment narrows like a basename.
            let refs: Vec<&str> = self.items.iter().map(|it| it.haystack.as_str()).collect();
            #[cfg(test)]
            {
                self.ranked_haystacks += refs.len();
            }
            let ranked = crate::fff_backend::fuzzy_rank(self.query.as_str(), &refs);
            self.matches.clear();
            self.match_scores.clear();
            self.matches.extend(ranked.iter().map(|(i, _)| *i));
            self.match_scores
                .extend(ranked.into_iter().map(|(_, score)| score));
        }
        self.clamp_selection_and_scroll();
    }

    fn rank_appended(&mut self, old_len: usize) {
        if self.query.as_str().trim().is_empty() {
            self.matches.extend(old_len..self.items.len());
            self.match_scores
                .resize(self.match_scores.len() + self.items.len() - old_len, 0);
        } else {
            let refs: Vec<&str> = self.items[old_len..]
                .iter()
                .map(|it| it.haystack.as_str())
                .collect();
            #[cfg(test)]
            {
                self.ranked_haystacks += refs.len();
            }
            let appended = crate::fff_backend::fuzzy_rank(self.query.as_str(), &refs)
                .into_iter()
                .map(|(index, score)| (old_len + index, score));
            let ranked_appended: Vec<(usize, u16)> = appended.collect();
            if ranked_appended.is_empty() {
                self.clamp_selection_and_scroll();
                return;
            }
            let mut merged_matches = Vec::with_capacity(self.matches.len() + ranked_appended.len());
            let mut merged_scores =
                Vec::with_capacity(self.match_scores.len() + ranked_appended.len());
            let (mut existing, mut added) = (0, 0);
            while existing < self.matches.len() && added < ranked_appended.len() {
                // New item indices are greater, so existing items win score ties.
                if self.match_scores[existing] >= ranked_appended[added].1 {
                    merged_matches.push(self.matches[existing]);
                    merged_scores.push(self.match_scores[existing]);
                    existing += 1;
                } else {
                    merged_matches.push(ranked_appended[added].0);
                    merged_scores.push(ranked_appended[added].1);
                    added += 1;
                }
            }
            merged_matches.extend_from_slice(&self.matches[existing..]);
            merged_scores.extend_from_slice(&self.match_scores[existing..]);
            merged_matches.extend(ranked_appended[added..].iter().map(|(index, _)| *index));
            merged_scores.extend(ranked_appended[added..].iter().map(|(_, score)| *score));
            self.matches = merged_matches;
            self.match_scores = merged_scores;
        }
        self.clamp_selection_and_scroll();
    }

    fn clamp_selection_and_scroll(&mut self) {
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
            title: "new project".into(),
            badge: Some(" tab: fuzzy ".into()),
            cols: COLS,
            rows,
            anchor: Anchor::TopThird,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let mut prompt = input_segs(&self.manual, "path, URL, or new dir…");
        if self.db_pending {
            prompt.push(seg(
                Tok::Slot(S::Ghost2),
                format!("  {}", crate::caps::active_glyphs().ellipsis),
            ));
        }
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(prompt),
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
            title: "new project".into(),
            badge: Some(" cloning ".into()),
            cols: COLS,
            rows: 4, // title + rule + progress + footer
            anchor: Anchor::TopThird,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        // The "working" glyph comes from the shared loading vocabulary — the
        // same animated spinner frames the center-pane splash draws for an
        // active step (repaints arrive with each clone-progress line), so the
        // modal and the splash can never drift apart or fall out of ASCII sync.
        let (spin, _) = crate::loading::plan::visual_glyph_live(
            crate::chrome::StepState::Active,
            termwiz::color::ColorAttribute::Default,
            true,
        );
        let title = Line::segs(vec![
            seg(Tok::Slot(S::Accent), format!("{spin} ")).bold(),
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
                title: "new project".into(),
                badge: Some(" tab: manual ".into()),
                cols: COLS,
                rows: shown + 4, // prompt + rule + items + rule + footer
                anchor: Anchor::TopThird,
                ..LayerSpec::default()
            };
            let Some(inner) = open_layer(surface, screen, &spec) else {
                return;
            };

            let mut prompt = input_segs(&self.query, "type to filter repos…");
            if self.db_pending {
                prompt.push(seg(
                    Tok::Slot(S::Ghost2),
                    format!("  {}", crate::caps::active_glyphs().ellipsis),
                ));
            }
            seg::draw_line(
                surface,
                inner.x,
                inner.y,
                inner.cols,
                &Line::segs(prompt),
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
                let count_str = if self.truncated {
                    format!("{total} repos (partial scan)")
                } else if self.scanning {
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
    let mut it = thegn_core::theme::RED
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
        out.push(crate::seg::caret());
        out.push(seg(Tok::Slot(S::Ghost3), placeholder.to_string()));
        return out;
    }
    let s = field.as_str();
    let (before, after) = s.split_at(field.cursor());
    out.push(seg(Tok::Slot(S::Text), before.to_string()));
    out.push(crate::seg::caret());
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
/// unknown — git hasn't emitted a percentage yet). Cell glyphs come from the
/// shared loading vocabulary ([`crate::loading::plan::bar`]) so the modal's
/// bar matches the splash's and degrades to `=`/`-` on ASCII terminals
/// (previously hardcoded `▓░`, which leaked Unicode there).
fn bar_segs(pct: Option<u8>) -> Vec<Seg> {
    const W: usize = 12;
    let (fill, empty) = crate::loading::plan::bar(W, pct.map(|p| p as f64 / 100.0));
    vec![
        seg(Tok::Slot(S::Accent), fill),
        seg(Tok::Slot(S::Ghost3), empty),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(paths: &[&str]) -> WorkspacePicker {
        WorkspacePicker::new(
            paths.iter().map(|s| s.to_string()).collect(),
            HashSet::new(),
        )
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
        let mut p = picker(&["/code/thegn", "/code/other", "/work/thegn-tools"]);
        type_str(&mut p, "thegn");
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/code/thegn"), "{paths:?}");
        assert!(paths.contains(&"/work/thegn-tools"), "{paths:?}");
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
        let mut p = WorkspacePicker::new(paths, HashSet::new());
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
        p.generation = 7;
        p.result_tx
            .try_send(DiscoveryEvent::Batch {
                generation: 7,
                paths: vec!["/code/seeded".into(), "/code/found".into()],
            })
            .unwrap();
        assert!(p.drain_discovery());
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/code/seeded", "/code/found"]);
        p.result_tx
            .try_send(DiscoveryEvent::Finished {
                generation: 7,
                report: thegn_core::repo::DiscoveryReport {
                    truncated: true,
                    ..thegn_core::repo::DiscoveryReport::default()
                },
            })
            .unwrap();
        assert!(p.drain_discovery());
        assert!(!p.scanning);
        assert!(p.truncated, "the UI must retain partial scan state");
        // Idempotent: nothing new → no change.
        assert!(!p.drain_discovery());
    }

    #[test]
    fn item_paths_track_items_through_seed_and_discovery() {
        let mut p = picker(&["/a", "/b"]);
        p.scanning = true;
        p.generation = 7;
        p.result_tx
            .try_send(DiscoveryEvent::Batch {
                generation: 7,
                paths: vec!["/b".into(), "/c".into()],
            })
            .unwrap();
        assert!(p.drain_discovery());

        p.apply_db_seed(
            vec!["/c".into(), "/d".into()],
            HashSet::from(["/a".to_string()]),
        );

        let paths: Vec<&str> = p.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["/c", "/d", "/b"]);
        assert_eq!(
            p.item_paths,
            HashSet::from(["/c".to_string(), "/d".to_string(), "/b".to_string()])
        );
    }

    #[test]
    fn drain_discovery_dedups_twenty_thousand_paths_in_scan_order() {
        let mut p = WorkspacePicker::new(vec!["/r/00007".into()], HashSet::new());
        p.scanning = true;
        p.generation = 7;

        let mut next = 0;
        let mut previous = None;
        while next < 20_000 {
            let mut batch = Vec::with_capacity(64);
            if let Some(path) = previous.take() {
                batch.push(path);
            }
            while batch.len() < 64 && next < 20_000 {
                batch.push(format!("/r/{next:05}"));
                next += 1;
            }
            previous = batch.last().cloned();
            p.result_tx
                .try_send(DiscoveryEvent::Batch {
                    generation: 7,
                    paths: batch,
                })
                .unwrap();
            assert!(p.drain_discovery());
        }

        let paths: Vec<&str> = p.items.iter().map(|item| item.path.as_str()).collect();
        let mut expected = vec!["/r/00007".to_string()];
        expected.extend(
            (0..20_000)
                .filter(|index| *index != 7)
                .map(|index| format!("/r/{index:05}")),
        );
        assert_eq!(
            paths,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(p.items.len(), 20_000);
        assert_eq!(p.item_paths.len(), 20_000);
    }

    #[test]
    fn discovery_batches_rank_only_new_items_against_a_live_query() {
        let mut p = WorkspacePicker::new(vec!["/r/seed".into()], HashSet::new());
        type_str(&mut p, "r");
        p.generation = 7;
        p.scanning = true;

        let mut next = 0;
        while next < 20_000 {
            let end = (next + 64).min(20_000);
            let paths = (next..end).map(|index| format!("/r/{index:05}")).collect();
            p.result_tx
                .try_send(DiscoveryEvent::Batch {
                    generation: 7,
                    paths,
                })
                .unwrap();
            assert!(p.drain_discovery());
            next = end;
        }

        assert_eq!(p.items.len(), 20_001);
        assert!(
            p.ranked_haystacks <= 2 * p.items.len(),
            "ranked {} haystacks for {} items",
            p.ranked_haystacks,
            p.items.len()
        );
    }

    #[test]
    fn incremental_discovery_matches_equal_a_full_recompute() {
        let mut p = WorkspacePicker::new(
            vec!["/seed/rust".into(), "/seed/zzz".into()],
            HashSet::new(),
        );
        type_str(&mut p, "rust");
        p.generation = 7;
        p.scanning = true;

        for paths in [
            vec!["/code/rust".into(), "/code/rust-analyzer".into()],
            vec![
                "/x/r/u/s/t".into(),
                "/deep/trusty".into(),
                "/code/zzz".into(),
            ],
            vec!["/first/rust".into(), "/second/rust".into()],
        ] {
            p.result_tx
                .try_send(DiscoveryEvent::Batch {
                    generation: 7,
                    paths,
                })
                .unwrap();
            assert!(p.drain_discovery());
        }

        let mut fresh = WorkspacePicker::new(
            p.items.iter().map(|entry| entry.path.clone()).collect(),
            HashSet::new(),
        );
        type_str(&mut fresh, "rust");
        let incremental_paths: Vec<&str> = p
            .matches()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        let fresh_paths: Vec<&str> = fresh
            .matches()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(incremental_paths, fresh_paths);
        assert_eq!(p.match_scores, fresh.match_scores);
    }

    #[test]
    fn finished_only_drain_does_not_rerank() {
        let mut p = picker(&["/code/rust", "/code/other"]);
        type_str(&mut p, "rust");
        p.generation = 7;
        p.scanning = true;
        let ranked_before = p.ranked_haystacks;
        p.result_tx
            .try_send(DiscoveryEvent::Finished {
                generation: 7,
                report: thegn_core::repo::DiscoveryReport::default(),
            })
            .unwrap();

        assert!(p.drain_discovery());
        assert_eq!(p.ranked_haystacks, ranked_before);
    }

    #[test]
    fn non_matching_discovery_batch_keeps_existing_match_allocation() {
        let mut p = picker(&["/code/rust", "/code/rust-analyzer"]);
        type_str(&mut p, "rust");
        p.generation = 7;
        p.scanning = true;
        let matches_ptr = p.matches.as_ptr();
        let scores_ptr = p.match_scores.as_ptr();

        p.result_tx
            .try_send(DiscoveryEvent::Batch {
                generation: 7,
                paths: vec!["/code/python".into(), "/code/go".into()],
            })
            .unwrap();

        assert!(p.drain_discovery());
        assert_eq!(p.matches.as_ptr(), matches_ptr);
        assert_eq!(p.match_scores.as_ptr(), scores_ptr);
        assert_eq!(p.matches.len(), 2);
    }

    #[test]
    fn empty_query_discovery_appends_in_scan_order() {
        let mut p = picker(&["/seed"]);
        p.generation = 7;
        p.scanning = true;
        for paths in [vec!["/scan/a".into()], vec!["/scan/b".into()]] {
            p.result_tx
                .try_send(DiscoveryEvent::Batch {
                    generation: 7,
                    paths,
                })
                .unwrap();
            assert!(p.drain_discovery());
        }

        let paths: Vec<&str> = p
            .matches()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, vec!["/seed", "/scan/a", "/scan/b"]);
    }

    #[test]
    fn apply_db_seed_keeps_existing_items_once_after_the_seed() {
        let mut p = picker(&["/x", "/y"]);
        p.apply_db_seed(vec!["/y".into(), "/z".into()], HashSet::new());

        let paths: Vec<&str> = p.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["/y", "/z", "/x"]);
    }

    #[test]
    fn drain_discovery_keeps_excluded_paths_out_of_items_and_index() {
        let mut p = WorkspacePicker::new(Vec::new(), HashSet::from(["/excluded".to_string()]));
        p.scanning = true;
        p.generation = 7;
        p.result_tx
            .try_send(DiscoveryEvent::Batch {
                generation: 7,
                paths: vec!["/excluded".into(), "/included".into()],
            })
            .unwrap();

        assert!(p.drain_discovery());
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].path, "/included");
        assert_eq!(p.item_paths, HashSet::from(["/included".to_string()]));
    }

    #[test]
    fn stale_discovery_generation_is_ignored() {
        let mut p = picker(&[]);
        p.generation = 2;
        p.result_tx
            .try_send(DiscoveryEvent::Batch {
                generation: 1,
                paths: vec!["/code/stale".into()],
            })
            .unwrap();
        assert!(!p.drain_discovery());
        assert!(p.items.is_empty());
    }

    #[test]
    fn closing_picker_cancels_its_discovery_generation() {
        let mut p = picker(&[]);
        let cancel = Arc::new(AtomicBool::new(false));
        p.cancel = Some(cancel.clone());
        p.scanning = true;
        p.cancel_discovery();
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!p.scanning);
    }

    #[test]
    fn query_uses_precomputed_haystack() {
        let mut p = picker(&["/code/Alpha"]);
        let haystack = p.items[0].haystack.clone();
        assert_eq!(haystack, "alpha /code/alpha");
        type_str(&mut p, "alpha");
        assert_eq!(p.items[0].haystack, haystack);
        assert_eq!(p.matches().len(), 1);
    }

    #[test]
    fn config_reload_restarts_discovery_only_for_scan_config_changes() {
        let mut p = picker(&[]);
        let cfg = thegn_core::config::Config::default();
        p.scan_roots.clone_from(&cfg.repo_roots);
        p.scan_depth = cfg.repo_scan_depth;
        assert!(!p.discovery_config_changed(&cfg));

        let mut changed = cfg;
        changed.repo_scan_depth += 1;
        assert!(p.discovery_config_changed(&changed));
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
        let mut p = picker(&["/code/thegn", "/code/other"]);
        p.handle_paste("thegn");
        assert_eq!(p.query.as_str(), "thegn");
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/code/thegn"]);
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
    fn picker_seed_orders_recents_first_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("tg-picker-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let db = thegn_core::db::Db::open_at(&dir.join("db.sqlite")).unwrap();
        let (a_s, b_s) = (
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        );
        // `b` is known via a worktree row (not a workspace, so not excluded);
        // `a` is a (newer) recent repo, so it sorts first.
        db.put_worktree(
            "b/main",
            &b_s,
            &b.join("main").to_string_lossy(),
            "main",
            None,
            None,
        )
        .unwrap();
        db.touch_repo(&a_s, "a").unwrap();
        let (seed, _excluded) = picker_seed(&db);
        assert_eq!(seed, vec![a_s.clone(), b_s.clone()]);
        // A vanished dir is filtered out.
        db.touch_repo("/no/such/dir", "gone").unwrap();
        assert_eq!(picker_seed(&db).0, vec![a_s, b_s]);
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn picker_seed_excludes_already_added_workspaces() {
        let dir = std::env::temp_dir().join(format!("tg-picker-excl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let added = dir.join("added");
        let recent = dir.join("recent");
        std::fs::create_dir_all(&added).unwrap();
        std::fs::create_dir_all(&recent).unwrap();
        let db = thegn_core::db::Db::open_at(&dir.join("db.sqlite")).unwrap();
        let (added_s, recent_s) = (
            added.to_string_lossy().into_owned(),
            recent.to_string_lossy().into_owned(),
        );
        // `added` is a registered workspace (and also touched as a recent, to
        // prove exclusion still wins); `recent` was only ever opened.
        db.put_workspace(&added_s, "added", "repo").unwrap();
        db.touch_repo(&added_s, "added").unwrap();
        db.touch_repo(&recent_s, "recent").unwrap();
        let (seed, excluded) = picker_seed(&db);
        assert!(
            !seed.contains(&added_s),
            "added workspace excluded from the seed: {seed:?}"
        );
        assert!(
            seed.contains(&recent_s),
            "a plain recent (unregistered) repo is kept: {seed:?}"
        );
        assert!(excluded.contains(&added_s));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
