//! The calendar popup: a month grid, a per-day agenda, the current weather and
//! world clocks, opened from the `date`/`clock`/`weather` masthead widgets (or
//! `Alt-d`).
//!
//! A child module of `detail`, so it reaches the private `DetailOverlay`
//! fields; split out only to keep `detail.rs` under the god-file cap, the same
//! arrangement `ci_drill.rs` uses.
//!
//! All month arithmetic is pure ([`thegn_core::calendar`]), so paging months is
//! instant and never round-trips the event loop. The one exception is reaching
//! a month whose events aren't cached: navigation still repaints immediately
//! with blank markers, and a `DetailAction::FetchCalendar` fills them
//! in when it lands.

pub(crate) mod keys;
pub(crate) mod layout;
pub(crate) mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local, NaiveDate};
use chrono_tz::Tz;
use thegn_core::calendar::{CalCursor, CalEvent, ClockFormat, ResolvedClock};

use crate::calendar_docs::{CalUiCfg, CalendarDocs, CalendarViewError, WxUiCfg};
use crate::chrome::FrameModel;
use crate::compositor::Rect;

/// The popup's title. Also how the loop recognises an already-open calendar in
/// order to toggle it shut.
pub(crate) const TITLE: &str = "Calendar";

/// Which sub-surface owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalPane {
    Grid,
    Agenda,
}

/// Everything the calendar overlay owns.
///
/// Snapshotted at open time like every other detail content, plus the mutable
/// cursor the key handler drives.
#[derive(Debug, Clone)]
pub(crate) struct CalState {
    pub cursor: CalCursor,
    /// Frozen at open, refreshed on each clock tick so a popup left up past
    /// midnight stops highlighting yesterday.
    pub today: NaiveDate,
    pub pane: CalPane,
    pub agenda_sel: usize,
    pub events: BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>,
    pub loaded: BTreeSet<(i32, u32)>,
    /// Months whose last delivery failed or was incomplete (see
    /// [`crate::calendar_docs::fold_month`]).
    pub errors: BTreeMap<(i32, u32), CalendarViewError>,
    /// The month whose fetch is in flight. Guards against firing a second
    /// request for a month already being fetched, and lets a late payload for a
    /// month the user has navigated away from be dropped — the `pending_ci`
    /// pattern from the CI drill.
    pub pending: Option<(i32, u32)>,
    pub clocks: Vec<ResolvedClock>,
    pub now: DateTime<chrono::Utc>,
    pub home: Tz,
    pub ui: CalUiCfg,
    /// The weather reading at open time, or `None` when `[weather]` is off /
    /// nothing has landed. Refreshed by [`retick_open`] from the same model the
    /// masthead widget reads, so the block never disagrees with the bar.
    pub weather: Option<thegn_core::weather::WeatherSnapshot>,
    /// The `[weather]` knobs the block needs: staleness thresholds, whether to
    /// draw the day strip, and how many days of it.
    pub wx: WxUiCfg,
}

impl CalState {
    /// Whether the visible month's events are known.
    pub fn month_loaded(&self) -> bool {
        self.loaded.contains(&self.cursor.visible_month())
    }

    pub fn month_error(&self) -> Option<CalendarViewError> {
        self.errors.get(&self.cursor.visible_month()).copied()
    }

