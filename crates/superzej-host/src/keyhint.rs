//! Keybinding hint data + the transient **which-key** popup shown after a
//! pending prefix. The full cheatsheet is the panel's Keys section (9), whose
//! Full view renders [`cheatsheet_groups`] — rows stay derived from the core
//! keymap registry (`superzej_core::keymap::effective`) so labels live in one
//! place.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use crate::sequence::Key;

/// One overlay row: a chord hint on the left, a label on the right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintRow {
    pub chord: String,
    pub label: String,
}

/// A titled group of rows (cheatsheet sections).
#[derive(Debug, Clone)]
pub struct HintGroup {
    pub title: String,
    pub rows: Vec<HintRow>,
}

/// Build the grouped cheatsheet from the effective core registry. Actions are
/// bucketed by a coarse category derived from their id, so the overlay reads as
/// sections rather than one long list. Actions without a chord are skipped.
pub fn cheatsheet_groups(cfg: &superzej_core::config::Config) -> Vec<HintGroup> {
    use superzej_core::keymap;
    let mut lifecycle = Vec::new();
    let mut nav = Vec::new();
    let mut tools = Vec::new();
    let mut view = Vec::new();
    let mut other = Vec::new();

    for a in keymap::effective(cfg) {
        let Some(chord) = a.chords.first() else {
            continue;
        };
        let row = HintRow {
            chord: chord.to_hint(),
            label: a.menu_label.clone(),
        };
        let bucket = match a.id.as_str() {
            id if id.starts_with("new-") || id == "close-worktree" || id == "quit" => {
                &mut lifecycle
            }
            id if id.starts_with("focus-")
                || id.ends_with("-tab")
                || id == "switch-workspace"
                || id == "dashboard" =>
            {
                &mut nav
            }
            id if id.starts_with("split-")
                || id.starts_with("toggle-")
                || id == "show-diff"
                || id == "files-drawer" =>
            {
                &mut view
            }
            "lazygit" | "yazi" | "editor" | "palette" | "cheatsheet" => &mut tools,
            _ => &mut other,
        };
        bucket.push(row);
    }

    [
        ("Workspaces & worktrees", lifecycle),
        ("Navigation", nav),
        ("Panels & layout", view),
        ("Tools", tools),
        ("Other", other),
    ]
    .into_iter()
    .filter(|(_, rows)| !rows.is_empty())
    .map(|(title, rows)| HintGroup {
        title: title.to_string(),
        rows,
    })
    .collect()
}

/// Format a single `Key` for the which-key popup (e.g. `Ctrl-x`, `Space`, `↵`).
pub fn key_hint(key: &Key) -> String {
    let mut parts = Vec::new();
    if key.mods.contains(Modifiers::CTRL) {
        parts.push("Ctrl");
    }
    if key.mods.contains(Modifiers::SUPER) {
        parts.push("Super");
    }
    if key.mods.contains(Modifiers::ALT) {
        parts.push("Alt");
    }
    if key.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }
    let base = match key.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "↵".to_string(),
        KeyCode::Escape => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Backspace => "⌫".to_string(),
        KeyCode::LeftArrow => "←".to_string(),
        KeyCode::RightArrow => "→".to_string(),
        KeyCode::UpArrow => "↑".to_string(),
        KeyCode::DownArrow => "↓".to_string(),
        other => format!("{other:?}"),
    };
    parts.push(&base);
    parts.join("-")
}

/// A short human label for an action in the which-key popup. `custom` is the
/// host's parsed custom-action list, used to resolve `Action::Custom(idx)` to
/// the action's configured name (e.g. "File: Ready to merge").
fn action_label(
    action: &crate::keymap::Action,
    custom: &[crate::keymap::HostCustomAction],
) -> String {
    use crate::keymap::Action;
    match action {
        Action::SwitchMode(m) => format!("→ {} mode", m.as_str()),
        Action::Custom(idx) => custom
            .get(*idx as usize)
            .map(|a| a.name().to_string())
            .unwrap_or_else(|| "custom action".to_string()),
        other => other.key().replace('-', " "),
    }
}

/// Max rows shown in the filtered list window before scrolling.
const WK_MAX_ROWS: usize = 10;

/// One which-key candidate row: the next `Key` to press (re-fed through
/// `keymap.dispatch` when run from filter mode), its display chord + human
/// label, and the category header it groups under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhichKeyRow {
    pub key: Key,
    pub chord: String,
    pub label: String,
    pub group: String,
}

