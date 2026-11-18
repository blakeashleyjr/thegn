//! `schedule_due` producer inputs: cron / interval schedules (THE-2193).
//!
//! Pure: the daemon producer (thegn-host `automation_schedule`) passes the
//! clock and the time zone in. Schedules persist nothing; "no catch-up" means
//! the next due time is always computed from now.
//!
//! 5-field cron (`*`,
//! lists, ranges, steps, month and weekday names) in the host's local zone,
//! and `every = "<n>s|m|h|d"` (60 s to 7 d) anchored at producer start.

use serde::{Deserialize, Serialize};

use crate::automation::{AutomationEvent, AutomationEventKind};

/// A rule's schedule: a cron expression or a fixed interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schedule {
    Cron(CronSpec),
    Every { secs: u64 },
}

/// A parsed 5-field cron expression. `source` is the text it was parsed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    pub source: String,
}

/// Longest accepted `schedule` / `every` text.
const MAX_INPUT_BYTES: usize = 128;
/// `every` bounds, seconds.
const EVERY_MIN_SECS: u64 = 60;
const EVERY_MAX_SECS: u64 = 604_800;
/// How far ahead a cron search looks before declaring the spec never due.
const HORIZON_DAYS: u32 = 366 * 5;

/// Parse a rule's `schedule` (cron) or `every` (interval). `Ok(None)` when
/// neither is set; exclusivity is the config validator's job.
///
/// Cron grammar: 5 fields `minute hour day-of-month month day-of-week`, each
/// a comma list of `*`, `a`, `a-b`, `*/n` or `a-b/n`. Months accept `JAN-DEC`,
/// weekdays `SUN-SAT` (case-insensitive); weekday `0` and `7` are Sunday.
/// When BOTH day-of-month and day-of-week are restricted (do not start with
/// `*`) a day matches if EITHER matches (Vixie cron semantics). Six fields,
/// `@reboot` and the `@hourly`/`@daily`/... macros are refused: write the
/// five fields out. Inputs are at most 128 bytes.
pub fn parse_schedule(cron: Option<&str>, every: Option<&str>) -> Result<Option<Schedule>, String> {
    match (cron, every) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("set either a cron schedule or every, not both".into()),
        (Some(text), None) => {
            parse_cron(text)?;
            Ok(Some(Schedule::Cron(CronSpec {
                source: text.trim().to_string(),
            })))
        }
        (None, Some(text)) => Ok(Some(Schedule::Every {
            secs: parse_every(text)?,
        })),
    }
}

fn parse_every(text: &str) -> Result<u64, String> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(format!("every is longer than {MAX_INPUT_BYTES} bytes"));
    }
    let text = text.trim();
    let unit = text
        .chars()
        .last()
        .ok_or_else(|| "every is empty (use <n>s|m|h|d, e.g. \"15m\")".to_string())?;
    let mult = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(format!("every {text:?}: use <n>s|m|h|d, e.g. \"15m\"")),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("every {text:?}: use <n>s|m|h|d, e.g. \"15m\""));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("every {text:?}: number too large"))?;
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("every {text:?}: number too large"))?;
    if !(EVERY_MIN_SECS..=EVERY_MAX_SECS).contains(&secs) {
        return Err(format!(
            "every {text:?}: must be between 60s and 7d ({EVERY_MIN_SECS}..={EVERY_MAX_SECS} seconds)"
        ));
    }
    Ok(secs)
}