    /// Events on the selected day.
    pub fn selected_events(&self) -> &[Arc<CalEvent>] {
        self.events
            .get(&self.cursor.selected())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether a day should be dotted. Unknown months show no markers rather
    /// than a misleading "no events".
    pub fn has_events(&self, date: NaiveDate) -> bool {
        self.ui.show_markers
            && self.loaded.contains(&(date.year(), date.month()))
            && self.events.get(&date).is_some_and(|v| !v.is_empty())
    }
}

/// The calendar popup's content.
#[derive(Debug, Clone)]
pub(crate) struct CalendarDetail {
    pub st: CalState,
}

/// The off-loop answer to a [`crate::detail::DetailAction::FetchCalendar`].
#[derive(Debug, Clone)]
pub struct CalendarPayload {
    pub month: (i32, u32),
    /// Inclusive local-date bounds represented by this snapshot.
    pub covered: (NaiveDate, NaiveDate),
    /// Latest navigation request for this month, or `None` for periodic data.
    pub request: Option<u64>,
    /// The month's per-day buckets, sharing each occurrence's payload; `None`
    /// when the month could not be produced (the last valid snapshot stays).
    pub events: Option<Vec<(NaiveDate, Vec<Arc<CalEvent>>)>>,
    /// Why the month is unavailable — or, alongside `events`, incomplete.
    pub error: Option<CalendarViewError>,
}

/// Build the calendar popup for the `date`/`clock` bar items.
///
/// Falls back to the old two-row date/time key-value box when the terminal is
/// too narrow for even the tightest grid — 8 lines that keep the degenerate
/// case working rather than drawing something broken.
pub(super) fn open(
    ctx: &super::StatusCtx<'_>,
    near: super::Placement,
    model: &FrameModel,
) -> Option<super::DetailOverlay> {
    let docs = ctx.cal;
    let now = chrono::Utc::now();
    let today = now.with_timezone(&docs.home).date_naive();
    crate::hydrate_calendar::note_visible_month((today.year(), today.month()));
    // Snapshotted at open time like everything else here — the popup holds no
    // borrow on the model across frames.
    let weather = model.weather.clone();
    let wx = WxUiCfg::from_config(&model.weather_cfg);

    // The popup's own content width, before the layer's 4-cell border inset.
    let want = preferred_cols(
        docs,
        render::weather_cols(weather.as_ref(), &wx, now.timestamp()),
    );
    let cols = want.min(ctx.screen.cols.saturating_sub(6)).max(1);
    if layout::metrics(cols, docs.ui.show_week_numbers).is_none() {
        return Some(fallback(near, ctx));
    }
    let st = CalState {
        cursor: CalCursor::new(today),
        today,
        pane: CalPane::Grid,
        agenda_sel: 0,
        events: docs.events.clone(),
        loaded: docs.loaded.clone(),
        errors: docs.errors.clone(),
        pending: None,
        // The home row is synthesized HERE, not only in
        // `CalendarDocs::from_config`, so "there is always at least one clock"
        // holds however the docs were built — a default-constructed
        // `CalendarDocs` would otherwise render no clock block at all.
        clocks: with_home_clock(&docs.clocks, docs.home, docs.ui.twelve_hour),
        now,
        home: docs.home,
        ui: docs.ui.clone(),
        weather,
        wx,
    };
    Some(super::DetailOverlay {
        title: TITLE.to_string(),
        cols,
        // Clamp to what the layer will actually draw. Left unclamped, a tall
        // popup on a short terminal reports `scroll_max() == 0` and the
        // overflow is both clipped and unreachable (see `sections()`).
        rows: content_height(&st)
            .min(ctx.screen.rows.saturating_sub(3))
            .max(1),
        content: super::DetailContent::Calendar(Box::new(CalendarDetail { st })),
        placement: near,
        scroll: 0,
        sel: 0,
        hint: None,
        monitor_tab: None,
        pending_ci: None,
        live_ci: None,
        ci_autofix: None,
    })
}

/// The width the popup would like: enough for a roomy grid, for the widest
/// world-clock row, and — only when a weather block will actually be drawn —
/// for its current-conditions row.
///
/// `weather_cols` is `0` when weather is off, absent or expired, so the default
/// popup width is byte-identical to what it has always been. Widening it
/// unconditionally would shift every recorded e2e baseline.
fn preferred_cols(docs: &CalendarDocs, weather_cols: usize) -> usize {
    let grid = layout::GridMetrics {
        cell_w: 4,
        gap: 1,
        gutter: if docs.ui.show_week_numbers { 4 } else { 0 },
    }
    .width();
    // Room for `label  Fri  21:04  CEST  +5h30  +1d` at the widest label.
    let label = docs
        .clocks
        .iter()
        .map(|c| {
            let label = if c.label.trim().is_empty() {
                ResolvedClock::label_from_zone(c.zone)
            } else {
                c.label.clone()
            };
            thegn_core::calendar::display::DisplayText::new(
                &label,
                thegn_core::calendar::display::Field::ClockLabel,
            )
            .cells()
        })
        .max()
        .unwrap_or(6);
    grid.max(label + 30).max(44).max(weather_cols)
}

/// The configured clocks, guaranteed to lead with the user's own zone.
fn with_home_clock(clocks: &[ResolvedClock], home: Tz, twelve_hour: bool) -> Vec<ResolvedClock> {
    if clocks.iter().any(|c| c.is_home) {
        return clocks.to_vec();
    }
    let mut out = vec![ResolvedClock {
        label: "local".into(),
        zone: home,
        format: ClockFormat::inherited(twelve_hour),
        show_date: false,
        is_home: true,
    }];
    out.extend(clocks.iter().cloned());
    out
}

/// Total stacked height of the popup's sections.
pub(crate) fn content_height(st: &CalState) -> usize {
    render::sections_of(st)
        .iter()
        .map(super::Section::height)
        .sum()
}

/// The pre-calendar date/time box, kept for terminals too narrow to grid.
fn fallback(near: super::Placement, ctx: &super::StatusCtx<'_>) -> super::DetailOverlay {
    let now = Local::now();
    // Clamp like the grid path does: at 24 columns an unclamped 34-wide box is
    // truncated by the layer and loses the very rows it exists to show.
    let cols = 34.min(ctx.screen.cols.saturating_sub(6)).max(12);
    // A key/value row gives the VALUE the space it asks for and truncates the
    // key, so an over-long date doesn't shorten itself — it deletes its own
    // label. Shorten the value instead once the box gets tight.
    let roomy = cols >= 30;
    let date = if roomy {
        now.format("%A %B %-d, %Y").to_string()
    } else {
        now.format("%a %-d %b %Y").to_string()
    };
    let time = if roomy {
        now.format("%H:%M:%S").to_string()
    } else {
        now.format("%H:%M").to_string()
    };
    let mut rows = vec![
        (
            "date".into(),
            date,
            crate::seg::Tok::Slot(crate::chrome::S::Text),
        ),
        (
            "time".into(),
            time,
            crate::seg::Tok::Slot(crate::chrome::S::Dim),
        ),
    ];
    // A zone name is long and is the least urgent of the three; drop it rather
    // than let it eat its own label.
    if roomy {
        rows.push((
            "zone".into(),
            ctx.cal.home.name().to_string(),
            crate::seg::Tok::Slot(crate::chrome::S::Dim),
        ));
    }
    super::keyval("Date & time", rows, cols, near)
}

/// Resolve a click at absolute cell `(x, y)` inside the popup's content rect.
///
/// Walks the sections accumulating row offsets exactly as the renderer does, so
/// the cell that gets hit is always the cell that was drawn.
pub(crate) fn hit(
    inner: Rect,
    scroll: usize,
    st: &CalState,
    x: usize,
    y: usize,
) -> Option<layout::CalHit> {
    use super::Section;
    let mut row = inner.y as i64 - scroll as i64;
    // Tracks where the agenda's rows start, so a click there selects an event.
    let mut agenda_at: Option<(i64, usize)> = None;
    for sec in render::sections_of(st) {
        let h = sec.height() as i64;
        if let Section::MonthGrid(g) = &sec {
            let lay = layout::grid_layout(
                inner.x,
                row,
                inner.cols,
                &g.week_dates(),
                g.week_numbers.is_some(),
                crate::seg::cells(&g.title),
                crate::seg::cells(g.status.as_ref().unwrap_or(&g.today_chip)),
            );
            if let Some(lay) = lay
                && let Some(h) = layout::hit_grid(inner, &lay, x, y)
            {
                return Some(h);
            }
        }
        // The agenda is the FIRST table drawn after the grid; the weather and
        // clocks tables follow it and are not clickable. Anything new that
        // draws a `Section::Table` must go after the agenda for this to hold —
        // `the_agenda_hit_test_still_finds_the_agenda` pins it.
        if agenda_at.is_none()
            && st.ui.show_agenda
            && st.ui.has_sources
            && matches!(sec, Section::Table(_))
        {
            agenda_at = Some((row, h as usize));
        }
        row += h;
    }
    if let Some((y0, rows)) = agenda_at
        && (y as i64) >= y0
        && (y as i64) < y0 + rows as i64
        && y >= inner.y
        && y < inner.y + inner.rows
    {
        return Some(layout::CalHit::AgendaRow((y as i64 - y0) as usize));
    }
    None
}

/// Fill a fetched month into the live overlay.
///
/// Returns whether anything repainted. Drops the payload unless the overlay is
/// still a calendar *and* still wants this month — the user may have navigated
/// away, or closed and reopened onto something else entirely.
pub fn apply_calendar(slot: &mut Option<super::DetailOverlay>, payload: CalendarPayload) -> bool {
    let Some(ov) = slot.as_mut() else {
        return false;
    };
    let super::DetailContent::Calendar(c) = &mut ov.content else {
        return false;
    };
    if !crate::hydrate_calendar::calendar_payload_is_current(&payload) {
        return false;
    }
    let is_periodic = payload.request.is_none();
    let periodic_refresh = is_periodic && c.st.loaded.contains(&payload.month);
    if c.st.pending != Some(payload.month) && !periodic_refresh {
        return false;
    }
    crate::calendar_docs::fold_month(
        &mut c.st.events,
        &mut c.st.loaded,
        &mut c.st.errors,
        &payload,
    );
    if !is_periodic {
        c.st.pending = None;
    }
    true
}

/// Re-resolve an open calendar's clocks on a clock tick, and re-take its
/// weather reading from the model.
///
/// Also refreshes `today`: a popup left open across midnight would otherwise
/// keep ringing yesterday's cell. No I/O — just the current instant and a
/// snapshot the loop already owns.
///
/// `weather` comes from `model.weather`, the same field the masthead widget
/// reads, so a delivery that repaints the bar can never leave an open popup
/// showing an older sky. Advancing `now` is what ages the block out on its own:
/// hard expiry is evaluated at draw time, so no timer is needed to make a
/// reading disappear.
pub fn retick_open(
    slot: &mut Option<super::DetailOverlay>,
    weather: Option<&thegn_core::weather::WeatherSnapshot>,
) -> bool {
    let Some(ov) = slot.as_mut() else {
        return false;
    };
    let super::DetailContent::Calendar(c) = &mut ov.content else {
        return false;
    };
    let now = chrono::Utc::now();
    let today = now.with_timezone(&c.st.home).date_naive();
    let changed = c.st.today != today || c.st.now != now || c.st.weather.as_ref() != weather;
    c.st.now = now;
    c.st.today = today;
    c.st.weather = weather.cloned();
    changed
}

#[cfg(test)]
mod request_tests {
    use super::*;

