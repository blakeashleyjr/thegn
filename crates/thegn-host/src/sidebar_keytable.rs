//! The sidebar's key table — **one** declaration of every key the sidebar
//! handles while it owns focus, feeding both dispatch and every hint surface.
//!
//! This is the same shape as [`crate::panel::gitui::context_keys`]: a chord, a
//! help label, and a dispatch discriminant are one datum, so a key cannot exist
//! without surfacing and a hint cannot drift from what the key does.
//!
//! Before this table the sidebar's keys were a bare `match` on [`KeyCode`] in
//! `handlers/sidebar_keys.rs`, invisible to the keymap registry, with their
//! hints hand-copied into three separately-edited places (the statusbar strip,
//! the sidebar's NAVIGATE footer, and `docs/help/sidebar.md`). `e` and `g` were
//! real working keys that appeared in none of them.
//!
//! Consumers:
//! - dispatch — [`resolve`], called from `handlers::sidebar_keys::handle_key`
//! - statusbar strip — [`hints`] at [`HintTier::Essential`], via `sidebar_help`
//! - NAVIGATE footer — [`footer_hints`], via `model.sidebar_hints`
//! - row context menu accelerator chips — [`chord_of`]
//! - `docs/help/sidebar.md` — enforced by the drift test below

use termwiz::input::{KeyCode, Modifiers};

/// How prominently a key is advertised. Ordered: a surface asking for `Common`
/// gets `Essential` rows too (see [`hints`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HintTier {
    /// The handful a newcomer needs first — fits the statusbar's one line.
    Essential,
    /// Worth advertising in the sidebar's taller footer column.
    Common,
    /// Documented and dispatchable, but not advertised in the chrome.
    Full,
}

/// A sidebar action. One variant per distinct behaviour; alias keys (`j`/`↓`,
/// `r`/`F2`, …) share a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarKeyId {
    Defocus,
    ReorderUp,
    ReorderDown,
    CursorDown,
    CursorUp,
    PageDown,
    PageUp,
    CursorHome,
    CursorEnd,
    Activate,
    Expand,
    Collapse,
    Filter,
    ToggleWide,
    ToggleFlat,
    CycleDetail,
    TogglePin,
    Mark,
    RowMenu,
    Delete,
    Rename,
    NewWorktree,
    NewWorkspace,
    Fork,
    Folder,
    CopyPath,
    SortMenu,
    WidthDec,
    WidthInc,
    Help,
}

impl SidebarKeyId {
    /// Whether this action reads or moves the cursor, and so must run against a
    /// row that is actually on screen. The wheel scrolls the viewport without
    /// moving the cursor, so `SidebarState::handle_key` re-anchors the cursor
    /// into the window before dispatching one of these — a relative move should
    /// start from where you are looking, and an action must never target an
    /// invisible row.
    ///
    /// The complement (filter, width, wide/flat toggles, help, sort menu, "new
    /// workspace") is viewport-independent and deliberately left alone.
    pub fn is_cursor_relative(self) -> bool {
        use SidebarKeyId as I;
        matches!(
            self,
            I::CursorDown
                | I::CursorUp
                | I::PageDown
                | I::PageUp
                | I::CursorHome
                | I::CursorEnd
                | I::Activate
                | I::Expand
                | I::Collapse
                | I::TogglePin
                | I::Mark
                | I::RowMenu
                | I::Delete
                | I::Rename
                | I::NewWorktree
                | I::Fork
                | I::Folder
                | I::CopyPath
                | I::ReorderUp
                | I::ReorderDown
        )
    }
}

/// One row of the table.
pub struct SidebarKey {
    pub id: SidebarKeyId,
    /// Every [`KeyCode`] that fires this action. Empty for entries whose
    /// trigger needs a modifier and so is matched ahead of the table
    /// ([`SidebarKeyId::ReorderUp`] / [`SidebarKeyId::ReorderDown`]).
    pub keys: &'static [KeyCode],
    /// Display chord for the hint surfaces.
    pub chord: &'static str,
    /// Short help label.
    pub label: &'static str,
    pub tier: HintTier,
}

const fn k(
    id: SidebarKeyId,
    keys: &'static [KeyCode],
    chord: &'static str,
    label: &'static str,
    tier: HintTier,
) -> SidebarKey {
    SidebarKey {
        id,
        keys,
        chord,
        label,
        tier,
    }
}

