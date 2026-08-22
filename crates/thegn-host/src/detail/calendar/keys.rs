//! The calendar popup's key and mouse handling.
//!
//! Almost everything is a pure [`CalState`] mutation returning
//! [`DetailOutcome::Pending`] — the in-place-drill technique the log viewer and
//! CI drill already use, applied to the whole navigation surface. Month
//! arithmetic is pure, so paging never waits on anything.
//!
//! The single exception is landing on a month whose events aren't cached: the
//! grid still repaints instantly, and a `FetchCalendar` action asks the loop to
//! fill in the markers and agenda when it can.

use termwiz::input::{KeyCode, Modifiers};
use thegn_core::calendar::CalNav;

use super::super::{DetailAction, DetailOutcome};
use super::layout::CalHit;
use super::{CalPane, CalState};

/// Ask the loop to fetch the visible month, unless it is already known or
/// already in flight.
///
/// The `pending` guard is what keeps a user paging quickly through months from
/// firing a burst of duplicate fetches for the same one.
fn ensure_loaded(st: &mut CalState) -> DetailOutcome {
    let month = st.cursor.visible_month();
    if !st.ui.has_sources || st.loaded.contains(&month) || st.pending == Some(month) {
        return DetailOutcome::Pending;
    }
    st.pending = Some(month);
    let Some((from, to)) = thegn_core::calendar::month_bounds(month.0, month.1) else {
        st.pending = None;
        return DetailOutcome::Pending;
    };
    DetailOutcome::Act(DetailAction::FetchCalendar {
        year: month.0,
        month: month.1,
        from,
        to,
    })
}

/// Apply a navigation step, then request the month's events if needed.
fn nav(st: &mut CalState, n: CalNav) -> DetailOutcome {
    let before = st.cursor.visible_month();
    st.cursor.apply(n, st.today);
    // Moving the day cursor resets the agenda cursor: the old row index means
    // nothing against a different day's event list.
    st.agenda_sel = 0;
    if st.cursor.visible_month() != before {
        return ensure_loaded(st);
    }
    DetailOutcome::Pending
}