/// Title-case a bucket key: first char upper, rest lower (`"media"` → `"Media"`).
fn titlecase(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + &cs.as_str().to_lowercase(),
    }
}

/// The category header a continuation groups under. `SwitchMode` → "Mode"; a
/// label with a `":"` prefix ("File: Ready to merge") → that prefix ("File");
/// otherwise the first `-`-segment of the action key ("media-next" → "Media").
fn group_of(action: &crate::keymap::Action, label: &str) -> String {
    use crate::keymap::Action;
    if matches!(action, Action::SwitchMode(_)) {
        return "Mode".to_string();
    }
    if let Some((prefix, _)) = label.split_once(':') {
        let p = prefix.trim();
        if !p.is_empty() {
            return titlecase(p);
        }
    }
    match action.key().split_once('-') {
        Some((head, _)) if !head.is_empty() => titlecase(head),
        _ => titlecase(action.key()),
    }
}

/// Build the which-key rows for the live continuations, grouped by category.
/// Rows are stable-sorted by group (original order preserved within a group) so
/// same-group rows are contiguous for header rendering. `custom` resolves
/// `Action::Custom` rows to their configured name (e.g. "File: Ready to merge").
pub fn build_rows(
    continuations: &[(Key, crate::keymap::Action)],
    custom: &[crate::keymap::HostCustomAction],
) -> Vec<WhichKeyRow> {
    let mut rows: Vec<WhichKeyRow> = continuations
        .iter()
        .map(|(k, a)| {
            let label = action_label(a, custom);
            let group = group_of(a, &label);
            WhichKeyRow {
                key: k.clone(),
                chord: key_hint(k),
                label,
                group,
            }
        })
        .collect();
    // Stable: keeps insertion order within each group.
    rows.sort_by(|a, b| a.group.cmp(&b.group));
    rows
}

/// The which-key popup's filter/grep session; `None` on the popup means plain
/// reference mode. Mirrors the command palette's matcher over the current rows.
#[derive(Debug, Clone, Default)]
pub struct WhichKeyFilter {
    pub query: String,
    pub selected: usize,
    pub scroll_offset: usize,
    /// Indices into the popup rows, best-first (or `0..len` when the query is
    /// empty).
    pub matches: Vec<usize>,
}

impl WhichKeyFilter {
    pub fn new(rows: &[WhichKeyRow]) -> Self {
        let mut f = Self::default();
        f.recompute(rows);
        f
    }

    /// Re-rank `matches` for the current query. Empty query = identity list;
    /// otherwise fuzzy-rank over `"{chord} {label}"` so `p` matches the p-row and
    /// "play" matches by word.
    pub fn recompute(&mut self, rows: &[WhichKeyRow]) {
        if self.query.trim().is_empty() {
            self.matches = (0..rows.len()).collect();
        } else {
            let hay: Vec<String> = rows
                .iter()
                .map(|r| format!("{} {}", r.chord, r.label))
                .collect();
            let refs: Vec<&str> = hay.iter().map(String::as_str).collect();
            self.matches = crate::fff_backend::fuzzy_rank(&self.query, &refs)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.clamp_scroll();
    }

    pub fn push_char(&mut self, c: char, rows: &[WhichKeyRow]) {
        self.query.push(c);
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute(rows);
    }

    /// Delete one query char. Returns `false` when the query was already empty
    /// (the caller drops filter mode back to the plain popup).
    pub fn backspace(&mut self, rows: &[WhichKeyRow]) -> bool {
        if self.query.pop().is_none() {
            return false;
        }
        self.selected = 0;
        self.scroll_offset = 0;
        self.recompute(rows);
        true
    }

    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
            self.clamp_scroll();
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.clamp_scroll();
    }

    /// Keep `selected` inside the visible `[offset, offset + WK_MAX_ROWS)` window.
    fn clamp_scroll(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + WK_MAX_ROWS {
            self.scroll_offset = self.selected + 1 - WK_MAX_ROWS;
        }
    }

    /// The row index (into the popup rows) the selection points at.
    pub fn selected_row(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }
}

/// The outcome of feeding a key to the open which-key popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhichKeyKey {
    /// Not a popup key — the caller lets it fall through to normal dispatch, so
    /// the pending prefix's single-letter accelerators still run instantly.
    Ignore,
    /// The popup consumed the key (filter edit / navigation); just redraw.
    Redraw,
    /// Dismiss the popup and reset the pending prefix.
    Dismiss,
    /// Run the row at this index (its `Key` is re-fed through `keymap.dispatch`).
    Run(usize),
}

