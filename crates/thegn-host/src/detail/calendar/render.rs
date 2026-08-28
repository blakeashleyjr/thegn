//! Building the calendar popup's sections, and painting the month grid.
//!
//! The sections are rebuilt from [`CalState`] every frame rather than cached:
//! the state *is* the model, so a key press repaints correctly with no
//! invalidation bookkeeping, and the cost is a handful of small allocations on
//! a popup that repaints at most once a minute.

use chrono::{Datelike, NaiveDate, Timelike};
use termwiz::surface::Surface;

use super::layout::{self, GRID_HEADER_ROWS};
use super::{CalPane, CalState};
use crate::chrome::S;
use crate::compositor::Rect;
use crate::seg::{self, Line, Tok, Under, seg};

/// A month grid, ready to paint.
#[derive(Debug, Clone, PartialEq)]
pub struct MonthGridSection {
    pub title: String,
    pub today_chip: String,
    pub dow: [String; 7],
    pub week_numbers: Option<Vec<u32>>,
    pub weeks: Vec<[DayCell; 7]>,
}

/// One day in the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayCell {
    pub date: NaiveDate,
    pub tone: Tok,
    pub bold: bool,
    /// `Single` marks today.
    pub under: Under,
    pub selected: bool,
    /// `Some(tone)` when the day has events; `None` for none *or* unknown.
    pub marker: Option<Tok>,
}

impl MonthGridSection {
    /// Rows this block occupies: title, blank, weekday names, then the weeks.
    pub fn height(&self) -> usize {
        GRID_HEADER_ROWS + self.weeks.len()
    }

    /// The dates of each week row, for the layout/hit-test pass.
    pub fn week_dates(&self) -> Vec<[NaiveDate; 7]> {
        self.weeks
            .iter()
            .map(|w| {
                let mut out = [w[0].date; 7];
                for (i, c) in w.iter().enumerate() {
                    out[i] = c.date;
                }
                out
            })
            .collect()
    }
}

/// Build every section of the popup, top to bottom.
pub(crate) fn sections_of(st: &CalState) -> Vec<super::super::Section> {
    use super::super::Section;
    let mut out = vec![Section::MonthGrid(Box::new(month_section(st)))];

    if st.ui.show_agenda && st.ui.has_sources {
        out.push(super::super::spacer());
        out.push(agenda_heading(st));
        out.push(agenda_table(st));
    }

    // Above the clocks: weather is "here, right now" and the clocks are
    // "elsewhere, right now", which reads in that order — and it keeps the
    // clocks anchored at the bottom where existing users expect them.
    if let Some((heading, table)) =
        weather_sections(st.weather.as_ref(), &st.wx, st.now.timestamp())
    {
        out.push(super::super::spacer());
        out.push(heading);
        out.push(table);
    }

    if !st.clocks.is_empty() {
        out.push(super::super::spacer());
        out.push(Section::Heading {
            label: "WORLD CLOCKS".into(),
            note: None,
        });
        out.push(clocks_table(st));
    }
    out
}

