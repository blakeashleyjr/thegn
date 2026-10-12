//! IANA zone resolution and world-clock readings.
//!
//! All offsets and deltas are computed **at a given instant**, never stored as
//! constants. That is what makes India (+5:30), Nepal (+5:45), Lord Howe
//! (+10:30) and every DST shoulder season correct for free instead of needing
//! special cases.

use chrono::{DateTime, Datelike, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::{OffsetComponents, Tz};
use serde::{Deserialize, Serialize};

/// What to do with a local wall time that does not exist, i.e. one that falls
/// in the hour skipped by a spring-forward transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GapPolicy {
    /// Shift forward past the gap: an 02:30 alarm fires at 03:30. What every
    /// mainstream calendar client does, so it is the default.
    #[default]
    ShiftForward,
    /// Treat the occurrence as not happening at all.
    Skip,
    /// Clamp back to the last instant before the gap.
    Earliest,
}

/// A reference to an IANA zone by name.
///
/// Deliberately a `String` rather than a resolved [`Tz`]: a zone this build's
/// bundled database does not know still round-trips through the cache and the
/// plugin wire instead of failing deserialization and poisoning the whole
/// payload. Resolution happens at use time via [`TzRef::resolve`].
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct TzRef(pub String);

impl TzRef {
    pub fn new(name: impl Into<String>) -> Self {
        TzRef(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Resolve to a real zone, or `None` if this build doesn't know the name.
    pub fn resolve(&self) -> Option<Tz> {
        resolve_tzid(&self.0)
    }
}

impl std::fmt::Display for TzRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Look up an IANA zone name, case-insensitively.
///
/// Exact match first (the overwhelmingly common path, and a plain table probe),
/// then a case-insensitive sweep so `america/new_york` resolves rather than
/// being reported as unknown.
pub fn resolve_zone(name: &str) -> Option<Tz> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    if let Ok(tz) = n.parse::<Tz>() {
        return Some(tz);
    }
    chrono_tz::TZ_VARIANTS
        .iter()
        .find(|tz| tz.name().eq_ignore_ascii_case(n))
        .copied()
}

const WINDOWS_TZ_ALIASES: &[(&str, &str)] = &[
    ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"),
    ("Mountain Standard Time", "America/Denver"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("GMT Standard Time", "Europe/London"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("China Standard Time", "Asia/Shanghai"),
];

/// Resolve an iCalendar TZID, including a small set of CLDR Windows aliases
/// and vendor paths that suffix an IANA zone with a provider namespace.
pub fn resolve_tzid(name: &str) -> Option<Tz> {
    let name = name.trim();
    if let Some(zone) = resolve_zone(name) {
        return Some(zone);
    }
    if let Some((_, target)) = WINDOWS_TZ_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
    {
        return resolve_zone(target);
    }
    if name.starts_with('/') {
        let parts: Vec<&str> = name.trim_start_matches('/').split('/').collect();
        for start in 0..parts.len().saturating_sub(1) {
            let candidate = parts[start..].join("/");
            if let Some(zone) = resolve_zone(&candidate) {
                return Some(zone);
            }
        }
    }
    None
}

/// Suggest zone names close to a typo, best first.
///
/// Used by config validation to turn a rejected zone into an actionable
/// `did you mean "America/New_York"?`. Exact and city-name matches rank first,
/// then substring hits, then edit distance.
///
/// Note the substring test only runs one way — does a zone name contain what
/// the user typed. The reverse would match any short zone that happens to
/// appear inside the input: `"america/new_yrok"` contains `"rok"`, and `ROK` is
/// a real zone, so it would beat the answer the user actually wanted.
pub fn suggest_zones(name: &str, limit: usize) -> Vec<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() || limit == 0 {
        return Vec::new();
    }
    // The city segment is what users actually typo; matching on it catches
    // "New_York" when the region half is wrong or missing.
    let tail = n.rsplit('/').next().unwrap_or(&n).to_string();
    let mut scored: Vec<(u8, &'static str)> = chrono_tz::TZ_VARIANTS
        .iter()
        .filter_map(|tz| {
            let full = tz.name();
            let lower = full.to_ascii_lowercase();
            let ltail = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            let rank = if lower == n {
                0
            } else if ltail == tail {
                1
            } else if lower.contains(&n) {
                2
            } else if tail.len() >= 3 && ltail.contains(&tail) {
                3
            } else {
                return None;
            };
            Some((rank, full))
        })
        .collect();
    if scored.is_empty() {
        // No exact/substring hit — the name is genuinely misspelled rather than
        // merely miscased. Fall back to the fuzzy matcher so `America/New_Yrok`
        // still points at `America/New_York`; substring matching alone can
        // never recover a transposition.
        return fuzzy_zones(&tail, limit);
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, n)| n).collect()
}

/// Nearest zones by edit distance, for a genuinely misspelled name.
///
/// Deliberately edit distance rather than the SIMD subsequence matcher used for
/// typeahead elsewhere: subsequence scoring answers "does this contain those
/// letters in order", which for `new_yrok` happily returns `ROK`. Spelling
/// correction needs "how many keystrokes away is this".
fn fuzzy_zones(needle: &str, limit: usize) -> Vec<&'static str> {
    // Generous enough for a transposition or two typos, tight enough that an
    // unrelated word suggests nothing at all.
    let budget = (needle.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &'static str)> = chrono_tz::TZ_VARIANTS
        .iter()
        .filter_map(|tz| {
            let full = tz.name();
            let lower = full.to_ascii_lowercase();
            let tail = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            // Score against the city alone and the whole name, whichever is
            // closer — the user may have typed either.
            let dist = edit_distance(needle, &tail, budget)
                .into_iter()
                .chain(edit_distance(needle, &lower, budget))
                .min()?;
            Some((dist, full))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, n)| n).collect()
}

/// Optimal-string-alignment distance, or `None` once it provably exceeds
/// `budget`.
///
/// OSA rather than plain Levenshtein so a transposition (`toyko` → `tokyo`)
/// costs 1, not 2 — transpositions are the single most common typing slip, and
/// charging 2 for them forces a threshold loose enough to admit noise.
fn edit_distance(a: &str, b: &str, budget: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    // A length gap alone already exceeds the budget; skip the matrix.
    if a.len().abs_diff(b.len()) > budget {
        return None;
    }
    // Three rows rotate through each other, so all three must be full length —
    // the rotation hands the oldest buffer back as the next `cur`.
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let sub = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + sub);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        // Every future row is >= this row's minimum, so we can stop early.
        if row_min > budget {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.last().copied().filter(|d| *d <= budget)
}

/// Turn a local wall time into an instant, resolving DST edges.
///
/// - Unambiguous: use it.
/// - Ambiguous (the repeated hour at fall-back, e.g. 01:30 twice): take the
///   **earlier** of the two, which is what RFC 5545 and every major client do.
/// - Nonexistent (the skipped hour at spring-forward): apply `gap`.
pub fn resolve_local(local: NaiveDateTime, zone: Tz, gap: GapPolicy) -> Option<DateTime<Utc>> {
    match zone.from_local_datetime(&local) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, _later) => Some(earlier.with_timezone(&Utc)),
        LocalResult::None => match gap {
            GapPolicy::Skip => None,
            // Both policies derive the offsets from the real transition around
            // `local` (see `gap_transition`), never by reading `local`'s
            // numeric fields as UTC.
            //
            // ShiftForward interprets the wall time against the pre-transition
            // offset, so 02:15, 02:30 and 02:45 keep their position within the
            // gap (a scan for the first valid instant would collapse them onto
            // one moment) and the shift is the zone's real width: 30 minutes
            // at Lord Howe, a whole date at Apia.
            GapPolicy::ShiftForward => {
                let (_, before) = gap_transition(local, zone)?;
                Some(DateTime::from_naive_utc_and_offset(local - before, Utc))
            }
            // Earliest clamps to the last whole second before the transition.
            GapPolicy::Earliest => {
                let (at, _) = gap_transition(local, zone)?;
                Some(DateTime::from_naive_utc_and_offset(
                    at - chrono::Duration::seconds(1),
                    Utc,
                ))
            }
        },
    }
}