/// Feed a key to the open which-key popup. In plain mode only `/` (open filter)
/// and `Esc` (dismiss) are claimed — every other key is `Ignore`d so the pending
/// prefix's single-letter accelerators keep working. In filter mode, printable
/// chars edit the query, arrows move the selection, `Enter` runs the selection,
/// `Backspace` edits (empty → back to plain), `Esc` returns to plain.
pub fn handle_which_key_key(
    k: &termwiz::input::KeyEvent,
    rows: &[WhichKeyRow],
    filter: &mut Option<WhichKeyFilter>,
) -> WhichKeyKey {
    // SHIFT is fine (uppercase chars); CTRL/ALT/SUPER means it's a real chord.
    let plain_mods = (k.modifiers & !Modifiers::SHIFT) == Modifiers::NONE;
    match filter {
        None => match k.key {
            KeyCode::Char('/') if plain_mods => {
                *filter = Some(WhichKeyFilter::new(rows));
                WhichKeyKey::Redraw
            }
            key if crate::input::is_escape_key(&key) => WhichKeyKey::Dismiss,
            _ => WhichKeyKey::Ignore,
        },
        Some(f) => match k.key {
            KeyCode::Enter => match f.selected_row() {
                Some(idx) => WhichKeyKey::Run(idx),
                None => WhichKeyKey::Redraw,
            },
            KeyCode::UpArrow => {
                f.move_up();
                WhichKeyKey::Redraw
            }
            KeyCode::DownArrow => {
                f.move_down();
                WhichKeyKey::Redraw
            }
            KeyCode::Backspace => {
                if !f.backspace(rows) {
                    *filter = None;
                }
                WhichKeyKey::Redraw
            }
            key if crate::input::is_escape_key(&key) => {
                *filter = None;
                WhichKeyKey::Redraw
            }
            KeyCode::Char(c) if plain_mods => {
                f.push_char(c, rows);
                WhichKeyKey::Redraw
            }
            _ => WhichKeyKey::Ignore,
        },
    }
}

/// The which-key popup's render input: the human prefix, the candidate rows, and
/// the optional filter session (`None` = plain reference mode).
pub struct WhichKeyView<'a> {
    pub prefix: &'a str,
    pub rows: &'a [WhichKeyRow],
    pub filter: Option<&'a WhichKeyFilter>,
}

