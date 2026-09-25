//! The month grid: a 7-column matrix of day cells with leading/trailing days
//! from the neighbouring months, plus configured-start week numbers.

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

    /// The configured-start week number for each row.
    ///
    /// Week 1 is the configured-start week containing January 4, the same
    /// four-day minimum-days rule used by ISO-8601. A Monday-first grid is
    /// therefore ISO-8601; Sunday- and Saturday-first grids use the same rule
    /// with their configured row boundary.
    pub fn week_numbers(&self) -> Vec<u32> {
        self.weeks
            .iter()
            .map(|week| configured_week_number(week, self.week_start))
            .collect()
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

/// Find the configured-start date of week 1 for `year`.
fn week_one_start(year: i32, week_start: Weekday) -> Option<NaiveDate> {
    let jan4 = NaiveDate::from_ymd_opt(year, 1, 4)?;
    let days_back =
        jan4.weekday().num_days_from_monday() as i64 - week_start.num_days_from_monday() as i64;
    jan4.checked_sub_days(Days::new(days_back.rem_euclid(7) as u64))
}

/// Calculate a row's configured-start week number without changing the
/// per-cell ISO values kept in [`DayCell::iso_week`].
fn configured_week_number(row: &[DayCell; 7], week_start: Weekday) -> u32 {
    let row_start = row[0].date;
    let row_year = row_start.year();

    // A row can begin in the previous, current, or next week-year relative to
    // its first cell's calendar year. All year arithmetic is checked because
    // NaiveDate's representable range is much smaller than i32's.
    let candidates = [
        row_year.checked_sub(1),
        Some(row_year),
        row_year.checked_add(1),
    ];
    for candidate in candidates.into_iter().flatten() {
        let Some(week_one) = week_one_start(candidate, week_start) else {
            // At chrono's minimum date, the configured week-one boundary can
            // be a few conceptual days before NaiveDate::MIN. Keep that
            // boundary arithmetic exact without constructing an unrepresentable
            // date. A valid grid row can only reach this case in the minimum
            // calendar year.
            if candidate == NaiveDate::MIN.year() && row_year == candidate {
                let jan4 = NaiveDate::from_ymd_opt(candidate, 1, 4);
                let Some(jan4) = jan4 else {
                    continue;
                };
                let days_back = (jan4.weekday().num_days_from_monday() as i64
                    - week_start.num_days_from_monday() as i64)
                    .rem_euclid(7);
                let distance =
                    row_start.signed_duration_since(NaiveDate::MIN).num_days() + days_back;
                return week_number_from_distance(distance);
            }
            continue;
        };

        let in_week_year = match candidate
            .checked_add(1)
            .and_then(|next| week_one_start(next, week_start))
        {
            Some(next_week_one) => row_start >= week_one && row_start < next_week_one,
            // No representable next year means this is the final possible
            // week-year, so its upper bound is the end of NaiveDate's range.
            None => row_start >= week_one,
        };
        if in_week_year {
            return week_number_from_distance(row_start.signed_duration_since(week_one).num_days());
        }
    }

    // MonthGrid::build creates contiguous seven-day rows, so one of the
    // checked candidates above always owns a valid row. Keep this method total
    // for manually-constructed MonthGrid values as well; this branch is not a
    // possible result for a grid returned by build().
    row_start.iso_week().week()
}

fn week_number_from_distance(days_from_week_one: i64) -> u32 {
    let week = days_from_week_one.div_euclid(7).saturating_add(1);
    // A valid Gregorian year has at most 53 such weeks. The saturating
    // fallback keeps the public Vec<u32> API total even for a malformed
    // hand-built grid.
    u32::try_from(week).unwrap_or(u32::MAX)
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
