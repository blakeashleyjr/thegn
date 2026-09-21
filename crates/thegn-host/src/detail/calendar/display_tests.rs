use super::*;
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::calendar::EventTime;

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
                format: String::new(),
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
        format: String::new(),
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
        format: String::new(),
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
    use crate::calendar_docs::CalendarViewError;
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
        Some(CalendarViewError::MalformedCache),
    ));
    assert!(note.ends_with(" incomplete"), "{note}");
}

#[test]
fn a_failed_month_is_visible_without_the_agenda() {
    // `show_agenda = false` never builds the agenda note, so the grid header
    // has to carry the state — or a first-load failure paints as an ordinary
    // empty month and a stale one paints as current.
    use crate::calendar_docs::CalendarViewError;
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
            Some(CalendarViewError::MalformedCache)
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
