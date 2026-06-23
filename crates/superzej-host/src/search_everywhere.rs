//! Search Everywhere — the palette's multi-mode async search layer.
//!
//! The command palette (Ctrl+Space) gains five search modes selected by a
//! leading prefix character in the query:
//!
//!   (none)  All      — existing palette items + recent files
//!   `>`     Files    — gitignore-respecting file index, nucleo fuzzy
//!   `/`     Content  — grep-searcher streaming, 20-result batches
//!   `@`     Git      — branches, commits (recent), stashes, tags
//!   `#`     Symbols  — pattern grep filtered by language extension
//!
//! Tab cycles modes. A mode chip appears in the header.
//!
//! All I/O runs on `spawn_blocking`; results flow back through a tokio mpsc
//! channel owned by `PaletteSession`. `search_gen` increments on every
//! keystroke; stale results are silently discarded by generation comparison.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum visible rows for async-mode result lists (Files/Content/Git/Symbols).
const MAX_ASYNC_ITEMS: usize = 8;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use termwiz::surface::Surface;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::palette::PaletteItem;
use crate::seg::{self, Line, Tok, seg, sp};

// ── Mode ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    All,
    Files,
    Content,
    Git,
    Symbols,
    /// `!` — runnable tasks (item 523).
    Tasks,
    /// `$` — diagnostics / problems (item 523).
    Problems,
    /// `%` — discovered tests (item 523).
    Tests,
}

impl PaletteMode {
    /// Split a raw query string into (mode, query_without_prefix).
    pub fn parse(raw: &str) -> (Self, &str) {
        match raw.chars().next() {
            Some('>') => (PaletteMode::Files, raw[1..].trim_start()),
            Some('/') => (PaletteMode::Content, raw[1..].trim_start()),
            Some('@') => (PaletteMode::Git, raw[1..].trim_start()),
            Some('#') => (PaletteMode::Symbols, raw[1..].trim_start()),
            Some('!') => (PaletteMode::Tasks, raw[1..].trim_start()),
            Some('$') => (PaletteMode::Problems, raw[1..].trim_start()),
            Some('%') => (PaletteMode::Tests, raw[1..].trim_start()),
            _ => (PaletteMode::All, raw),
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            PaletteMode::All => "",
            PaletteMode::Files => ">",
            PaletteMode::Content => "/",
            PaletteMode::Git => "@",
            PaletteMode::Symbols => "#",
            PaletteMode::Tasks => "!",
            PaletteMode::Problems => "$",
            PaletteMode::Tests => "%",
        }
    }

    pub fn chip_label(self) -> &'static str {
        match self {
            PaletteMode::All => " all ",
            PaletteMode::Files => " files ",
            PaletteMode::Content => " content ",
            PaletteMode::Git => " git ",
            PaletteMode::Symbols => " symbols ",
            PaletteMode::Tasks => " tasks ",
            PaletteMode::Problems => " problems ",
            PaletteMode::Tests => " tests ",
        }
    }

    /// Whether this mode's results are filled synchronously from in-memory
    /// panel state (tasks/tests/problems) rather than via the async workers.
    pub fn is_local(self) -> bool {
        matches!(
            self,
            PaletteMode::Tasks | PaletteMode::Problems | PaletteMode::Tests
        )
    }

    pub fn cycle(self) -> Self {
        match self {
            PaletteMode::All => PaletteMode::Files,
            PaletteMode::Files => PaletteMode::Content,
            PaletteMode::Content => PaletteMode::Git,
            PaletteMode::Git => PaletteMode::Symbols,
            PaletteMode::Symbols => PaletteMode::Tasks,
            PaletteMode::Tasks => PaletteMode::Tests,
            PaletteMode::Tests => PaletteMode::Problems,
            PaletteMode::Problems => PaletteMode::All,
        }
    }
}

// ── File index ───────────────────────────────────────────────────────────────

/// A snapshot of all (non-ignored) relative paths in a worktree, built once
/// per worktree per FS-watcher cycle. Cheap to clone (`Arc` paths).
#[derive(Clone)]
pub struct FileIndex {
    pub paths: Arc<Vec<Arc<str>>>,
    #[allow(dead_code)]
    pub root: PathBuf,
    #[allow(dead_code)]
    pub generation: u64,
}

impl FileIndex {
    pub fn build(root: &Path, include_hidden: bool) -> Self {
        use ignore::WalkBuilder;
        let mut paths = Vec::with_capacity(4096);
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(!include_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false)
            .max_depth(None);
        for entry in builder.build().flatten() {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                let s: Arc<str> = rel.to_string_lossy().into();
                paths.push(s);
            }
        }
        FileIndex {
            paths: Arc::new(paths),
            root: root.to_owned(),
            generation: 0,
        }
    }
}

// ── Async result types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileMatch {
    pub path: Arc<str>,
    pub score: u32,
}

#[derive(Debug, Clone)]
pub struct ContentMatch {
    pub path: String,
    pub line_no: u64,
    pub line_text: String,
}

