//! Control-API calendar reads (`calendar.events`, `calendar.clocks`).
//!
//! Pure helpers over the calendar cache and `[calendar]` config, kept out of
//! `service.rs` so the service only wires them to `with_db`. Everything here
//! serves thegn's calendar *cache*, which hydration/sync populates; this module
//! never fetches from a provider.

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};
use thegn_core::calendar::{
    CalEvent, CalendarWindow, ClockFormat, ExpansionError, ExpansionLimit, GapPolicy,
    ResolvedClock, expand_calendar, tz,
};
use thegn_core::config_calendar::CalendarConfig;
use thegn_svc::control::{CalendarEventsReply, ControlError, ControlResult};

/// Maximum inclusive date span accepted by `calendar.events`.
pub(crate) const MAX_WINDOW_DAYS: u64 = 366;

/// The zone dates and all-day events are read in: `[calendar] home_zone`, else
/// the system zone (the same fallback the calendar popup uses).
pub(crate) fn home_zone(cfg: &CalendarConfig) -> Tz {
    cfg.home_zone().unwrap_or_else(tz::system_zone)
}

/// Parse the inclusive ISO `from`/`to` dates. Bad input is the caller's fault.
pub(crate) fn parse_window(from: &str, to: &str) -> ControlResult<(NaiveDate, NaiveDate)> {
    let parse = |name: &str, raw: &str| {
        NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|_| {
            ControlError::InvalidArgument(format!("`{name}` must be an ISO date (YYYY-MM-DD)"))
        })
    };
    let (from, to) = (parse("from", from)?, parse("to", to)?);
    if from > to {
        return Err(ControlError::InvalidArgument(
            "`from` must not be after `to`".into(),
        ));
    }
    let days = (to - from).num_days() as u64 + 1;
    if days > MAX_WINDOW_DAYS {
        return Err(ControlError::InvalidArgument(format!(
            "calendar window exceeds the maximum of {MAX_WINDOW_DAYS} inclusive days"
        )));
    }
    Ok((from, to))
}

/// Cached events expanded into the concrete occurrences inside `[from, to]`,
/// flat and sorted by start (all-day events sort at local midnight), ties
/// broken by title then uid.
///
/// A cache row that fails to deserialize, or that is row-locally malformed, is
/// skipped and counted in the reply. A failed cache read, or an expansion that
/// exhausts the shared budget, is an error: a truncated calendar is never
/// returned as complete.
pub(crate) fn events_from_rows(
    rows: Vec<(String, String)>,
    from: NaiveDate,
    to: NaiveDate,
    home: Tz,
) -> ControlResult<CalendarEventsReply> {
    let _window = CalendarWindow::new(from, to, home).ok_or_else(|| {
        ControlError::InvalidArgument("the requested calendar window is not representable".into())
    })?;
    let mut cached = Vec::with_capacity(rows.len());
    let mut bad_rows = 0usize;
    for (_, json) in rows {
        match serde_json::from_str::<CalEvent>(&json) {
            Ok(event) => cached.push(event),
            Err(_) => bad_rows += 1,
        }
    }
    let expanded = expand_calendar(&cached, from, to, home).map_err(expansion_error)?;
    let skipped_rows = (bad_rows + expanded.skipped).min(u32::MAX as usize) as u32;
    if skipped_rows > 0 {
        tracing::warn!(
            target: "thegn::calendar",
            undecodable = bad_rows,
            malformed = expanded.skipped,
            "calendar.events: skipped cache rows; the result is incomplete"
        );
    }
    let occurrences = expanded.occurrences;
    drop(expanded.by_date);
    Ok(CalendarEventsReply {
        events: flatten_sorted(occurrences, home),
        skipped_rows,
        incomplete: skipped_rows > 0,
    })
}

