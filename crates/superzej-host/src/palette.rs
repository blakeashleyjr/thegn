//! The Cmd-K command palette, rebuilt as a native in-process overlay. It reuses
//! nucleo (the matcher the original iocraft palette engine used) for fuzzy
//! ranking and draws a centered box into the back-buffer `Surface`. Action
//! dispatch calls host methods directly — no subprocess hop, no IPC.
//!
//! This is the native view + matcher the host drives, populated from host state.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Tok, seg, sp};

/// A selectable palette row. `key` is the stable dispatch/frecency key; `label`
/// is what the user sees and what fuzzy matching runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub key: String,
    pub label: String,
}

impl PaletteItem {
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
        }
    }
}

/// Order palette items by frecency for the empty-query view: items seen in
/// `usage` (`(key, count, last_used)`) float to the top by most-recent then
/// most-frequent; unseen items keep their original relative order below. Pure →
/// unit-tested. (This is the host port of the old engine's frecency source.)
pub fn order_by_frecency(
    items: Vec<PaletteItem>,
    usage: &[(String, i64, i64)],
) -> Vec<PaletteItem> {
    use std::cmp::Ordering;
    use std::collections::HashMap;
    let rank: HashMap<&str, (i64, i64)> = usage
        .iter()
        .map(|(k, c, l)| (k.as_str(), (*l, *c)))
        .collect();
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| {
        match (
            rank.get(items[a].key.as_str()),
            rank.get(items[b].key.as_str()),
        ) {
            (Some(x), Some(y)) => y.cmp(x), // higher (last_used, count) first
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(&b), // stable: original order
        }
    });
    idx.into_iter().map(|i| items[i].clone()).collect()
}

/// Maximum visible rows in the palette list at one time.
const MAX_ITEMS: usize = 10;

pub struct Palette {
    items: Vec<PaletteItem>,
    matcher: Matcher,
    query: String,
    selected: usize,
    /// Scroll offset: index of the first visible match row.
    scroll_offset: usize,
    /// Indices into `items`, best match first (or original order when empty).
    matches: Vec<usize>,
}