#[derive(Debug, Clone)]
pub struct GitRefMatch {
    pub kind: GitRefKind,
    pub name: String,
    pub extra: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRefKind {
    Branch,
    RemoteBranch,
    Tag,
    Commit,
    Stash,
}

#[derive(Debug, Clone)]
pub struct SymbolMatch {
    pub path: String,
    pub line_no: u64,
    pub symbol: String,
    /// Short kind label ("fn", "struct", …) — from LSP when available, else the
    /// matched keyword. `None` falls back to "fn" in the UI.
    pub kind: Option<String>,
}

/// A runnable task match (item 523, `!` mode).
#[derive(Debug, Clone)]
pub struct TaskMatch {
    pub name: String,
    /// Short kind label ("test", "build", …) for the row glyph/context.
    pub kind: String,
}

/// A diagnostic match (item 523, `$` mode).
#[derive(Debug, Clone)]
pub struct ProblemMatch {
    pub file: String,
    pub line: u64,
    /// "error" | "warning" | "info" | "hint".
    pub severity: String,
    pub message: String,
}

/// A discovered-test match (item 523, `%` mode).
#[derive(Debug, Clone)]
pub struct TestMatch {
    pub label: String,
    /// File + line to jump to; empty path means "no location" (not jumpable).
    pub path: String,
    pub line: u64,
    /// "pass" | "fail" | "running" | "skip" | "" for the row glyph.
    pub state: String,
}

pub enum AsyncSearchResult {
    FileIndexReady {
        sg: u64,
        index: Arc<Vec<Arc<str>>>,
        root: PathBuf,
    },
    FileMatches {
        sg: u64,
        matches: Vec<FileMatch>,
    },
    ContentMatches {
        sg: u64,
        matches: Vec<ContentMatch>,
        done: bool,
    },
    GitMatches {
        sg: u64,
        matches: Vec<GitRefMatch>,
    },
    SymbolMatches {
        sg: u64,
        matches: Vec<SymbolMatch>,
    },
}

// ── AsyncResults — collected results per mode ─────────────────────────────────

#[derive(Default)]
pub struct AsyncResults {
    pub files: Vec<FileMatch>,
    pub content: Vec<ContentMatch>,
    pub content_done: bool,
    pub git: Vec<GitRefMatch>,
    pub symbols: Vec<SymbolMatch>,
    // Synchronous (local) providers — filled from panel state (item 523).
    pub tasks: Vec<TaskMatch>,
    pub problems: Vec<ProblemMatch>,
    pub tests: Vec<TestMatch>,
}

impl AsyncResults {
    pub fn clear(&mut self) {
        self.files.clear();
        self.content.clear();
        self.content_done = false;
        self.git.clear();
        self.symbols.clear();
        self.tasks.clear();
        self.problems.clear();
        self.tests.clear();
    }
}

// ── PaletteSession ────────────────────────────────────────────────────────────

/// Replaces `Option<crate::palette::Palette>` in the event loop. Holds the
/// async channel and carries the inner `Palette` for All-mode.
pub struct PaletteSession {
    pub raw_query: String,
    pub mode: PaletteMode,
    /// All-mode static items (built once on open, re-sorted by query).
    pub palette: crate::palette::Palette,
    pub async_results: AsyncResults,
    pub selected: usize,
    /// Scroll offset for async-mode result lists (All-mode uses the inner
    /// Palette's own scroll_offset).
    pub scroll_offset: usize,
    pub result_rx: UnboundedReceiver<AsyncSearchResult>,
    pub result_tx: UnboundedSender<AsyncSearchResult>,
    pub search_gen: u64,
    pub searching: bool,
}

impl PaletteSession {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        let (tx, rx) = unbounded_channel();
        PaletteSession {
            raw_query: String::new(),
            mode: PaletteMode::All,
            palette: crate::palette::Palette::new(items),
            async_results: AsyncResults::default(),
            selected: 0,
            scroll_offset: 0,
            result_rx: rx,
            result_tx: tx,
            search_gen: 0,
            searching: false,
        }
    }

    /// Append a character, detect mode from the prefix, reset selection.
    /// Returns (new_mode, query_without_prefix).
    pub fn push_char(&mut self, c: char) -> (PaletteMode, String) {
        self.raw_query.push(c);
        self.apply_query()
    }

    /// Remove the last character, re-detect mode.
    pub fn backspace(&mut self) -> (PaletteMode, String) {
        self.raw_query.pop();
        self.apply_query()
    }

    /// Cycle to the next mode, updating the raw_query prefix.
    pub fn cycle_mode(&mut self) -> (PaletteMode, String) {
        let next = self.mode.cycle();
        // Strip the old prefix and prepend the new one.
        let (_, inner) = PaletteMode::parse(&self.raw_query);
        self.raw_query = format!("{}{}", next.prefix(), inner);
        self.apply_query()
    }

    fn apply_query(&mut self) -> (PaletteMode, String) {
        let (mode, inner) = PaletteMode::parse(&self.raw_query);
        if mode != self.mode {
            self.mode = mode;
            self.async_results.clear();
        }
        self.selected = 0;
        self.scroll_offset = 0;
        self.search_gen += 1;
        // The spinner is only for the async workers. `All` resolves in-place
        // here; the local providers (tasks/tests/problems) resolve synchronously
        // in `kick_palette_search`, which always runs this turn — so neither
        // should flash a spinner.
        self.searching = !mode.is_local();
        if mode == PaletteMode::All {
            self.palette.set_query(inner.to_string());
            self.searching = false;
        }
        (mode, inner.to_string())
    }

