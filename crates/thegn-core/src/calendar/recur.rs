//! RFC 5545 recurrence: the `RRULE`/`EXDATE`/`RDATE` model and its expansion.
//!
//! # The one rule that matters
//!
//! **Iterate in local wall time, in the event's own zone, and convert to an
//! instant only at the very end.** Never add 86_400_000 ms. A weekly 09:00
//! America/Chicago meeting is 14:00Z in winter and 13:00Z in summer; advancing
//! by fixed durations makes it drift by an hour twice a year.
//!
//! Expansion is lazy and bounded by the query window, so an endless `RRULE`
//! costs nothing extra.

use std::collections::BTreeSet;

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, Timelike, Weekday};

use super::{EventTime, ExpansionBudget, ExpansionError, ExpansionLimit, TzRef};

/// How often a rule repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Freq {
    Secondly,
    Minutely,
    Hourly,
    Daily,
    #[default]
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    pub fn parse(s: &str) -> Option<Freq> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "SECONDLY" => Freq::Secondly,
            "MINUTELY" => Freq::Minutely,
            "HOURLY" => Freq::Hourly,
            "DAILY" => Freq::Daily,
            "WEEKLY" => Freq::Weekly,
            "MONTHLY" => Freq::Monthly,
            "YEARLY" => Freq::Yearly,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Freq::Secondly => "SECONDLY",
            Freq::Minutely => "MINUTELY",
            Freq::Hourly => "HOURLY",
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }
}

/// A `BYDAY` entry: a weekday, optionally the nth such weekday within the
/// period (`-1FR` = the last Friday).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByDay {
    pub nth: Option<i8>,
    pub weekday: Weekday,
}

/// A parsed `RRULE`.
///
/// Every `BY*` part is stored even when the expander doesn't act on it, so a
/// rule round-trips losslessly through the cache and the plugin wire rather
/// than being silently simplified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RRule {
    pub freq: Freq,
    pub interval: u32,
    pub count: Option<u32>,
    pub until: Option<NaiveDateTime>,
    pub by_second: Vec<u32>,
    pub by_minute: Vec<u32>,
    pub by_hour: Vec<u32>,
    pub by_day: Vec<ByDay>,
    pub by_month_day: Vec<i8>,
    pub by_year_day: Vec<i16>,
    pub by_week_no: Vec<i8>,
    pub by_month: Vec<u32>,
    pub by_set_pos: Vec<i32>,
    pub wkst: Weekday,
}

impl Default for RRule {
    fn default() -> Self {
        RRule {
            freq: Freq::Weekly,
            interval: 1,
            count: None,
            until: None,
            by_second: Vec::new(),
            by_minute: Vec::new(),
            by_hour: Vec::new(),
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            by_year_day: Vec::new(),
            by_week_no: Vec::new(),
            by_month: Vec::new(),
            by_set_pos: Vec::new(),
            wkst: Weekday::Mon,
        }
    }
}

/// Why a rule could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurError {
    /// The `FREQ` part was missing or unrecognised — the one truly required part.
    BadFreq(String),
    /// A numeric part would not parse.
    BadValue(String),
}

impl std::fmt::Display for RecurError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecurError::BadFreq(s) => write!(f, "unsupported or missing FREQ in {s:?}"),
            RecurError::BadValue(s) => write!(f, "invalid recurrence value {s:?}"),
        }
    }
}

/// Serialize an [`RRule`] as its iCalendar string rather than as a struct.
///
/// The plugin wire format and the cache both carry `"FREQ=WEEKLY;BYDAY=MO"` —
/// the spelling every calendar tool already speaks, and one a shell plugin can
/// emit by hand. A JSON object with twelve `by_*` arrays would be neither.
impl serde::Serialize for RRule {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rrule())
    }
}

impl<'de> serde::Deserialize<'de> for RRule {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        RRule::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The full recurrence description attached to an event.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Recurrence {
    pub rules: Vec<RRule>,
    /// Extra dates to include beyond the rules.
    pub rdates: Vec<EventTime>,
    /// Dates to exclude. Matched on the *local* recurrence-id, per RFC 5545.
    pub exdates: Vec<EventTime>,
}

impl Recurrence {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.rdates.is_empty()
    }
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn parse_by_day(s: &str) -> Option<ByDay> {
    let t = s.trim();
    if t.len() < 2 {
        return None;
    }
    let split = t.len() - 2;
    let (num, day) = t.split_at(split);
    let weekday = weekday(day)?;
    let nth = if num.is_empty() {
        None
    } else {
        Some(num.parse::<i8>().ok()?)
    };
    Some(ByDay { nth, weekday })
}

fn nums<T: std::str::FromStr>(v: &str) -> Result<Vec<T>, RecurError> {
    v.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse::<T>()
                .map_err(|_| RecurError::BadValue(s.to_string()))
        })
        .collect()
}