/// For a nonexistent `local`, the UTC instant of the transition that skips it
/// and the total offset in force just before it.
///
/// Every real offset lies within -12h..+14h of UTC, so the transition that
/// swallows `local` lies within `local - 14h .. local + 12h` read as UTC. We
/// only use that as a *search window*: the offsets are sampled at its far ends
/// and the transition is bisected to the second, so no result is derived from
/// reinterpreting local fields as UTC. `None` if the window shows no change.
fn gap_transition(local: NaiveDateTime, zone: Tz) -> Option<(NaiveDateTime, chrono::Duration)> {
    let total = |utc: NaiveDateTime| {
        let o = zone.offset_from_utc_datetime(&utc);
        o.base_utc_offset() + o.dst_offset()
    };
    let mut lo = local.checked_sub_signed(chrono::Duration::hours(15))?;
    let mut hi = local.checked_add_signed(chrono::Duration::hours(15))?;
    let before = total(lo);
    if total(hi) == before {
        return None;
    }
    // Invariant: offset(lo) == before, offset(hi) != before.
    while (hi - lo) > chrono::Duration::seconds(1) {
        // Whole seconds only: halving an odd span would add sub-second noise.
        let mid = lo + chrono::Duration::seconds((hi - lo).num_seconds() / 2);
        if total(mid) == before {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((hi, before))
}

/// The system's IANA zone.
///
/// The one environment-reading function in this module — everything else takes
/// its zone as a parameter. `chrono::Local` only exposes an *offset*, which is
/// not enough: a world clock needs the zone name to know DST rules and print an
/// abbreviation. `$TZ` wins when it names a real zone (so tests and containers
/// can pin it), then the platform lookup, then UTC.
pub fn system_zone() -> Tz {
    if let Ok(tz) = std::env::var("TZ")
        && let Some(z) = resolve_zone(tz.trim_start_matches(':'))
    {
        return z;
    }
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|n| resolve_zone(&n))
        .unwrap_or(Tz::UTC)
}

/// The effective format for one world-clock row.
///
/// The inherited variants are resolved before a row reaches the host so the
/// renderer never has to look up calendar configuration. Custom formats are
/// validated at config admission and are deliberately limited to values that
/// remain truthful at the existing minute-resolution tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockFormat {
    H12,
    H24,
    Custom(String),
}

