//! The Cmd-K command palette, rebuilt as a native in-process overlay. It reuses
//! nucleo (the matcher the original iocraft palette engine used) for fuzzy
//! ranking and draws a centered box into the back-buffer `Surface`. Action
//! dispatch calls host methods directly — no subprocess hop, no IPC.
//!
//! This is the native view + matcher the host drives, populated from host state.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use termwiz::cell::{AttributeChange, Intensity};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Position, Surface};

use crate::chrome::theme_color;
use crate::compositor::Rect;
use superzej_core::theme;

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

pub struct Palette {
    items: Vec<PaletteItem>,
    matcher: Matcher,
    query: String,
    selected: usize,
    /// Indices into `items`, best match first (or original order when empty).
    matches: Vec<usize>,
    accent: String,
}

impl Palette {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        let mut p = Self {
            items,
            matcher: Matcher::new(Config::DEFAULT),
            query: String::new(),
            selected: 0,
            matches: Vec::new(),
            accent: theme::TEAL.to_string(),
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

    #[allow(dead_code)] // used by tests; the live loop types via push_char/backspace
    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
        self.selected = 0;
        self.recompute();
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.recompute();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.recompute();
    }

    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
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

    /// Draw the palette as a centered box within `screen`.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let box_cols = (screen.cols * 6 / 10).clamp(20, 100).min(screen.cols);
        let max_rows = 12usize;
        let box_rows = (self.matches.len() + 2).clamp(3, max_rows).min(screen.rows);
        let x = screen.x + (screen.cols.saturating_sub(box_cols)) / 2;
        let y = screen.y + (screen.rows.saturating_sub(box_rows)) / 3;
        let rect = Rect {
            x,
            y,
            cols: box_cols,
            rows: box_rows,
        };

        let bg = theme_color(theme::PANEL);
        crate::chrome::fill(surface, rect, bg);

        // Query line.
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(x + 1),
            y: Position::Absolute(y),
        });
        surface.add_change(Change::Attribute(AttributeChange::Foreground(theme_color(
            &self.accent,
        ))));
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Attribute(AttributeChange::Intensity(
            Intensity::Bold,
        )));
        let prompt = format!("› {}", self.query);
        let clipped: String = prompt.chars().take(box_cols.saturating_sub(2)).collect();
        surface.add_change(Change::Text(clipped));

        // Result rows.
        let rows_avail = box_rows.saturating_sub(2);
        for (row, &item_idx) in self.matches.iter().take(rows_avail).enumerate() {
            let Some(item) = self.items.get(item_idx) else {
                continue;
            };
            let ry = y + 2 + row;
            let (fg, rbg) = if row == self.selected {
                (theme_color(theme::TEXT), theme_color(theme::PANEL2))
            } else {
                (theme_color(theme::DIM), bg)
            };
            if row == self.selected {
                crate::chrome::fill(
                    surface,
                    Rect {
                        x,
                        y: ry,
                        cols: box_cols,
                        rows: 1,
                    },
                    rbg,
                );
            }
            surface.add_change(Change::CursorPosition {
                x: Position::Absolute(x + 2),
                y: Position::Absolute(ry),
            });
            surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
            surface.add_change(Change::Attribute(AttributeChange::Background(rbg)));
            surface.add_change(Change::Attribute(AttributeChange::Intensity(
                Intensity::Normal,
            )));
            let label: String = item
                .label
                .chars()
                .take(box_cols.saturating_sub(3))
                .collect();
            surface.add_change(Change::Text(label));
        }
        // Reset attrs so subsequent draws aren't tinted.
        surface.add_change(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
        surface.add_change(Change::Attribute(AttributeChange::Background(
            ColorAttribute::Default,
        )));
    }
}

/// Build the palette's item list: the command actions + a nav row per open tab
/// (`tab:<name>`), ordered by frecency for the empty-query view (the host port
/// of the old engine's command + nav + frecency sources).
pub(crate) fn build_palette(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
    cfg: &superzej_core::config::Config,
) -> Vec<crate::palette::PaletteItem> {
    use crate::palette::PaletteItem;
    let mut items = vec![
        PaletteItem::new("new-worktree", "New worktree"),
        PaletteItem::new("new-workspace", "New workspace"),
        PaletteItem::new("switch-workspace", "Switch workspace"),
        PaletteItem::new("close-worktree", "Close worktree"),
        PaletteItem::new("show-diff", "Show diff"),
        PaletteItem::new("open-pr", "Open pull request"),
        PaletteItem::new("files-drawer", "Toggle files drawer"),
        PaletteItem::new("lazygit", "Open lazygit"),
        PaletteItem::new("toggle-strip", "Toggle pin strip"),
        PaletteItem::new("quit", "Quit superzej"),
    ];

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

    // Add workspaces (repos) for switching
    if let Ok(workspaces) = db.workspaces() {
        for w in workspaces {
            // Don't add the current workspace as a switch target
            if w.repo_path != session.id {
                let name = w.name;
                items.push(PaletteItem::new(
                    format!("repo:{}", w.repo_path),
                    format!("✦ {}", name),
                ));
            }
        }
    }

    let usage = db.palette_usage().unwrap_or_default();
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
        assert!(text.contains("› work"), "query prompt drawn");
        assert!(text.contains("New work"), "a matching label drawn");
    }
}
