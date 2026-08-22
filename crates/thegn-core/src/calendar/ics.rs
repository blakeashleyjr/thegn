//! iCalendar (RFC 5545) parsing.
//!
//! Hand-rolled rather than taken from a crate: the content-line grammar is
//! ~200 lines we would need anyway to round-trip provider data faithfully, and
//! living here (rather than in the service layer) keeps it pure and under the
//! core coverage gate — `IcsBackend` is then a thin I/O shell.
//!
//! Deliberately lenient. A calendar feed with one malformed event should show
//! the other ninety-nine, so unparseable properties are skipped rather than
//! failing the whole document.

use std::collections::BTreeMap;

use chrono::NaiveDate;

use super::recur::{RRule, Recurrence, parse_ics_datetime};
use super::{CalEvent, EventStatus, EventTime, Reminder, TzRef};

/// One unfolded content line: `NAME;PARAM=v:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLine {
    pub name: String,
    pub params: BTreeMap<String, String>,
    pub value: String,
}

/// Unfold RFC 5545 line folding: a CRLF followed by a space or tab is a
/// continuation, not a new line.
///
/// Feeds wrap at 75 octets mid-word (and mid-UTF-8-sequence), so unfolding
/// before anything else is what stops long summaries and URLs being mangled.
pub fn unfold(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix([' ', '\t'])
            && let Some(last) = out.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        out.push(line.to_string());
    }
    out
}

/// Split a content line into name, parameters, and value.
pub fn parse_line(line: &str) -> Option<ContentLine> {
    // The value starts at the first colon that is not inside a quoted param.
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = line.split_at(colon);
    let value = &value[1..];

    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut params = BTreeMap::new();
    for p in parts {
        if let Some((k, v)) = p.split_once('=') {
            params.insert(
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_string(),
            );
        }
    }
    Some(ContentLine {
        name,
        params,
        value: unescape(value),
    })
}

/// Undo RFC 5545 TEXT escaping (`\n`, `\,`, `\;`, `\\`).
pub fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Apply RFC 5545 TEXT escaping.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Turn a DTSTART/DTEND-style line into an [`EventTime`].
///
/// `VALUE=DATE` and a bare `YYYYMMDD` are floating dates; a trailing `Z` is
/// UTC; a `TZID=` parameter names the zone; anything else is floating local
/// time, which we anchor to the caller's `default_zone`.
pub fn parse_time(line: &ContentLine, default_zone: &str) -> Option<EventTime> {
    let v = line.value.trim();
    let is_date = line.params.get("VALUE").is_some_and(|s| s == "DATE") || !v.contains('T');
    if is_date {
        return NaiveDate::parse_from_str(v, "%Y%m%d")
            .ok()
            .map(|date| EventTime::Date { date });
    }
    let local = parse_ics_datetime(v)?;
    if v.ends_with('Z') {
        return Some(EventTime::Instant {
            at: chrono::DateTime::from_naive_utc_and_offset(local, chrono::Utc),
        });
    }
    let zone = line
        .params
        .get("TZID")
        .map(String::as_str)
        .unwrap_or(default_zone);
    Some(EventTime::Zoned {
        local,
        zone: TzRef::new(zone),
    })
}

