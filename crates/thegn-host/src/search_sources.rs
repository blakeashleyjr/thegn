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

/// "worktree · tab · program", skipping whichever parts are empty. Pure.
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

/// The live `(worktree group, tab)` that owns `pane_id`. A pane can briefly
/// outlive its tab during teardown, hence the `Option`.
fn owner(
    session: &crate::session::Session,
    pane_id: u32,
) -> Option<(&crate::session::WorktreeGroup, &crate::session::Tab)> {
    session.worktrees.iter().find_map(|group| {
        group
            .tabs
            .iter()
            .find(|tab| tab.center.pane_ids().contains(&pane_id))
            .map(|tab| (group, tab))
    })
}

/// Build the list of `(pane_id, label, &HistoryBuffer)` sources for the search
/// engine. The scope determines which live panes from `session` contribute;
/// every row is labelled with the worktree, tab and program it came from so a
/// result is never just a bare line (THE-2241).
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
        // A pane-scoped search can still be read a day later (the overlay
        // restores its query), so the row says where it came from too.
        SearchScope::Pane(id) => match owner(session, id) {
            Some((group, tab)) => push(id, Some(group.name.as_str()), &tab.title),
            // Mid-teardown: name the pane alone rather than invent a scope.
            None => push(id, None, ""),
        },
        SearchScope::Tab => {
            if let Some(tab) = session.active_tab() {
                let worktree = session.active_group().map(|g| g.name.as_str());
                for pane_id in tab.center.pane_ids() {
                    push(pane_id, worktree, &tab.title);
                }
            }
        }
        SearchScope::Worktree => {
            if let Some(group) = session.active_group() {
                for tab in &group.tabs {
                    for pane_id in tab.center.pane_ids() {
                        push(pane_id, Some(group.name.as_str()), &tab.title);
                    }
                }
            }
        }
        // A workspace search spans groups, so the worktree name is the
        // part that cannot be guessed: two tabs can share a title.
        SearchScope::Workspace => {
            for group in &session.worktrees {
                for tab in &group.tabs {
                    for pane_id in tab.center.pane_ids() {
                        push(pane_id, Some(group.name.as_str()), &tab.title);
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
    use crate::center::CenterTree;
    use crate::session::{GroupKind, Session, Tab, WorktreeGroup};

    fn fixture() -> (Session, Panes) {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = Panes::new(tx);
        panes.insert_test_pane(1);
        panes.insert_test_pane(2);
        panes.insert_test_pane(3);

        let mut group = WorktreeGroup::new("app/feat", GroupKind::Branch, "/wt/app");
        group.tabs = vec![tab("home", 1), tab("review", 2)];
        let mut other = WorktreeGroup::new("app/main", GroupKind::Home, "/wt/main");
        other.tabs = vec![tab("home", 3)];

        let mut session = Session::default();
        session.add_group(group);
        session.add_group(other);
        session.active = 0;
        (session, panes)
    }

    fn tab(title: &str, pane: u32) -> Tab {
        let mut tab = Tab::new(title);
        tab.center = CenterTree::Leaf(pane);
        tab.focused_pane = pane;
        tab
    }

    fn rows(scope: SearchScope, session: &Session, panes: &Panes) -> Vec<(u32, String)> {
        build_search_sources(scope, session, panes, 1)
            .into_iter()
            .map(|(id, label, _)| (id, label))
            .collect()
    }

    #[test]
    fn worktree_scope_names_each_tabs_worktree_tab_and_program() {
        let (session, mut panes) = fixture();
        // The second tab runs something other than a plain shell.
        if let Some(pane) = panes.table.get_mut(&2) {
            pane.feed(b"\x1b]0;claude\x07");
        }
        assert_eq!(
            rows(SearchScope::Worktree, &session, &panes),
            [
                (1, "app/feat · home · shell".to_string()),
                (2, "app/feat · review · claude".to_string()),
            ]
        );
    }

    #[test]
    fn workspace_scope_disambiguates_equal_tab_titles_by_worktree() {
        let (session, panes) = fixture();
        let labels = rows(SearchScope::Workspace, &session, &panes);
        let home: Vec<_> = labels
            .iter()
            .filter(|(_, label)| label.contains("· home ·"))
            .map(|(_, label)| label.to_string())
            .collect();
        assert_eq!(
            home,
            [
                "app/feat · home · shell".to_string(),
                "app/main · home · shell".to_string(),
            ],
            "two tabs share the title `home`; the worktree part must separate them"
        );
    }

    #[test]
    fn pane_and_tab_scopes_still_say_where_the_line_came_from() {
        let (session, panes) = fixture();
        assert_eq!(
            rows(SearchScope::Pane(2), &session, &panes),
            [(2, "app/feat · review · shell".to_string())]
        );
        assert_eq!(
            rows(SearchScope::Tab, &session, &panes),
            [(1, "app/feat · home · shell".to_string())]
        );
    }

    #[test]
    fn pane_scope_names_a_pane_outside_the_active_group() {
        let (session, panes) = fixture();
        // 3 lives in the non-active group: a pane-scoped row still names it.
        assert_eq!(
            rows(SearchScope::Pane(3), &session, &panes),
            [(3, "app/main · home · shell".to_string())]
        );
        // A pane that is no longer live contributes no source at all, so the
        // overlay never shows a row it cannot jump to.
        assert_eq!(rows(SearchScope::Pane(99), &session, &panes), []);
    }

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