impl Palette {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        let mut p = Self {
            items,
            matcher: Matcher::new(Config::DEFAULT),
            query: String::new(),
            selected: 0,
            scroll_offset: 0,
            matches: Vec::new(),
        };
        p.recompute();
        p
    }

    #[allow(dead_code)] // accessor used by tests; live loop reads via render/selected_item
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Visible rows (resolved items), best match first.
    #[allow(dead_code)] // accessor used by tests
    pub fn matches(&self) -> Vec<&PaletteItem> {
        self.matches
            .iter()
            .filter_map(|&i| self.items.get(i))
            .collect()
    }

    pub fn selected_item(&self) -> Option<&PaletteItem> {
        self.matches
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// The raw selected index (used to sync `PaletteSession.selected`).
    pub fn selected_idx(&self) -> usize {
        self.selected
    }

    /// The current scroll offset (used by `PaletteSession::render`).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute();
    }

    #[allow(dead_code)] // used in tests
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute();
    }

    #[allow(dead_code)] // used in tests
    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute();
    }

    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
            // Keep the cursor visible: scroll down when it passes the bottom.
            self.clamp_scroll(MAX_ITEMS);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        // Keep the cursor visible: scroll up when it passes the top.
        self.clamp_scroll(MAX_ITEMS);
    }

    /// Adjust scroll_offset so `selected` stays within [offset, offset+visible).
    fn clamp_scroll(&mut self, visible: usize) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
    }

    fn recompute(&mut self) {
        if self.query.trim().is_empty() {
            self.matches = (0..self.items.len()).collect();
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                pattern
                    .score(Utf32Str::new(&it.label, &mut buf), &mut self.matcher)
                    .map(|s| (i, s))
            })
            .collect();
        scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
        self.matches = scored.into_iter().map(|(i, _)| i).collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    /// Draw the palette as the boxed "jump" layer (dim backdrop + shadow,
    /// upper-third anchor). The badge reads " menu " — the honest name for
    /// the Ctrl+Space binding (the mockup's ⌘K chip).
    #[allow(dead_code)] // used in tests
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        const COLS: usize = 66;
        let shown = self.matches.len().min(MAX_ITEMS);
        let spec = LayerSpec {
            title: "jump".into(),
            badge: Some(" menu ".into()),
            cols: COLS,
            rows: shown + 4, // prompt + rule + items + rule + footer
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

        // Prompt row: accent chevron, live query (ghost placeholder when empty).
        let mut prompt = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
        if self.query.is_empty() {
            prompt.push(seg(Tok::Slot(S::Ghost3), "type to filter…"));
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

        // Item rows: render the window [scroll_offset, scroll_offset+rows_avail).
        // The selected row carries the accent selection tint.
        let rows_avail = inner.rows.saturating_sub(4);
        let offset = self.scroll_offset;
        for row in 0..rows_avail {
            let match_idx = offset + row;
            let Some(&item_idx) = self.matches.get(match_idx) else {
                break;
            };
            let Some(item) = self.items.get(item_idx) else {
                continue;
            };
            let selected = match_idx == self.selected;
            let pad = if selected { Tok::SelAccent } else { panel };
            let name = if selected {
                seg(Tok::Slot(S::Text), item.label.clone()).bold()
            } else {
                seg(Tok::Slot(S::Dim), item.label.clone())
            };
            seg::draw_line(
                surface,
                inner.x,
                inner.y + 2 + row,
                inner.cols,
                &Line::segs(vec![sp(1), name]),
                pad,
            );
        }

        // Footer: navigation hints + the live match count + scroll indicator.
        if inner.rows >= 4 {
            let fy = inner.y + inner.rows - 2;
            seg::draw_line(surface, inner.x, fy, inner.cols, &rule, panel);
            let total = self.matches.len();
            let count_str = if total > MAX_ITEMS {
                let end = (self.scroll_offset + MAX_ITEMS).min(total);
                format!("{}-{}/{}", self.scroll_offset + 1, end, total)
            } else {
                format!("{total} matches")
            };
            let footer = Line::split(
                vec![
                    seg(Tok::Slot(S::Ghost2), "↑↓"),
                    seg(Tok::Slot(S::Ghost), " move   "),
                    seg(Tok::Slot(S::Ghost2), "↵"),
                    seg(Tok::Slot(S::Ghost), " run   "),
                    seg(Tok::Slot(S::Ghost2), "esc"),
                    seg(Tok::Slot(S::Ghost), " dismiss"),
                ],
                vec![seg(Tok::Slot(S::Ghost3), count_str)],
            );
            seg::draw_line(surface, inner.x, fy + 1, inner.cols, &footer, panel);
        }
    }
}

/// Build the static command/action rows for Cmd-K from the native host action
/// registry. Labels include the effective chord so the palette doubles as
/// searchable keybind help; custom `[[actions]]` with `menu = true` join the
/// same list automatically.
pub(crate) fn build_command_palette_items(
    cfg: &superzej_core::config::Config,
) -> Vec<crate::palette::PaletteItem> {
    let mut items: Vec<crate::palette::PaletteItem> = crate::keymap::action_specs()
        .iter()
        .filter(|spec| spec.palette)
        .map(|spec| {
            let label = crate::keymap::chord_hint_for(cfg, spec.id)
                .map(|chord| format!("{}  ({chord})", spec.label))
                .unwrap_or_else(|| spec.label.to_string());
            crate::palette::PaletteItem::new(spec.id, label)
        })
        .collect();

    for action in &cfg.actions {
        if !action.menu {
            continue;
        }
        let label = if action.key.trim().is_empty() {
            action.name.clone()
        } else {
            format!("{}  ({})", action.name, action.key.replace(' ', "-"))
        };
        items.push(crate::palette::PaletteItem::new(action.name.clone(), label));
    }

    items
}