    pub fn move_up(&mut self) {
        if self.mode == PaletteMode::All {
            // Delegate to inner palette so selected_item() stays in sync.
            self.palette.move_up();
            self.selected = self.palette.selected_idx();
        } else {
            self.selected = self.selected.saturating_sub(1);
            self.clamp_scroll(MAX_ASYNC_ITEMS);
        }
    }

    pub fn move_down(&mut self) {
        if self.mode == PaletteMode::All {
            // Delegate to inner palette so selected_item() stays in sync.
            self.palette.move_down();
            self.selected = self.palette.selected_idx();
        } else {
            let total = self.visible_count();
            if total > 0 {
                self.selected = (self.selected + 1).min(total - 1);
                self.clamp_scroll(MAX_ASYNC_ITEMS);
            }
        }
    }

    fn clamp_scroll(&mut self, visible: usize) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
    }

    fn visible_count(&self) -> usize {
        match self.mode {
            PaletteMode::All => self.palette.matches().len(),
            PaletteMode::Files => self.async_results.files.len(),
            PaletteMode::Content => self.async_results.content.len(),
            PaletteMode::Git => self.async_results.git.len(),
            PaletteMode::Symbols => self.async_results.symbols.len(),
            PaletteMode::Tasks => self.async_results.tasks.len(),
            PaletteMode::Problems => self.async_results.problems.len(),
            PaletteMode::Tests => self.async_results.tests.len(),
        }
    }

    /// The dispatch key for the currently selected item, or `None`.
    pub fn selected_key(&self) -> Option<String> {
        match self.mode {
            PaletteMode::All => self.palette.selected_item().map(|i| i.key.clone()),
            PaletteMode::Files => self
                .async_results
                .files
                .get(self.selected)
                .map(|m| format!("open-file:{}:1", m.path)),
            PaletteMode::Content => self
                .async_results
                .content
                .get(self.selected)
                .map(|m| format!("open-file:{}:{}", m.path, m.line_no)),
            PaletteMode::Git => self
                .async_results
                .git
                .get(self.selected)
                .map(|m| match m.kind {
                    GitRefKind::Branch | GitRefKind::RemoteBranch => {
                        format!("git-branch:{}", m.name)
                    }
                    GitRefKind::Tag => format!("git-tag:{}", m.name),
                    GitRefKind::Commit => format!("git-commit:{}", m.name),
                    GitRefKind::Stash => format!("git-stash:{}", m.extra),
                }),
            PaletteMode::Symbols => self
                .async_results
                .symbols
                .get(self.selected)
                .map(|m| format!("open-file:{}:{}", m.path, m.line_no)),
            PaletteMode::Tasks => self
                .async_results
                .tasks
                .get(self.selected)
                .map(|m| format!("run-task:{}", m.name)),
            PaletteMode::Problems => self
                .async_results
                .problems
                .get(self.selected)
                .map(|m| format!("open-file:{}:{}", m.file, m.line)),
            PaletteMode::Tests => self.async_results.tests.get(self.selected).and_then(|m| {
                // Only jumpable tests (with a location) dispatch; others are
                // inert rows.
                (!m.path.is_empty()).then(|| format!("open-file:{}:{}", m.path, m.line))
            }),
        }
    }

    /// Drain the async result channel. Returns `true` if any new results arrived.
    pub fn drain_results(&mut self) -> bool {
        let sg = self.search_gen;
        let mut dirty = false;
        while let Ok(result) = self.result_rx.try_recv() {
            match result {
                AsyncSearchResult::FileIndexReady { sg: g, index, root } => {
                    // Propagate to event loop via a side channel isn't possible
                    // here; instead we smuggle it as a special result variant
                    // and the caller must handle FileIndexReady separately.
                    // Re-emit it so callers can pick it up.
                    let _ = self.result_tx.send(AsyncSearchResult::FileIndexReady {
                        sg: g,
                        index,
                        root,
                    });
                    break; // caller will drain again
                }
                AsyncSearchResult::FileMatches { sg: g, matches } if g == sg => {
                    self.async_results.files = matches;
                    self.searching = false;
                    dirty = true;
                }
                AsyncSearchResult::ContentMatches {
                    sg: g,
                    matches,
                    done,
                } if g == sg => {
                    self.async_results.content.extend(matches);
                    if done {
                        self.searching = false;
                    }
                    dirty = true;
                }
                AsyncSearchResult::GitMatches { sg: g, matches } if g == sg => {
                    self.async_results.git = matches;
                    self.searching = false;
                    dirty = true;
                }
                AsyncSearchResult::SymbolMatches { sg: g, matches } if g == sg => {
                    self.async_results.symbols = matches;
                    self.searching = false;
                    dirty = true;
                }
                _ => {} // stale generation — discard
            }
        }
        dirty
    }

    /// Draw the palette layer. Two-line rows for content/symbol/file matches.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        const COLS: usize = 72;
        const MAX_ITEMS: usize = 8;
        let shown = self.visible_count().min(MAX_ITEMS);
        // Two-line modes need 2 rows per item.
        let item_rows = match self.mode {
            PaletteMode::Content | PaletteMode::Symbols => shown * 2,
            _ => shown,
        };
        let spec = LayerSpec {
            title: "jump".into(),
            badge: Some(self.mode.chip_label().into()),
            cols: COLS,
            rows: item_rows + 4, // prompt + rule + items + rule + footer
            anchor: Anchor::TopThird,
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

        // Prompt row: mode-aware prefix chip + query.
        let mut prompt = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
        let display_query = if self.mode == PaletteMode::All {
            self.raw_query.clone()
        } else {
            // Strip mode prefix from display
            let (_, inner) = PaletteMode::parse(&self.raw_query);
            format!("{}{}", self.mode.prefix(), inner)
        };
        if display_query
            .trim_start_matches(self.mode.prefix())
            .is_empty()
        {
            prompt.push(seg(Tok::Slot(S::Ghost3), "type to search…"));
        } else {
            prompt.push(seg(Tok::Slot(S::Text), display_query));
        }
        if self.searching {
            prompt.push(seg(Tok::Slot(S::Ghost2), "  …"));
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
        let mut row_y = inner.y + 2;

        match self.mode {
            PaletteMode::All => {
                // All-mode: inner Palette manages its own scroll_offset and selected.
                let offset = self.palette.scroll_offset();
                let all_matches = self.palette.matches();
                for row in 0..rows_avail {
                    let match_idx = offset + row;
                    let Some(item) = all_matches.get(match_idx) else {
                        break;
                    };
                    let selected = match_idx == self.palette.selected_idx();
                    draw_single_line_item(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &item.label,
                        selected,
                    );
                    row_y += 1;
                }
            }
            PaletteMode::Files => {
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .files
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(rows_avail)
                {
                    let selected = row == self.selected;
                    draw_single_line_item(surface, inner.x, row_y, inner.cols, &m.path, selected);
                    row_y += 1;
                }
                if self.async_results.files.is_empty() && !self.searching {
                    let msg = seg(Tok::Slot(S::Ghost2), "No files matched");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Content => {
                let items_avail = rows_avail / 2;
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .content
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(items_avail)
                {
                    let selected = row == self.selected;
                    let primary = format!("{}:{}", m.path, m.line_no);
                    let context = m.line_text.trim().chars().take(60).collect::<String>();
                    draw_two_line_item(
                        surface, inner.x, row_y, inner.cols, &primary, &context, selected,
                    );
                    row_y += 2;
                }
                if self.async_results.content.is_empty() && !self.searching {
                    let msg = seg(Tok::Slot(S::Ghost2), "No matches");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Git => {
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .git
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(rows_avail)
                {
                    let selected = row == self.selected;
                    let glyph = match m.kind {
                        GitRefKind::Branch | GitRefKind::RemoteBranch => "⎇ ",
                        GitRefKind::Tag => "⌖ ",
                        GitRefKind::Commit => "● ",
                        GitRefKind::Stash => "⟳ ",
                    };
                    let label = format!("{glyph}{}", m.name);
                    draw_single_line_item(surface, inner.x, row_y, inner.cols, &label, selected);
                    row_y += 1;
                }
                if self.async_results.git.is_empty() && !self.searching {
                    let msg = seg(Tok::Slot(S::Ghost2), "No git refs matched");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Symbols => {
                let items_avail = rows_avail / 2;
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .symbols
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(items_avail)
                {
                    let selected = row == self.selected;
                    let primary = format!("{} {}", m.kind.as_deref().unwrap_or("fn"), m.symbol);
                    let context = format!("{}:{}", m.path, m.line_no);
                    draw_two_line_item(
                        surface, inner.x, row_y, inner.cols, &primary, &context, selected,
                    );
                    row_y += 2;
                }
                if self.async_results.symbols.is_empty() && !self.searching {
                    let msg = seg(Tok::Slot(S::Ghost2), "No symbols matched");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Tasks => {
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .tasks
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(rows_avail)
                {
                    let selected = row == self.selected;
                    let label = format!("▸ {}  ({})", m.name, m.kind);
                    draw_single_line_item(surface, inner.x, row_y, inner.cols, &label, selected);
                    row_y += 1;
                }
                if self.async_results.tasks.is_empty() {
                    let msg = seg(Tok::Slot(S::Ghost2), "No tasks");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Problems => {
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .problems
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(rows_avail)
                {
                    let selected = row == self.selected;
                    let glyph = match m.severity.as_str() {
                        "error" => "✖ ",
                        "warning" => "▲ ",
                        "info" => "ℹ ",
                        _ => "· ",
                    };
                    let msg = m.message.chars().take(48).collect::<String>();
                    let label = format!("{glyph}{}:{}  {msg}", m.file, m.line);
                    draw_single_line_item(surface, inner.x, row_y, inner.cols, &label, selected);
                    row_y += 1;
                }
                if self.async_results.problems.is_empty() {
                    let msg = seg(Tok::Slot(S::Ghost2), "No problems");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
            PaletteMode::Tests => {
                let offset = self.scroll_offset;
                for (row, m) in self
                    .async_results
                    .tests
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(rows_avail)
                {
                    let selected = row == self.selected;
                    let glyph = match m.state.as_str() {
                        "pass" => "✓ ",
                        "fail" => "✗ ",
                        "running" => "… ",
                        "skip" => "○ ",
                        _ => "• ",
                    };
                    let label = format!("{glyph}{}", m.label);
                    draw_single_line_item(surface, inner.x, row_y, inner.cols, &label, selected);
                    row_y += 1;
                }
                if self.async_results.tests.is_empty() {
                    let msg = seg(Tok::Slot(S::Ghost2), "No tests");
                    seg::draw_line(
                        surface,
                        inner.x,
                        row_y,
                        inner.cols,
                        &Line::segs(vec![sp(1), msg]),
                        panel,
                    );
                }
            }
        }

        // Footer: navigation hints + match count with scroll position.
        if inner.rows >= 4 {
            let fy = inner.y + inner.rows - 2;
            seg::draw_line(surface, inner.x, fy, inner.cols, &rule, panel);
            let total = self.visible_count();
            let count_str = if total > MAX_ASYNC_ITEMS {
                let offset = if self.mode == PaletteMode::All {
                    self.palette.scroll_offset()
                } else {
                    self.scroll_offset
                };
                let end = (offset + MAX_ASYNC_ITEMS).min(total);
                format!("{}-{}/{}", offset + 1, end, total)
            } else if self.mode == PaletteMode::Content && !self.async_results.content_done {
                format!("{total}+ matches")
            } else {
                format!("{total} matches")
            };
            let footer = Line::split(
                vec![
                    seg(Tok::Slot(S::Ghost2), "↑↓"),
                    seg(Tok::Slot(S::Ghost), " move   "),
                    seg(Tok::Slot(S::Ghost2), "↵"),
                    seg(Tok::Slot(S::Ghost), " open   "),
                    seg(Tok::Slot(S::Ghost2), "tab"),
                    seg(Tok::Slot(S::Ghost), " mode   "),
                    seg(Tok::Slot(S::Ghost2), "esc"),
                    seg(Tok::Slot(S::Ghost), " close"),
                ],
                vec![seg(Tok::Slot(S::Ghost3), count_str)],
            );
            seg::draw_line(surface, inner.x, fy + 1, inner.cols, &footer, panel);
        }
    }
}

fn draw_single_line_item(
    surface: &mut Surface,
    x: usize,
    y: usize,
    cols: usize,
    label: &str,
    selected: bool,
) {
    let panel = Tok::Slot(S::Panel);
    let pad = if selected { Tok::SelAccent } else { panel };
    let name = if selected {
        seg(Tok::Slot(S::Text), label.to_string()).bold()
    } else {
        seg(Tok::Slot(S::Dim), label.to_string())
    };
    seg::draw_line(surface, x, y, cols, &Line::segs(vec![sp(1), name]), pad);
}

fn draw_two_line_item(
    surface: &mut Surface,
    x: usize,
    y: usize,
    cols: usize,
    primary: &str,
    secondary: &str,
    selected: bool,
) {
    let panel = Tok::Slot(S::Panel);
    let pad = if selected { Tok::SelAccent } else { panel };
    let name = if selected {
        seg(Tok::Slot(S::Text), primary.to_string()).bold()
    } else {
        seg(Tok::Slot(S::Dim), primary.to_string())
    };
    let ctx = seg(Tok::Slot(S::Ghost2), format!("  {secondary}"));
    seg::draw_line(surface, x, y, cols, &Line::segs(vec![sp(1), name]), pad);
    seg::draw_line(surface, x, y + 1, cols, &Line::segs(vec![ctx]), pad);
}

// ── Worker spawn functions ────────────────────────────────────────────────────

pub fn spawn_file_index_build(
    root: PathBuf,
    sg: u64,
    tx: UnboundedSender<AsyncSearchResult>,
    waker: termwiz::terminal::TerminalWaker,
    include_hidden: bool,
) {
    tokio::task::spawn_blocking(move || {
        let index = FileIndex::build(&root, include_hidden);
        let _ = tx.send(AsyncSearchResult::FileIndexReady {
            sg,
            index: index.paths,
            root,
        });
        let _ = waker.wake();
    });
}

pub fn spawn_file_search(
    paths: Arc<Vec<Arc<str>>>,
    query: String,
    sg: u64,
    max_results: usize,
    tx: UnboundedSender<AsyncSearchResult>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(Arc<str>, u32)> = paths
            .iter()
            .filter_map(|p| {
                pattern
                    .score(Utf32Str::new(p, &mut buf), &mut matcher)
                    .map(|s| (p.clone(), s))
            })
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.1));
        scored.truncate(max_results);
        let matches = scored
            .into_iter()
            .map(|(path, score)| FileMatch { path, score })
            .collect();
        let _ = tx.send(AsyncSearchResult::FileMatches { sg, matches });
        let _ = waker.wake();
    });
}

pub fn spawn_content_search(
    root: PathBuf,
    query: String,
    sg: u64,
    max_results: usize,
    include_hidden: bool,
    tx: UnboundedSender<AsyncSearchResult>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        use grep_regex::RegexMatcher;
        use grep_searcher::SearcherBuilder;
        use grep_searcher::sinks::UTF8;
        use ignore::WalkBuilder;

        // Treat query as a literal substring pattern for safety; escape regex special chars.
        let escaped = regex_escape(&query);
        let matcher = match RegexMatcher::new_line_matcher(&escaped) {
            Ok(m) => m,
            Err(_) => return,
        };
        let mut searcher = SearcherBuilder::new()
            .binary_detection(grep_searcher::BinaryDetection::quit(0))
            .build();

        let mut all: Vec<ContentMatch> = Vec::new();
        let mut walk = WalkBuilder::new(&root);
        walk.hidden(!include_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);

        const BATCH: usize = 20;
        let mut batch: Vec<ContentMatch> = Vec::with_capacity(BATCH);
        let tx2 = tx.clone();
        let waker2 = waker.clone();

        for entry in walk.build().flatten() {
            if gen_stale(sg) {
                break;
            }
            if all.len() >= max_results {
                break;
            }
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path().to_owned();
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();

            let rel2 = rel.clone();
            let _ = searcher.search_path(
                &matcher,
                &path,
                UTF8(|line_no, line| {
                    if all.len() + batch.len() >= max_results {
                        return Ok(false);
                    }
                    batch.push(ContentMatch {
                        path: rel2.clone(),
                        line_no,
                        line_text: line.trim_end_matches('\n').to_string(),
                    });
                    if batch.len() >= BATCH {
                        let drained: Vec<_> = std::mem::take(&mut batch);
                        all.extend(drained.clone());
                        let _ = tx2.send(AsyncSearchResult::ContentMatches {
                            sg,
                            matches: drained,
                            done: false,
                        });
                        let _ = waker2.wake();
                    }
                    Ok(true)
                }),
            );
        }

        // Flush any remaining batch.
        if !batch.is_empty() {
            let _ = tx.send(AsyncSearchResult::ContentMatches {
                sg,
                matches: batch,
                done: true,
            });
        } else {
            let _ = tx.send(AsyncSearchResult::ContentMatches {
                sg,
                matches: vec![],
                done: true,
            });
        }
        let _ = waker.wake();
    });
}

pub fn spawn_git_search(
    root: PathBuf,
    query: String,
    sg: u64,
    tx: UnboundedSender<AsyncSearchResult>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let mut matches: Vec<GitRefMatch> = Vec::new();
        let q_lower = query.to_ascii_lowercase();

        // Branches via `git for-each-ref`
        if let Ok(out) = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname:short) %(objecttype)",
                "refs/",
            ])
            .current_dir(&root)
            .output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut parts = line.splitn(2, ' ');
                let name = parts.next().unwrap_or("").trim();
                let kind_s = parts.next().unwrap_or("").trim();
                if name.is_empty() {
                    continue;
                }
                let name_lower = name.to_ascii_lowercase();
                if !q_lower.is_empty() && !name_lower.contains(&q_lower) {
                    continue;
                }
                let kind = if kind_s == "tag" {
                    GitRefKind::Tag
                } else if name.starts_with("origin/") || name.contains('/') {
                    GitRefKind::RemoteBranch
                } else {
                    GitRefKind::Branch
                };
                matches.push(GitRefMatch {
                    kind,
                    name: name.to_string(),
                    extra: String::new(),
                });
                if matches.len() >= 100 {
                    break;
                }
            }
        }

        // Recent commits (last 50)
        if let Ok(out) = std::process::Command::new("git")
            .args(["log", "--oneline", "-50", "--no-decorate"])
            .current_dir(&root)
            .output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((sha, msg)) = line.split_once(' ') {
                    let haystack = format!("{sha} {msg}").to_ascii_lowercase();
                    if !q_lower.is_empty() && !haystack.contains(&q_lower) {
                        continue;
                    }
                    matches.push(GitRefMatch {
                        kind: GitRefKind::Commit,
                        name: format!("{sha} {msg}"),
                        extra: sha.to_string(),
                    });
                }
            }
        }

        // Stash list
        if let Ok(out) = std::process::Command::new("git")
            .args(["stash", "list"])
            .current_dir(&root)
            .output()
        {
            for (idx, line) in String::from_utf8_lossy(&out.stdout).lines().enumerate() {
                let line_lower = line.to_ascii_lowercase();
                if !q_lower.is_empty() && !line_lower.contains(&q_lower) {
                    continue;
                }
                // line format: "stash@{0}: ..."
                let name = line
                    .split_once(": ")
                    .map(|x| x.1)
                    .unwrap_or(line)
                    .to_string();
                matches.push(GitRefMatch {
                    kind: GitRefKind::Stash,
                    name,
                    extra: idx.to_string(),
                });
            }
        }

        let _ = tx.send(AsyncSearchResult::GitMatches { sg, matches });
        let _ = waker.wake();
    });
}