/// The full sidebar key surface. **Order is display order** for the hint
/// surfaces — the footer clips from the tail, so keys worth discovering sit
/// near the top.
pub const SIDEBAR_KEYS: &[SidebarKey] = &[
    k(
        SidebarKeyId::CursorDown,
        &[KeyCode::Char('j'), KeyCode::DownArrow],
        "j / k",
        "move up/down",
        HintTier::Common,
    ),
    // Page/Home/End: without table rows these fell through to the global
    // keymap and SCROLLED THE TERMINAL behind a focused sidebar.
    k(
        SidebarKeyId::PageUp,
        &[KeyCode::PageUp],
        "PgUp",
        "page up",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::PageDown,
        &[KeyCode::PageDown],
        "PgDn",
        "page down",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::CursorHome,
        &[KeyCode::Home],
        "Home",
        "first row",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::CursorEnd,
        &[KeyCode::End],
        "End",
        "last row",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Activate,
        &[KeyCode::Enter],
        "\u{21b5}",
        "open / fold",
        HintTier::Essential,
    ),
    k(
        SidebarKeyId::Filter,
        &[KeyCode::Char('/')],
        "/",
        "filter",
        HintTier::Common,
    ),
    k(
        SidebarKeyId::ToggleWide,
        &[KeyCode::Char('e')],
        "e",
        "wide",
        HintTier::Common,
    ),
    k(
        SidebarKeyId::ToggleFlat,
        &[KeyCode::Char('g')],
        "g",
        "flat / grouped",
        HintTier::Common,
    ),
    k(
        SidebarKeyId::CycleDetail,
        &[KeyCode::Char('i')],
        "i",
        "row detail",
        HintTier::Common,
    ),
    k(
        SidebarKeyId::NewWorktree,
        &[KeyCode::Char('n')],
        "n",
        "new",
        HintTier::Essential,
    ),
    k(
        SidebarKeyId::Delete,
        &[KeyCode::Char('d'), KeyCode::Delete],
        "d",
        // The chooser's safe default is CLOSE (delete is the explicit second
        // choice) — a bare "delete" label overstated the key.
        "close/delete",
        HintTier::Essential,
    ),
    k(
        SidebarKeyId::RowMenu,
        &[KeyCode::Char('m')],
        "m",
        "menu",
        HintTier::Essential,
    ),
    k(
        SidebarKeyId::SortMenu,
        &[KeyCode::Char('s')],
        "s",
        "sort",
        HintTier::Essential,
    ),
    k(
        SidebarKeyId::Help,
        &[KeyCode::Char('?')],
        "?",
        "all keys",
        HintTier::Essential,
    ),
    // ── Full tier: dispatchable + documented, not advertised in the chrome ──
    k(
        SidebarKeyId::CursorUp,
        &[KeyCode::Char('k'), KeyCode::UpArrow],
        "k / \u{2191}",
        "move up",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Expand,
        &[KeyCode::Char('l'), KeyCode::RightArrow],
        "l / \u{2192}",
        "expand",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Collapse,
        &[KeyCode::Char('h'), KeyCode::LeftArrow],
        "h / \u{2190}",
        "collapse",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::TogglePin,
        &[KeyCode::Char('p')],
        "p",
        "pin / unpin",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Mark,
        &[KeyCode::Char(' ')],
        "Space",
        "mark row",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Rename,
        &[KeyCode::Char('r'), KeyCode::Function(2)],
        "r / F2",
        "rename",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::NewWorkspace,
        &[KeyCode::Char('N')],
        "N",
        "new workspace",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Fork,
        &[KeyCode::Char('b')],
        "b",
        "branch from here",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Folder,
        &[KeyCode::Char('f')],
        "f",
        // On a workspace/folder row the same key CREATES a folder.
        "folder (move/new)",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::CopyPath,
        &[KeyCode::Char('c')],
        "c",
        "copy path",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::WidthDec,
        &[KeyCode::Char('<'), KeyCode::Char(',')],
        "<",
        "narrower",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::WidthInc,
        &[KeyCode::Char('>'), KeyCode::Char('.')],
        ">",
        "wider",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::Defocus,
        &[KeyCode::Char('q')],
        "q / Esc",
        "back to terminal",
        HintTier::Full,
    ),
    // Modifier-gated: matched ahead of the table in `resolve`.
    k(
        SidebarKeyId::ReorderUp,
        &[],
        "Shift+\u{2191}",
        "move item up",
        HintTier::Full,
    ),
    k(
        SidebarKeyId::ReorderDown,
        &[],
        "Shift+\u{2193}",
        "move item down",
        HintTier::Full,
    ),
];

