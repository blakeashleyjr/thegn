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
use crate::seg::{self, Line, Tok, seg, sp};

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

pub(crate) struct WorkspacePicker {
    mode: PickerMode,
    /// Fuzzy-filter query (kept across Tab toggles).
    query: String,
    /// Manual-entry buffer (kept across Tab toggles).
    manual: String,
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
            query: String::new(),
            manual: String::new(),
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
        if self.query.trim().is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
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

    pub(crate) fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> PickerOutcome {
        if mods.contains(Modifiers::CTRL) {
            return match key {
                KeyCode::Char('c' | 'C' | 'g' | 'G') => PickerOutcome::Cancel,
                KeyCode::Char('j' | 'J' | 'n' | 'N') if self.mode == PickerMode::Fuzzy => {
                    self.move_down();
                    PickerOutcome::Pending
                }
                KeyCode::Char('k' | 'K' | 'p' | 'P') if self.mode == PickerMode::Fuzzy => {
                    self.move_up();
                    PickerOutcome::Pending
                }
                _ => PickerOutcome::Pending,
            };
        }
        if mods.contains(Modifiers::ALT) || mods.contains(Modifiers::SUPER) {
            return PickerOutcome::Pending;
        }
        if crate::input::is_escape_key(key) {
            return PickerOutcome::Cancel;
        }
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
                KeyCode::Backspace => {
                    self.query.pop();
                    self.selected = 0;
                    self.scroll_offset = 0;
                    self.recompute();
                    PickerOutcome::Pending
                }
                KeyCode::Char(c) => {
                    self.query.push(*c);
                    self.selected = 0;
                    self.scroll_offset = 0;
                    self.recompute();
                    PickerOutcome::Pending
                }
                _ => PickerOutcome::Pending,
            },
            PickerMode::Manual => match key {
                KeyCode::Enter => {
                    let v = self.manual.trim().to_string();
                    if v.is_empty() {
                        PickerOutcome::Pending
                    } else {
                        PickerOutcome::Manual(v)
                    }
                }
                KeyCode::Backspace => {
                    self.manual.pop();
                    PickerOutcome::Pending
                }
                KeyCode::Char(c) => {
                    self.manual.push(*c);
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
        const COLS: usize = 72;
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        match self.mode {
            PickerMode::Manual => {
                let spec = LayerSpec {
                    title: "new workspace".into(),
                    badge: Some(" tab: fuzzy ".into()),
                    cols: COLS,
                    rows: 3, // input + rule + footer
                    anchor: Anchor::TopThird,
                    ..LayerSpec::default()
                };
                let Some(inner) = open_layer(surface, screen, &spec) else {
                    return;
                };
                let mut prompt = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
                if self.manual.is_empty() {
                    prompt.push(seg(Tok::Slot(S::Ghost3), "path, URL, or new dir…"));
                } else {
                    prompt.push(seg(Tok::Slot(S::Text), self.manual.clone()));
                    prompt.push(seg(Tok::Slot(S::Accent), "▏"));
                }
                seg::draw_line(
                    surface,
                    inner.x,
                    inner.y,
                    inner.cols,
                    &Line::segs(prompt),
                    panel,
                );
                if inner.rows >= 3 {
                    seg::draw_line(surface, inner.x, inner.y + 1, inner.cols, &rule, panel);
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
                    seg::draw_line(surface, inner.x, inner.y + 2, inner.cols, &footer, panel);
                }
            }
            PickerMode::Fuzzy => {
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

                let mut prompt = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
                if self.query.is_empty() {
                    prompt.push(seg(Tok::Slot(S::Ghost3), "type to filter repos…"));
                } else {
                    prompt.push(seg(Tok::Slot(S::Text), self.query.clone()));
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
        assert_eq!(p.query, "abc");
        key(&mut p, KeyCode::Tab);
        assert_eq!(p.manual, "/tmp/x");
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
        assert!(p.query.contains("r0"));
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