/// Draw the which-key popup: a bottom-anchored, bordered card styled with the
/// shared layer/seg toolkit (a sibling of the command palette). Plain mode lists
/// candidates grouped under category headers; filter mode shows a flat,
/// fuzzy-ranked, selectable list behind a query prompt.
pub fn render_which_key(surface: &mut Surface, screen: Rect, view: &WhichKeyView) {
    if view.rows.is_empty() {
        return;
    }
    const COLS: usize = 46;
    let panel = Tok::Slot(S::Panel);
    let rule = Line::Fill {
        ch: '╌',
        fg: Tok::Slot(S::Ghost3),
    };

    // Compose the body as (line, pad_bg) pairs so the box height matches the
    // content exactly and the selected filter row can carry its own tint.
    let mut body: Vec<(Line, Tok)> = Vec::new();
    let chip = |chord: &str| Seg::key(format!(" {chord} "));
    match view.filter {
        // ── Filter/grep mode: prompt + flat ranked, windowed, selectable list.
        Some(f) => {
            let mut prompt = vec![seg(Tok::Slot(S::Accent), "❯ ").bold()];
            if f.query.is_empty() {
                prompt.push(seg(Tok::Slot(S::Ghost3), "filter…"));
            } else {
                prompt.push(seg(Tok::Slot(S::Text), f.query.clone()));
            }
            body.push((Line::segs(prompt), panel));
            body.push((rule.clone(), panel));
            let shown = f.matches.len().min(WK_MAX_ROWS);
            for row in 0..shown {
                let Some(&ri) = f.matches.get(f.scroll_offset + row) else {
                    break;
                };
                let Some(hr) = view.rows.get(ri) else {
                    continue;
                };
                let selected = f.scroll_offset + row == f.selected;
                let pad = if selected { Tok::SelAccent } else { panel };
                let label = if selected {
                    seg(Tok::Slot(S::Text), hr.label.clone()).bold()
                } else {
                    seg(Tok::Slot(S::Dim), hr.label.clone())
                };
                body.push((Line::segs(vec![sp(1), chip(&hr.chord), sp(1), label]), pad));
            }
        }
        // ── Plain reference mode: rows grouped under category headers.
        None => {
            let mut last_group: Option<&str> = None;
            for hr in view.rows {
                if last_group != Some(hr.group.as_str()) {
                    body.push((
                        Line::segs(vec![seg(Tok::Slot(S::Ghost2), hr.group.clone()).bold()]),
                        panel,
                    ));
                    last_group = Some(hr.group.as_str());
                }
                body.push((
                    Line::segs(vec![
                        sp(1),
                        chip(&hr.chord),
                        sp(1),
                        seg(Tok::Slot(S::Dim), hr.label.clone()),
                    ]),
                    panel,
                ));
            }
        }
    }

    // Footer: rule + hint line (+ match count in filter mode).
    body.push((rule, panel));
    let footer = match view.filter {
        Some(f) => Line::split(
            vec![
                seg(Tok::Slot(S::Ghost2), "↑↓"),
                seg(Tok::Slot(S::Ghost), " move  "),
                seg(Tok::Slot(S::Ghost2), "↵"),
                seg(Tok::Slot(S::Ghost), " run  "),
                seg(Tok::Slot(S::Ghost2), "esc"),
                seg(Tok::Slot(S::Ghost), " back"),
            ],
            vec![seg(
                Tok::Slot(S::Ghost3),
                format!("{} matches", f.matches.len()),
            )],
        ),
        None => Line::segs(vec![
            seg(Tok::Slot(S::Ghost2), "/"),
            seg(Tok::Slot(S::Ghost), " filter  "),
            seg(Tok::Slot(S::Ghost2), "↵"),
            seg(Tok::Slot(S::Ghost), " run  "),
            seg(Tok::Slot(S::Ghost2), "esc"),
            seg(Tok::Slot(S::Ghost), " dismiss"),
        ]),
    };
    body.push((footer, panel));

    let filtering = view.filter.is_some();
    let spec = LayerSpec {
        title: format!("{}…", view.prefix),
        badge: Some(" keys ".into()),
        cols: COLS,
        rows: body.len(),
        anchor: Anchor::Bottom,
        dim: false,
        shadow: true,
        bg: panel,
        border: if filtering {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Faint)
        },
    };
    let Some(inner) = open_layer(surface, screen, &spec) else {
        return;
    };
    for (i, (line, pad)) in body.iter().enumerate() {
        if i >= inner.rows {
            break;
        }
        seg::draw_line(surface, inner.x, inner.y + i, inner.cols, line, *pad);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cheatsheet_groups_bucket_known_actions() {
        let cfg = superzej_core::config::Config::default();
        let groups = cheatsheet_groups(&cfg);
        assert!(!groups.is_empty(), "registry yields groups");
        let all: Vec<&HintRow> = groups.iter().flat_map(|g| &g.rows).collect();
        // A representative lifecycle action with its chord is present.
        assert!(
            all.iter().any(|r| r.label.contains("worktree")),
            "has a worktree row: {all:?}"
        );
        assert!(all.iter().all(|r| !r.chord.is_empty()));
    }

    #[test]
    fn key_hint_formats_modifiers_and_specials() {
        assert_eq!(key_hint(&Key::ctrl('x')), "Ctrl-x");
        assert_eq!(key_hint(&Key::char(' ')), "Space");
        assert_eq!(key_hint(&Key::from_code(KeyCode::Enter)), "↵");
        assert_eq!(
            key_hint(&Key::modified(KeyCode::Char('w'), Modifiers::ALT)),
            "Alt-w"
        );
    }

    #[test]
    fn build_rows_maps_chords_and_groups_by_category() {
        let cont = vec![
            (Key::char('m'), crate::keymap::Action::MediaPlayPause),
            (Key::char('n'), crate::keymap::Action::MediaNext),
            (Key::char('p'), crate::keymap::Action::TogglePanel),
        ];
        let rows = build_rows(&cont, &[]);
        assert_eq!(rows.len(), 3);
        // Media rows share a group and stay contiguous (stable sort).
        let media: Vec<&str> = rows
            .iter()
            .filter(|r| r.group == "Media")
            .map(|r| r.chord.as_str())
            .collect();
        assert_eq!(media, vec!["m", "n"]);
        assert!(rows.iter().any(|r| r.group == "Toggle" && r.chord == "p"));
    }

    #[test]
    fn build_rows_resolve_custom_and_group_by_label_prefix() {
        use crate::keymap::{Action, CompositeAction, HostCustomAction};
        let custom = vec![HostCustomAction::Composite {
            name: "File: Ready to merge".to_string(),
            action: CompositeAction::FileWorktree {
                folder: "Ready to merge".to_string(),
            },
        }];
        let rows = build_rows(&[(Key::char('r'), Action::Custom(0))], &custom);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "File: Ready to merge");
        assert_eq!(rows[0].group, "File", "label ':' prefix drives the group");
        assert_eq!(rows[0].key, Key::char('r'), "raw key kept for re-dispatch");
        // Out-of-range custom index falls back to the generic label + group.
        let rows = build_rows(&[(Key::char('x'), Action::Custom(9))], &custom);
        assert_eq!(rows[0].label, "custom action");
        assert_eq!(rows[0].group, "Custom");
    }

    fn media_rows() -> Vec<WhichKeyRow> {
        build_rows(
            &[
                (Key::char('m'), crate::keymap::Action::MediaPlayPause),
                (Key::char('n'), crate::keymap::Action::MediaNext),
            ],
            &[],
        )
    }

    #[test]
    fn filter_narrows_and_maps_back_to_rows() {
        let rows = media_rows();
        let mut f = WhichKeyFilter::new(&rows);
        assert_eq!(f.matches.len(), 2, "empty query = identity");
        for c in "play".chars() {
            f.push_char(c, &rows);
        }
        let idx = f.selected_row().expect("a match for 'play'");
        assert_eq!(rows[idx].label, "media play pause");
        // Backspacing to empty, then once more, signals exit to plain mode.
        for _ in 0..4 {
            assert!(f.backspace(&rows));
        }
        assert!(!f.backspace(&rows));
    }

    fn kev(key: KeyCode) -> termwiz::input::KeyEvent {
        termwiz::input::KeyEvent {
            key,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn plain_mode_ignores_letters_so_accelerators_survive() {
        let rows = media_rows();
        let mut filter = None;
        // A candidate letter must fall through (Ignore) so the chord still runs.
        assert_eq!(
            handle_which_key_key(&kev(KeyCode::Char('m')), &rows, &mut filter),
            WhichKeyKey::Ignore
        );
        assert!(filter.is_none());
        // Esc dismisses; `/` opens the filter.
        assert_eq!(
            handle_which_key_key(&kev(KeyCode::Escape), &rows, &mut filter),
            WhichKeyKey::Dismiss
        );
        assert_eq!(
            handle_which_key_key(&kev(KeyCode::Char('/')), &rows, &mut filter),
            WhichKeyKey::Redraw
        );
        assert!(filter.is_some());
    }

    #[test]
    fn filter_mode_edits_runs_and_exits() {
        let rows = media_rows();
        let mut filter = Some(WhichKeyFilter::new(&rows));
        // A char edits the query.
        assert_eq!(
            handle_which_key_key(&kev(KeyCode::Char('p')), &rows, &mut filter),
            WhichKeyKey::Redraw
        );
        // Enter runs the selected row.
        assert!(matches!(
            handle_which_key_key(&kev(KeyCode::Enter), &rows, &mut filter),
            WhichKeyKey::Run(_)
        ));
        // Esc drops back to plain mode (not a full dismiss).
        assert_eq!(
            handle_which_key_key(&kev(KeyCode::Escape), &rows, &mut filter),
            WhichKeyKey::Redraw
        );
        assert!(filter.is_none());
    }

    #[test]
    fn render_which_key_plain_shows_prefix_group_and_hints() {
        let rows = media_rows();
        let view = WhichKeyView {
            prefix: "Alt-m",
            rows: &rows,
            filter: None,
        };
        let mut s = Surface::new(80, 24);
        render_which_key(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
            &view,
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("Alt-m"), "prefix shown: {text:?}");
        assert!(text.contains("Media"), "group header shown");
        assert!(text.contains("media play pause"), "label shown");
        assert!(text.contains("keys"), "badge shown");
        assert!(text.contains("filter"), "footer hint shown");
    }

    #[test]
    fn render_which_key_filter_shows_query_and_count() {
        let rows = media_rows();
        let mut f = WhichKeyFilter::new(&rows);
        for c in "play".chars() {
            f.push_char(c, &rows);
        }
        let view = WhichKeyView {
            prefix: "Alt-m",
            rows: &rows,
            filter: Some(&f),
        };
        let mut s = Surface::new(80, 24);
        render_which_key(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
            &view,
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("play"), "query echoed: {text:?}");
        assert!(text.contains("matches"), "match count shown");
    }
}