/// Resolve a key press to its sidebar action. `None` means the sidebar does not
/// claim the key and the loop should fall through to global dispatch.
///
/// Escape and the Shift+arrow reorder pair are matched ahead of the table:
/// escape has its own multi-encoding helper, and the reorder arrows would
/// otherwise be shadowed by the plain cursor arrows.
pub fn resolve(key: &KeyCode, mods: Modifiers) -> Option<SidebarKeyId> {
    if crate::input::is_escape_key(key) {
        return Some(SidebarKeyId::Defocus);
    }
    if mods.contains(Modifiers::SHIFT) {
        match key {
            KeyCode::UpArrow => return Some(SidebarKeyId::ReorderUp),
            KeyCode::DownArrow => return Some(SidebarKeyId::ReorderDown),
            _ => {}
        }
    }
    SIDEBAR_KEYS
        .iter()
        .find(|e| e.keys.contains(key))
        .map(|e| e.id)
}

/// The display chord for an action — used by the row context menu's
/// accelerator chips so they can't drift from the table.
pub fn chord_of(id: SidebarKeyId) -> &'static str {
    SIDEBAR_KEYS
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.chord)
        .unwrap_or("")
}

/// `(chord, label)` pairs for every key at or below `tier`, in table order.
pub fn hints(tier: HintTier) -> Vec<(String, String)> {
    SIDEBAR_KEYS
        .iter()
        .filter(|e| e.tier <= tier)
        .map(|e| (e.chord.to_string(), e.label.to_string()))
        .collect()
}

/// The NAVIGATE footer's rows: the registry-derived worktree/workspace jump
/// chords (which follow user rebinds) followed by the table's advertised keys.
///
/// The jump rows go through [`crate::keymap::chord_hint_for`] — the same helper
/// the command palette uses — so rebinding `summon-worktree-1` updates the hint
/// instead of leaving a stale literal.
pub fn footer_hints(cfg: &thegn_core::config::Config) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (id, label) in [
        ("summon-worktree-1", "jump worktree"),
        ("summon-workspace-1", "jump workspace"),
    ] {
        if let Some(chord) = crate::keymap::chord_hint_for(cfg, id) {
            // "Alt-1" describes a family of nine bindings; render the range.
            out.push((digit_range(&chord), label.to_string()));
        }
    }
    out.extend(hints(HintTier::Common));
    out
}