/// The `WEATHER · <place>` heading and its table, or `None` when there is
/// nothing worth drawing.
///
/// Absent entirely — not an empty block — with no reading or a hard-expired
/// one, mirroring how the agenda is suppressed without an event source. `now`
/// is passed in (from `st.now`, which `retick_open` refreshes) so the age never
/// calls a clock at a draw site.
pub(crate) fn weather_sections(
    snap: Option<&thegn_core::weather::WeatherSnapshot>,
    wx: &crate::calendar_docs::WxUiCfg,
    now: i64,
) -> Option<(super::super::Section, super::super::Section)> {
    use super::super::{Cell, Section, TableSection};
    use thegn_core::weather::{self, Freshness};

    let snap = snap?;
    let fresh = weather::freshness(
        snap.fetched_at,
        now,
        wx.stale_after_secs,
        wx.hard_expiry_secs,
    );
    if fresh == Freshness::Expired {
        return None;
    }
    let glyphs = crate::caps::active_glyphs();
    let heading = Section::Heading {
        label: if snap.place.is_empty() {
            "WEATHER".into()
        } else {
            format!("WEATHER {} {}", glyphs.middot, snap.place)
        },
        // A dated reading says so; a current one says nothing at all.
        note: (fresh == Freshness::Stale).then(|| weather::fmt_age(snap.fetched_at, now)),
    };

    let sky = weather::sky_glyph(snap.sky, glyphs);
    let u = snap.units;
    // A `Table` rather than a `Grid` for the reason `clocks_table` gives: a
    // `Grid` tones the whole value string at once and flattens the row.
    let mut rows = vec![vec![
        Cell::Text(
            if sky.is_empty() {
                snap.description.clone()
            } else {
                format!("{sky} {}", snap.description)
            },
            Tok::Slot(S::Text),
        ),
        Cell::Text(weather::fmt_temp(snap.temp, u), Tok::Slot(S::Text)),
        Cell::Text(
            format!("feels {}", weather::fmt_temp(snap.feels_like, u)),
            Tok::Slot(S::Dim),
        ),
        Cell::Text(
            format!(
                "H {} L {}",
                weather::fmt_temp(snap.hi, u),
                weather::fmt_temp(snap.lo, u)
            ),
            Tok::Hue(thegn_core::theme::Hue::Amber),
        ),
        Cell::Text(format!("{}%", snap.humidity_pct), Tok::Slot(S::Faint)),
        Cell::Text(weather::fmt_wind(snap.wind, u), Tok::Slot(S::Faint)),
    ]];

    if wx.show_forecast {
        // `draw_table` sizes each column to its widest cell, so these short
        // rows collapse on their own — no explicit shedding needed.
        for d in snap.forecast.iter().take(wx.forecast_days) {
            rows.push(vec![
                Cell::Text(d.date.format("%a").to_string(), Tok::Slot(S::Dim)),
                Cell::Text(
                    weather::sky_glyph(d.sky, glyphs).to_string(),
                    Tok::Slot(S::Text),
                ),
                Cell::Text(
                    format!(
                        "{} / {}",
                        weather::fmt_temp(d.hi, u),
                        weather::fmt_temp(d.lo, u)
                    ),
                    Tok::Slot(S::Dim),
                ),
            ]);
        }
    }

    Some((
        heading,
        Section::Table(TableSection {
            header: Vec::new(),
            rows,
            sel: None,
        }),
    ))
}

/// The columns the weather table wants, or `0` when the block is absent.
///
/// Measured through `sections::table_cols`, the same sizing `draw_table` uses,
/// so the popup widens by exactly what the block needs — and by nothing at all
/// when weather is off, which is what keeps every recorded e2e baseline valid.
pub(crate) fn weather_cols(
    snap: Option<&thegn_core::weather::WeatherSnapshot>,
    wx: &crate::calendar_docs::WxUiCfg,
    now: i64,
) -> usize {
    use super::super::Section;
    match weather_sections(snap, wx, now) {
        Some((_, Section::Table(t))) => crate::sections::table_cols(&t),
        _ => 0,
    }
}

/// The month grid block.
fn month_section(st: &CalState) -> MonthGridSection {
    let (y, m) = st.cursor.visible_month();
    let grid =
        thegn_core::calendar::MonthGrid::build(y, m, st.ui.week_start, st.today, st.ui.six_weeks);
    let selected = st.cursor.selected();
    let weeks = grid
        .as_ref()
        .map(|g| {
            g.weeks
                .iter()
                .map(|row| {
                    let mut cells = [DayCell {
                        date: row[0].date,
                        tone: Tok::Slot(S::Text),
                        bold: false,
                        under: Under::None,
                        selected: false,
                        marker: None,
                    }; 7];
                    for (i, c) in row.iter().enumerate() {
                        cells[i] = DayCell {
                            date: c.date,
                            tone: if c.is_today {
                                Tok::Slot(S::Accent)
                            } else if c.in_month {
                                Tok::Slot(S::Text)
                            } else {
                                // The faintest slot: borrowed neighbour days are
                                // context, not content.
                                Tok::Slot(S::Ghost3)
                            },
                            bold: c.is_today,
                            // Today keeps its ring *on top of* the selection
                            // chip, so "today" and "selected" stay separately
                            // readable when they land on the same cell.
                            under: if c.is_today {
                                Under::Single
                            } else {
                                Under::None
                            },
                            selected: c.date == selected,
                            marker: st.has_events(c.date).then_some(if c.in_month {
                                Tok::Slot(S::Accent)
                            } else {
                                Tok::Slot(S::Ghost3)
                            }),
                        };
                    }
                    cells
                })
                .collect()
        })
        .unwrap_or_default();

    let dow = thegn_core::calendar::weekday_headers(
        st.ui.week_start,
        thegn_core::calendar::WeekdayStyle::Two,
    )
    .map(String::from);

    MonthGridSection {
        title: format!("{} {}", month_name(m), y),
        // Never a bare Unicode separator: the frame is composed in Unicode and
        // degraded at the draw sites, so this has to go through `caps` too.
        today_chip: format!(
            "today {} {}",
            crate::caps::active_glyphs().middot,
            st.today.format("%a %-d %b")
        ),
        dow,
        week_numbers: st
            .ui
            .show_week_numbers
            .then(|| grid.as_ref().map(|g| g.week_numbers()).unwrap_or_default()),
        weeks,
    }
}