/// Picker rows for "Move worktree to folder…": one row per existing folder in
/// the workspace (`move-to-folder:<name>`) plus a trailing "new folder" row
/// (`move-to-folder:__new__`) that prompts for a name.
pub(crate) fn build_move_to_folder_items(
    db: &superzej_core::db::Db,
    repo_path: &str,
) -> Vec<crate::palette::PaletteItem> {
    use crate::palette::PaletteItem;
    let mut items: Vec<PaletteItem> = db
        .folders_for_workspace(repo_path)
        .unwrap_or_default()
        .into_iter()
        .map(|f| PaletteItem::new(format!("move-to-folder:{}", f.name), f.name))
        .collect();
    items.push(PaletteItem::new("move-to-folder:__new__", "＋ New folder…"));
    items
}

/// The display label for a workspace switch entry. Carries the Ctrl+1..9
/// quick-jump slot (1-based position in the visible sidebar `workspace_order`)
/// for slots 1..9, so the entry fuzzy-matches by number or name and the digit
/// matches the sidebar hint. Workspaces past slot 9, or not in `workspace_order`
/// (no DB row yet), get no number.
fn workspace_palette_label(name: &str, repo_path: &str, workspace_order: &[String]) -> String {
    match workspace_order
        .iter()
        .position(|p| p == repo_path)
        .filter(|i| *i < 9)
        .map(|i| i + 1)
    {
        Some(n) => format!("\u{2726} {n} \u{b7} {name}"),
        None => format!("\u{2726} {name}"),
    }
}

/// Build the palette's item list: the command actions + a nav row per open tab
/// (`tab:<name>`), ordered by frecency for the empty-query view (the host port
/// of the old engine's command + nav + frecency sources).
pub(crate) fn build_palette(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
    cfg: &superzej_core::config::Config,
    issues: &[superzej_core::issue::Issue],
    workspace_order: &[String],
) -> Vec<crate::palette::PaletteItem> {
    use crate::palette::PaletteItem;
    let mut items = build_command_palette_items(cfg);

    // Configured pins (scope-filtered to the current workspace): summon by name.
    let ws = (!session.id.is_empty()).then_some(session.id.as_str());
    for (i, p) in crate::pins::PinSupervisor::resolve(cfg, ws)
        .into_iter()
        .enumerate()
    {
        items.push(PaletteItem::new(
            format!("summon-pin-{}", i + 1),
            format!("\u{1f4cc} {}", p.display_label()),
        ));
    }

    // Add the session's open worktrees (the palette jumps to a worktree; its
    // remembered active tab is restored).
    for g in &session.worktrees {
        items.push(PaletteItem::new(
            format!("tab:{}", g.name),
            format!("→ {}", g.name),
        ));
    }

    // Add persisted worktrees from other workspaces so the palette can jump
    // directly to a worktree and persist that target workspace's active tab.
    if let Ok(worktrees) = db.worktrees() {
        for wt in worktrees {
            if session.worktrees.iter().any(|g| g.name == wt.tab_name) {
                continue;
            }
            let label = if wt.branch.trim().is_empty() {
                wt.tab_name.clone()
            } else {
                wt.branch.clone()
            };
            items.push(PaletteItem::new(
                format!("wt:{}\t{}", wt.repo_root, wt.tab_name),
                format!("⎇ {label}"),
            ));
        }
    }

    // Terminals: jump directly to an existing terminal session.
    if let Ok(terms) = db.terminals() {
        for t in terms {
            let label = if t.connection_string.starts_with("ssh") {
                format!("🌐 {}", t.name)
            } else if t.connection_string.starts_with("mosh") {
                format!("🚀 {}", t.name)
            } else {
                format!("💻 {}", t.name)
            };
            items.push(PaletteItem::new(format!("tab:{}", t.name), label));
        }
    }

    // Add workspaces (repos) for switching. The label carries the Ctrl+1..9
    // quick-jump slot (from the visible sidebar order) for slots 1..9, so the
    // entry is fuzzy-matchable by number *or* name and the digit matches the
    // sidebar hint. The active workspace stays excluded, but its slot is still
    // reserved in `workspace_order` so the numbers line up.
    if let Ok(workspaces) = db.workspaces() {
        for w in workspaces {
            // Don't add the current workspace as a switch target
            if w.repo_path != session.id {
                let label = workspace_palette_label(&w.name, &w.repo_path, workspace_order);
                items.push(PaletteItem::new(format!("repo:{}", w.repo_path), label));
            }
        }
    }

    // Tracked issues: `issue:<id>` prefix, searchable by number + title.
    for issue in issues {
        let status_glyph = issue.status.glyph();
        let label = format!("{status_glyph} {} {}", issue.number, issue.title);
        items.push(PaletteItem::new(format!("issue:{}", issue.id), label));
    }

    // Recent files surfaced in All mode via frecency. Keys: "open-file:{path}:1".
    // We surface up to 10 recently opened files so they appear immediately.
    let usage = db.palette_usage().unwrap_or_default();
    for (key, _count, _last) in &usage {
        if let Some(payload) = key.strip_prefix("open-file:") {
            // payload = "{rel_path}:{line_no}"
            if let Some((rel_path, _)) = payload.rsplit_once(':') {
                // Avoid duplicates (the key itself is unique, but the path label might overlap).
                let full_key = key.clone();
                if !items.iter().any(|i| i.key == full_key) {
                    items.push(PaletteItem::new(full_key, format!("📄 {rel_path}")));
                }
            }
        }
    }

    crate::palette::order_by_frecency(items, &usage)
}