impl RRule {
    /// Parse an `RRULE` value (with or without the `RRULE:` prefix).
    pub fn parse(input: &str) -> Result<RRule, RecurError> {
        let body = input
            .trim()
            .strip_prefix("RRULE:")
            .unwrap_or_else(|| input.trim());
        let mut r = RRule::default();
        let mut saw_freq = false;
        for part in body.split(';') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let key = k.trim().to_ascii_uppercase();
            let v = v.trim();
            match key.as_str() {
                "FREQ" => {
                    r.freq = Freq::parse(v).ok_or_else(|| RecurError::BadFreq(v.to_string()))?;
                    saw_freq = true;
                }
                // RFC 5545: INTERVAL must be positive; 0 would be an infinite
                // loop, so treat it as the default rather than trusting it.
                "INTERVAL" => {
                    r.interval = v.parse::<u32>().unwrap_or(1).max(1);
                }
                "COUNT" => r.count = v.parse::<u32>().ok(),
                "UNTIL" => r.until = parse_ics_datetime(v),
                "BYSECOND" => r.by_second = nums(v)?,
                "BYMINUTE" => r.by_minute = nums(v)?,
                "BYHOUR" => r.by_hour = nums(v)?,
                "BYDAY" => r.by_day = v.split(',').filter_map(parse_by_day).collect(),
                "BYMONTHDAY" => r.by_month_day = nums(v)?,
                "BYYEARDAY" => r.by_year_day = nums(v)?,
                "BYWEEKNO" => r.by_week_no = nums(v)?,
                "BYMONTH" => r.by_month = nums(v)?,
                "BYSETPOS" => r.by_set_pos = nums(v)?,
                "WKST" => r.wkst = weekday(v).unwrap_or(Weekday::Mon),
                // Unknown parts are ignored rather than fatal: a newer RFC
                // extension shouldn't make an otherwise usable rule unusable.
                _ => {}
            }
        }
        if !saw_freq {
            return Err(RecurError::BadFreq(body.to_string()));
        }
        Ok(r)
    }

    /// Render back to an `RRULE` value.
    pub fn to_rrule(&self) -> String {
        let mut parts = vec![format!("FREQ={}", self.freq.as_str())];
        if self.interval != 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if let Some(c) = self.count {
            parts.push(format!("COUNT={c}"));
        }
        if let Some(u) = self.until {
            parts.push(format!("UNTIL={}", u.format("%Y%m%dT%H%M%SZ")));
        }
        let list = |name: &str, v: &[u32]| {
            (!v.is_empty()).then(|| {
                format!(
                    "{name}={}",
                    v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
                )
            })
        };
        parts.extend(list("BYSECOND", &self.by_second));
        parts.extend(list("BYMINUTE", &self.by_minute));
        parts.extend(list("BYHOUR", &self.by_hour));
        if !self.by_day.is_empty() {
            parts.push(format!(
                "BYDAY={}",
                self.by_day
                    .iter()
                    .map(|d| format!(
                        "{}{}",
                        d.nth.map(|n| n.to_string()).unwrap_or_default(),
                        wd_str(d.weekday)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let ilist = |name: &str, v: &[i8]| {
            (!v.is_empty()).then(|| {
                format!(
                    "{name}={}",
                    v.iter().map(i8::to_string).collect::<Vec<_>>().join(",")
                )
            })
        };
        parts.extend(ilist("BYMONTHDAY", &self.by_month_day));
        if !self.by_year_day.is_empty() {
            parts.push(format!(
                "BYYEARDAY={}",
                self.by_year_day
                    .iter()
                    .map(i16::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        parts.extend(ilist("BYWEEKNO", &self.by_week_no));
        parts.extend(list("BYMONTH", &self.by_month));
        if !self.by_set_pos.is_empty() {
            parts.push(format!(
                "BYSETPOS={}",
                self.by_set_pos
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if self.wkst != Weekday::Mon {
            parts.push(format!("WKST={}", wd_str(self.wkst)));
        }
        parts.join(";")
    }
}

fn wd_str(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

/// Parse an iCalendar `DATE` or `DATE-TIME` (`20260821`, `20260821T093000`,
/// `20260821T093000Z`).
pub fn parse_ics_datetime(s: &str) -> Option<NaiveDateTime> {
    let t = s.trim().trim_end_matches('Z');
    if let Ok(d) = NaiveDateTime::parse_from_str(t, "%Y%m%dT%H%M%S") {
        return Some(d);
    }
    NaiveDate::parse_from_str(t, "%Y%m%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// Local cap on one sub-daily walk, independent of the shared budget: a
/// SECONDLY rule over a month is millions of instants and no UI wants them.
/// Reaching it is a refusal ([`ExpansionError::Budget`]), never a silently
/// truncated list.
const MAX_SUBDAILY_STEPS: usize = 200_000;

fn work_exhausted() -> ExpansionError {
    ExpansionError::Budget(ExpansionLimit::RecurrenceWork)
}

/// Expand a recurrence into local wall times within `[from, to]` — test-only
/// convenience that swallows a refusal into an empty list. Production goes
/// through [`expand_local_bounded`].
#[cfg(test)]
pub fn expand_local(
    rec: &Recurrence,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<NaiveDateTime> {
    let mut budget = ExpansionBudget::default();
    expand_local_bounded(rec, start, from, to, &mut budget).unwrap_or_default()
}

/// Expand a recurrence into local wall times within `[from, to]`, charging
/// every period, candidate, and RDATE/EXDATE visited to `budget`.
///
/// `start` is DTSTART's local time — the seed for every unspecified field, per
/// RFC 5545 (a `FREQ=WEEKLY` with no `BYDAY` repeats on DTSTART's weekday, at
/// DTSTART's time).
///
/// Returns local `NaiveDateTime`s, deliberately *not* instants: the caller
/// converts through the event's own zone so DST is applied per occurrence.
///
/// A rule without COUNT is fast-forwarded to the window, so an endless rule
/// from decades ago costs about as much as one from last week. A COUNT rule is
/// walked from DTSTART (the skipped periods consume its COUNT); when that walk
/// would exceed the budget the answer is a typed refusal, not a guess.
pub fn expand_local_bounded(
    rec: &Recurrence,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
    budget: &mut ExpansionBudget,
) -> Result<Vec<NaiveDateTime>, ExpansionError> {
    if from > to {
        return Err(ExpansionError::InvalidWindow);
    }
    // Every RDATE and EXDATE is visited below, in or out of the window.
    let listed = rec
        .rdates
        .len()
        .checked_add(rec.exdates.len())
        .ok_or_else(work_exhausted)?;
    budget.recurrence_work(listed)?;
    let mut out: BTreeSet<NaiveDateTime> = BTreeSet::new();
    for rule in &rec.rules {
        out.extend(expand_rule(rule, start, from, to, budget)?);
    }
    // RDATEs are additional occurrences, independent of any rule.
    for r in &rec.rdates {
        if let Some(dt) = local_of(r)
            && dt.date() >= from
            && dt.date() <= to
        {
            out.insert(dt);
        }
    }
    // A rule-less event is its own single occurrence.
    if rec.rules.is_empty() && rec.rdates.is_empty() && start.date() >= from && start.date() <= to {
        out.insert(start);
    }
    // EXDATE matches the recurrence-id — the LOCAL value — not the instant.
    let ex: BTreeSet<NaiveDateTime> = rec.exdates.iter().filter_map(local_of).collect();
    Ok(out.into_iter().filter(|d| !ex.contains(d)).collect())
}

/// The local wall time an `EventTime` denotes, for EXDATE/RDATE matching.
fn local_of(t: &EventTime) -> Option<NaiveDateTime> {
    match t {
        EventTime::Date { date } => date.and_hms_opt(0, 0, 0),
        EventTime::Zoned { local, .. } => Some(*local),
        EventTime::Instant { at } => Some(at.naive_utc()),
    }
}

/// Per-rule work prices, computed once. Every `BY*` list is attacker-sized, so
/// the cost of walking them is charged, not just the candidates they produce.
struct RuleCost {
    /// Upper bound on the work to build one period's candidate dates.
    period: usize,
    /// Work to run the day-level filters over one candidate date.
    filter: usize,
}

impl RuleCost {
    fn of(r: &RRule) -> Option<RuleCost> {
        let by_day = r.by_day.len();
        let month_day = r.by_month_day.len();
        // One month under BYDAY walks ≤31 days per entry, and each produced
        // date (≤5 per entry) is filtered through BYMONTHDAY and BYMONTH.
        let per_month = by_day
            .checked_mul(31)?
            .checked_add(
                by_day
                    .checked_mul(5)?
                    .checked_mul(month_day.checked_add(r.by_month.len())?.max(1))?,
            )?
            .checked_add(month_day)?
            .checked_add(1)?;
        let dates = match r.freq {
            Freq::Secondly | Freq::Minutely | Freq::Hourly | Freq::Daily => 1,
            Freq::Weekly => by_day.max(1).checked_mul(7)?,
            Freq::Monthly => per_month,
            Freq::Yearly => r
                .by_month
                .len()
                .max(1)
                .checked_mul(per_month)?
                .checked_add(r.by_year_day.len())?
                .checked_add(r.by_week_no.len().checked_mul(by_day.max(1))?)?,
        };
        // Plus sorting the time lists and selecting BYSETPOS once per period.
        let period = dates
            .checked_add(r.by_hour.len())?
            .checked_add(r.by_minute.len())?
            .checked_add(r.by_second.len())?
            .checked_add(r.by_set_pos.len())?
            .checked_add(1)?;
        let filter = [
            r.by_month_day.len(),
            r.by_year_day.len(),
            r.by_day.len(),
            r.by_week_no.len(),
            r.by_month.len(),
        ]
        .into_iter()
        .try_fold(1usize, |n, len| n.checked_add(len))?;
        Some(RuleCost { period, filter })
    }
}

/// Expand one rule.
fn expand_rule(
    r: &RRule,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
    budget: &mut ExpansionBudget,
) -> Result<Vec<NaiveDateTime>, ExpansionError> {
    let cost = RuleCost::of(r).ok_or_else(work_exhausted)?;
    // Sub-daily rules step by a time unit, not by a calendar period, so their
    // INTERVAL means hours/minutes/seconds. Folding them into the day-stepping
    // loop below would read `HOURLY;INTERVAL=24` as "every 24 days".
    if matches!(r.freq, Freq::Secondly | Freq::Minutely | Freq::Hourly) {
        return expand_subdaily(r, start, from, to, budget, &cost);
    }
    let mut out = Vec::new();
    let mut emitted: u32 = 0;
    let mut period = initial_period(r, start.date(), from);

    loop {
        budget.recurrence_work(cost.period)?;
        for dt in period_candidates(r, start, period, budget, &cost)? {
            // Occurrences before DTSTART are never emitted, but they DO consume
            // COUNT in the sense that COUNT counts from DTSTART — so simply
            // skipping them is correct because they cannot precede it anyway.
            if dt < start {
                continue;
            }
            if let Some(u) = r.until
                && dt > u
            {
                return Ok(out);
            }
            if let Some(c) = r.count {
                if emitted >= c {
                    return Ok(out);
                }
                emitted += 1;
            }
            if dt.date() >= from && dt.date() <= to {
                out.push(dt);
            }
        }
        // Stop once a whole period is past the window. A period's candidates
        // can start up to a week before its nominal date (a WKST-aligned week,
        // an ISO week straddling New Year), so allow one week of spill. Nothing
        // after the window can matter — COUNT only ever removes later ones.
        if period
            .checked_sub_days(Days::new(7))
            .is_some_and(|p| p > to)
        {
            return Ok(out);
        }
        // The end of the representable calendar: nothing later exists.
        let Some(next) = advance(r, period) else {
            return Ok(out);
        };
        // Defensive: a non-advancing period would spin forever.
        if next <= period {
            return Err(work_exhausted());
        }
        period = next;
    }
}

/// The first period to walk.
///
/// COUNT must be walked from DTSTART, since every earlier period consumes it.
/// Without COUNT (UNTIL only caps the end) the earlier periods cannot affect
/// the window, so jump to the period containing `from` — then back one more
/// step, as slack for candidates that spill across a period boundary.
fn initial_period(r: &RRule, start: NaiveDate, from: NaiveDate) -> NaiveDate {
    if r.count.is_some() || from <= start {
        return start;
    }
    let interval = u64::from(r.interval.max(1));
    let jumped = match r.freq {
        Freq::Daily | Freq::Weekly => {
            let step = if r.freq == Freq::Weekly {
                interval.saturating_mul(7)
            } else {
                interval
            };
            let elapsed = u64::try_from(from.signed_duration_since(start).num_days()).unwrap_or(0);
            let periods = (elapsed / step).saturating_sub(1);
            // periods * step <= elapsed, so this cannot overflow.
            start.checked_add_days(Days::new(periods * step))
        }
        Freq::Monthly | Freq::Yearly => {
            let step = if r.freq == Freq::Yearly {
                interval.saturating_mul(12)
            } else {
                interval
            };
            let elapsed = (i64::from(from.year()) - i64::from(start.year())) * 12
                + i64::from(from.month())
                - i64::from(start.month());
            let elapsed = u64::try_from(elapsed).unwrap_or(0);
            let periods = (elapsed / step).saturating_sub(1);
            u32::try_from(periods * step)
                .ok()
                .and_then(|m| start.checked_add_months(Months::new(m)))
        }
        Freq::Secondly | Freq::Minutely | Freq::Hourly => None,
    };
    jumped.unwrap_or(start)
}

/// Expand a SECONDLY/MINUTELY/HOURLY rule.
///
/// These step the clock rather than the calendar, so the loop walks instants
/// from DTSTART (fast-forwarded to the window when there is no COUNT) and
/// every `BY*` part acts as a **filter** (RFC 5545: a part finer than the
/// frequency limits, it does not expand).
fn expand_subdaily(
    r: &RRule,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
    budget: &mut ExpansionBudget,
    cost: &RuleCost,
) -> Result<Vec<NaiveDateTime>, ExpansionError> {
    let unit: i64 = match r.freq {
        Freq::Secondly => 1,
        Freq::Minutely => 60,
        _ => 3_600,
    };
    let step_secs = unit
        .checked_mul(i64::from(r.interval.max(1)))
        .ok_or(ExpansionError::Arithmetic)?;
    let step = chrono::Duration::try_seconds(step_secs).ok_or(ExpansionError::Arithmetic)?;

    let mut cur = start;
    if r.count.is_none()
        && let Some(window_start) = from.and_hms_opt(0, 0, 0)
        && window_start > start
    {
        let elapsed = window_start.signed_duration_since(start).num_seconds();
        // (elapsed / step) * step <= elapsed, so neither can overflow.
        let skip = chrono::Duration::try_seconds((elapsed / step_secs) * step_secs)
            .ok_or(ExpansionError::Arithmetic)?;
        cur = start
            .checked_add_signed(skip)
            .ok_or(ExpansionError::Arithmetic)?;
    }

    let mut out = Vec::new();
    let mut emitted: u32 = 0;
    let mut steps = 0usize;
    loop {
        if steps >= MAX_SUBDAILY_STEPS {
            return Err(work_exhausted());
        }
        steps += 1;
        budget.recurrence_work(cost.filter)?;
        if let Some(u) = r.until
            && cur > u
        {
            break;
        }
        if cur.date() > to {
            break;
        }
        let keep = day_matches(r, cur.date())
            && (r.by_month.is_empty() || r.by_month.contains(&cur.month()))
            && (r.by_hour.is_empty() || r.by_hour.contains(&cur.hour()))
            && (r.by_minute.is_empty() || r.by_minute.contains(&cur.minute()))
            && (r.by_second.is_empty() || r.by_second.contains(&cur.second()));
        if keep {
            if let Some(c) = r.count {
                if emitted >= c {
                    break;
                }
                emitted += 1;
            }
            if cur.date() >= from {
                out.push(cur);
            }
        }
        // The end of the representable calendar: nothing later exists.
        let Some(next) = cur.checked_add_signed(step) else {
            break;
        };
        cur = next;
    }
    Ok(out)
}

/// Move to the next period start; `None` past the representable calendar.
fn advance(r: &RRule, period: NaiveDate) -> Option<NaiveDate> {
    let n = u64::from(r.interval.max(1));
    match r.freq {
        // Sub-daily frequencies never reach here (see `expand_subdaily`).
        Freq::Secondly | Freq::Minutely | Freq::Hourly | Freq::Daily => {
            period.checked_add_days(Days::new(n))
        }
        Freq::Weekly => period.checked_add_days(Days::new(n.checked_mul(7)?)),
        Freq::Monthly => period.checked_add_months(Months::new(r.interval.max(1))),
        Freq::Yearly => period.checked_add_months(Months::new(r.interval.max(1).checked_mul(12)?)),
    }
}

/// Every occurrence the rule produces within the period beginning at `period`.
///
/// The caller has already charged [`RuleCost::period`]; each candidate date's
/// filter pass and each expanded time is charged here, before it is built.
fn period_candidates(
    r: &RRule,
    start: NaiveDateTime,
    period: NaiveDate,
    budget: &mut ExpansionBudget,
    cost: &RuleCost,
) -> Result<Vec<NaiveDateTime>, ExpansionError> {
    let dates: Vec<NaiveDate> = match r.freq {
        Freq::Secondly | Freq::Minutely | Freq::Hourly | Freq::Daily => vec![period],
        Freq::Weekly => week_dates(r, start, period),
        Freq::Monthly => month_dates(r, start, period),
        Freq::Yearly => year_dates(r, start, period),
    };
    let filtering = dates
        .len()
        .checked_mul(cost.filter)
        .ok_or_else(work_exhausted)?;
    budget.recurrence_work(filtering)?;

    // BYMONTH filters every frequency except YEARLY, where it *expands*.
    let dates: Vec<NaiveDate> = dates
        .into_iter()
        .filter(|d| {
            (r.by_month.is_empty() || r.freq == Freq::Yearly || r.by_month.contains(&d.month()))
                && day_matches(r, *d)
        })
        .collect();

    // Times: BYHOUR/BYMINUTE/BYSECOND expand; unset fields inherit DTSTART's.
    let hours = or_default(&r.by_hour, start.hour());
    let minutes = or_default(&r.by_minute, start.minute());
    let seconds = or_default(&r.by_second, start.second());

    let mut out: Vec<NaiveDateTime> = Vec::new();
    for d in dates {
        for h in &hours {
            for mi in &minutes {
                for s in &seconds {
                    // Charged before growth: a BYHOUR×BYMINUTE×BYSECOND cross
                    // product is refused as it grows, not after.
                    budget.recurrence_work(1)?;
                    if let Some(t) = d.and_hms_opt(*h, *mi, *s) {
                        out.push(t);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();

    // BYSETPOS selects from the period's ordered candidate list — "the last
    // weekday of the month" is BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1.
    if !r.by_set_pos.is_empty() {
        let n = i64::try_from(out.len()).map_err(|_| work_exhausted())?;
        let picked: BTreeSet<usize> = r
            .by_set_pos
            .iter()
            .filter_map(|p| {
                let p = i64::from(*p);
                match p {
                    0 => None,
                    p if p > 0 && p <= n => usize::try_from(p - 1).ok(),
                    p if p < 0 && -p <= n => usize::try_from(n + p).ok(),
                    _ => None,
                }
            })
            .collect();
        return Ok(picked
            .into_iter()
            .filter_map(|i| out.get(i).copied())
            .collect());
    }
    Ok(out)
}

fn or_default(v: &[u32], fallback: u32) -> Vec<u32> {
    if v.is_empty() {
        vec![fallback]
    } else {
        let mut s: Vec<u32> = v.to_vec();
        s.sort_unstable();
        s.dedup();
        s
    }
}

/// Does this date pass the day-level filters that apply across frequencies?
fn day_matches(r: &RRule, d: NaiveDate) -> bool {
    // For MONTHLY/YEARLY these are expansions, handled by the *_dates
    // functions; here they act as filters for the finer frequencies.
    if matches!(r.freq, Freq::Monthly | Freq::Yearly) {
        return true;
    }
    if !r.by_month_day.is_empty() && !month_day_matches(&r.by_month_day, d) {
        return false;
    }
    if !r.by_year_day.is_empty() && !year_day_matches(&r.by_year_day, d) {
        return false;
    }
    if !r.by_day.is_empty()
        && !r
            .by_day
            .iter()
            .any(|bd| bd.nth.is_none() && bd.weekday == d.weekday())
        && r.freq != Freq::Weekly
    {
        return false;
    }
    if !r.by_week_no.is_empty() {
        let wk = d.iso_week().week() as i8;
        let weeks_in_year = weeks_in_iso_year(d.year()) as i8;
        if !r
            .by_week_no
            .iter()
            .any(|n| *n == wk || (*n < 0 && weeks_in_year + n + 1 == wk))
        {
            return false;
        }
    }
    true
}

fn month_day_matches(list: &[i8], d: NaiveDate) -> bool {
    let dim = days_in(d.year(), d.month()) as i32;
    let day = d.day() as i32;
    list.iter()
        .any(|n| (*n > 0 && *n as i32 == day) || (*n < 0 && dim + *n as i32 + 1 == day))
}

fn year_day_matches(list: &[i16], d: NaiveDate) -> bool {
    let diy = if is_leap(d.year()) { 366 } else { 365 };
    let doy = d.ordinal() as i32;
    list.iter()
        .any(|n| (*n > 0 && *n as i32 == doy) || (*n < 0 && diy + *n as i32 + 1 == doy))
}

/// The days a WEEKLY rule produces in the week beginning at `period`.
fn week_dates(r: &RRule, _start: NaiveDateTime, period: NaiveDate) -> Vec<NaiveDate> {
    if r.by_day.is_empty() {
        return vec![period];
    }
    // Walk the seven days from the week's WKST-aligned start so BYDAY order
    // doesn't dictate chronological order.
    let base = week_start_of(period, r.wkst);
    (0..7)
        .filter_map(|i| base.checked_add_days(Days::new(i)))
        .filter(|d| r.by_day.iter().any(|bd| bd.weekday == d.weekday()))
        .collect()
}

fn week_start_of(d: NaiveDate, wkst: Weekday) -> NaiveDate {
    let back = (d.weekday().num_days_from_monday() as i64 - wkst.num_days_from_monday() as i64)
        .rem_euclid(7) as u64;
    d.checked_sub_days(Days::new(back)).unwrap_or(d)
}

/// The days a MONTHLY rule produces in `period`'s month.
fn month_dates(r: &RRule, start: NaiveDateTime, period: NaiveDate) -> Vec<NaiveDate> {
    let (y, m) = (period.year(), period.month());
    let dim = days_in(y, m);
    let mut out: Vec<NaiveDate> = Vec::new();

    if !r.by_day.is_empty() {
        // BYDAY expands; BYMONTHDAY, when also present, then FILTERS. The
        // by-month-day dates must NOT be seeded separately as well, or
        // `BYDAY=FR;BYMONTHDAY=13` yields the 13th whatever weekday it is
        // instead of only Friday the 13th.
        for bd in &r.by_day {
            match bd.nth {
                None => out.extend(all_weekdays_in_month(y, m, bd.weekday)),
                Some(n) => {
                    if let Some(d) = nth_weekday_in_month(y, m, bd.weekday, n) {
                        out.push(d)
                    }
                }
            }
        }
        if !r.by_month_day.is_empty() {
            out.retain(|d| month_day_matches(&r.by_month_day, *d));
        }
    } else if !r.by_month_day.is_empty() {
        for n in &r.by_month_day {
            // RFC 5545: a BYMONTHDAY that doesn't exist in this month is
            // SKIPPED, not clamped — `BYMONTHDAY=31` simply produces nothing in
            // February, April, June, September and November.
            let day = if *n > 0 {
                *n as i64
            } else {
                dim as i64 + *n as i64 + 1
            };
            if day >= 1
                && day <= dim as i64
                && let Some(d) = NaiveDate::from_ymd_opt(y, m, day as u32)
            {
                out.push(d);
            }
        }
    } else {
        // Neither part: repeat on DTSTART's day-of-month, skipping months too
        // short to contain it (the same rule as BYMONTHDAY).
        if let Some(d) = NaiveDate::from_ymd_opt(y, m, start.day()) {
            out.push(d);
        }
    }
    if !r.by_month.is_empty() {
        out.retain(|d| r.by_month.contains(&d.month()));
    }
    out.sort();
    out.dedup();
    out
}

/// The days a YEARLY rule produces in `period`'s year.
fn year_dates(r: &RRule, start: NaiveDateTime, period: NaiveDate) -> Vec<NaiveDate> {
    let y = period.year();
    let mut out: Vec<NaiveDate> = Vec::new();

    // BYYEARDAY and BYWEEKNO are the explicit-ordinal forms. When either is
    // present it is authoritative: if it matches nothing this year, the answer
    // is nothing. Falling through to the DTSTART default would silently invent
    // an occurrence the rule never asked for.
    let ordinal_form = !r.by_year_day.is_empty() || !r.by_week_no.is_empty();

    if !r.by_year_day.is_empty() {
        let diy = if is_leap(y) { 366 } else { 365 };
        for n in &r.by_year_day {
            let doy = if *n > 0 {
                *n as i32
            } else {
                diy + *n as i32 + 1
            };
            if doy >= 1
                && doy <= diy
                && let Some(d) = NaiveDate::from_yo_opt(y, doy as u32)
            {
                out.push(d);
            }
        }
    }

    if !r.by_week_no.is_empty() {
        let weeks = weeks_in_iso_year(y) as i32;
        for n in &r.by_week_no {
            let wk = if *n > 0 {
                *n as i32
            } else {
                weeks + *n as i32 + 1
            };
            if wk < 1 || wk > weeks {
                continue;
            }
            let days: Vec<Weekday> = if r.by_day.is_empty() {
                vec![start.weekday()]
            } else {
                r.by_day.iter().map(|b| b.weekday).collect()
            };
            for wd in days {
                if let Some(d) = NaiveDate::from_isoywd_opt(y, wk as u32, wd) {
                    out.push(d);
                }
            }
        }
    }

    if out.is_empty() && !ordinal_form {
        // The common shapes: BYMONTH (+ BYMONTHDAY / BYDAY), else DTSTART's
        // month and day.
        let months: Vec<u32> = if r.by_month.is_empty() {
            vec![start.month()]
        } else {
            r.by_month.clone()
        };
        for m in months {
            if !r.by_day.is_empty() {
                for bd in &r.by_day {
                    match bd.nth {
                        None => out.extend(all_weekdays_in_month(y, m, bd.weekday)),
                        Some(n) => {
                            if let Some(d) = nth_weekday_in_month(y, m, bd.weekday, n) {
                                out.push(d)
                            }
                        }
                    }
                }
                if !r.by_month_day.is_empty() {
                    out.retain(|d| month_day_matches(&r.by_month_day, *d));
                }
            } else if !r.by_month_day.is_empty() {
                let dim = days_in(y, m);
                for n in &r.by_month_day {
                    let day = if *n > 0 {
                        *n as i64
                    } else {
                        dim as i64 + *n as i64 + 1
                    };
                    if day >= 1
                        && day <= dim as i64
                        && let Some(d) = NaiveDate::from_ymd_opt(y, m, day as u32)
                    {
                        out.push(d);
                    }
                }
            } else {
                // Feb 29 yearly exists only in leap years — skipped elsewhere,
                // never clamped to the 28th.
                if let Some(d) = NaiveDate::from_ymd_opt(y, m, start.day()) {
                    out.push(d);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn all_weekdays_in_month(y: i32, m: u32, wd: Weekday) -> Vec<NaiveDate> {
    let dim = days_in(y, m);
    (1..=dim)
        .filter_map(|d| NaiveDate::from_ymd_opt(y, m, d))
        .filter(|d| d.weekday() == wd)
        .collect()
}

fn nth_weekday_in_month(y: i32, m: u32, wd: Weekday, n: i8) -> Option<NaiveDate> {
    let all = all_weekdays_in_month(y, m, wd);
    if n > 0 {
        all.get((n - 1) as usize).copied()
    } else if n < 0 {
        let idx = all.len() as i32 + n as i32;
        (idx >= 0).then(|| all.get(idx as usize).copied()).flatten()
    } else {
        None
    }
}

pub fn days_in(y: i32, m: u32) -> u32 {
    super::grid::days_in_month(y, m).unwrap_or(30)
}

fn is_leap(y: i32) -> bool {
    NaiveDate::from_ymd_opt(y, 2, 29).is_some()
}

/// ISO weeks in a year: 52, or 53 when the year has one.
fn weeks_in_iso_year(y: i32) -> u32 {
    NaiveDate::from_ymd_opt(y, 12, 28)
        .map(|d| d.iso_week().week())
        .unwrap_or(52)
}

/// The local wall time a recurrence is seeded from: DTSTART's own wall time
/// (a floating date at midnight; an instant as seen from `home`).
pub(crate) fn seed_of(start: &EventTime, home: chrono_tz::Tz) -> Option<NaiveDateTime> {
    match start {
        EventTime::Zoned { local, .. } => Some(*local),
        EventTime::Date { date } => date.and_hms_opt(0, 0, 0),
        EventTime::Instant { at } => Some(at.with_timezone(&home).naive_local()),
    }
}

/// One expanded wall time as an event time of DTSTART's shape: a floating date
/// stays a date; anything else becomes a wall time in the event's own zone
/// (or `home`, for an instant-seeded rule).
pub(crate) fn instance_time(
    local: NaiveDateTime,
    start: &EventTime,
    home: chrono_tz::Tz,
) -> EventTime {
    match start {
        EventTime::Date { .. } => EventTime::Date { date: local.date() },
        EventTime::Zoned { zone, .. } => EventTime::Zoned {
            local,
            zone: zone.clone(),
        },
        EventTime::Instant { .. } => EventTime::Zoned {
            local,
            zone: TzRef::new(home.name()),
        },
    }
}

/// Expand a recurring event into event times within a date window —
/// test-only; production materializes instances one at a time in
/// [`super::CalEvent::occurrences_bounded`].
///
/// Wall times come from [`expand_local_bounded`]; each is then resolved
/// through the event's own zone, so a DST boundary shifts the *instant* while
/// the local time stays put.
#[cfg(test)]
pub fn occurrences(
    rec: &Recurrence,
    start: &EventTime,
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
    gap: super::GapPolicy,
) -> Vec<EventTime> {
    let mut budget = ExpansionBudget::default();
    let Some(seed) = seed_of(start, home) else {
        return Vec::new();
    };
    expand_local_bounded(rec, seed, from, to, &mut budget)
        .unwrap_or_default()
        .into_iter()
        .map(|local| instance_time(local, start, home))
        // Under `GapPolicy::Skip` an occurrence that falls in a spring-forward
        // gap genuinely does not happen, so it is dropped rather than nudged.
        .filter(|t| t.is_all_day() || t.instant_in(home, gap).is_some())
        .collect()
}

#[cfg(test)]
#[path = "recur_tests.rs"]
mod tests;
