//! Search source construction and exact jumps into live terminal scrollback.

use thegn_core::search::SearchScope;

use crate::panes::Panes;

const SEARCH_LABEL_MAX_CHARS: usize = 32;
const SEARCH_JUMP_MISSING: &str = "search: that line is no longer in this pane's scrollback (or came from a full-screen app); Alt r opens replay";

/// Sanitize one user- or pane-provided label component without doing any I/O.
fn clean_part(value: &str) -> String {
    let mut chars = value.chars().filter(|c| !c.is_control());
    let mut out = String::new();
    for _ in 0..SEARCH_LABEL_MAX_CHARS.saturating_sub(1) {
        let Some(c) = chars.next() else {
            return out;
        };
        out.push(c);
    }
    if chars.next().is_some() {
        out.push_str(crate::caps::active_glyphs().ellipsis);
    }
    out
}

/// "worktree · tab · program"; parts omitted by scope. Pure.
pub(crate) fn source_label(worktree: Option<&str>, tab_title: &str, pane_name: &str) -> String {
    let worktree = worktree.map(clean_part).filter(|s| !s.is_empty());
    let tab = clean_part(tab_title);
    let tab = if !tab.is_empty() && tab.chars().all(|c| c.is_ascii_digit()) {
        format!("tab {tab}")
    } else {
        tab
    };
    let pane = clean_part(pane_name);

    [
        worktree.as_deref().unwrap_or(""),
        tab.as_str(),
        pane.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" · ")
}

/// OSC title when set, else `Pane::program()`; control chars removed, cut to
/// 32 chars with the seg ellipsis glyph. This deliberately never reads `/proc`.
pub(crate) fn pane_display_name(title: Option<String>, program: &str) -> String {
    let title = title.as_deref().map(clean_part).filter(|s| !s.is_empty());
    title.unwrap_or_else(|| clean_part(program))
}

/// Build the list of `(pane_id, label, &HistoryBuffer)` sources for the search
/// engine. The scope determines which live panes from `session` contribute.
pub(crate) fn build_search_sources<'a>(
    scope: thegn_core::search::SearchScope,
    session: &crate::session::Session,
    panes: &'a Panes,
    _focused: u32,
) -> Vec<(u32, String, &'a thegn_core::history::HistoryBuffer)> {
    let mut out = Vec::new();
    let mut push = |pane_id: u32, worktree: Option<&str>, tab_title: &str| {
        if let Some(pane) = panes.table.get(&pane_id) {
            let pane_name = pane_display_name(pane.emulator().title(), pane.program());
            let label = source_label(worktree, tab_title, &pane_name);
            out.push((pane_id, label, &pane.history));
        }
    };

    match scope {
        SearchScope::Pane(id) => push(id, None, ""),
        SearchScope::Tab => {
            if let Some(tab) = session.active_tab() {
                for pane_id in tab.center.pane_ids() {
                    push(pane_id, None, &tab.title);
                }
            }
        }
        SearchScope::Worktree => {
            if let Some(group) = session.active_group() {
                for tab in &group.tabs {
                    for pane_id in tab.center.pane_ids() {
                        push(pane_id, None, &tab.title);
                    }
                }
            }
        }
        SearchScope::Workspace | SearchScope::Profile => {
            for group in &session.worktrees {
                let worktree = thegn_core::util::basename(&group.path);
                for tab in &group.tabs {
                    for pane_id in tab.center.pane_ids() {
                        push(pane_id, Some(worktree), &tab.title);
                    }
                }
            }
        }
    }
    out
}

/// What a search jump did: the line to select and whether it switched worktree group.
pub(crate) struct SearchJump {
    /// The found line as a selection in absolute lines (copymode::Selection).
    pub(crate) highlight: Option<(u32, crate::copymode::Selection)>,
    /// The jump changed the active worktree group.
    pub(crate) switched_group: bool,
}

/// Jump to the matched line: switch tab if needed, scroll the pane so the
/// matched line is visible, and focus the center terminal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_search_jump(
    m: thegn_core::search::SearchMatch,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    focus: &mut crate::focus::FocusState,
    model: &mut crate::chrome::FrameModel,
    sb: &mut crate::run::SidebarState,
    need_relayout: &mut bool,
) -> SearchJump {
    use crate::focus::Zone;

    let mut switched = false;
    let mut switched_group = false;
    'outer: for (gi, group) in session.worktrees.iter().enumerate() {
        for (ti, tab) in group.tabs.iter().enumerate() {
            if tab.center.pane_ids().contains(&m.pane_id) {
                if gi != session.active {
                    session.switch_to(gi);
                    switched = true;
                    switched_group = true;
                }
                if gi == session.active {
                    let group = &mut session.worktrees[gi];
                    if group.active_tab != ti {
                        group.active_tab = ti;
                        switched = true;
                    }
                }
                break 'outer;
            }
        }
    }
    if switched {
        crate::handlers::switch::refresh_tab_model(model, session, sb);
        *need_relayout = true;
    }

    focus.zone = Zone::Center;
    if let Some(tab) = session.active_tab_mut() {
        tab.focus_pane(m.pane_id);
    }

    let mut highlight = None;
    match panes.table.get_mut(&m.pane_id) {
        Some(pane) => {
            if let Some((offset, top, rows)) =
                pane.emulator().locate_line_span(&m.line, m.from_tail)
            {
                let (_, cols) = pane.emulator().size();
                highlight = Some((
                    m.pane_id,
                    crate::copymode::Selection {
                        anchor: (top, 0),
                        cursor: (top + rows as i32 - 1, cols.saturating_sub(1)),
                    },
                ));
                pane.scroll_down(usize::MAX / 2);
                pane.scroll_up(offset);
            } else {
                model.status = SEARCH_JUMP_MISSING.into();
            }
        }
        None => model.status = SEARCH_JUMP_MISSING.into(),
    }

    SearchJump {
        highlight,
        switched_group,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_label_by_scope_table() {
        assert_eq!(source_label(None, "", "bash"), "bash");
        assert_eq!(source_label(None, "dev", "bash"), "dev · bash");
        assert_eq!(
            source_label(Some("repo"), "dev", "bash"),
            "repo · dev · bash"
        );
    }

    #[test]
    fn pane_display_name_strips_controls_and_truncates() {
        let title = format!("title\u{1b}[31m{}", "x".repeat(40));
        let name = pane_display_name(Some(title), "bash");
        assert_eq!(
            name.chars().count(),
            31 + crate::caps::active_glyphs().ellipsis.chars().count()
        );
        assert!(!name.chars().any(char::is_control));
        assert!(name.ends_with(crate::caps::active_glyphs().ellipsis));
    }

    #[test]
    fn numeric_tab_title_reads_tab_n() {
        assert_eq!(source_label(None, "7", "zsh"), "tab 7 · zsh");
        assert_eq!(source_label(None, "main", "zsh"), "main · zsh");
    }
}