impl ClockFormat {
    pub fn inherited(twelve_hour: bool) -> Self {
        if twelve_hour { Self::H12 } else { Self::H24 }
    }

    pub fn custom_or_inherited(format: &str, twelve_hour: bool) -> Self {
        if format.is_empty() {
            Self::inherited(twelve_hour)
        } else {
            Self::Custom(format.to_owned())
        }
    }
}

/// A world clock after config resolution: a label plus a known-good zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClock {
    pub label: String,
    pub zone: Tz,
    /// The validated, effective row format.
    pub format: ClockFormat,
    /// Whether this row uses its full local date instead of a weekday and
    /// relative day marker.
    pub show_date: bool,
    /// True for the synthesized row showing the user's own zone.
    pub is_home: bool,
}

impl ResolvedClock {
    /// Derive a display label from a zone name: the city segment with
    /// underscores turned back into spaces (`America/New_York` → `New York`).
    pub fn label_from_zone(zone: Tz) -> String {
        zone.name()
            .rsplit('/')
            .next()
            .unwrap_or(zone.name())
            .replace('_', " ")
    }
}

/// One clock, evaluated at an instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockReading {
    pub label: String,
    pub zone: Tz,
    /// Local wall time in this zone, for formatting.
    pub local: NaiveDateTime,
    /// The resolved row format, carried with the reading for pure rendering.
    pub format: ClockFormat,
    /// Whether the row's date cell should show its full local date.
    pub show_date: bool,
    /// Total UTC offset in seconds at this instant (base + any DST shift).
    pub utc_offset_secs: i32,
    /// Whether DST is in effect right now.
    pub is_dst: bool,
    /// The DST-aware abbreviation (`CST`/`CDT`). Some zones have none, in which
    /// case this is the numeric offset (`+05:45`).
    pub abbrev: String,
    /// Signed minutes this zone is ahead of home. Zero for the home row.
    pub delta_from_home_mins: i32,
    /// Calendar-date difference from home: -1, 0 or +1.
    pub day_delta: i8,
    pub is_home: bool,
}