pub fn spawn_symbol_search(
    root: PathBuf,
    query: String,
    sg: u64,
    max_results: usize,
    lsp: std::sync::Arc<crate::lsp::LspInner>,
    tx: UnboundedSender<AsyncSearchResult>,
    waker: termwiz::terminal::TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        // 1. Fast path: a tree-sitter-free regex sweep. Always runs (it's the
        //    fallback when no LSP server is available) and is sent immediately so
        //    the palette shows results without waiting on a language server.
        let (regex_hits, langs) = regex_symbol_sweep(&root, &query, max_results);
        let _ = tx.send(AsyncSearchResult::SymbolMatches {
            sg,
            matches: regex_hits.clone(),
        });
        let _ = waker.wake();

        // 2. Upgrade: query each present language's server (lazily starting it)
        //    for workspace symbols, and re-send LSP-first results when richer.
        let lsp_hits = lsp_workspace_symbols(&lsp, &root, &query, &langs);
        if !lsp_hits.is_empty() {
            let mut seen: std::collections::HashSet<(String, u64)> = lsp_hits
                .iter()
                .map(|m| (m.path.clone(), m.line_no))
                .collect();
            let mut merged = lsp_hits;
            for m in regex_hits {
                if seen.insert((m.path.clone(), m.line_no)) {
                    merged.push(m);
                }
            }
            merged.truncate(max_results);
            let _ = tx.send(AsyncSearchResult::SymbolMatches {
                sg,
                matches: merged,
            });
            let _ = waker.wake();
        }
    });
}