fn agenda_heading(st: &CalState) -> super::super::Section {
    let sel = st.cursor.selected();
    let n = st.selected_events().len();
    super::super::Section::Heading {
        label: format!(
            "AGENDA {} {}",
            crate::caps::active_glyphs().middot,
            sel.format("%a %-d %b")
        ),
        note: Some(if !st.month_loaded() {
            format!("loading{}", crate::caps::active_glyphs().ellipsis)
        } else {
            match n {
                0 => "no events".into(),
                1 => "1 event".into(),
                n => format!("{n} events"),
            }
        }),
    }
}

/// The per-day event list.
fn agenda_table(st: &CalState) -> super::super::Section {
    use super::super::{Cell, Section, TableSection};
    let evs = st.selected_events();
    let rows: Vec<Vec<Cell>> = evs
        .iter()
        .take(st.ui.agenda_rows)
        .enumerate()
        .map(|(i, e)| {
            let focused = st.pane == CalPane::Agenda && i == st.agenda_sel;
            let title_tone = if focused {
                Tok::SelAccent
            } else {
                Tok::Slot(S::Text)
            };
            vec![
                Cell::Text(event_when(e, st), Tok::Slot(S::Dim)),
                Cell::Text(e.title.clone(), title_tone),
                Cell::Text(
                    e.calendar.clone(),
                    e.color.map(Tok::Hue).unwrap_or(Tok::Slot(S::Ghost)),
                ),
            ]
        })
        .collect();
    Section::Table(TableSection {
        header: Vec::new(),
        rows,
        sel: None,
    })
}

/// `09:30-10:00`, `all day`, or `9:30 am` depending on config and event shape.
fn event_when(e: &thegn_core::calendar::CalEvent, st: &CalState) -> String {
    if e.all_day() {
        return "all day".into();
    }
    let fmt = |t: &thegn_core::calendar::EventTime| -> String {
        t.instant_in(st.home, thegn_core::calendar::GapPolicy::ShiftForward)
            .map(|at| {
                let l = at.with_timezone(&st.home);
                if st.ui.twelve_hour {
                    let h12 = if l.hour() % 12 == 0 {
                        12
                    } else {
                        l.hour() % 12
                    };
                    let ampm = if l.hour() < 12 { "am" } else { "pm" };
                    format!("{h12}:{:02}{ampm}", l.minute())
                } else {
                    format!("{:02}:{:02}", l.hour(), l.minute())
                }
            })
            .unwrap_or_default()
    };
    format!("{}-{}", fmt(&e.start), fmt(&e.end))
}

/// The world-clock block.
///
/// A `Table` rather than a `Grid`: `Grid` gives one tone to a whole value
/// string, which would flatten `Fri 21:04 CEST +1d` to a single color, and the
/// per-cell tones are what make the row scannable.
fn clocks_table(st: &CalState) -> super::super::Section {
    use super::super::{Cell, Section, TableSection};
    let readings = thegn_core::calendar::read_clocks(&st.clocks, st.now, st.home);
    // `draw_table` sizes each column to its widest cell, so the empty delta and
    // day-offset cells on the home row collapse to nothing rather than leaving
    // a gap — no explicit shedding needed.
    let rows = readings
        .iter()
        .map(|r| {
            let time = if st.ui.twelve_hour {
                let h = r.local.hour();
                let h12 = if h % 12 == 0 { 12 } else { h % 12 };
                format!(
                    "{h12}:{:02}{}",
                    r.local.minute(),
                    if h < 12 { "am" } else { "pm" }
                )
            } else {
                format!("{:02}:{:02}", r.local.hour(), r.local.minute())
            };
            vec![
                Cell::Text(
                    r.label.clone(),
                    if r.is_home {
                        Tok::Slot(S::Text)
                    } else {
                        Tok::Slot(S::Dim)
                    },
                ),
                Cell::Text(r.local.format("%a").to_string(), Tok::Slot(S::Faint)),
                Cell::Text(time, Tok::Slot(S::Text)),
                Cell::Text(r.abbrev.clone(), Tok::Slot(S::Faint)),
                Cell::Text(
                    thegn_core::calendar::tz::fmt_delta(r.delta_from_home_mins),
                    Tok::Hue(thegn_core::theme::Hue::Blue),
                ),
                Cell::Text(
                    match r.day_delta {
                        1 => "+1d".into(),
                        -1 => "-1d".into(),
                        _ => String::new(),
                    },
                    Tok::Hue(thegn_core::theme::Hue::Amber),
                ),
            ]
        })
        .collect();
    Section::Table(TableSection {
        header: Vec::new(),
        rows,
        sel: None,
    })
}