/// Turn a concrete slot-1 chord into the family it belongs to: `"Alt-1"` →
/// `"Alt-1-9"`. A chord that doesn't end in `1` is left alone.
fn digit_range(chord: &str) -> String {
    match chord.strip_suffix('1') {
        Some(prefix) => format!("{prefix}1-9"),
        None => chord.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every id appears exactly once, so `chord_of` and the hint surfaces are
    /// unambiguous.
    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for e in SIDEBAR_KEYS {
            assert!(seen.insert(e.id), "duplicate table entry for {:?}", e.id);
        }
    }

    /// No two entries claim the same KeyCode — otherwise dispatch would depend
    /// on table order.
    #[test]
    fn keycodes_are_unique() {
        let mut seen = HashSet::new();
        for e in SIDEBAR_KEYS {
            for key in e.keys {
                assert!(
                    seen.insert(format!("{key:?}")),
                    "{key:?} claimed twice (second by {:?})",
                    e.id
                );
            }
        }
    }

    /// Every advertised key round-trips through dispatch: a hint can never
    /// point at a key that does nothing.
    #[test]
    fn every_table_key_resolves() {
        for e in SIDEBAR_KEYS {
            for key in e.keys {
                assert_eq!(
                    resolve(key, Modifiers::NONE),
                    Some(e.id),
                    "{key:?} should resolve to {:?}",
                    e.id
                );
            }
            assert!(!e.chord.is_empty(), "{:?} has no display chord", e.id);
            assert!(!e.label.is_empty(), "{:?} has no label", e.id);
        }
    }

    #[test]
    fn modifier_gated_keys_resolve() {
        assert_eq!(
            resolve(&KeyCode::UpArrow, Modifiers::SHIFT),
            Some(SidebarKeyId::ReorderUp)
        );
        assert_eq!(
            resolve(&KeyCode::DownArrow, Modifiers::SHIFT),
            Some(SidebarKeyId::ReorderDown)
        );
        // Unmodified, the same arrows move the cursor.
        assert_eq!(
            resolve(&KeyCode::UpArrow, Modifiers::NONE),
            Some(SidebarKeyId::CursorUp)
        );
        assert_eq!(
            resolve(&KeyCode::Escape, Modifiers::NONE),
            Some(SidebarKeyId::Defocus)
        );
    }

    #[test]
    fn unclaimed_keys_fall_through() {
        assert_eq!(resolve(&KeyCode::Char('Z'), Modifiers::NONE), None);
        assert_eq!(resolve(&KeyCode::Function(9), Modifiers::NONE), None);
    }

    /// The statusbar strip has one line — keep it skimmable.
    #[test]
    fn essential_tier_stays_short() {
        let essential = hints(HintTier::Essential);
        assert!(
            essential.len() <= 6,
            "statusbar hints must stay skimmable, got {}",
            essential.len()
        );
        assert!(essential.iter().any(|(c, _)| c == "?"));
    }

    /// Tiers are cumulative, and the keys this table was introduced to surface
    /// are actually advertised in the footer.
    #[test]
    fn common_tier_includes_the_view_toggles() {
        let common = hints(HintTier::Common);
        for chord in ["e", "g", "i"] {
            assert!(
                common.iter().any(|(c, _)| c == chord),
                "`{chord}` must reach the sidebar footer: {common:?}"
            );
        }
        // Cumulative: Essential rows come along.
        assert!(common.iter().any(|(c, _)| c == "?"));
        assert!(hints(HintTier::Full).len() > common.len());
    }

    /// Every character key the sidebar handles must be documented on the help
    /// page that claims `zone:sidebar`. This generalises the hand-listed
    /// assertion that used to live in `sidebar_help.rs` — the gate that should
    /// have caught `e` and `g` never being surfaced.
    #[test]
    fn help_page_documents_every_character_key() {
        let page = include_str!("../../../docs/help/sidebar.md");
        for e in SIDEBAR_KEYS {
            for key in e.keys {
                let token = match key {
                    KeyCode::Char(' ') => "`Space`".to_string(),
                    KeyCode::Char(c) => format!("`{c}`"),
                    KeyCode::Function(n) => format!("`F{n}`"),
                    // Arrows / Enter / Delete are structural; the page covers
                    // them prosaically.
                    _ => continue,
                };
                assert!(
                    page.contains(&token),
                    "docs/help/sidebar.md is missing {token} (for {:?})",
                    e.id
                );
            }
        }
    }

    #[test]
    fn chord_of_reads_the_table() {
        assert_eq!(chord_of(SidebarKeyId::ToggleFlat), "g");
        assert_eq!(chord_of(SidebarKeyId::CycleDetail), "i");
        assert_eq!(chord_of(SidebarKeyId::Activate), "\u{21b5}");
    }

    #[test]
    fn digit_range_expands_a_slot_one_chord() {
        assert_eq!(digit_range("Alt-1"), "Alt-1-9");
        assert_eq!(digit_range("Ctrl-1"), "Ctrl-1-9");
        // Not a slot chord — left alone rather than mangled.
        assert_eq!(digit_range("Alt-w"), "Alt-w");
    }

    /// The footer's jump rows track a rebind instead of showing a stale literal.
    #[test]
    fn footer_jump_hints_follow_rebinds() {
        let mut cfg = thegn_core::config::Config::default();
        let default = footer_hints(&cfg);
        assert!(
            default.iter().any(|(_, l)| l == "jump worktree"),
            "{default:?}"
        );

        cfg.keybinds
            .insert("summon-worktree-1".to_string(), "Super 1".to_string());
        let rebound = footer_hints(&cfg);
        let chord = rebound
            .iter()
            .find(|(_, l)| l == "jump worktree")
            .map(|(c, _)| c.clone())
            .expect("jump worktree row");
        assert_eq!(chord, "Super-1-9", "hint must follow the rebind");
    }
}