/// A regex symbol sweep over the worktree's code files. Returns the matches plus
/// the set of LSP-supported languages encountered (to drive the LSP upgrade).
fn regex_symbol_sweep(
    root: &std::path::Path,
    query: &str,
    max_results: usize,
) -> (
    Vec<SymbolMatch>,
    std::collections::HashSet<superzej_core::semantic::Lang>,
) {
    use grep_matcher::Matcher;
    use grep_regex::RegexMatcher;
    use grep_searcher::SearcherBuilder;
    use grep_searcher::sinks::UTF8;
    use ignore::WalkBuilder;
    use superzej_core::semantic::Lang;

    let sym_pat = r"(?m)^\s*(pub\s+)?(async\s+)?(?:fn|def|class|func|function|struct|impl|type|interface|enum)\s+(\w+)";
    let Ok(matcher) = RegexMatcher::new(sym_pat) else {
        return (Vec::new(), std::collections::HashSet::new());
    };
    let q_lower = query.to_ascii_lowercase();
    let code_exts = &[
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "c", "cpp", "h", "java", "kt", "swift", "rb",
        "php", "cs", "zig", "ex", "exs",
    ];
    let mut searcher = SearcherBuilder::new()
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .build();
    let mut all: Vec<SymbolMatch> = Vec::new();
    let mut langs = std::collections::HashSet::new();

    let mut walk = WalkBuilder::new(root);
    walk.hidden(false).git_ignore(true);
    for entry in walk.build().flatten() {
        if all.len() >= max_results {
            break;
        }
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().to_owned();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !code_exts.contains(&ext.as_str()) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if let Some(l) = Lang::from_path(&rel) {
            langs.insert(l);
        }

        let q_lower2 = q_lower.clone();
        let rel2 = rel.clone();
        let mut local: Vec<SymbolMatch> = Vec::new();
        let _ = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_no, line| {
                if let Ok(Some(m)) = matcher.find(line.as_bytes()) {
                    let hit = &line[m];
                    let symbol = hit
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .trim_matches('{')
                        .to_string();
                    if !symbol.is_empty() {
                        let sym_lower = symbol.to_ascii_lowercase();
                        if q_lower2.is_empty() || sym_lower.contains(&q_lower2) {
                            local.push(SymbolMatch {
                                path: rel2.clone(),
                                line_no,
                                symbol,
                                kind: keyword_kind(hit),
                            });
                        }
                    }
                }
                Ok(true)
            }),
        );
        all.extend(local);
    }
    all.truncate(max_results);
    (all, langs)
}