    fn overlay(month: (i32, u32)) -> super::super::DetailOverlay {
        let today = NaiveDate::from_ymd_opt(month.0, month.1, 1).unwrap();
        let st = CalState {
            cursor: CalCursor::new(today),
            today,
            pane: CalPane::Grid,
            agenda_sel: 0,
            events: BTreeMap::new(),
            loaded: BTreeSet::new(),
            errors: BTreeMap::new(),
            pending: Some(month),
            clocks: Vec::new(),
            now: today.and_hms_opt(12, 0, 0).unwrap().and_utc(),
            home: chrono_tz::UTC,
            ui: CalUiCfg::default(),
            weather: None,
            wx: WxUiCfg::default(),
        };
        super::super::DetailOverlay {
            title: TITLE.into(),
            content: super::super::DetailContent::Calendar(Box::new(CalendarDetail { st })),
            cols: 1,
            rows: 1,
            placement: super::super::Placement::Center,
            scroll: 0,
            sel: 0,
            hint: None,
            pending_ci: None,
            monitor_tab: None,
            live_ci: None,
            ci_autofix: None,
        }
    }

    fn payload(month: (i32, u32), request: u64) -> CalendarPayload {
        let (from, to) = thegn_core::calendar::month_bounds(month.0, month.1).unwrap();
        let covered = crate::hydrate_calendar::widen(from, to);
        CalendarPayload {
            month,
            covered,
            request: Some(request),
            events: Some(Vec::new()),
            error: None,
        }
    }

