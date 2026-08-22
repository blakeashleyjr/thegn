//! IANA zone resolution and world-clock readings.
//!
//! All offsets and deltas are computed **at a given instant**, never stored as
//! constants. That is what makes India (+5:30), Nepal (+5:45), Lord Howe
//! (+10:30) and every DST shoulder season correct for free instead of needing
//! special cases.

use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, TimeZone, Utc};
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
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
        resolve_zone(&self.0)
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
            // Shift forward by the WIDTH of the gap, so 02:30 becomes 03:30.
            //
            // Deliberately not a scan for the first valid instant: that would
            // collapse 02:15, 02:30 and 02:45 all onto 03:00, firing three
            // separate events simultaneously. Interpreting the wall time
            // against the pre-transition offset preserves each one's position
            // within the hour, and it derives the shift from tzdb rather than
            // assuming 60 minutes (Lord Howe's is 30).
            GapPolicy::ShiftForward => {
                let before = zone.offset_from_utc_datetime(&local);
                let shifted = local - (before.base_utc_offset() + before.dst_offset());
                Some(DateTime::from_naive_utc_and_offset(shifted, Utc))
            }
            GapPolicy::Earliest => {
                let mut probe = local;
                for _ in 0..(4 * 60) {
                    probe -= chrono::Duration::minutes(1);
                    match zone.from_local_datetime(&probe) {
                        LocalResult::Single(dt) => return Some(dt.with_timezone(&Utc)),
                        LocalResult::Ambiguous(_, l) => return Some(l.with_timezone(&Utc)),
                        LocalResult::None => continue,
                    }
                }
                None
            }
        },
    }
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

/// A world clock after config resolution: a label plus a known-good zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClock {
    pub label: String,
    pub zone: Tz,
    /// Per-clock strftime override; empty means "inherit the resolved format".
    pub format: String,
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