/// Handle a key while the calendar popup is up.
pub(crate) fn handle_calendar_key(
    st: &mut CalState,
    key: &KeyCode,
    mods: Modifiers,
) -> DetailOutcome {
    if mods.contains(Modifiers::CTRL) {
        return match key {
            KeyCode::Char('c' | 'C' | 'g' | 'G') => DetailOutcome::Close,
            _ => DetailOutcome::Pending,
        };
    }
    // Alt/Super chords belong to the app, not the popup — but they must not
    // fall through to the keymap either, or `Alt-d` would reopen what it just
    // closed. Swallowed as Pending, exactly like the other detail popups.
    if mods.intersects(Modifiers::ALT | Modifiers::SUPER) {
        return DetailOutcome::Pending;
    }
    if crate::input::is_escape_key(key) {
        return DetailOutcome::Close;
    }
    let shift = mods.contains(Modifiers::SHIFT);
    match key {
        KeyCode::Char('q') => DetailOutcome::Close,

        // Day / week movement.
        KeyCode::Char('h') | KeyCode::LeftArrow => nav(st, CalNav::PrevDay),
        KeyCode::Char('l') | KeyCode::RightArrow => nav(st, CalNav::NextDay),
        KeyCode::Char('k') | KeyCode::UpArrow if st.pane == CalPane::Grid => {
            nav(st, CalNav::PrevWeek)
        }
        KeyCode::Char('j') | KeyCode::DownArrow if st.pane == CalPane::Grid => {
            nav(st, CalNav::NextWeek)
        }
        // In the agenda the same keys walk the event list.
        KeyCode::Char('k') | KeyCode::UpArrow => {
            st.agenda_sel = st.agenda_sel.saturating_sub(1);
            DetailOutcome::Pending
        }
        KeyCode::Char('j') | KeyCode::DownArrow => {
            let max = st.selected_events().len().saturating_sub(1);
            st.agenda_sel = (st.agenda_sel + 1).min(max);
            DetailOutcome::Pending
        }

        // Month / year paging.
        KeyCode::Char('[') => nav(st, CalNav::PrevMonth),
        KeyCode::Char(']') => nav(st, CalNav::NextMonth),
        KeyCode::Char('{') => nav(st, CalNav::PrevYear),
        KeyCode::Char('}') => nav(st, CalNav::NextYear),
        KeyCode::PageUp if shift => nav(st, CalNav::PrevYear),
        KeyCode::PageDown if shift => nav(st, CalNav::NextYear),
        KeyCode::PageUp => nav(st, CalNav::PrevMonth),
        KeyCode::PageDown => nav(st, CalNav::NextMonth),

        KeyCode::Char('g') => nav(st, CalNav::FirstOfMonth),
        KeyCode::Char('G') => nav(st, CalNav::LastOfMonth),
        KeyCode::Home => nav(st, CalNav::FirstOfMonth),
        KeyCode::End => nav(st, CalNav::LastOfMonth),
        KeyCode::Char('t') | KeyCode::Char('T') => nav(st, CalNav::Today),

        // termwiz has no BackTab: Shift-Tab arrives as Tab with SHIFT, and
        // the pane cycle is two-state anyway, so both directions do the same.
        KeyCode::Tab => {
            st.pane = other_pane(st);
            DetailOutcome::Pending
        }

        // Refresh the visible month, bypassing the loaded/pending guard.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            st.loaded.remove(&st.cursor.visible_month());
            st.pending = None;
            ensure_loaded(st)
        }

        KeyCode::Enter => match st.pane {
            // From the grid, Enter focuses the day's events rather than
            // closing — the popup is a place to look around in.
            CalPane::Grid if !st.selected_events().is_empty() => {
                st.pane = CalPane::Agenda;
                st.agenda_sel = 0;
                DetailOutcome::Pending
            }
            CalPane::Grid => DetailOutcome::Pending,
            CalPane::Agenda => open_selected(st),
        },

        _ => DetailOutcome::Pending,
    }
}

/// The pane Tab would move to. Only meaningful when there is an agenda with
/// something in it.
fn other_pane(st: &CalState) -> CalPane {
    match st.pane {
        CalPane::Grid if st.ui.show_agenda && !st.selected_events().is_empty() => CalPane::Agenda,
        CalPane::Grid => CalPane::Grid,
        CalPane::Agenda => CalPane::Grid,
    }
}

/// Open the focused event's URL, if it has one.
fn open_selected(st: &CalState) -> DetailOutcome {
    match st.selected_events().get(st.agenda_sel) {
        Some(e) if !e.url.trim().is_empty() => {
            DetailOutcome::Act(DetailAction::OpenUrl(e.url.clone()))
        }
        // Nothing to open is not a reason to close the popup.
        _ => DetailOutcome::Pending,
    }
}

/// Resolve a click inside the popup.
pub(crate) fn handle_calendar_click(st: &mut CalState, hit: CalHit) -> DetailOutcome {
    match hit {
        CalHit::PrevMonth => nav(st, CalNav::PrevMonth),
        CalHit::NextMonth => nav(st, CalNav::NextMonth),
        CalHit::Today => nav(st, CalNav::Today),
        CalHit::Day(d) => nav(st, CalNav::Goto(d)),
        CalHit::AgendaRow(i) => {
            st.pane = CalPane::Agenda;
            st.agenda_sel = i.min(st.selected_events().len().saturating_sub(1));
            DetailOutcome::Pending
        }
    }
}

/// Wheel over the calendar pages months — the gesture people expect from every
/// other date picker.
pub(crate) fn handle_calendar_wheel(st: &mut CalState, delta: i32) -> DetailOutcome {
    if delta < 0 {
        nav(st, CalNav::PrevMonth)
    } else if delta > 0 {
        nav(st, CalNav::NextMonth)
    } else {
        DetailOutcome::Pending
    }
}
