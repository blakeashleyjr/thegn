//! The month grid: a 7-column matrix of day cells with leading/trailing days
//! from the neighbouring months, plus ISO week numbers.

use chrono::{Datelike, Days, NaiveDate, Weekday};

/// How wide to render weekday header labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WeekdayStyle {
    /// `Mo Tu We` — two cells, the default for a compact grid.
    #[default]
    Two,
    /// `Mon Tue Wed`.
    Three,
    /// `M T W` — for the tightest layouts. Ambiguous by design (T/T, S/S); the
    /// column position disambiguates.
    One,
}

/// One cell of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayCell {
    pub date: NaiveDate,
    /// False for the leading/trailing days borrowed from the adjacent months.
    pub in_month: bool,
    pub is_today: bool,
    /// ISO-8601 week number of this cell's date.
    pub iso_week: u32,
    pub weekday: Weekday,
}

/// A month laid out as weeks of seven days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthGrid {
    pub year: i32,
    pub month: u32,
    pub week_start: Weekday,
    /// Week rows, each exactly seven cells.
    pub weeks: Vec<[DayCell; 7]>,
}

impl MonthGrid {
    /// Build the grid for `year`/`month`.
    ///
    /// `fixed_six_weeks` pads short months out to six rows. **Default it to
    /// true**: a popup whose height changes as you page Feb→Mar is geometry
    /// damage, which forces a full frame and visibly jitters the box.
    ///
    /// Returns `None` for an out-of-range month.
    pub fn build(
        year: i32,
        month: u32,
        week_start: Weekday,
        today: NaiveDate,
        fixed_six_weeks: bool,
    ) -> Option<MonthGrid> {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        // Days to step back from the 1st to reach the configured week start.
        let lead = first.weekday().num_days_from_monday() as i64
            - week_start.num_days_from_monday() as i64;
        let lead = lead.rem_euclid(7) as u64;
        let origin = first.checked_sub_days(Days::new(lead))?;

        let days_in_month = days_in_month(year, month)?;
        let natural_rows = ((lead as u32 + days_in_month) as f32 / 7.0).ceil() as usize;
        let rows = if fixed_six_weeks {
            6
        } else {
            natural_rows.max(1)
        };

        let mut weeks = Vec::with_capacity(rows);
        for w in 0..rows {
            let mut row = [DayCell {
                date: origin,
                in_month: false,
                is_today: false,
                iso_week: 1,
                weekday: week_start,
            }; 7];
            for (d, slot) in row.iter_mut().enumerate() {
                let date = origin.checked_add_days(Days::new((w * 7 + d) as u64))?;
                *slot = DayCell {
                    date,
                    in_month: date.year() == year && date.month() == month,
                    is_today: date == today,
                    // Never hand-roll this: Jan 1 can be week 52/53 of the
                    // *previous* ISO year.
                    iso_week: date.iso_week().week(),
                    weekday: date.weekday(),
                };
            }
            weeks.push(row);
        }
        Some(MonthGrid {
            year,
            month,
            week_start,
            weeks,
        })
    }

    /// The full span the grid covers, **including** the borrowed leading and
    /// trailing days.
    ///
    /// This is what event queries must key on — an event on Jan 31 has to show
    /// up in February's first cell, and one on Mar 1 in February's last.
    pub fn span(&self) -> (NaiveDate, NaiveDate) {
        let first = self.weeks.first().map(|w| w[0].date).unwrap_or_default();
        let last = self.weeks.last().map(|w| w[6].date).unwrap_or(first);
        (first, last)
    }

    /// The ISO week number for each row.
    pub fn week_numbers(&self) -> Vec<u32> {
        self.weeks.iter().map(|w| w[0].iso_week).collect()
    }

    /// Locate a date in the grid as `(row, col)`.
    pub fn position(&self, date: NaiveDate) -> Option<(usize, usize)> {
        self.weeks
            .iter()
            .enumerate()
            .find_map(|(r, w)| w.iter().position(|c| c.date == date).map(|c| (r, c)))
    }

    /// Every cell, row-major.
    pub fn cells(&self) -> impl Iterator<Item = &DayCell> {
        self.weeks.iter().flatten()
    }
}

/// Weekday header labels, rotated to start at `week_start`.
pub fn weekday_headers(week_start: Weekday, style: WeekdayStyle) -> [&'static str; 7] {
    const ONE: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];
    const TWO: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
    const THREE: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let base = match style {
        WeekdayStyle::One => ONE,
        WeekdayStyle::Two => TWO,
        WeekdayStyle::Three => THREE,
    };
    let shift = week_start.num_days_from_monday() as usize;
    let mut out = [""; 7];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = base[(i + shift) % 7];
    }
    out
}

/// Number of days in a month, leap years included.
pub fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = next_month(year, month)?;
    let next_first = NaiveDate::from_ymd_opt(next.0, next.1, 1)?;
    Some((next_first - first).num_days() as u32)
}

/// The first and last date of a month.
pub fn month_bounds(year: i32, month: u32) -> Option<(NaiveDate, NaiveDate)> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let last = NaiveDate::from_ymd_opt(year, month, days_in_month(year, month)?)?;
    Some((first, last))
}

/// The month after `(year, month)`, rolling the year.
pub fn next_month(year: i32, month: u32) -> Option<(i32, u32)> {
    if !(1..=12).contains(&month) {
        return None;
    }
    Some(if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month + 1)
    })
}

/// The month before `(year, month)`, rolling the year.
pub fn prev_month(year: i32, month: u32) -> Option<(i32, u32)> {
    if !(1..=12).contains(&month) {
        return None;
    }
    Some(if month == 1 {
        (year.checked_sub(1)?, 12)
    } else {
        (year, month - 1)
    })
}