#[cfg(test)]
fn events_in_window(
    db: &impl thegn_core::store::CalendarStore,
    from: NaiveDate,
    to: NaiveDate,
    home: Tz,
) -> ControlResult<CalendarEventsReply> {
    let window = CalendarWindow::new(from, to, home).ok_or_else(|| {
        ControlError::InvalidArgument("the requested calendar window is not representable".into())
    })?;
    let rows = db
        .get_calendar_events(window.start_ms, window.end_exclusive_ms, &[])
        .map_err(|e| ControlError::Internal(e.context("read calendar cache")))?;
    events_from_rows(rows, from, to, home)
}

fn expansion_error(e: ExpansionError) -> ControlError {
    match e {
        ExpansionError::InvalidWindow | ExpansionError::Budget(ExpansionLimit::WindowDays) => {
            ControlError::InvalidArgument(format!("{e}: narrow the requested window"))
        }
        ExpansionError::Budget(_) => ControlError::ResourceExhausted(e.to_string()),
        ExpansionError::InvalidSpan | ExpansionError::Arithmetic => {
            ControlError::Internal(anyhow::anyhow!("{e}"))
        }
    }
}

/// Unwrap the shared occurrence handles and order them by start instant.
fn flatten_sorted(occurrences: Vec<std::sync::Arc<CalEvent>>, home: Tz) -> Vec<CalEvent> {
    let mut out: Vec<CalEvent> = occurrences
        .into_iter()
        .map(|e| std::sync::Arc::try_unwrap(e).unwrap_or_else(|shared| (*shared).clone()))
        .collect();
    out.sort_by(|a, b| {
        a.start
            .instant_in(home, GapPolicy::ShiftForward)
            .cmp(&b.start.instant_in(home, GapPolicy::ShiftForward))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.uid.cmp(&b.uid))
    });
    out
}