fn month_name(m: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES
        .get((m as usize).saturating_sub(1))
        .copied()
        .unwrap_or("")
}

/// Paint the whole popup: build the sections and stack them, exactly as
/// `render_sections` does for the other Sections-shaped popups.
pub(crate) fn render_calendar(
    surface: &mut Surface,
    inner: Rect,
    scroll: usize,
    d: &super::CalendarDetail,
) {
    let mut y = inner.y as i64 - scroll as i64;
    for sec in sections_of(&d.st) {
        crate::sections::draw_section(surface, inner, inner.x, y, inner.cols, &sec);
        y += sec.height() as i64;
    }
}

/// Paint one month-grid block.
pub(crate) fn draw_month_grid(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y0: i64,
    w: usize,
    g: &MonthGridSection,
) {
    let glyphs = crate::caps::active_glyphs();
    let title_w = seg::cells(&g.title);
    // Below this the chip would collide with the month title; dropping it is
    // better than truncating the title the user navigates by.
    let today_chip = if w >= layout::TODAY_CHIP_MIN_COLS {
        g.today_chip.as_str()
    } else {
        ""
    };
    let today_w = seg::cells(today_chip);
    let Some(lay) = layout::grid_layout(
        x,
        y0,
        w,
        &g.week_dates(),
        g.week_numbers.is_some(),
        title_w,
        today_w,
    ) else {
        return;
    };
    let m = lay.metrics;

    // Header: `⟨h⟩ August 2026 ⟨l⟩` … `today · Fri 21 Aug`.
    // Keycaps rather than chevrons: they are the click target *and* they teach
    // the binding, and the glyph set has no left chevron to pair with `›`.
    crate::sections::put_line(
        surface,
        clip,
        x,
        y0,
        w,
        &Line::split(
            vec![
                seg::Seg::key(" h "),
                seg(Tok::Slot(S::Text), format!(" {} ", g.title)).bold(),
                seg::Seg::key(" l "),
            ],
            vec![seg(Tok::Slot(S::Ghost), today_chip.to_string())],
        ),
        super::super::panel(),
    );

    // Weekday header row, with the week-number gutter left blank.
    let mut segs = Vec::with_capacity(8);
    if m.gutter > 0 {
        segs.push(seg(Tok::Slot(S::Ghost), " ".repeat(m.gutter)));
    }
    for (i, name) in g.dow.iter().enumerate() {
        if i > 0 && m.gap > 0 {
            segs.push(seg(Tok::Slot(S::Ghost), " ".repeat(m.gap)));
        }
        // Right-align the label in the cell so it sits over the day numbers.
        let label = seg::take_cols(name, m.cell_w);
        let pad = m.cell_w.saturating_sub(seg::cells(label) + 1);
        segs.push(seg(
            Tok::Slot(S::Dim),
            format!("{}{}{}", " ".repeat(pad), label, " "),
        ));
    }
    crate::sections::put_line(
        surface,
        clip,
        x,
        y0 + 2,
        w,
        &Line::segs(segs),
        super::super::panel(),
    );

    // Week rows.
    for (r, week) in g.weeks.iter().enumerate() {
        let wy = y0 + (GRID_HEADER_ROWS + r) as i64;
        let mut segs: Vec<seg::Seg> = Vec::with_capacity(16);
        if m.gutter > 0 {
            let n = g
                .week_numbers
                .as_ref()
                .and_then(|v| v.get(r))
                .copied()
                .unwrap_or(0);
            segs.push(seg(
                Tok::Slot(S::Ghost),
                format!("{:<width$}", format!("w{n}"), width = m.gutter),
            ));
        }
        for (c, cell) in week.iter().enumerate() {
            if c > 0 && m.gap > 0 {
                segs.push(seg(Tok::Slot(S::Ghost), " ".repeat(m.gap)));
            }
            let marker = match cell.marker {
                Some(_) => glyphs.dot_filled,
                None => " ",
            };
            // `cell_w - 1` for the number, one column for the marker, so the
            // dots line up in their own column instead of shifting the digits.
            let text = format!(
                "{:>width$}{}",
                cell.date.day(),
                marker,
                width = m.cell_w.saturating_sub(seg::cells(marker)).max(1)
            );
            let mut s = seg(cell.tone, text);
            if cell.bold {
                s = s.bold();
            }
            if cell.under != Under::None {
                s = s.under(cell.under);
            }
            if cell.selected {
                s = s.bg(Tok::SelAccent);
            }
            segs.push(s);
        }
        crate::sections::put_line(
            surface,
            clip,
            x,
            wy,
            w,
            &Line::segs(segs),
            super::super::panel(),
        );
    }
}