/// A parsed cron expression as bit sets.
struct Cron {
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    /// Bits 0..=6, Sunday = 0.
    dow: u8,
    dom_star: bool,
    dow_star: bool,
}

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DOW_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_cron(text: &str) -> Result<Cron, String> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(format!("schedule is longer than {MAX_INPUT_BYTES} bytes"));
    }
    let text = text.trim();
    if text.is_empty() {
        return Err("schedule is empty".into());
    }
    if text.starts_with('@') {
        return Err(format!(
            "schedule {text:?}: @-macros (including @reboot) are not supported; write five fields"
        ));
    }
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "schedule needs exactly 5 fields (minute hour day-of-month month day-of-week), got {}",
            fields.len()
        ));
    }
    let (minutes, _) = parse_field(fields[0], 0, 59, &[], 0, "minute")?;
    let (hours, _) = parse_field(fields[1], 0, 23, &[], 0, "hour")?;
    let (dom, dom_star) = parse_field(fields[2], 1, 31, &[], 0, "day-of-month")?;
    let (months, _) = parse_field(fields[3], 1, 12, &MONTH_NAMES, 1, "month")?;
    let (dow, dow_star) = parse_field(fields[4], 0, 7, &DOW_NAMES, 0, "day-of-week")?;
    // Fold 7 (Sunday) onto 0.
    let dow = (dow & 0x7f) | ((dow >> 7) & 1);
    Ok(Cron {
        minutes,
        hours: hours as u32,
        dom: dom as u32,
        months: months as u16,
        dow: dow as u8,
        dom_star,
        dow_star,
    })
}

/// Parse one field into a bit set (bit `v`), plus whether it started with `*`.
fn parse_field(
    field: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<(u64, bool), String> {
    let star = field.starts_with('*');
    let mut bits = 0u64;
    for part in field.split(',') {
        if part.is_empty() {
            return Err(format!("{what}: empty list element in {field:?}"));
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| format!("{what}: bad step in {part:?}"))?;
                (range, Some(step))
            }
            None => (part, None),
        };
        let (lo, hi) = if range == "*" {
            (min, if what == "day-of-week" { 6 } else { max })
        } else if let Some((a, b)) = range.split_once('-') {
            (
                parse_value(a, min, max, names, name_base, what)?,
                parse_value(b, min, max, names, name_base, what)?,
            )
        } else {
            if step.is_some() {
                return Err(format!("{what}: a step needs * or a range, got {part:?}"));
            }
            let v = parse_value(range, min, max, names, name_base, what)?;
            (v, v)
        };
        if lo > hi {
            return Err(format!("{what}: range {part:?} runs backwards"));
        }
        let step = step.unwrap_or(1);
        let mut v = lo;
        loop {
            bits |= 1u64 << v;
            match v.checked_add(step) {
                Some(n) if n <= hi => v = n,
                _ => break,
            }
        }
    }
    Ok((bits, star))
}

fn parse_value(
    text: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    what: &str,
) -> Result<u32, String> {
    if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(text)) {
        return Ok(i as u32 + name_base);
    }
    let v: u32 = text
        .parse()
        .map_err(|_| format!("{what}: {text:?} is not a number or name"))?;
    if !(min..=max).contains(&v) {
        return Err(format!("{what}: {v} is outside {min}..={max}"));
    }
    Ok(v)
}

impl Cron {
    fn day_matches(&self, date: chrono::NaiveDate) -> bool {
        use chrono::Datelike;
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let dom_ok = self.dom & (1 << date.day()) != 0;
        let dow_ok = self.dow & (1 << date.weekday().num_days_from_sunday()) != 0;
        // Vixie: both restricted means either may match.
        if !self.dom_star && !self.dow_star {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }
}

fn next_cron<Tz: chrono::TimeZone>(cron: &Cron, now_secs: i64, tz: &Tz) -> Option<i64> {
    use chrono::{LocalResult, NaiveDate};
    let from = now_secs.div_euclid(60) * 60 + 60;
    let start = tz.timestamp_opt(from, 0).earliest()?.date_naive();
    let mut date: NaiveDate = start;
    for _ in 0..=HORIZON_DAYS {
        if cron.day_matches(date) {
            for hour in (0..24u32).filter(|h| cron.hours & (1 << h) != 0) {
                for minute in (0..60u32).filter(|m| cron.minutes & (1 << m) != 0) {
                    let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
                        continue;
                    };
                    let at = match tz.from_local_datetime(&naive) {
                        LocalResult::None => continue,
                        LocalResult::Single(t) => t,
                        LocalResult::Ambiguous(first, _) => first,
                    };
                    if at.timestamp() >= from {
                        return Some(at.timestamp());
                    }
                }
            }
        }
        date = date.succ_opt()?;
    }
    None
}

