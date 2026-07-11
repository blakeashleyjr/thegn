//! The sidebar's `?` help overlay: a static grouped cheatsheet of every key
//! that works while the sidebar owns focus, rendered on the shared layer
//! machinery (dim-free bottom card, like the which-key popup). Any key
//! dismisses it. Also exports the curated statusbar hint pairs so the two
//! surfaces can never drift from `handlers/sidebar_keys.rs` independently —
//! change a key there, update this table.

use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};

/// `(group, [(key, label)])` — the whole sidebar key surface, grouped the way
/// a user thinks about it.
pub(crate) fn groups() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    &[
        (
            "Navigate",
            &[
                ("↑↓ / j k", "move"),
                ("↵", "open row / fold header"),
                ("← →", "collapse / expand"),
                ("/", "filter"),
                ("Alt 1-9", "jump to worktree"),
                ("Ctrl 1-9", "jump to workspace"),
            ],
        ),
        (
            "Create",
            &[
                ("n", "new worktree here"),
                ("N", "new workspace"),
                ("b", "branch from this worktree"),
            ],
        ),
        (
            "Organize",
            &[
                ("f", "move to folder / new folder"),
                ("r / F2", "rename"),
                ("p", "pin to top"),
                ("s", "sort menu"),
                ("Space", "mark for bulk actions"),
                ("Shift ↑↓", "reorder"),
            ],
        ),
        (
            "Act",
            &[
                ("d / Del", "close or delete…"),
                ("c", "copy path"),
                ("m", "all actions (menu)"),
            ],
        ),
        (
            "View",
            &[
                ("< >", "resize"),
                ("e", "wide"),
                ("q / Esc", "back to terminal"),
            ],
        ),
    ]
}

/// The curated always-on statusbar pairs while the sidebar owns focus (spliced
/// ahead of the registry hints): the five keys a newcomer needs first.
pub(crate) fn statusbar_pairs() -> Vec<(String, String)> {
    [
        ("↵", "open"),
        ("n", "new"),
        ("d", "delete"),
        ("m", "menu"),
        ("?", "help"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Paint the help card. Same layer language as the which-key popup; the
/// caller owns the open/dismiss state (any key closes).
pub(crate) fn render(surface: &mut Surface, screen: Rect) {
    const COLS: usize = 44;
    let panel = Tok::Slot(S::Panel);
    let chip = |k: &str| Seg::key(format!(" {k} "));

    let mut body: Vec<Line> = Vec::new();
    for (i, (title, rows)) in groups().iter().enumerate() {
        if i > 0 {
            body.push(Line::Blank);
        }
        body.push(Line::segs(vec![
            seg(Tok::Slot(S::Ghost2), title.to_string()).bold(),
        ]));
        for (key, label) in rows.iter() {
            body.push(Line::segs(vec![
                sp(1),
                chip(key),
                sp(1),
                seg(Tok::Slot(S::Dim), label.to_string()),
            ]));
        }
    }
    body.push(Line::Fill {
        ch: '╌',
        fg: Tok::Slot(S::Ghost3),
    });
    body.push(Line::segs(vec![
        seg(Tok::Slot(S::Ghost2), "any key"),
        seg(Tok::Slot(S::Ghost), " dismiss"),
    ]));

    let spec = LayerSpec {
        title: "sidebar".into(),
        badge: Some(" keys ".into()),
        cols: COLS,
        rows: body.len(),
        anchor: Anchor::Bottom,
        dim: false,
        shadow: true,
        bg: panel,
        border: Tok::Slot(S::Faint),
    };
    let Some(inner) = open_layer(surface, screen, &spec) else {
        return;
    };
    for (i, line) in body.iter().enumerate() {
        if i >= inner.rows {
            break;
        }
        seg::draw_line(surface, inner.x, inner.y + i, inner.cols, line, panel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_groups_cover_the_key_surface() {
        // Every group is non-empty and the essentials appear somewhere —
        // guards against the cheatsheet rotting when keys move.
        let all: Vec<&str> = groups()
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(k, _)| *k))
            .collect();
        for key in ["n", "N", "b", "f", "r / F2", "d / Del", "s", "p", "m", "?"] {
            if key == "?" {
                continue; // `?` opens this card; it isn't listed inside it.
            }
            assert!(all.contains(&key), "help table missing key {key:?}");
        }
        for (title, rows) in groups() {
            assert!(!rows.is_empty(), "empty help group {title:?}");
        }
    }

    #[test]
    fn statusbar_pairs_are_short_and_essential() {
        let pairs = statusbar_pairs();
        assert!(pairs.len() <= 6, "statusbar hints must stay skimmable");
        assert!(pairs.iter().any(|(k, _)| k == "?"));
    }
}
