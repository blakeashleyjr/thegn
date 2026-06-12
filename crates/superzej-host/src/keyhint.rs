//! Keybinding hint data + the transient **which-key** popup shown after a
//! pending prefix. The full cheatsheet is the panel's Keys section (9), whose
//! Full view renders [`cheatsheet_groups`] — rows stay derived from the core
//! keymap registry (`superzej_core::keymap::effective`) so labels live in one
//! place.

use termwiz::cell::{AttributeChange, Intensity};
use termwiz::color::ColorAttribute;
use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::{Change, Position, Surface};

use crate::chrome::{draw_text, fill, theme_color};
use crate::compositor::Rect;
use crate::sequence::Key;
use superzej_core::theme;

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

/// A short human label for an action in the which-key popup.
fn action_label(action: &crate::keymap::Action) -> String {
    use crate::keymap::Action;
    match action {
        Action::SwitchMode(m) => format!("→ {} mode", m.as_str()),
        Action::Custom(_) => "custom action".to_string(),
        other => other.key().replace('-', " "),
    }
}

/// Which-key rows for the live continuations (next key → action label).
pub fn which_key_rows(continuations: &[(Key, crate::keymap::Action)]) -> Vec<HintRow> {
    continuations
        .iter()
        .map(|(k, a)| HintRow {
            chord: key_hint(k),
            label: action_label(a),
        })
        .collect()
}

/// Draw the bottom-anchored which-key popup listing next-key candidates.
pub fn render_which_key(
    surface: &mut Surface,
    screen: Rect,
    prefix: &str,
    rows: &[HintRow],
    accent: &str,
) {
    if rows.is_empty() {
        return;
    }
    let box_cols = (screen.cols / 2).clamp(20, 60).min(screen.cols);
    let box_rows = (rows.len() + 2).min(screen.rows);
    let x = screen.x + screen.cols.saturating_sub(box_cols);
    let y = screen.y + screen.rows.saturating_sub(box_rows + 1);
    let rect = Rect {
        x,
        y,
        cols: box_cols,
        rows: box_rows,
    };
    fill(surface, rect, theme_color(theme::PANEL2));

    let title = format!(" {prefix}…");
    draw_title(surface, x + 1, y, &title, accent, box_cols);

    for (i, row) in rows.iter().enumerate() {
        let r = y + 1 + i;
        if r >= y + box_rows {
            break;
        }
        draw_row(surface, x, r, row, box_cols);
    }
}

fn draw_title(surface: &mut Surface, x: usize, y: usize, text: &str, accent: &str, max: usize) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(theme_color(
        accent,
    ))));
    surface.add_change(Change::Attribute(AttributeChange::Background(theme_color(
        theme::PANEL,
    ))));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(
        Intensity::Bold,
    )));
    let clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    surface.add_change(Change::Text(clipped));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(
        Intensity::Normal,
    )));
}

/// A `chord   label` row: the chord left-aligned in an accent-ish dim color, the
/// label after a gutter.
fn draw_row(surface: &mut Surface, x: usize, y: usize, row: &HintRow, box_cols: usize) {
    const CHORD_W: usize = 14;
    draw_text(
        surface,
        x + 1,
        y,
        &row.chord,
        theme_color(theme::AMBER),
        ColorAttribute::Default,
        CHORD_W,
    );
    let label_x = x + 1 + CHORD_W + 1;
    let max = box_cols.saturating_sub(CHORD_W + 3);
    draw_text(
        surface,
        label_x,
        y,
        &row.label,
        theme_color(theme::TEXT),
        ColorAttribute::Default,
        max,
    );
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
    fn which_key_rows_map_continuations() {
        let cont = vec![
            (Key::char('p'), crate::keymap::Action::TogglePanel),
            (Key::char('s'), crate::keymap::Action::ToggleSidebar),
        ];
        let rows = which_key_rows(&cont);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].chord, "p");
        assert_eq!(rows[1].chord, "s");
    }

    #[test]
    fn render_which_key_lists_next_keys() {
        let rows = vec![
            HintRow {
                chord: "p".into(),
                label: "toggle panel".into(),
            },
            HintRow {
                chord: "s".into(),
                label: "toggle sidebar".into(),
            },
        ];
        let mut s = Surface::new(80, 24);
        render_which_key(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
            "Space",
            &rows,
            theme::TEAL,
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("Space"), "prefix shown: {text:?}");
        assert!(text.contains("toggle panel"));
    }
}
