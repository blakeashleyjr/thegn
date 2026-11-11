use super::*;
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::calendar::EventTime;

#[test]
fn transp_free_event_title_renders_dim_in_agenda() {
    use crate::chrome::S;
    use crate::detail::{Cell, Section};
    use crate::seg::Tok;

    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let mut free = CalEvent::new(
        "free",
        "Free event",
        EventTime::Date { date },
        EventTime::Date {
            date: NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
        },
    );
    free.busy = thegn_core::calendar::Busy::Free;
    let busy = CalEvent::new(
        "busy",
        "Busy event",
        EventTime::Date { date },
        EventTime::Date {
            date: NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
        },
    );
    let st = status_state(
        BTreeMap::from([(date, vec![Arc::new(free), Arc::new(busy)])]),
        true,
        None,
    );
    let sections = render::sections_of(&st);
    let Some(Section::Table(table)) = sections.get(3) else {
        panic!("agenda table missing")
    };
    let tone_for = |title: &str| {
        table.rows.iter().find_map(|row| match row.get(1) {
            Some(Cell::Text(label, tone)) if label == title => Some(*tone),
            _ => None,
        })
    };
    assert_eq!(tone_for("Free event"), Some(Tok::Slot(S::Dim)));
    assert_eq!(tone_for("Busy event"), Some(Tok::Slot(S::Text)));
}

