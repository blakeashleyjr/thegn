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

use super::{EventTime, GapPolicy, TzRef};

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

/// A safety valve on pathological rules (`BYSETPOS` over a year of candidates
/// that never matches). Expansion is already window-bounded; this bounds the
/// *empty* periods it will skip through before giving up.
const MAX_EMPTY_PERIODS: usize = 3_000;

/// Expand a recurrence into local wall times within `[from, to]`.
///
/// `start` is DTSTART's local time — the seed for every unspecified field, per
/// RFC 5545 (a `FREQ=WEEKLY` with no `BYDAY` repeats on DTSTART's weekday, at
/// DTSTART's time).
///
/// Returns local `NaiveDateTime`s, deliberately *not* instants: the caller
/// converts through the event's own zone so DST is applied per occurrence.
pub fn expand_local(
    rec: &Recurrence,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<NaiveDateTime> {
    let mut out: BTreeSet<NaiveDateTime> = BTreeSet::new();
    for rule in &rec.rules {
        for dt in expand_rule(rule, start, from, to) {
            out.insert(dt);
        }
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
    out.into_iter().filter(|d| !ex.contains(d)).collect()
}

/// The local wall time an `EventTime` denotes, for EXDATE/RDATE matching.
fn local_of(t: &EventTime) -> Option<NaiveDateTime> {
    match t {
        EventTime::Date { date } => date.and_hms_opt(0, 0, 0),
        EventTime::Zoned { local, .. } => Some(*local),
        EventTime::Instant { at } => Some(at.naive_utc()),
    }
}

/// Expand one rule.
fn expand_rule(
    r: &RRule,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<NaiveDateTime> {
    // Sub-daily rules step by a time unit, not by a calendar period, so their
    // INTERVAL means hours/minutes/seconds. Folding them into the day-stepping
    // loop below would read `HOURLY;INTERVAL=24` as "every 24 days".
    if matches!(r.freq, Freq::Secondly | Freq::Minutely | Freq::Hourly) {
        return expand_subdaily(r, start, from, to);
    }
    let mut out = Vec::new();
    let mut emitted: u32 = 0;
    let mut empty_periods = 0usize;
    let mut period = start.date();

    loop {
        let candidates = period_candidates(r, start, period);
        if candidates.is_empty() {
            empty_periods += 1;
        } else {
            empty_periods = 0;
        }

        for dt in candidates {
            // Occurrences before DTSTART are never emitted, but they DO consume
            // COUNT in the sense that COUNT counts from DTSTART — so simply
            // skipping them is correct because they cannot precede it anyway.
            if dt < start {
                continue;
            }
            if let Some(u) = r.until
                && dt > u
            {
                return out;
            }
            if let Some(c) = r.count {
                if emitted >= c {
                    return out;
                }
                emitted += 1;
            }
            if dt.date() > to {
                // Past the window: nothing later can be in it either, and
                // without COUNT there is no reason to keep walking.
                if r.count.is_none() {
                    return out;
                }
                continue;
            }
            if dt.date() >= from {
                out.push(dt);
            }
        }

        // Stop once the whole period is past the window (and no COUNT budget
        // still has to be walked off).
        if period > to && r.count.is_none() {
            return out;
        }
        if empty_periods > MAX_EMPTY_PERIODS {
            return out;
        }
        let Some(next) = advance(r, period) else {
            return out;
        };
        // Defensive: a non-advancing period would spin forever.
        if next <= period {
            return out;
        }
        period = next;
    }
}

/// Expand a SECONDLY/MINUTELY/HOURLY rule.
///
/// These step the clock rather than the calendar, so the loop walks instants
/// from DTSTART and every `BY*` part acts as a **filter** (RFC 5545: a part
/// finer than the frequency limits, it does not expand).
fn expand_subdaily(
    r: &RRule,
    start: NaiveDateTime,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<NaiveDateTime> {
    let step = match r.freq {
        Freq::Secondly => chrono::Duration::seconds(r.interval.max(1) as i64),
        Freq::Minutely => chrono::Duration::minutes(r.interval.max(1) as i64),
        _ => chrono::Duration::hours(r.interval.max(1) as i64),
    };
    // Bound the walk: a SECONDLY rule over a month is millions of instants, and
    // no calendar UI wants them. This is the one place the expander refuses
    // rather than obeying.
    const MAX_STEPS: usize = 200_000;

    let mut out = Vec::new();
    let mut emitted: u32 = 0;
    let mut cur = start;
    for _ in 0..MAX_STEPS {
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
        let Some(next) = cur.checked_add_signed(step) else {
            break;
        };
        cur = next;
    }
    out
}

/// Move to the next period start.
fn advance(r: &RRule, period: NaiveDate) -> Option<NaiveDate> {
    let n = r.interval.max(1) as u64;
    match r.freq {
        // Sub-daily frequencies never reach here (see `expand_subdaily`).
        Freq::Secondly | Freq::Minutely | Freq::Hourly | Freq::Daily => {
            period.checked_add_days(Days::new(n))
        }
        Freq::Weekly => period.checked_add_days(Days::new(n * 7)),
        Freq::Monthly => period.checked_add_months(Months::new(r.interval.max(1))),
        Freq::Yearly => period.checked_add_months(Months::new(r.interval.max(1) * 12)),
    }
}

/// Every occurrence the rule produces within the period beginning at `period`.
fn period_candidates(r: &RRule, start: NaiveDateTime, period: NaiveDate) -> Vec<NaiveDateTime> {
    let dates: Vec<NaiveDate> = match r.freq {
        Freq::Secondly | Freq::Minutely | Freq::Hourly | Freq::Daily => vec![period],
        Freq::Weekly => week_dates(r, start, period),
        Freq::Monthly => month_dates(r, start, period),
        Freq::Yearly => year_dates(r, start, period),
    };

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
        let n = out.len() as i32;
        let picked: BTreeSet<usize> = r
            .by_set_pos
            .iter()
            .filter_map(|p| match *p {
                0 => None,
                p if p > 0 && p <= n => Some((p - 1) as usize),
                p if p < 0 && -p <= n => Some((n + p) as usize),
                _ => None,
            })
            .collect();
        return picked
            .into_iter()
            .filter_map(|i| out.get(i).copied())
            .collect();
    }
    out
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
fn week_dates(r: &RRule, start: NaiveDateTime, period: NaiveDate) -> Vec<NaiveDate> {
    if r.by_day.is_empty() {
        return vec![period];
    }
    // Walk the seven days from the week's WKST-aligned start so BYDAY order
    // doesn't dictate chronological order.
    let base = week_start_of(period, r.wkst);
    let _ = start;
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

/// Expand a recurring event into absolute instants within a date window.
///
/// Wall times come from [`expand_local`]; each is then resolved through the
/// event's own zone, so a DST boundary shifts the *instant* while the local
/// time stays put.
pub fn occurrences(
    rec: &Recurrence,
    start: &EventTime,
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
    gap: GapPolicy,
) -> Vec<EventTime> {
    let (seed, zone) = match start {
        EventTime::Zoned { local, zone } => (*local, Some(zone.clone())),
        EventTime::Date { date } => match date.and_hms_opt(0, 0, 0) {
            Some(d) => (d, None),
            None => return Vec::new(),
        },
        EventTime::Instant { at } => (at.with_timezone(&home).naive_local(), None),
    };
    let all_day = matches!(start, EventTime::Date { .. });
    expand_local(rec, seed, from, to)
        .into_iter()
        .map(|local| {
            if all_day {
                EventTime::Date { date: local.date() }
            } else {
                EventTime::Zoned {
                    local,
                    zone: zone.clone().unwrap_or_else(|| TzRef::new(home.name())),
                }
            }
        })
        // Under `GapPolicy::Skip` an occurrence that falls in a spring-forward
        // gap genuinely does not happen, so it is dropped rather than nudged.
        .filter(|t| t.is_all_day() || t.instant_in(home, gap).is_some())
        .collect()
}

#[cfg(test)]
#[path = "recur_tests.rs"]
mod tests;