/// Build the sandbox picker shown before the agent picker for a new worktree.
pub(crate) fn build_sandbox_palette(
    cfg: &superzej_core::config::Config,
) -> Vec<crate::palette::PaletteItem> {
    let def = cfg.sandbox.default_backend.as_str();
    let mut rows = vec![
        ("auto", "Auto (configured chain)"),
        ("podman-rootless", "Rootless Podman"),
        ("podman-rootful", "Rootful Podman"),
        ("docker", "Docker"),
        ("bwrap", "Bubblewrap"),
        ("host", "Host / uncontained"),
    ];
    rows.sort_by_key(|(k, _)| if *k == def { 0 } else { 1 });
    rows.into_iter()
        .map(|(key, label)| {
            let suffix = if key == def { "  default" } else { "" };
            crate::palette::PaletteItem::new(format!("sandbox:{key}"), format!("▣ {label}{suffix}"))
        })
        .collect()
}
/// Build the agent-picker palette items for `cfg`: one row per agent/tool, plus
/// a literal shell. The key is the bare choice name (the `PendingAgent` gate in
/// the Enter handler routes it to a launch, not a command dispatch).
pub(crate) fn build_agent_palette(
    cfg: &superzej_core::config::Config,
) -> Vec<crate::palette::PaletteItem> {
    crate::agent::choices(cfg)
        .into_iter()
        .map(|name| {
            let label = format!("{} {name}", superzej_core::theme::agent_glyph(&name));
            crate::palette::PaletteItem::new(name, label)
        })
        .collect()
}