/// The next due instant (unix seconds), strictly after `now_secs`. `anchor_secs`
/// is the producer start an `Every` schedule counts from.
///
/// Cron: the earliest whole minute after `now_secs` in `tz`; nonexistent local
/// times (spring forward) are skipped, an ambiguous one (fall back) fires once
/// at the earlier instant; `None` when nothing matches within five years
/// (`0 0 30 2 *`). Every: `anchor + k*secs` for the smallest `k >= 1` that is
/// after now, so a long gap yields one fire, never a catch-up burst.
pub fn next_due<Tz: chrono::TimeZone>(
    s: &Schedule,
    now_secs: i64,
    anchor_secs: i64,
    tz: &Tz,
) -> Option<i64> {
    match s {
        Schedule::Cron(spec) => next_cron(&parse_cron(&spec.source).ok()?, now_secs, tz),
        Schedule::Every { secs } => {
            let secs = i64::try_from(*secs).ok().filter(|s| *s > 0)?;
            let k = (now_secs.saturating_sub(anchor_secs).div_euclid(secs) + 1).max(1);
            anchor_secs.checked_add(k.checked_mul(secs)?)
        }
    }
}

/// The event a due schedule produces. Targeted at its own rule only, keyed by
/// the due instant so a re-fire of the same slot coalesces.
pub fn schedule_event(rule_id: &str, due_secs: i64) -> AutomationEvent {
    let mut event = AutomationEvent::bare(
        AutomationEventKind::ScheduleDue,
        format!("schedule:{rule_id}:{due_secs}"),
        due_secs,
    );
    event.target_rule = Some(rule_id.to_string());
    event.source_ref = Some(format!("schedule:{rule_id}"));
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_event_targets_its_rule_and_keys_by_due_time() {
        let event = schedule_event("nightly", 1_700_000_000);
        assert_eq!(event.kind, AutomationEventKind::ScheduleDue);
        assert_eq!(event.key.0, "schedule:nightly:1700000000");
        assert_eq!(event.target_rule.as_deref(), Some("nightly"));
        assert_eq!(event.occurred_at, 1_700_000_000);
        assert_ne!(event.id, schedule_event("nightly", 1_700_000_060).id);
    }

    fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        use chrono::TimeZone;
        chrono_tz::America::New_York
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn cron_next<Tz: chrono::TimeZone>(expr: &str, now: i64, tz: &Tz) -> Option<i64> {
        let s = parse_schedule(Some(expr), None).unwrap().unwrap();
        next_due(&s, now, 0, tz)
    }

    #[test]
    fn cron_field_table() {
        for ok in [
            "* * * * *",
            "*/5 * * * *",
            "0 0 1 JAN *",
            "0 9-17/2 * * mon-fri",
            "0,30 * * * 7",
            "0 0 * dec sun",
            "59 23 31 12 6",
            "  1   2 3 4 5 ",
            "5-10/4294967295 * * * *",
        ] {
            assert!(parse_schedule(Some(ok), None).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "   ",
            "* * * *",
            "* * * * * *",
            "@reboot",
            "@hourly",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "5/2 * * * *",
            "1,,2 * * * *",
            "a * * * *",
            "* * * foo *",
        ] {
            assert!(parse_schedule(Some(bad), None).is_err(), "{bad:?}");
        }
        assert!(parse_schedule(Some(&"*".repeat(129)), None).is_err());
        assert!(parse_schedule(Some("* * * * *"), Some("5m")).is_err());
    }

    #[test]
    fn every_parsing_bounds() {
        let secs = |t: &str| parse_schedule(None, Some(t));
        assert_eq!(secs("60s"), Ok(Some(Schedule::Every { secs: 60 })));
        assert_eq!(secs("2m"), Ok(Some(Schedule::Every { secs: 120 })));
        assert_eq!(secs("7d"), Ok(Some(Schedule::Every { secs: 604_800 })));
        for bad in [
            "59s",
            "8d",
            "169h",
            "",
            "m",
            "5",
            "1.5h",
            "-5m",
            "5x",
            "99999999999999999999d",
        ] {
            assert!(secs(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cron_next_is_strictly_after_now_on_whole_minutes() {
        let utc = chrono::Utc;
        assert_eq!(cron_next("* * * * *", 120, &utc), Some(180));
        assert_eq!(cron_next("* * * * *", 121, &utc), Some(180));
        assert_eq!(cron_next("*/15 * * * *", 0, &utc), Some(900));
    }

    #[test]
    fn cron_month_end_and_leap_day() {
        let utc = chrono::Utc;
        use chrono::TimeZone;
        let t = |y, mo, d, h, mi| {
            utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
                .unwrap()
                .timestamp()
        };
        assert_eq!(
            cron_next("0 0 31 * *", t(2026, 4, 1, 0, 0), &utc),
            Some(t(2026, 5, 31, 0, 0))
        );
        assert_eq!(
            cron_next("0 12 29 2 *", t(2026, 1, 1, 0, 0), &utc),
            Some(t(2028, 2, 29, 12, 0))
        );
    }

    #[test]
    fn cron_vixie_or_when_both_day_fields_restricted() {
        let utc = chrono::Utc;
        use chrono::TimeZone;
        let t = |d| {
            utc.with_ymd_and_hms(2026, 3, d, 0, 0, 0)
                .unwrap()
                .timestamp()
        };
        // 2026-03-02 is a Monday; dom 15 OR Monday.
        assert_eq!(cron_next("0 0 15 * mon", t(1), &utc), Some(t(2)));
        assert_eq!(cron_next("0 0 15 * mon", t(10), &utc), Some(t(15)));
        // Only dow restricted: AND with `*` dom.
        assert_eq!(cron_next("0 0 * * mon", t(3), &utc), Some(t(9)));
        // 0 and 7 are both Sunday.
        assert_eq!(
            cron_next("0 0 * * 7", t(3), &utc),
            cron_next("0 0 * * 0", t(3), &utc)
        );
    }

    #[test]
    fn cron_dst_spring_gap_is_skipped() {
        let tz = chrono_tz::America::New_York;
        // 02:30 does not exist on 2026-03-08; next is 03-09 02:30 EDT.
        assert_eq!(
            cron_next("30 2 * * *", ny(2026, 3, 8, 0, 0), &tz),
            Some(ny(2026, 3, 9, 2, 30))
        );
    }

    #[test]
    fn cron_dst_fall_overlap_fires_once_at_earlier_instant() {
        let tz = chrono_tz::America::New_York;
        // 01:30 happens twice on 2026-11-01; pick the first (EDT).
        let first = ny(2026, 11, 1, 1, 30);
        assert_eq!(
            cron_next("30 1 * * *", ny(2026, 11, 1, 0, 0), &tz),
            Some(first)
        );
        // From inside the repeated hour the second pass is not a second fire.
        assert_eq!(
            cron_next("30 1 * * *", first, &tz),
            Some(ny(2026, 11, 2, 1, 30))
        );
    }

    #[test]
    fn cron_europe_london() {
        use chrono::TimeZone;
        let tz = chrono_tz::Europe::London;
        let t = |y, mo, d, h, mi| tz.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp();
        // 01:30 is skipped on 2026-03-29 (clocks go 01:00 -> 02:00).
        assert_eq!(
            cron_next("30 1 * * *", t(2026, 3, 29, 0, 0), &tz),
            Some(t(2026, 3, 30, 1, 30))
        );
    }

    #[test]
    fn cron_never_due_returns_none() {
        assert_eq!(cron_next("0 0 30 2 *", 0, &chrono::Utc), None);
    }

    #[test]
    fn every_aligns_to_anchor_with_no_catch_up() {
        let s = Schedule::Every { secs: 120 };
        let tz = chrono::Utc;
        assert_eq!(next_due(&s, 1000, 1000, &tz), Some(1120));
        assert_eq!(next_due(&s, 1119, 1000, &tz), Some(1120));
        assert_eq!(next_due(&s, 1120, 1000, &tz), Some(1240));
        // Six minutes gone: one next slot, not three missed ones.
        assert_eq!(next_due(&s, 1000 + 360, 1000, &tz), Some(1000 + 480));
        // Clock before the anchor still waits one full interval.
        assert_eq!(next_due(&s, 500, 1000, &tz), Some(1120));
    }

    #[test]
    fn no_schedule_parses_to_none() {
        assert_eq!(parse_schedule(None, None), Ok(None));
    }
}