    #[test]
    fn only_the_newest_same_month_result_applies_and_clears_pending() {
        for (year, month) in [(2052, 3), (2053, 4)] {
            let key = (year, month);
            let old_request = crate::hydrate_calendar::register_month_request(key);
            let new_request = crate::hydrate_calendar::register_month_request(key);
            let mut older_first = Some(overlay(key));
            assert!(!apply_calendar(&mut older_first, payload(key, old_request)));
            let super::super::DetailContent::Calendar(calendar) =
                &mut older_first.as_mut().unwrap().content
            else {
                panic!("calendar overlay expected");
            };
            assert_eq!(calendar.st.pending, Some(key));
            assert!(apply_calendar(&mut older_first, payload(key, new_request)));
            let super::super::DetailContent::Calendar(calendar) =
                &mut older_first.as_mut().unwrap().content
            else {
                panic!("calendar overlay expected");
            };
            assert_eq!(calendar.st.pending, None);

            let mut newer_first = Some(overlay(key));
            assert!(apply_calendar(&mut newer_first, payload(key, new_request)));
            assert!(!apply_calendar(&mut newer_first, payload(key, old_request)));
            let super::super::DetailContent::Calendar(calendar) =
                &mut newer_first.as_mut().unwrap().content
            else {
                panic!("calendar overlay expected");
            };
            assert_eq!(calendar.st.pending, None);
        }
    }