/// Parse a whole `.ics` document into events.
///
/// `default_zone` anchors floating times that name no `TZID`. Components other
/// than `VEVENT` (todos, journals, timezone definitions) are skipped.
pub fn parse_ics(input: &str, default_zone: &str) -> Vec<CalEvent> {
    let mut out = Vec::new();
    let mut cur: Option<Builder> = None;
    // Depth of nested non-VEVENT components (notably VALARM inside VEVENT, and
    // VTIMEZONE's STANDARD/DAYLIGHT), so their properties don't leak into the
    // event being built.
    let mut nested: Vec<String> = Vec::new();
    let mut cal_name = String::new();

    for raw in unfold(input) {
        let Some(line) = parse_line(&raw) else {
            continue;
        };
        match (line.name.as_str(), line.value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VEVENT") => {
                cur = Some(Builder::default());
                nested.clear();
                continue;
            }
            ("END", "VEVENT") => {
                if let Some(b) = cur.take()
                    && let Some(mut e) = b.finish(default_zone)
                {
                    e.calendar = cal_name.clone();
                    out.push(e);
                }
                continue;
            }
            ("BEGIN", other) => {
                if cur.is_some() {
                    nested.push(other.to_string());
                }
                continue;
            }
            ("END", _) => {
                nested.pop();
                continue;
            }
            _ => {}
        }
        if line.name == "X-WR-CALNAME" && cur.is_none() {
            cal_name = line.value.clone();
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        // Inside a VALARM: pick up the reminder trigger, ignore everything else.
        if nested.last().is_some_and(|n| n == "VALARM") {
            if line.name == "TRIGGER"
                && let Some(mins) = parse_trigger_minutes(&line.value)
            {
                b.reminders.push(Reminder {
                    minutes_before: mins,
                });
            }
            continue;
        }
        if !nested.is_empty() {
            continue;
        }
        b.property(&line);
    }
    out
}

/// `-PT15M` / `-PT1H` / `-P1D` → minutes before. A positive (after-start)
/// trigger has no meaning for a reminder here and is dropped.
pub fn parse_trigger_minutes(v: &str) -> Option<u32> {
    let t = v.trim().to_ascii_uppercase();
    let neg = t.starts_with('-');
    if !neg {
        return None;
    }
    let body = t.trim_start_matches(['-', '+']).trim_start_matches('P');
    let (date_part, time_part) = match body.split_once('T') {
        Some((d, t)) => (d, t),
        None => (body, ""),
    };
    let mut mins: u32 = 0;
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u32 = num.parse().unwrap_or(0);
            num.clear();
            match c {
                'W' => mins += n * 7 * 24 * 60,
                'D' => mins += n * 24 * 60,
                _ => {}
            }
        }
    }
    num.clear();
    for c in time_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u32 = num.parse().unwrap_or(0);
            num.clear();
            match c {
                'H' => mins += n * 60,
                'M' => mins += n,
                // Sub-minute reminder lead times round to "now".
                'S' => {}
                _ => {}
            }
        }
    }
    Some(mins)
}

/// Accumulates one VEVENT's properties.
#[derive(Default)]
struct Builder {
    uid: String,
    summary: String,
    description: String,
    location: String,
    url: String,
    status: Option<EventStatus>,
    start: Option<EventTime>,
    end: Option<EventTime>,
    duration: Option<chrono::Duration>,
    rrules: Vec<RRule>,
    rdates: Vec<EventTime>,
    exdates: Vec<EventTime>,
    reminders: Vec<Reminder>,
    organizer: String,
    categories: String,
    last_modified: i64,
    extra: BTreeMap<String, String>,
}

impl Builder {
    fn property(&mut self, line: &ContentLine) {
        match line.name.as_str() {
            "UID" => self.uid = line.value.clone(),
            "SUMMARY" => self.summary = line.value.clone(),
            "DESCRIPTION" => self.description = line.value.clone(),
            "LOCATION" => self.location = line.value.clone(),
            "URL" => self.url = line.value.clone(),
            "CATEGORIES" => self.categories = line.value.clone(),
            "ORGANIZER" => {
                self.organizer = line
                    .value
                    .trim()
                    .strip_prefix("mailto:")
                    .unwrap_or(&line.value)
                    .to_string()
            }
            "STATUS" => {
                self.status = match line.value.trim().to_ascii_uppercase().as_str() {
                    "TENTATIVE" => Some(EventStatus::Tentative),
                    "CANCELLED" => Some(EventStatus::Cancelled),
                    _ => Some(EventStatus::Confirmed),
                }
            }
            "DTSTART" => self.start = parse_time(line, ""),
            "DTEND" => self.end = parse_time(line, ""),
            "DURATION" => self.duration = parse_duration(&line.value),
            "RRULE" => {
                if let Ok(r) = RRule::parse(&line.value) {
                    self.rrules.push(r);
                }
            }
            "RDATE" => self.rdates.extend(multi_time(line)),
            "EXDATE" => self.exdates.extend(multi_time(line)),
            "LAST-MODIFIED" | "DTSTAMP" => {
                if self.last_modified == 0
                    && let Some(d) = parse_ics_datetime(&line.value)
                {
                    self.last_modified = d.and_utc().timestamp_millis();
                }
            }
            // Anything else worth keeping for a round trip, but only the
            // X- extensions — copying every standard property would bloat every
            // cached row for no gain.
            n if n.starts_with("X-") => {
                self.extra.insert(n.to_string(), line.value.clone());
            }
            _ => {}
        }
    }