/// The home row followed by the configured world clocks, evaluated at `now`.
pub(crate) fn clocks_json(cfg: &CalendarConfig, now: DateTime<Utc>) -> Value {
    let home = home_zone(cfg);
    let twelve_hour = thegn_core::calendar::resolve_time_format(
        cfg.twelve_hour_pref(),
        crate::calendar_docs::CalendarDocs::env_locale().as_deref(),
    );
    let mut clocks = vec![ResolvedClock {
        label: "local".into(),
        zone: home,
        format: ClockFormat::inherited(twelve_hour),
        show_date: false,
        is_home: true,
    }];
    clocks.extend(cfg.active_clocks(twelve_hour));
    let readings = tz::read_clocks(&clocks, now, home);
    Value::Array(
        readings
            .into_iter()
            .map(|r| {
                let time = match &r.format {
                    ClockFormat::H12 => {
                        let h = r.local.hour();
                        let h12 = if h % 12 == 0 { 12 } else { h % 12 };
                        format!(
                            "{h12}:{:02}{}",
                            r.local.minute(),
                            if h < 12 { "am" } else { "pm" }
                        )
                    }
                    ClockFormat::H24 => format!("{:02}:{:02}", r.local.hour(), r.local.minute()),
                    ClockFormat::Custom(fmt) => r.local.format(fmt).to_string(),
                };
                json!({
                    "label": r.label,
                    "zone": r.zone.name(),
                    "local": r.local.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "time": time,
                    "format": match &r.format {
                        ClockFormat::H12 => "h12".to_string(),
                        ClockFormat::H24 => "h24".to_string(),
                        ClockFormat::Custom(fmt) => fmt.clone(),
                    },
                    "show_date": r.show_date,
                    "utc_offset_secs": r.utc_offset_secs,
                    "is_dst": r.is_dst,
                    "abbrev": r.abbrev,
                    "delta_from_home_mins": r.delta_from_home_mins,
                    "day_delta": r.day_delta,
                    "is_home": r.is_home,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::calendar::{EventTime, RRule, Recurrence};
    use thegn_core::db::Db;
    use thegn_core::store::{CalendarRow, CalendarStore};

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn timed(uid: &str, title: &str, at: &str) -> CalEvent {
        let start: DateTime<Utc> = at.parse().unwrap();
        CalEvent::new(
            uid,
            title,
            EventTime::Instant { at: start },
            EventTime::Instant {
                at: start + chrono::Duration::hours(1),
            },
        )
    }

    fn row(e: &CalEvent) -> CalendarRow {
        let ms = |t: &EventTime| {
            t.instant_in(Tz::UTC, GapPolicy::ShiftForward)
                .unwrap()
                .timestamp_millis()
        };
        CalendarRow {
            uid: e.uid.clone(),
            recurrence_key: e.recurrence_key(),
            calendar: String::new(),
            start_ms: ms(&e.start),
            end_ms: ms(&e.end),
            recurring: false,
            json: serde_json::to_string(e).unwrap(),
            href: None,
        }
    }

    #[test]
    fn parse_window_rejects_bad_dates_and_inverted_ranges() {
        assert_eq!(
            parse_window("2026-03-01", " 2026-03-31 ").unwrap(),
            (day("2026-03-01"), day("2026-03-31"))
        );
        for (a, b) in [
            ("nope", "2026-03-31"),
            ("2026-03-01", ""),
            ("2026-04-01", "2026-03-01"),
        ] {
            assert!(matches!(
                parse_window(a, b),
                Err(ControlError::InvalidArgument(_))
            ));
        }
        assert!(matches!(
            parse_window("2024-01-01", "2025-01-01"),
            Err(ControlError::InvalidArgument(_))
        ));
    }

    #[test]
    fn events_are_flat_sorted_and_a_bad_row_is_skipped() {
        let db = Db::open_memory().unwrap();
        let late = timed("b", "late", "2026-03-10T15:00:00Z");
        let early = timed("a", "early", "2026-03-10T09:00:00Z");
        let outside = timed("c", "outside", "2026-05-10T09:00:00Z");
        let mut bad = row(&early);
        bad.uid = "bad".into();
        bad.json = "{not json".into();
        db.put_calendar_events("work", &[row(&late), row(&early), row(&outside), bad])
            .unwrap();
        let got = events_in_window(&db, day("2026-03-01"), day("2026-03-31"), Tz::UTC).unwrap();
        let titles: Vec<_> = got.events.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["early", "late"]);
        assert_eq!(got.skipped_rows, 1);
        assert!(got.incomplete);
    }

    #[test]
    fn recurring_master_expands_to_occurrences_inside_the_window() {
        let db = Db::open_memory().unwrap();
        let mut master = timed("m", "standup", "2026-03-02T09:00:00Z");
        master.recurrence = Some(Recurrence {
            rules: vec![RRule::parse("FREQ=DAILY;COUNT=3").unwrap()],
            ..Recurrence::default()
        });
        let mut r = row(&master);
        r.recurring = true;
        db.put_calendar_events("work", &[r]).unwrap();
        let got = events_in_window(&db, day("2026-03-03"), day("2026-03-10"), Tz::UTC).unwrap();
        assert_eq!(got.events.len(), 2, "2026-03-03 and 2026-03-04 only");
        assert!(got.events.windows(2).all(|w| {
            w[0].start.instant_in(Tz::UTC, GapPolicy::ShiftForward)
                <= w[1].start.instant_in(Tz::UTC, GapPolicy::ShiftForward)
        }));
    }

    #[test]
    fn oversized_window_is_an_invalid_argument() {
        assert!(matches!(
            parse_window("2000-01-01", "2026-01-01"),
            Err(ControlError::InvalidArgument(_))
        ));
    }

    #[test]
    fn clocks_lead_with_the_home_row() {
        let cfg = CalendarConfig {
            home_zone: "UTC".into(),
            time_format: thegn_core::config_calendar::TimeFormat::H12,
            ..CalendarConfig::default()
        };
        let now: DateTime<Utc> = "2026-03-10T12:00:00Z".parse().unwrap();
        let v = clocks_json(&cfg, now);
        let rows = v.as_array().unwrap();
        assert_eq!(rows[0]["is_home"], true);
        assert_eq!(rows[0]["zone"], "UTC");
        assert_eq!(rows[0]["local"], "2026-03-10T12:00:00");
        assert_eq!(rows[0]["format"], "h12");
        assert_eq!(rows[0]["time"], "12:00pm");
        assert_eq!(rows[0]["show_date"], false);
    }
}