/// Evaluate every clock at `now`, relative to `home`.
pub fn read_clocks(clocks: &[ResolvedClock], now: DateTime<Utc>, home: Tz) -> Vec<ClockReading> {
    let home_dt = home.from_utc_datetime(&now.naive_utc());
    let home_offset = total_offset_secs(home, now);
    let home_date = home_dt.date_naive();
    clocks
        .iter()
        .map(|c| {
            let dt = c.zone.from_utc_datetime(&now.naive_utc());
            let off = total_offset_secs(c.zone, now);
            let comps = c.zone.offset_from_utc_datetime(&now.naive_utc());
            let date = dt.date_naive();
            ClockReading {
                label: if c.label.trim().is_empty() {
                    ResolvedClock::label_from_zone(c.zone)
                } else {
                    c.label.clone()
                },
                zone: c.zone,
                local: dt.naive_local(),
                format: c.format.clone(),
                show_date: c.show_date,
                utc_offset_secs: off,
                is_dst: comps.dst_offset().num_seconds() != 0,
                abbrev: abbrev_or_offset(c.zone, now),
                // Computed from both offsets AT `now`, so half-hour zones and
                // mismatched DST shoulder seasons need no special casing.
                delta_from_home_mins: (off - home_offset) / 60,
                day_delta: match date.num_days_from_ce() - home_date.num_days_from_ce() {
                    d if d > 0 => 1,
                    d if d < 0 => -1,
                    _ => 0,
                },
                is_home: c.is_home,
            }
        })
        .collect()
}

/// Total offset from UTC (base + DST) for `zone` at `at`.
pub fn total_offset_secs(zone: Tz, at: DateTime<Utc>) -> i32 {
    let c = zone.offset_from_utc_datetime(&at.naive_utc());
    (c.base_utc_offset() + c.dst_offset()).num_seconds() as i32
}

/// One bounded STANDARD/DAYLIGHT observance from an embedded VTIMEZONE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VtzObservance {
    onset_local: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
    yearly: Option<(u32, i8, Weekday, Option<NaiveDateTime>)>,
}

/// The small, deliberately conservative subset of VTIMEZONE rules we can
/// compare against the bundled IANA database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VtzRules {
    observances: Vec<VtzObservance>,
}

impl VtzRules {
    pub(crate) fn new(observances: Vec<VtzObservance>) -> Option<Self> {
        (!observances.is_empty() && observances.len() <= 8).then_some(Self { observances })
    }

    /// Parse a single observance. Unsupported fields/rule forms refuse the
    /// entire definition rather than silently changing its meaning.
    pub(crate) fn observance(
        dtstart: &str,
        offset_from: &str,
        offset_to: &str,
        rrule: Option<&str>,
        has_rdate: bool,
    ) -> Option<VtzObservance> {
        if has_rdate {
            return None;
        }
        let onset_local = super::recur::parse_ics_datetime(dtstart)?;
        let offset_from = parse_vtz_offset(offset_from)?;
        let offset_to = parse_vtz_offset(offset_to)?;
        let yearly = match rrule {
            None => None,
            Some(rule) => Some(parse_vtz_yearly_rule(rule)?),
        };
        Some(VtzObservance {
            onset_local,
            offset_from,
            offset_to,
            yearly,
        })
    }