    fn finish(self, default_zone: &str) -> Option<CalEvent> {
        let start = self.start?;
        // RFC 5545: DTEND is optional. With a DURATION, add it; with neither, a
        // dated event lasts one day and a timed event is a point in time.
        let end = self.end.clone().unwrap_or_else(|| match &start {
            EventTime::Date { date } => EventTime::Date {
                date: date.succ_opt().unwrap_or(*date),
            },
            EventTime::Zoned { local, zone } => EventTime::Zoned {
                local: *local + self.duration.unwrap_or_default(),
                zone: zone.clone(),
            },
            EventTime::Instant { at } => EventTime::Instant {
                at: *at + self.duration.unwrap_or_default(),
            },
        });
        // Re-anchor floating times that named no TZID onto the account's zone.
        let start = anchor(start, default_zone);
        let end = anchor(end, default_zone);

        let mut e = CalEvent::new(
            if self.uid.is_empty() {
                // A feed without UIDs still needs stable identity, or every
                // sync would look like a full replacement.
                format!("{}@{}", stable_hash(&self.summary), ics_key(&start))
            } else {
                self.uid.clone()
            },
            self.summary.clone(),
            start,
            end,
        );
        e.description = self.description;
        e.location = self.location;
        e.url = self.url;
        e.organizer = self.organizer;
        e.category = self.categories;
        e.status = self.status.unwrap_or_default();
        e.reminders = self.reminders;
        e.updated_at_ms = self.last_modified;
        e.extra = self.extra;
        if !self.rrules.is_empty() || !self.rdates.is_empty() || !self.exdates.is_empty() {
            e.recurrence = Some(Recurrence {
                rules: self.rrules,
                rdates: self.rdates,
                exdates: self.exdates,
            });
        }
        Some(e)
    }
}

/// Give a zone to a time that parsed as floating-with-no-TZID.
fn anchor(t: EventTime, zone: &str) -> EventTime {
    match t {
        EventTime::Zoned { local, zone: z } if z.as_str().is_empty() => EventTime::Zoned {
            local,
            zone: TzRef::new(zone),
        },
        other => other,
    }
}

fn ics_key(t: &EventTime) -> String {
    match t {
        EventTime::Date { date } => date.to_string(),
        EventTime::Zoned { local, .. } => local.to_string(),
        EventTime::Instant { at } => at.to_rfc3339(),
    }
}

/// A small stable hash for synthesising a UID. Not cryptographic — it only has
/// to be deterministic across syncs.
fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// RDATE/EXDATE may carry a comma-separated list.
fn multi_time(line: &ContentLine) -> Vec<EventTime> {
    line.value
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|v| {
            parse_time(
                &ContentLine {
                    name: line.name.clone(),
                    params: line.params.clone(),
                    value: v.trim().to_string(),
                },
                "",
            )
        })
        .collect()
}

/// Parse an RFC 5545 DURATION (`PT1H30M`, `P1D`).
pub fn parse_duration(v: &str) -> Option<chrono::Duration> {
    let mins = parse_trigger_minutes(&format!("-{}", v.trim().trim_start_matches(['-', '+'])))?;
    Some(chrono::Duration::minutes(mins as i64))
}

#[cfg(test)]
#[path = "ics_tests.rs"]
mod tests;
