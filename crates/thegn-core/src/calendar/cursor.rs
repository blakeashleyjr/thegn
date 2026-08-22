//! The calendar selection cursor: which day is selected and which month the
//! grid is showing.
//!
//! Pure state, so the popup can page months instantly without any round trip
//! to the event loop or a provider.

use super::grid::{self, MonthGrid};
use chrono::{Datelike, Days, NaiveDate, Weekday};

/// One navigation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalNav {
    PrevDay,
    NextDay,
    PrevWeek,
    NextWeek,
    PrevMonth,
    NextMonth,
    PrevYear,
    NextYear,
    FirstOfMonth,
    LastOfMonth,
    Today,
    Goto(NaiveDate),
}

/// Selected day + visible month.
///
/// The two are related but not identical: paging to a month you then leave
/// without selecting anything keeps the selection where it was, and moving the
/// day cursor across a month boundary pulls the visible month along with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalCursor {
    selected: NaiveDate,
    view: (i32, u32),
    /// The day-of-month the user last *chose*, as opposed to the one a short
    /// month clamped them to.
    ///
    /// Without this, paging Jan 31 → Feb (clamps to 28) → Mar lands on Mar 28.
    /// Real calendars land on Mar 31, because the intent was "the 31st".
    anchor_dom: u32,
}

impl CalCursor {
    /// Start with `date` selected and its month in view.
    pub fn new(date: NaiveDate) -> Self {
        CalCursor {
            selected: date,
            view: (date.year(), date.month()),
            anchor_dom: date.day(),
        }
    }

    pub fn selected(&self) -> NaiveDate {
        self.selected
    }

    /// The `(year, month)` the grid is showing.
    pub fn visible_month(&self) -> (i32, u32) {
        self.view
    }

    /// Build the grid for the visible month.
    pub fn grid(
        &self,
        week_start: Weekday,
        today: NaiveDate,
        six_weeks: bool,
    ) -> Option<MonthGrid> {
        MonthGrid::build(self.view.0, self.view.1, week_start, today, six_weeks)
    }

    /// The date span the visible grid covers, borrowed days included — what an
    /// event query must be keyed on.
    pub fn visible_range(
        &self,
        week_start: Weekday,
        six_weeks: bool,
    ) -> Option<(NaiveDate, NaiveDate)> {
        // `today` only affects an is-today flag, never geometry.
        let g = MonthGrid::build(
            self.view.0,
            self.view.1,
            week_start,
            self.selected,
            six_weeks,
        )?;
        Some(g.span())
    }

    /// Apply a navigation step. Returns whether anything actually changed.
    pub fn apply(&mut self, nav: CalNav, today: NaiveDate) -> bool {
        let before = (self.selected, self.view, self.anchor_dom);
        match nav {
            CalNav::PrevDay => self.shift_days(-1),
            CalNav::NextDay => self.shift_days(1),
            CalNav::PrevWeek => self.shift_days(-7),
            CalNav::NextWeek => self.shift_days(7),
            CalNav::PrevMonth => self.shift_months(-1),
            CalNav::NextMonth => self.shift_months(1),
            CalNav::PrevYear => self.shift_months(-12),
            CalNav::NextYear => self.shift_months(12),
            CalNav::FirstOfMonth => self.select(self.with_day(1)),
            CalNav::LastOfMonth => {
                let last = grid::days_in_month(self.view.0, self.view.1).unwrap_or(28);
                self.select(self.with_day(last));
            }
            CalNav::Today => self.select(Some(today)),
            CalNav::Goto(d) => self.select(Some(d)),
        }
        before != (self.selected, self.view, self.anchor_dom)
    }

    /// Move the selection by whole days, dragging the view along.
    ///
    /// A day step is an explicit choice of that day, so it re-anchors: stepping
    /// off Jan 31 onto Feb 1 and then paging months should track the 1st.
    fn shift_days(&mut self, delta: i64) {
        let next = if delta >= 0 {
            self.selected.checked_add_days(Days::new(delta as u64))
        } else {
            self.selected
                .checked_sub_days(Days::new(delta.unsigned_abs()))
        };
        self.select(next);
    }

    /// Page the view by whole months, carrying the selection with it.
    ///
    /// The selected day-of-month comes from `anchor_dom`, clamped to the target
    /// month's length — and `anchor_dom` is deliberately *not* updated, so the
    /// clamp is remembered as temporary.
    fn shift_months(&mut self, delta: i32) {
        let total = self.view.0 as i64 * 12 + (self.view.1 as i64 - 1) + delta as i64;
        let (y, m) = (
            (total.div_euclid(12)) as i32,
            (total.rem_euclid(12) + 1) as u32,
        );
        let len = grid::days_in_month(y, m).unwrap_or(28);
        let day = self.anchor_dom.min(len);
        if let Some(d) = NaiveDate::from_ymd_opt(y, m, day) {
            self.selected = d;
            self.view = (y, m);
            // anchor_dom intentionally preserved.
        }
    }

    /// Select an explicit date: moves the view to match and re-anchors.
    fn select(&mut self, date: Option<NaiveDate>) {
        let Some(d) = date else { return };
        self.selected = d;
        self.view = (d.year(), d.month());
        self.anchor_dom = d.day();
    }

    fn with_day(&self, day: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(self.view.0, self.view.1, day)
    }
}