    /// Return the deterministic IANA zone whose offsets match these rules in
    /// the requested event year.
    pub(crate) fn match_zone(&self, year: i32, preferred: Option<Tz>) -> Option<Tz> {
        let probes = self.probes(year)?;
        let mut candidates = Vec::new();
        if let Some(zone) = preferred {
            candidates.push(zone);
        }
        let offsets: std::collections::BTreeSet<i32> = self
            .observances
            .iter()
            .flat_map(|o| [o.offset_from, o.offset_to])
            .collect();
        if offsets.len() == 1
            && let Some(offset) = offsets.first()
            && offset % 3600 == 0
        {
            let hours = offset / 3600;
            let name = if hours == 0 {
                "Etc/GMT".to_string()
            } else if hours > 0 {
                format!("Etc/GMT-{hours}")
            } else {
                format!("Etc/GMT+{}", hours.abs())
            };
            if let Some(zone) = resolve_zone(&name) {
                candidates.push(zone);
            }
        }
        candidates.extend(chrono_tz::TZ_VARIANTS.iter().copied());
        let mut seen = std::collections::BTreeSet::new();
        candidates.into_iter().find(|zone| {
            seen.insert(zone.name())
                && probes
                    .iter()
                    .all(|(at, expected)| total_offset_secs(*zone, *at) == *expected)
        })
    }

    fn probes(&self, year: i32) -> Option<Vec<(DateTime<Utc>, i32)>> {
        let mut instants = Vec::new();
        for month in 1..=12 {
            for day in [1, 15] {
                let date = NaiveDate::from_ymd_opt(year, month, day)?;
                instants.push(date.and_hms_opt(12, 0, 0)?.and_utc());
            }
        }
        for onset in self.onsets_for_year(year)? {
            let instant = onset.instant();
            if instant.year() == year {
                instants.push(instant - chrono::Duration::seconds(1));
                instants.push(instant);
            }
        }
        instants.sort();
        instants.dedup();
        Some(
            instants
                .into_iter()
                .map(|at| (at, self.offset_at(at)))
                .collect(),
        )
    }

    fn onsets_for_year(&self, year: i32) -> Option<Vec<Onset>> {
        let mut onsets = Vec::new();
        for observance in &self.observances {
            if observance.yearly.is_none() {
                // A one-shot observance remains in effect in all later years.
                onsets.push(Onset {
                    local: observance.onset_local,
                    offset_from: observance.offset_from,
                    offset_to: observance.offset_to,
                });
            } else {
                for y in [year.checked_sub(1)?, year] {
                    if y < observance.onset_local.year() {
                        continue;
                    }
                    let Some((month, nth, weekday, until)) = observance.yearly else {
                        continue;
                    };
                    let date = nth_weekday(y, month, nth, weekday)?;
                    let local = date.and_time(observance.onset_local.time());
                    if until.is_none_or(|bound| local <= bound) {
                        onsets.push(Onset {
                            local,
                            offset_from: observance.offset_from,
                            offset_to: observance.offset_to,
                        });
                    }
                }
            }
        }
        Some(onsets)
    }

    fn offset_at(&self, at: DateTime<Utc>) -> i32 {
        let onsets = self.onsets_for_year(at.year()).unwrap_or_default();
        onsets
            .iter()
            .copied()
            .filter(|onset| onset.instant() <= at)
            .max_by_key(|onset| onset.instant())
            .map(|onset| onset.offset_to)
            .or_else(|| {
                onsets
                    .iter()
                    .copied()
                    .min_by_key(|onset| onset.instant())
                    .map(|onset| onset.offset_from)
            })
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy)]
struct Onset {
    local: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
}

impl Onset {
    fn instant(self) -> DateTime<Utc> {
        (self.local - chrono::Duration::seconds(i64::from(self.offset_from))).and_utc()
    }
}