    #[test]
    fn periodic_delivery_does_not_clear_pending_navigation() {
        let key = (2054, 5);
        let request = crate::hydrate_calendar::register_month_request(key);
        let mut slot = Some(overlay(key));
        let mut periodic = payload(key, request);
        periodic.request = None;

        assert!(apply_calendar(&mut slot, periodic));
        let super::super::DetailContent::Calendar(calendar) = &mut slot.as_mut().unwrap().content
        else {
            panic!("calendar overlay expected");
        };
        assert_eq!(calendar.st.pending, Some(key));

        assert!(apply_calendar(&mut slot, payload(key, request)));
        let super::super::DetailContent::Calendar(calendar) = &mut slot.as_mut().unwrap().content
        else {
            panic!("calendar overlay expected");
        };
        assert_eq!(calendar.st.pending, None);
    }

    #[test]
    fn periodic_refresh_of_a_non_current_visible_month_applies() {
        let key = (2055, 6);
        let today = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let delivered = crate::hydrate_calendar::periodic_months(today, Some(key));
        assert_eq!(delivered, vec![(2026, 10), key]);
        let mut slot = Some(overlay(key));
        let super::super::DetailContent::Calendar(calendar) = &mut slot.as_mut().unwrap().content
        else {
            panic!("calendar overlay expected");
        };
        calendar.st.loaded.insert(key);

        let mut refresh = payload(key, 0);
        refresh.request = None;
        assert!(apply_calendar(&mut slot, refresh));
    }
}

#[cfg(test)]
mod display_tests;