#[test]
fn hostile_calendar_provider_values_stay_inside_popup_and_raw_values_survive() {
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let raw = format!(
        "start\r\nATTACK\t\x1b\x07\u{202e}👩‍💻界e\u{301}{}",
        "x".repeat(8000)
    );
    for provider in ["ics", "ics_url", "caldav", "command"] {
        // Three providers decode ICS; the command provider decodes CalEvent
        // JSON. Transport/network admission is separately tested in svc.
        let mut event = if provider == "command" {
            let event = CalEvent::new(
                "fixture",
                &raw,
                EventTime::Date { date },
                EventTime::Date { date },
            );
            serde_json::from_value::<CalEvent>(serde_json::to_value(event).unwrap()).unwrap()
        } else {
            let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:fixture\r\nDTSTART;VALUE=DATE:20260913\r\nDTEND;VALUE=DATE:20260914\r\nSUMMARY:start\\nATTACK\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
            let mut event = thegn_core::calendar::parse_ics(body, "UTC").remove(0);
            assert!(event.title.contains('\n'));
            event.title.push_str(&raw);
            event
        };
        event.calendar = raw.clone();
        event.location = raw.clone();
        event.organizer = raw.clone();
        event.category = raw.clone();
        event.url = raw.clone();
        let original = event.clone();
        let st = CalState {
            cursor: CalCursor::new(date),
            today: date,
            pane: CalPane::Agenda,
            agenda_sel: 0,
            events: BTreeMap::from([(date, vec![Arc::new(event)])]),
            loaded: BTreeSet::from([(2026, 9)]),
            errors: BTreeMap::new(),
            pending: None,
            clocks: vec![ResolvedClock {
                label: raw.clone(),
                zone: Tz::UTC,
                format: ClockFormat::H24,
                show_date: false,
                is_home: true,
            }],
            now: date.and_hms_opt(12, 0, 0).unwrap().and_utc(),
            home: Tz::UTC,
            ui: CalUiCfg {
                has_sources: true,
                ..Default::default()
            },
            weather: None,
            wx: WxUiCfg::default(),
        };
        let detail = CalendarDetail { st };
        for inner in [
            Rect {
                x: 9,
                y: 4,
                cols: 44,
                rows: 16,
            },
            Rect {
                x: 11,
                y: 8,
                cols: 7,
                rows: 4,
            },
        ] {
            for scroll in [0, 12, 24] {
                let mut surface = Surface::new(100, 40);
                render::render_calendar(&mut surface, inner, scroll, &detail);
                for (y, row) in surface.screen_cells().iter().enumerate() {
                    for (x, cell) in row.iter().enumerate() {
                        if x < inner.x
                            || x >= inner.x + inner.cols
                            || y < inner.y
                            || y >= inner.y + inner.rows
                        {
                            assert!(
                                cell.str().trim().is_empty(),
                                "{provider}: outside ({x},{y}) = {:?}",
                                cell.str()
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(&*detail.st.events[&date][0], &original);
    }
}

#[test]
fn calendar_width_uses_the_same_sanitized_clock_label_as_drawing() {
    let mut docs = CalendarDocs::default();
    docs.clocks.push(ResolvedClock {
        label: format!("\r\n{}", "界".repeat(10000)),
        zone: Tz::UTC,
        format: ClockFormat::H24,
        show_date: false,
        is_home: false,
    });
    assert_eq!(preferred_cols(&docs, 0), 94);
}

#[test]
fn calendar_width_and_clock_readings_share_whitespace_fallback() {
    let mut docs = CalendarDocs::default();
    docs.clocks.push(ResolvedClock {
        label: "\n\r ".into(),
        zone: "America/Argentina/ComodRivadavia".parse().unwrap(),
        format: ClockFormat::H24,
        show_date: false,
        is_home: false,
    });
    let readings = thegn_core::calendar::read_clocks(&docs.clocks, chrono::Utc::now(), Tz::UTC);
    let label = thegn_core::calendar::display::DisplayText::new(
        &readings[0].label,
        thegn_core::calendar::display::Field::ClockLabel,
    );
    assert_eq!(preferred_cols(&docs, 0), (label.cells() + 30).max(44));
}

#[test]
fn calendar_display_sites_keep_the_safe_projection() {
    // Narrow cross-surface ratchet: raw values stay in domain state, while all
    // currently reachable untrusted calendar display sites name the policy.
    let render = include_str!("render.rs");
    for raw_site in [
        "e.title.clone()",
        "e.calendar.clone()",
        "r.label.clone()",
        "r.abbrev.clone()",
    ] {
        assert!(
            !render.contains(raw_site),
            "raw calendar draw site: {raw_site}"
        );
    }
    for field in ["Field::Title", "Field::Calendar", "Field::ClockLabel"] {
        assert!(render.contains(field));
    }
    let reminder = include_str!("../../handlers/calendar.rs");
    for field in [
        "Field::Title",
        "Field::Location",
        "Field::Url",
        "Field::Reminder",
    ] {
        assert!(reminder.contains(field));
    }
    let layout = include_str!("mod.rs");
    assert!(layout.contains("Field::ClockLabel"));
}

#[test]
fn world_clock_rows_honor_mixed_formats_dates_and_safe_narrow_output() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
    let mut st = status_state(BTreeMap::new(), true, None);
    st.ui.show_agenda = false;
    st.ui.has_sources = false;
    st.now = date.and_hms_opt(22, 0, 0).unwrap().and_utc();
    st.home = Tz::UTC;
    st.clocks = vec![
        ResolvedClock {
            label: "home".into(),
            zone: Tz::UTC,
            format: ClockFormat::H24,
            show_date: false,
            is_home: true,
        },
        ResolvedClock {
            label: "tokyo".into(),
            zone: Tz::Asia__Tokyo,
            format: ClockFormat::Custom("%I:%M %p".into()),
            show_date: true,
            is_home: false,
        },
        ResolvedClock {
            label: "new york".into(),
            zone: Tz::America__New_York,
            format: ClockFormat::H24,
            show_date: true,
            is_home: false,
        },
        ResolvedClock {
            label: "kathmandu".into(),
            zone: Tz::Asia__Kathmandu,
            format: ClockFormat::Custom("%H:%M%n%t".into()),
            show_date: false,
            is_home: false,
        },
    ];
    let detail = CalendarDetail { st };
    let inner = Rect {
        x: 2,
        y: 1,
        cols: 52,
        rows: 20,
    };
    let mut surface = Surface::new(80, 30);
    render::render_calendar(&mut surface, inner, 0, &detail);
    let painted_rows: Vec<String> = surface
        .screen_cells()
        .iter()
        .map(|row| row.iter().map(|c| c.str().to_string()).collect::<String>())
        .collect();
    let painted: String = painted_rows.join("\n");
    assert!(painted.contains("22:00"), "{painted}");
    // 2026-08-21 22:00 UTC is 2026-08-22 07:00 in Tokyo (UTC+9), so this row's
    // `%I:%M %p` override renders AM. It also proves the per-row custom format
    // beat the global 24-hour setting that the `home` row above still uses.
    assert!(painted.contains("07:00 AM"), "{painted}");
    assert!(painted.contains("2026-08-22"), "{painted}");
    assert!(
        painted.contains("2026-08-21"),
        "same-day show_date: {painted}"
    );
    assert!(
        painted.contains("+1d"),
        "hidden dates retain the delta: {painted}"
    );
    // The `kathmandu` row's format is `%H:%M%n%t`, whose `%n`/`%t` expand to a
    // newline and a tab. They must not survive into the surface. Check the
    // rows individually: `painted` is joined with '\n', so asserting on it
    // would always trip on the separator rather than on rendered content.
    for (y, row) in painted_rows.iter().enumerate() {
        assert!(
            !row.chars().any(char::is_control),
            "control char in row {y}: {row:?}"
        );
    }
    for row in surface.screen_cells() {
        for (x, cell) in row.iter().enumerate() {
            if x < inner.x || x >= inner.x + inner.cols {
                assert!(
                    cell.str().trim().is_empty(),
                    "outside x={x}: {:?}",
                    cell.str()
                );
            }
        }
    }

    // A genuinely narrow popup may clip columns, but it must still keep all
    // painted cells inside the requested rectangle and free of controls.
    let narrow = Rect {
        x: 3,
        y: 2,
        cols: 20,
        rows: 10,
    };
    let mut narrow_surface = Surface::new(40, 20);
    render::render_calendar(&mut narrow_surface, narrow, 0, &detail);
    for row in narrow_surface.screen_cells() {
        for (x, cell) in row.iter().enumerate() {
            assert!(!cell.str().chars().any(char::is_control));
            if x < narrow.x || x >= narrow.x + narrow.cols {
                assert!(
                    cell.str().trim().is_empty(),
                    "outside x={x}: {:?}",
                    cell.str()
                );
            }
        }
    }
}

fn status_state(
    events: BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>,
    loaded: bool,
    error: Option<crate::calendar_docs::CalendarViewError>,
) -> CalState {
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    CalState {
        cursor: CalCursor::new(date),
        today: date,
        pane: CalPane::Grid,
        agenda_sel: 0,
        events,
        loaded: if loaded {
            BTreeSet::from([(2026, 9)])
        } else {
            BTreeSet::new()
        },
        errors: error.map(|e| ((2026, 9), e)).into_iter().collect(),
        pending: None,
        clocks: Vec::new(),
        now: date.and_hms_opt(12, 0, 0).unwrap().and_utc(),
        home: Tz::UTC,
        ui: CalUiCfg {
            has_sources: true,
            ..Default::default()
        },
        weather: None,
        wx: WxUiCfg::default(),
    }
}

fn month_status_of(st: &CalState) -> Option<String> {
    let sections = render::sections_of(st);
    let Some(crate::detail::Section::MonthGrid(grid)) = sections.first() else {
        panic!("the month grid is always the first section")
    };
    grid.status.clone()
}

fn agenda_note_of(st: &CalState) -> String {
    let sections = render::sections_of(st);
    let Some(crate::detail::Section::Heading { note, .. }) = sections.get(2) else {
        panic!("calendar agenda heading missing")
    };
    note.clone().expect("agenda heading carries a note")
}

#[test]
fn calendar_expansion_failure_is_visible_as_unavailable() {
    use crate::calendar_docs::{CacheSkips, CalendarViewError};
    use thegn_core::calendar::{ExpansionError, ExpansionLimit};
    let failed =
        CalendarViewError::Expansion(ExpansionError::Budget(ExpansionLimit::BucketEntries));
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let event = Arc::new(CalEvent::new(
        "e",
        "E",
        EventTime::Date { date },
        EventTime::Date {
            date: date.succ_opt().unwrap(),
        },
    ));

    // A complete empty month is the ONLY state that says "no events".
    assert_eq!(
        agenda_note_of(&status_state(BTreeMap::new(), true, None)),
        "no events"
    );
    // A first-load failure is unavailable, never a successful empty month.
    assert_eq!(
        agenda_note_of(&status_state(BTreeMap::new(), false, Some(failed))),
        "unavailable"
    );
    // A failed refresh keeps the last good events and says they are stale.
    let kept = BTreeMap::from([(date, vec![Arc::clone(&event)])]);
    let note = agenda_note_of(&status_state(kept.clone(), true, Some(failed)));
    assert!(
        note.starts_with("1 event ") && note.ends_with(" stale"),
        "{note}"
    );
    // Unreadable rows: the readable events show, marked incomplete.
    let note = agenda_note_of(&status_state(
        kept,
        true,
        Some(CalendarViewError::MalformedCache(CacheSkips::default())),
    ));
    assert!(note.ends_with(" incomplete"), "{note}");
}

#[test]
fn a_failed_month_is_visible_without_the_agenda() {
    // `show_agenda = false` never builds the agenda note, so the grid header
    // has to carry the state — or a first-load failure paints as an ordinary
    // empty month and a stale one paints as current.
    use crate::calendar_docs::{CacheSkips, CalendarViewError};
    use thegn_core::calendar::{ExpansionError, ExpansionLimit};
    let failed =
        CalendarViewError::Expansion(ExpansionError::Budget(ExpansionLimit::RetainedBytes));
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let events = BTreeMap::from([(
        date,
        vec![Arc::new(CalEvent::new(
            "e",
            "E",
            EventTime::Date { date },
            EventTime::Date {
                date: date.succ_opt().unwrap(),
            },
        ))],
    )]);
    let hide_agenda = |mut st: CalState| {
        st.ui.show_agenda = false;
        st
    };

    let healthy = hide_agenda(status_state(events.clone(), true, None));
    assert_eq!(month_status_of(&healthy), None, "a good month says nothing");
    assert_eq!(
        month_status_of(&hide_agenda(status_state(
            BTreeMap::new(),
            false,
            Some(failed)
        )))
        .as_deref(),
        Some("unavailable")
    );
    assert_eq!(
        month_status_of(&hide_agenda(status_state(
            events.clone(),
            true,
            Some(failed)
        )))
        .as_deref(),
        Some("stale")
    );
    assert_eq!(
        month_status_of(&hide_agenda(status_state(
            events,
            true,
            Some(CalendarViewError::MalformedCache(CacheSkips::default()))
        )))
        .as_deref(),
        Some("incomplete")
    );
    // And it reaches the surface: the header draws the status where the today
    // chip would be.
    let mut surface = Surface::new(100, 40);
    let detail = CalendarDetail {
        st: hide_agenda(status_state(BTreeMap::new(), false, Some(failed))),
    };
    let inner = Rect {
        x: 2,
        y: 1,
        cols: 44,
        rows: 20,
    };
    render::render_calendar(&mut surface, inner, 0, &detail);
    let painted: String = surface
        .screen_cells()
        .iter()
        .map(|row| row.iter().map(|c| c.str().to_string()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(painted.contains("unavailable"), "{painted}");
}

fn busy_day(n: usize, rows: usize) -> CalState {
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let events: Vec<Arc<CalEvent>> = (0..n)
        .map(|i| {
            let mut ev = CalEvent::new(
                format!("id{i}"),
                format!("Event {i}"),
                EventTime::Date { date },
                EventTime::Date {
                    date: NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
                },
            );
            ev.url = format!("https://example.invalid/{i}");
            Arc::new(ev)
        })
        .collect();
    let mut st = status_state(BTreeMap::from([(date, events)]), true, None);
    st.ui.agenda_rows = rows;
    st.pane = CalPane::Agenda;
    st
}

fn agenda_rows_of(st: &CalState) -> Vec<(String, crate::seg::Tok)> {
    use crate::detail::{Cell, Section};
    let sections = render::sections_of(st);
    let Some(Section::Table(table)) = sections.get(3) else {
        panic!("agenda table missing")
    };
    table
        .rows
        .iter()
        .filter_map(|row| match row.get(1) {
            Some(Cell::Text(label, tone)) => Some((label.clone(), *tone)),
            _ => None,
        })
        .collect()
}

fn agenda_titles(st: &CalState) -> Vec<String> {
    agenda_rows_of(st).into_iter().map(|(t, _)| t).collect()
}

fn focused_title(st: &CalState) -> Option<String> {
    agenda_rows_of(st)
        .into_iter()
        .find(|(_, tone)| *tone == crate::seg::Tok::SelAccent)
        .map(|(t, _)| t)
}

#[test]
fn agenda_window_follows_the_selection_past_the_row_cap() {
    let mut st = busy_day(8, 3);
    for (sel, want) in [(0, 0..3), (2, 0..3), (3, 1..4), (7, 5..8)] {
        st.agenda_sel = sel;
        assert_eq!(render::agenda_window(&st), want, "sel {sel}");
    }
    st.pane = CalPane::Grid;
    assert_eq!(render::agenda_window(&st), 0..3);
    assert_eq!(render::agenda_window(&busy_day(0, 3)), 0..0);
    // agenda_rows of 0 is treated as 1.
    let mut one = busy_day(4, 0);
    one.agenda_sel = 2;
    assert_eq!(render::agenda_window(&one), 2..3);
}

#[test]
fn agenda_table_draws_the_focused_event_past_the_cap() {
    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    assert_eq!(agenda_titles(&st), vec!["Event 3", "Event 4", "Event 5"]);
    assert_eq!(focused_title(&st), Some("Event 5".to_string()));
}

#[test]
fn down_key_walks_past_the_cap_with_a_visible_focus() {
    use termwiz::input::{KeyCode, Modifiers};
    let mut st = busy_day(8, 3);
    for _ in 0..5 {
        keys::handle_calendar_key(&mut st, &KeyCode::DownArrow, Modifiers::NONE);
        assert_eq!(
            focused_title(&st),
            Some(format!("Event {}", st.agenda_sel)),
            "focus must stay visible at sel {}",
            st.agenda_sel
        );
    }
}

#[test]
fn enter_never_opens_an_event_that_is_not_drawn() {
    use crate::detail::{DetailAction, DetailOutcome};
    use termwiz::input::{KeyCode, Modifiers};
    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    assert!(agenda_titles(&st).contains(&"Event 5".to_string()));
    match keys::handle_calendar_key(&mut st, &KeyCode::Enter, Modifiers::NONE) {
        DetailOutcome::Act(DetailAction::OpenUrl(u)) => {
            assert_eq!(u, "https://example.invalid/5");
        }
        _ => panic!("Enter on a drawn, focused row must open its URL"),
    }
    st.agenda_sel = 20;
    assert!(matches!(
        keys::handle_calendar_key(&mut st, &KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Pending
    ));
}

#[test]
fn agenda_click_maps_through_the_window() {
    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    let inner = Rect {
        x: 0,
        y: 0,
        cols: 100,
        rows: 40,
    };
    let mut y = inner.y;
    let mut agenda_y = None;
    for (i, sec) in render::sections_of(&st).iter().enumerate() {
        if i == 3 {
            agenda_y = Some(y);
        }
        y += sec.height();
    }
    let agenda_y = agenda_y.expect("agenda table located");
    assert_eq!(
        hit(inner, 0, &st, inner.x + 2, agenda_y),
        Some(layout::CalHit::AgendaRow(3))
    );
    assert_eq!(
        hit(inner, 0, &st, inner.x + 2, agenda_y + 2),
        Some(layout::CalHit::AgendaRow(5))
    );
}

fn agenda_day(st: &CalState) -> NaiveDate {
    st.cursor.selected()
}

#[test]
fn agenda_identity_follows_the_event_when_hydration_reorders_the_day() {
    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    let key = super::agenda_key(&st);
    let date = agenda_day(&st);
    st.events.get_mut(&date).unwrap().reverse();
    super::reselect_agenda(&mut st, key.as_ref());
    assert_eq!(st.agenda_sel, 2);
    assert_eq!(focused_title(&st), Some("Event 5".into()));
}

#[test]
fn agenda_identity_clamps_when_the_selected_event_is_deleted() {
    use crate::detail::{DetailAction, DetailOutcome};
    use termwiz::input::{KeyCode, Modifiers};

    let mut st = busy_day(8, 3);
    st.agenda_sel = 7;
    let key = super::agenda_key(&st);
    let date = agenda_day(&st);
    st.events.get_mut(&date).unwrap().pop();
    super::reselect_agenda(&mut st, key.as_ref());
    assert_eq!(st.agenda_sel, 6);
    assert_eq!(focused_title(&st), Some("Event 6".into()));

    st.agenda_sel = 6;
    st.events.get_mut(&date).unwrap().truncate(2);
    super::reselect_agenda(&mut st, key.as_ref());
    assert_eq!(st.agenda_sel, 1);
    match keys::handle_calendar_key(&mut st, &KeyCode::Enter, Modifiers::NONE) {
        DetailOutcome::Act(DetailAction::OpenUrl(u)) => {
            assert_eq!(u, "https://example.invalid/1");
        }
        _ => panic!("Enter must open the surviving selected event"),
    }
}

#[test]
fn agenda_identity_ignores_the_key_when_the_grid_is_focused() {
    let mut st = busy_day(8, 3);
    st.pane = CalPane::Grid;
    st.agenda_sel = 5;
    assert_eq!(super::agenda_key(&st), None);
    let date = agenda_day(&st);
    st.events.get_mut(&date).unwrap().truncate(3);
    super::reselect_agenda(&mut st, None);
    assert_eq!(st.agenda_sel, 2);
}

#[test]
fn agenda_identity_distinguishes_recurrence_overrides() {
    let date = NaiveDate::from_ymd_opt(2026, 9, 13).unwrap();
    let make = |recurrence_id: Option<EventTime>| {
        let mut ev = CalEvent::new(
            "series",
            "Series",
            EventTime::Date { date },
            EventTime::Date {
                date: NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
            },
        );
        ev.recurrence_id = recurrence_id;
        Arc::new(ev)
    };
    let master = make(None);
    let overridden = make(Some(EventTime::Date { date }));
    let mut st = status_state(
        BTreeMap::from([(date, vec![master.clone(), overridden.clone()])]),
        true,
        None,
    );
    st.pane = CalPane::Agenda;
    st.agenda_sel = 1;
    let key = super::agenda_key(&st);
    st.events.get_mut(&date).unwrap().reverse();
    super::reselect_agenda(&mut st, key.as_ref());
    assert_eq!(st.agenda_sel, 0);
    assert_eq!(
        st.selected_events()[st.agenda_sel].recurrence_id,
        overridden.recurrence_id
    );
}

#[test]
fn agenda_omitted_note_says_which_rows_are_shown() {
    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    assert_eq!(
        agenda_note_of(&st),
        format!("8 events {} 4-6 shown", crate::caps::active_glyphs().middot)
    );
    st.ui.agenda_rows = 8;
    assert_eq!(agenda_note_of(&st), "8 events");
    let one = busy_day(1, 3);
    assert_eq!(agenda_note_of(&one), "1 event");
}

#[test]
fn agenda_pane_home_end_and_paging_page_the_grid_not_the_list() {
    use crate::detail::DetailOutcome;
    use termwiz::input::{KeyCode, Modifiers};

    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    let outcome = keys::handle_calendar_key(&mut st, &KeyCode::Home, Modifiers::NONE);
    assert!(matches!(outcome, DetailOutcome::Pending));
    assert_eq!(
        st.cursor.selected(),
        NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()
    );
    assert_eq!(st.agenda_sel, 0);
    assert_eq!(st.pane, CalPane::Agenda);

    let mut st = busy_day(8, 3);
    st.agenda_sel = 5;
    let outcome = keys::handle_calendar_key(&mut st, &KeyCode::End, Modifiers::NONE);
    assert!(matches!(outcome, DetailOutcome::Pending));
    assert_eq!(
        st.cursor.selected(),
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    );
    assert_eq!(st.agenda_sel, 0);

    let mut st = busy_day(8, 3);
    let day = st.cursor.selected();
    keys::handle_calendar_key(&mut st, &KeyCode::DownArrow, Modifiers::NONE);
    keys::handle_calendar_key(&mut st, &KeyCode::DownArrow, Modifiers::NONE);
    assert_eq!(st.cursor.selected(), day);
    assert_eq!(st.agenda_sel, 2);
}