/// Build the account-switcher palette: every coding-agent account (config +
/// managed) grouped by provider, plus an "Add account" row per provider.
/// Selecting `account:<provider>:<name>` pins it as the focused repo's default
/// (or the global default when no repo is focused); `account-add:<provider>`
/// starts an interactive login. See [`superzej_core::account`].
pub(crate) fn build_account_palette(
    cfg: &superzej_core::config::Config,
    db: &superzej_core::db::Db,
) -> Vec<PaletteItem> {
    use superzej_core::account;
    let mut items = Vec::new();
    for p in account::PROVIDERS {
        for a in account::list(cfg, db, p.id) {
            let mark = if a.authed { "✓" } else { "• needs login" };
            items.push(PaletteItem::new(
                format!("account:{}:{}", p.id, a.name),
                format!("◈ {} · {} {mark}", p.id, a.name),
            ));
        }
        items.push(PaletteItem::new(
            format!("account-add:{}", p.id),
            format!("➕ Add {} account…", p.id),
        ));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<PaletteItem> {
        vec![
            PaletteItem::new("new-worktree", "New worktree"),
            PaletteItem::new("new-workspace", "New workspace"),
            PaletteItem::new("switch", "Switch workspace"),
            PaletteItem::new("diff", "Show diff"),
        ]
    }

    #[test]
    fn build_palette_numbers_workspace_entries_and_excludes_active() {
        use superzej_core::config::Config;
        use superzej_core::db::Db;

        let dir = std::env::temp_dir().join(format!(
            "sz-palette-build-{}-{}",
            std::process::id(),
            superzej_core::util::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_at(&dir.join("superzej.db")).unwrap();
        // Three workspaces in the DB; "/repo/app" is the active session.
        db.put_workspace("/repo/app", "app", "repo").unwrap();
        db.put_workspace("/repo/svc", "svc", "repo").unwrap();
        db.put_workspace("/repo/web", "web", "repo").unwrap();

        let session = crate::session::Session {
            id: "/repo/app".into(),
            worktrees: vec![],
            active: 0,
        };
        // Slot order (as `sidebar_workspace_order` would report): app=1, svc=2,
        // web=3. The active "app" still occupies slot 1 so svc/web get 2/3.
        let order = vec![
            "/repo/app".to_string(),
            "/repo/svc".to_string(),
            "/repo/web".to_string(),
        ];
        let cfg = Config::default();
        let items = build_palette(&session, &db, &cfg, &[], &order);

        let find = |key: &str| items.iter().find(|i| i.key == key).cloned();
        // Active workspace is not offered as a switch target.
        assert!(
            find("repo:/repo/app").is_none(),
            "active workspace excluded: {items:?}"
        );
        // The other two carry their quick-jump slot in the label.
        assert_eq!(
            find("repo:/repo/svc").map(|i| i.label),
            Some("\u{2726} 2 \u{b7} svc".to_string()),
        );
        assert_eq!(
            find("repo:/repo/web").map(|i| i.label),
            Some("\u{2726} 3 \u{b7} web".to_string()),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_label_carries_quick_jump_slot() {
        let order = vec![
            "/repo/app".to_string(),
            "/repo/svc".to_string(),
            "/repo/web".to_string(),
        ];
        // Slot = 1-based position in the visible sidebar order, matching Ctrl+N.
        assert_eq!(
            workspace_palette_label("svc", "/repo/svc", &order),
            "\u{2726} 2 \u{b7} svc"
        );
        // Unknown repo (no DB row in the order yet) → no number.
        assert_eq!(
            workspace_palette_label("ghost", "/repo/ghost", &order),
            "\u{2726} ghost"
        );
        // Past slot 9 → no number (Ctrl+N only covers 1..9).
        let mut long: Vec<String> = (0..10).map(|i| format!("/repo/w{i}")).collect();
        long.push("/repo/tenth".into());
        assert_eq!(
            workspace_palette_label("tenth", "/repo/tenth", &long),
            "\u{2726} tenth"
        );
        // The 9th (index 8) still gets a number.
        assert_eq!(
            workspace_palette_label("w8", "/repo/w8", &long),
            "\u{2726} 9 \u{b7} w8"
        );
    }

    #[test]
    fn frecency_floats_recent_then_frequent_to_the_top() {
        let items = vec![
            PaletteItem::new("a", "A"),
            PaletteItem::new("b", "B"),
            PaletteItem::new("c", "C"),
            PaletteItem::new("d", "D"),
        ];
        // c used most recently; a used earlier; b/d never.
        let usage = vec![("a".to_string(), 5, 100), ("c".to_string(), 2, 200)];
        let ordered = order_by_frecency(items, &usage);
        let out: Vec<&str> = ordered.iter().map(|i| i.key.as_str()).collect();
        // c (last=200) then a (last=100), then unseen b, d in original order.
        assert_eq!(out, vec!["c", "a", "b", "d"]);
    }

    #[test]
    fn empty_query_shows_all_in_order() {
        let p = Palette::new(items());
        let m: Vec<&str> = p.matches().iter().map(|i| i.key.as_str()).collect();
        assert_eq!(m, vec!["new-worktree", "new-workspace", "switch", "diff"]);
    }

    #[test]
    fn fuzzy_query_filters_and_ranks() {
        let mut p = Palette::new(items());
        p.set_query("worktree");
        let m = p.matches();
        assert_eq!(m.first().map(|i| i.key.as_str()), Some("new-worktree"));
        assert!(m.iter().all(|i| i.key != "diff"), "non-matches excluded");
    }

    #[test]
    fn fuzzy_subsequence_matches() {
        let mut p = Palette::new(items());
        p.set_query("nwk"); // subsequence of "New worKspace"/"New worKtree"
        assert!(
            !p.matches().is_empty(),
            "subsequence should match something"
        );
    }

    #[test]
    fn navigation_clamps_and_tracks_selection() {
        let mut p = Palette::new(items());
        assert_eq!(
            p.selected_item().map(|i| i.key.as_str()),
            Some("new-worktree")
        );
        p.move_up(); // clamps at 0
        assert_eq!(
            p.selected_item().map(|i| i.key.as_str()),
            Some("new-worktree")
        );
        p.move_down();
        assert_eq!(
            p.selected_item().map(|i| i.key.as_str()),
            Some("new-workspace")
        );
        for _ in 0..20 {
            p.move_down(); // clamps at the end
        }
        assert_eq!(p.selected_item().map(|i| i.key.as_str()), Some("diff"));
    }

    #[test]
    fn incremental_typing_updates_matches() {
        let mut p = Palette::new(items());
        p.push_char('d');
        p.push_char('i');
        p.push_char('f');
        assert_eq!(p.selected_item().map(|i| i.key.as_str()), Some("diff"));
        p.backspace();
        p.backspace();
        p.backspace();
        assert_eq!(p.matches().len(), 4, "cleared query shows all again");
    }

    #[test]
    fn command_palette_surfaces_every_registered_keybind() {
        let cfg = superzej_core::config::Config::default();
        let items = build_command_palette_items(&cfg);
        let keys: std::collections::BTreeSet<&str> = items.iter().map(|i| i.key.as_str()).collect();

        for key in [
            "close-tab",
            "new-pane",
            "split-down",
            "split-right",
            "focus-left",
            "focus-right",
            "toggle-key-lock",
        ] {
            assert!(
                keys.contains(key),
                "palette missing registered keybind {key}"
            );
        }
    }

    #[test]
    fn command_palette_labels_include_effective_chords() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("close-tab".into(), "Ctrl Alt x".into());
        let items = build_command_palette_items(&cfg);
        let close = items
            .iter()
            .find(|i| i.key == "close-tab")
            .expect("close-tab item");
        assert!(
            close.label.contains("Ctrl-Alt-x"),
            "label was {}",
            close.label
        );
    }

    #[test]
    fn command_palette_includes_custom_menu_actions_with_chords() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.actions.push(superzej_core::config::CustomAction {
            name: "run-tests".into(),
            key: "Ctrl Alt r".into(),
            run: Some("just test".into()),
            action: None,
            params: Default::default(),
            menu: true,
            hint: Some("tests".into()),
            floating: true,
            close_on_exit: true,
        });
        let items = build_command_palette_items(&cfg);
        let custom = items
            .iter()
            .find(|i| i.key == "run-tests")
            .expect("custom menu action");
        assert!(
            custom.label.contains("Ctrl-Alt-r"),
            "label was {}",
            custom.label
        );
    }

    #[test]
    fn render_draws_query_and_results_into_surface() {
        let mut p = Palette::new(items());
        p.set_query("work");
        let mut s = Surface::new(80, 24);
        p.render(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("❯ work"), "query prompt drawn: {text:?}");
        assert!(text.contains("New work"), "a matching label drawn");
        assert!(text.contains("jump"), "layer title drawn");
        assert!(text.contains("menu"), "badge drawn");
        assert!(text.contains("matches"), "footer count drawn");
    }
}