/// Map the matched declaration keyword to a short kind label.
fn keyword_kind(hit: &str) -> Option<String> {
    let kw = hit.split_whitespace().find(|w| {
        matches!(
            *w,
            "fn" | "def"
                | "class"
                | "func"
                | "function"
                | "struct"
                | "impl"
                | "type"
                | "interface"
                | "enum"
        )
    })?;
    Some(
        match kw {
            "struct" => "struct",
            "class" => "class",
            "impl" => "impl",
            "type" => "type",
            "interface" => "interface",
            "enum" => "enum",
            _ => "fn",
        }
        .to_string(),
    )
}

/// Query each present language's server (lazily started) for workspace symbols.
fn lsp_workspace_symbols(
    lsp: &crate::lsp::LspInner,
    root: &std::path::Path,
    query: &str,
    langs: &std::collections::HashSet<superzej_core::semantic::Lang>,
) -> Vec<SymbolMatch> {
    let mut hits = Vec::new();
    for &lang in langs {
        let Ok(client) = lsp.client(root, lang) else {
            continue;
        };
        let Ok(syms) = client.workspace_symbols(query) else {
            continue;
        };
        for s in syms {
            let path = std::path::Path::new(&s.location.path)
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| s.location.path.clone());
            hits.push(SymbolMatch {
                path,
                line_no: s.location.line_1based() as u64,
                symbol: s.name,
                kind: Some(s.kind.label().to_string()),
            });
        }
    }
    hits
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Escape a string so it can be used as a literal grep-regex pattern.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Placeholder for a generation staleness check (can't access the gen from
/// inside a closure without threading it). We always run to completion and
/// let the session's drain_results discard stale gen results.
fn gen_stale(_sg: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_all() {
        let (mode, inner) = PaletteMode::parse("hello");
        assert_eq!(mode, PaletteMode::All);
        assert_eq!(inner, "hello");
    }

    #[test]
    fn mode_parse_files() {
        let (mode, inner) = PaletteMode::parse(">foo");
        assert_eq!(mode, PaletteMode::Files);
        assert_eq!(inner, "foo");
    }

    #[test]
    fn mode_parse_content() {
        let (mode, inner) = PaletteMode::parse("/ error");
        assert_eq!(mode, PaletteMode::Content);
        assert_eq!(inner, "error");
    }

    #[test]
    fn mode_parse_git() {
        let (mode, inner) = PaletteMode::parse("@main");
        assert_eq!(mode, PaletteMode::Git);
        assert_eq!(inner, "main");
    }

    #[test]
    fn mode_parse_symbols() {
        let (mode, inner) = PaletteMode::parse("#fn_name");
        assert_eq!(mode, PaletteMode::Symbols);
        assert_eq!(inner, "fn_name");
    }

    #[test]
    fn mode_cycle() {
        assert_eq!(PaletteMode::All.cycle(), PaletteMode::Files);
        // Symbols now leads into the local providers before wrapping to All.
        assert_eq!(PaletteMode::Symbols.cycle(), PaletteMode::Tasks);
        assert_eq!(PaletteMode::Tasks.cycle(), PaletteMode::Tests);
        assert_eq!(PaletteMode::Tests.cycle(), PaletteMode::Problems);
        assert_eq!(PaletteMode::Problems.cycle(), PaletteMode::All);
        // The full cycle visits every mode exactly once and returns home.
        let mut seen = vec![PaletteMode::All];
        let mut m = PaletteMode::All;
        for _ in 0..8 {
            m = m.cycle();
            seen.push(m);
        }
        assert_eq!(seen.first(), seen.last());
        assert_eq!(seen.len(), 9, "8 modes + wrap");
    }

    #[test]
    fn mode_parse_local_providers() {
        // Item 523: the three new prefixes.
        assert_eq!(PaletteMode::parse("!build").0, PaletteMode::Tasks);
        assert_eq!(PaletteMode::parse("!build").1, "build");
        assert_eq!(PaletteMode::parse("$err").0, PaletteMode::Problems);
        assert_eq!(PaletteMode::parse("%my_test").0, PaletteMode::Tests);
        assert!(PaletteMode::Tasks.is_local());
        assert!(PaletteMode::Problems.is_local());
        assert!(PaletteMode::Tests.is_local());
        assert!(!PaletteMode::Files.is_local());
    }

    #[test]
    fn selected_key_local_providers() {
        let mut s = PaletteSession::new(vec![]);
        s.mode = PaletteMode::Tasks;
        s.async_results.tasks.push(TaskMatch {
            name: "test".into(),
            kind: "test".into(),
        });
        assert_eq!(s.selected_key(), Some("run-task:test".into()));

        s.mode = PaletteMode::Problems;
        s.async_results.problems.push(ProblemMatch {
            file: "src/a.rs".into(),
            line: 12,
            severity: "error".into(),
            message: "boom".into(),
        });
        assert_eq!(s.selected_key(), Some("open-file:src/a.rs:12".into()));

        s.mode = PaletteMode::Tests;
        s.async_results.tests.push(TestMatch {
            label: "it_works".into(),
            path: "tests/x.rs".into(),
            line: 7,
            state: "pass".into(),
        });
        assert_eq!(s.selected_key(), Some("open-file:tests/x.rs:7".into()));
        // A test with no location is inert (no dispatch key).
        s.async_results.tests.clear();
        s.async_results.tests.push(TestMatch {
            label: "no_loc".into(),
            path: String::new(),
            line: 0,
            state: "fail".into(),
        });
        assert_eq!(s.selected_key(), None);
    }

    #[test]
    fn palette_session_push_char_detects_mode() {
        let mut s = PaletteSession::new(vec![]);
        let (mode, inner) = s.push_char('>');
        assert_eq!(mode, PaletteMode::Files);
        assert_eq!(inner, "");
    }

    #[test]
    fn palette_session_backspace_restores_all() {
        let mut s = PaletteSession::new(vec![]);
        s.push_char('>');
        let (mode, _) = s.backspace();
        assert_eq!(mode, PaletteMode::All);
    }

    #[test]
    fn palette_session_cycle_mode_updates_prefix() {
        let mut s = PaletteSession::new(vec![]);
        let (mode, _) = s.cycle_mode();
        assert_eq!(mode, PaletteMode::Files);
        assert!(s.raw_query.starts_with('>'));
    }

    #[test]
    fn selected_key_file_mode() {
        let mut s = PaletteSession::new(vec![]);
        s.mode = PaletteMode::Files;
        s.async_results.files.push(FileMatch {
            path: "src/main.rs".into(),
            score: 100,
        });
        assert_eq!(s.selected_key(), Some("open-file:src/main.rs:1".into()));
    }

    #[test]
    fn selected_key_content_mode() {
        let mut s = PaletteSession::new(vec![]);
        s.mode = PaletteMode::Content;
        s.async_results.content.push(ContentMatch {
            path: "src/lib.rs".into(),
            line_no: 42,
            line_text: "let x = 1;".into(),
        });
        assert_eq!(s.selected_key(), Some("open-file:src/lib.rs:42".into()));
    }

    #[test]
    fn selected_key_git_branch() {
        let mut s = PaletteSession::new(vec![]);
        s.mode = PaletteMode::Git;
        s.async_results.git.push(GitRefMatch {
            kind: GitRefKind::Branch,
            name: "main".into(),
            extra: String::new(),
        });
        assert_eq!(s.selected_key(), Some("git-branch:main".into()));
    }

    #[test]
    fn regex_escape_handles_dots() {
        let escaped = regex_escape("foo.bar");
        assert_eq!(escaped, "foo\\.bar");
    }

    #[test]
    fn render_does_not_panic_on_small_surface() {
        let mut s = PaletteSession::new(vec![]);
        s.push_char('>');
        let mut surface = Surface::new(40, 12);
        s.render(
            &mut surface,
            Rect {
                x: 0,
                y: 0,
                cols: 40,
                rows: 12,
            },
        );
        // no panic = pass
    }
}