fn parse_vtz_offset(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hours = i32::from(value[1..3].parse::<u8>().ok()?);
    let minutes = i32::from(value[3..5].parse::<u8>().ok()?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    let sign = if bytes[0] == b'-' { -1 } else { 1 };
    Some(sign * (hours * 3600 + minutes * 60))
}

fn parse_vtz_yearly_rule(value: &str) -> Option<(u32, i8, Weekday, Option<NaiveDateTime>)> {
    let mut freq = None;
    let mut month = None;
    let mut byday = None;
    let mut until = None;
    for part in value
        .trim()
        .strip_prefix("RRULE:")
        .unwrap_or(value.trim())
        .split(';')
    {
        let (key, val) = part.split_once('=')?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" if freq.is_none() => freq = Some(val.to_ascii_uppercase()),
            "BYMONTH" if month.is_none() => month = Some(val.parse::<u32>().ok()?),
            "BYDAY" if byday.is_none() => byday = Some(parse_vtz_byday(val)?),
            "UNTIL" if until.is_none() => until = Some(super::recur::parse_ics_datetime(val)?),
            _ => return None,
        }
    }
    if freq.as_deref() != Some("YEARLY") {
        return None;
    }
    let (nth, weekday) = byday?;
    let month = month?;
    (1..=12)
        .contains(&month)
        .then_some((month, nth, weekday, until))
}

fn parse_vtz_byday(value: &str) -> Option<(i8, Weekday)> {
    let split = value.find(|ch: char| !ch.is_ascii_digit() && ch != '-')?;
    let nth: i8 = value[..split].parse().ok()?;
    if !(-5..=5).contains(&nth) || nth == 0 {
        return None;
    }
    let weekday = match &value[split..].to_ascii_uppercase()[..] {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    };
    Some((nth, weekday))
}

fn nth_weekday(year: i32, month: u32, nth: i8, weekday: Weekday) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    if nth > 0 {
        let delta = (weekday.num_days_from_monday() as i32
            - first.weekday().num_days_from_monday() as i32)
            .rem_euclid(7);
        let day = 1 + delta + (i32::from(nth) - 1) * 7;
        NaiveDate::from_ymd_opt(year, month, u32::try_from(day).ok()?)
    } else {
        let days = match month {
            2 if NaiveDate::from_ymd_opt(year, 2, 29).is_some() => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let last_day = first.with_day(days)?;
        let delta = (last_day.weekday().num_days_from_monday() as i32
            - weekday.num_days_from_monday() as i32)
            .rem_euclid(7);
        let day = last_day.day() as i32 - (i32::from(nth.abs()) - 1) * 7 - delta;
        NaiveDate::from_ymd_opt(year, month, u32::try_from(day).ok()?)
    }
}

/// The zone abbreviation, falling back to a numeric `+HH:MM` for zones whose
/// tzdb entry has no useful abbreviation (many render as `+0545`/`LMT`).
fn abbrev_or_offset(zone: Tz, at: DateTime<Utc>) -> String {
    let dt = zone.from_utc_datetime(&at.naive_utc());
    let abbr = format!("{}", dt.format("%Z"));
    // A numeric-looking abbreviation is tzdb's own "no name here" marker; render
    // it in the conventional +HH:MM shape rather than passing `+0545` through.
    if abbr.is_empty() || abbr.starts_with(['+', '-']) || abbr.chars().all(|c| c.is_ascii_digit()) {
        return fmt_offset(total_offset_secs(zone, at));
    }
    abbr
}

/// Render an offset in seconds as `+HH:MM` / `-HH:MM`.
pub fn fmt_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

/// Render a home-relative delta the way a world-clock row shows it: `+7h`,
/// `-6h`, `+5h30`, or an empty string for no difference.
pub fn fmt_delta(mins: i32) -> String {
    if mins == 0 {
        return String::new();
    }
    let sign = if mins < 0 { '-' } else { '+' };
    let a = mins.abs();
    let (h, m) = (a / 60, a % 60);
    if m == 0 {
        format!("{sign}{h}h")
    } else if h == 0 {
        format!("{sign}{m}m")
    } else {
        format!("{sign}{h}h{m:02}")
    }
}

#[cfg(test)]
#[path = "tz_tests.rs"]
mod tests;
