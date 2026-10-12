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

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;

use super::admission::{
    AdmissionBudget, AdmissionError, AdmissionLimit, AdmissionMeter, ENTRY_OVERHEAD,
    MAX_COMPONENT_DEPTH, MAX_LINE_BYTES,
};
use super::recur::{RRule, Recurrence, parse_ics_datetime};
use super::tz::resolve_tzid;
use super::validate::validate_event;
use super::{CalEvent, EventStatus, EventTime, Reminder, TzRef};

/// One unfolded content line: `NAME;PARAM=v:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentLine {
    pub name: String,
    pub params: BTreeMap<String, String>,
    pub value: String,
}

/// Value-free parser diagnostics. They deliberately contain no feed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcsDiagnostic {
    MalformedLine,
    MismatchedEnd,
    UnclosedChild,
    NestedEvent,
    EventWithoutStart,
    InvalidEvent,
    DuplicateProperty(&'static str),
    Truncated,
    MissingEnvelope,
    UnknownTimeZone,
    UnmappedTimeZone,
}

/// Bounded diagnostics from one iCalendar parse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IcsReport {
    diagnostics: Vec<IcsDiagnostic>,
    overflow: usize,
    document_failure: bool,
}

pub const MAX_ICS_DIAGNOSTICS: usize = 32;

impl IcsReport {
    /// The recorded diagnostics, up to [`MAX_ICS_DIAGNOSTICS`].
    pub fn diagnostics(&self) -> &[IcsDiagnostic] {
        &self.diagnostics
    }

    /// Number of diagnostics omitted after the cap was reached.
    pub fn overflow(&self) -> usize {
        self.overflow
    }

    /// Whether component boundaries or document completeness cannot be trusted.
    pub fn is_document_failure(&self) -> bool {
        self.document_failure
    }

    fn record(&mut self, diagnostic: IcsDiagnostic) {
        if matches!(
            diagnostic,
            IcsDiagnostic::Truncated
                | IcsDiagnostic::MismatchedEnd
                | IcsDiagnostic::NestedEvent
                | IcsDiagnostic::UnclosedChild
        ) {
            self.document_failure = true;
        }
        if self.diagnostics.len() < MAX_ICS_DIAGNOSTICS {
            self.diagnostics.push(diagnostic);
        } else {
            self.overflow = self.overflow.saturating_add(1);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ComponentName {
    Calendar,
    Event,
    Alarm,
    Timezone,
    Other(u64, usize),
}

impl ComponentName {
    fn parse(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "VCALENDAR" => Self::Calendar,
            "VEVENT" => Self::Event,
            "VALARM" => Self::Alarm,
            "VTIMEZONE" => Self::Timezone,
            _ => {
                // Fixed-size identity keeps arbitrary extension names out of the stack.
                let mut hash = 0xcbf2_9ce4_8422_2325u64;
                for byte in name.bytes().map(|byte| byte.to_ascii_uppercase()) {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
                Self::Other(hash, name.len())
            }
        }
    }

    fn matches(self, name: &str) -> bool {
        match (self, Self::parse(name)) {
            (Self::Calendar, Self::Calendar)
            | (Self::Event, Self::Event)
            | (Self::Alarm, Self::Alarm) => true,
            (Self::Timezone, Self::Timezone) => true,
            (Self::Other(a, al), Self::Other(b, bl)) => a == b && al == bl,
            _ => false,
        }
    }
}

/// Unfold RFC 5545 line folding: a CRLF followed by a space or tab is a
/// continuation, not a new line.
///
/// Feeds wrap at 75 octets mid-word (and mid-UTF-8-sequence), so unfolding
/// before anything else is what stops long summaries and URLs being mangled.
///
/// Materializes every line; the parser itself uses the bounded, borrowing
/// [`LogicalLines`] instead. Kept for fixtures and tests.
pub fn unfold(input: &str) -> Vec<String> {
    LogicalLines::new(input)
        .map(|l| l.text().into_owned())
        .collect()
}

/// One unfolded content line, still borrowed from the document.
#[derive(Debug, Clone, Copy)]
pub struct RawLine<'a> {
    /// The raw span from the first physical line through its last
    /// continuation, fold markers included.
    span: &'a str,
    /// The first physical line (no terminator).
    first: &'a str,
    /// Unfolded length in bytes, known before anything is allocated.
    len: usize,
    folded: bool,
}

impl<'a> RawLine<'a> {
    /// Unfolded length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The property name as far as the first physical line shows it,
    /// uppercased — enough to classify an oversized line without unfolding it.
    fn name_hint(&self) -> Option<String> {
        let end = self.first.find([';', ':'])?;
        Some(self.first[..end].trim().to_ascii_uppercase())
    }

    /// The unfolded text. Borrowed unless the line was actually folded; the
    /// owned case allocates exactly [`Self::len`] bytes.
    pub fn text(&self) -> std::borrow::Cow<'a, str> {
        if !self.folded {
            return std::borrow::Cow::Borrowed(self.first);
        }
        let mut out = String::with_capacity(self.len);
        for (i, raw) in self.span.split('\n').enumerate() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if i == 0 {
                out.push_str(line);
            } else {
                // Continuations start with exactly one space or tab.
                out.push_str(&line[1..]);
            }
        }
        std::borrow::Cow::Owned(out)
    }
}

/// Incremental unfolding: yields one logical line at a time without copying
/// the document, so the parser can size a line before it allocates it.
#[derive(Debug, Clone)]
pub struct LogicalLines<'a> {
    rest: Option<&'a str>,
}

impl<'a> LogicalLines<'a> {
    pub fn new(input: &'a str) -> Self {
        LogicalLines { rest: Some(input) }
    }
}

/// Split off one physical line: `(line without terminator, remainder)`.
fn physical(s: &str) -> (&str, Option<&str>) {
    match s.find('\n') {
        Some(i) => {
            let line = &s[..i];
            (line.strip_suffix('\r').unwrap_or(line), Some(&s[i + 1..]))
        }
        None => (s.strip_suffix('\r').unwrap_or(s), None),
    }
}

impl<'a> Iterator for LogicalLines<'a> {
    type Item = RawLine<'a>;

    fn next(&mut self) -> Option<RawLine<'a>> {
        let start = self.rest?;
        let (first, mut after) = physical(start);
        let mut len = first.len();
        let mut folded = false;
        // Byte offset in `start` just past the last line of this logical line
        // (before its terminator).
        let mut end = first.len();
        while let Some(r) = after
            && r.starts_with([' ', '\t'])
        {
            let (cont, next) = physical(r);
            let offset = start.len() - r.len();
            end = offset + cont.len();
            // Every byte of a continuation but its leading fold marker.
            len = len.saturating_add(cont.len() - 1);
            folded = true;
            after = next;
        }
        self.rest = after;
        Some(RawLine {
            span: &start[..end],
            first,
            len,
            folded,
        })
    }
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

    let mut parts = split_outside_quotes(head, ';')?.into_iter();
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut params = BTreeMap::new();
    for p in parts {
        let (k, v) = p.split_once('=')?;
        let key = k.trim().to_ascii_uppercase();
        if key.is_empty() || key.contains([';', ':', '"', ',']) {
            return None;
        }
        // PARAM-VALUE allows a comma-separated list. Keep it as one string in
        // ContentLine's compatibility map, but only treat commas outside
        // quoted-string values as separators. Decode RFC 6868 on each member.
        let values = split_outside_quotes(v.trim(), ',')?;
        let decoded = values
            .iter()
            .map(|value| decode_param_value(value.trim()))
            .collect::<Option<Vec<_>>>()?;
        let value = decoded.join(",");
        if matches!(key.as_str(), "TZID" | "VALUE")
            && params.get(&key).is_some_and(|previous| previous != &value)
        {
            return None;
        }
        params.insert(key, value);
    }
    let value = if is_text_property(&name) {
        unescape(value)
    } else {
        value.to_string()
    };
    Some(ContentLine {
        name,
        params,
        value,
    })
}

/// Split on a delimiter outside quoted-string values. An open quote makes the
/// content line malformed. RFC 5545 quoted-string does not use backslash
/// escaping; RFC 6868 caret encoding represents quote characters instead.
fn split_outside_quotes(input: &str, delimiter: char) -> Option<Vec<&str>> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in input.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c == delimiter && !in_quotes => {
                pieces.push(&input[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if in_quotes {
        return None;
    }
    pieces.push(&input[start..]);
    Some(pieces)
}

/// Parse one RFC 5545 paramtext or quoted-string and decode RFC 6868 escapes.
fn decode_param_value(value: &str) -> Option<String> {
    let value = if let Some(inner) = value.strip_prefix('"') {
        inner.strip_suffix('"')?
    } else {
        if value.contains('"') {
            return None;
        }
        value
    };
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '^' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some('\'') => out.push('"'),
            Some('^') => out.push('^'),
            Some(other) => {
                out.push('^');
                out.push(other);
            }
            None => out.push('^'),
        }
    }
    Some(out)
}

/// Known RFC 5545 TEXT properties, plus the common calendar name/description
/// extensions, whose values receive TEXT unescaping. URI, CAL-ADDRESS,
/// date/time, recurrence, status, and other extension values are retained
/// byte-for-byte.
fn is_text_property(name: &str) -> bool {
    matches!(
        name,
        "UID"
            | "SUMMARY"
            | "DESCRIPTION"
            | "LOCATION"
            | "CATEGORIES"
            | "COMMENT"
            | "CONTACT"
            | "RESOURCES"
            | "TZNAME"
            | "X-WR-CALNAME"
            | "X-WR-CALDESC"
    )
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
///
/// A convenience for fixtures and tests: it meters against a private pool at
/// the default budget and returns nothing on overflow. Every production source
/// parses through [`parse_ics_admitted`] with its account's meter instead.
pub fn parse_ics(input: &str, default_zone: &str) -> Vec<CalEvent> {
    let mut meter = AdmissionMeter::isolated(AdmissionBudget::default());
    let mut out = Vec::new();
    match parse_ics_admitted(input, default_zone, &mut meter, &mut out) {
        Ok(()) => out,
        Err(_) => Vec::new(),
    }
}

/// Retained VEVENT properties. Anything else is dropped by [`Builder::property`],
/// so an oversized line naming it can be skipped without being unfolded.
fn is_retained_property(name: &str) -> bool {
    matches!(
        name,
        "UID"
            | "SUMMARY"
            | "DESCRIPTION"
            | "LOCATION"
            | "URL"
            | "CATEGORIES"
            | "ORGANIZER"
            | "STATUS"
            | "DTSTART"
            | "DTEND"
            | "DURATION"
            | "RRULE"
            | "RDATE"
            | "EXDATE"
            | "LAST-MODIFIED"
            | "DTSTAMP"
    ) || name.starts_with("X-")
}

/// Parse `input` into `out`, charging `meter` for everything retained.
///
/// Admission happens *during* parsing: each logical line is sized before it is
/// unfolded, each retained value is charged before it is stored, and each event
/// is admitted as a record before its [`CalEvent`] is built. Overflow returns
/// the typed error at that point — `out` then holds a prefix the caller must
/// discard, never publish.
pub fn parse_ics_admitted(
    input: &str,
    default_zone: &str,
    meter: &mut AdmissionMeter,
    out: &mut Vec<CalEvent>,
) -> Result<(), AdmissionError> {
    parse_ics_window(input, default_zone, None, meter, out)
}

/// [`parse_ics_admitted`], admitting only events that can occur within
/// `window` (inclusive dates, with two days of slack each side for zones).
///
/// A whole-document feed (a subscribed or local `.ics`) routinely carries
/// years of history. Counting all of it against `max_events` would refuse
/// calendars whose relevant part is small, so events that provably cannot
/// touch the window — a one-off that ends before it or starts after it, a
/// series whose `UNTIL` (plus the event's own length) ends before it — are
/// parsed, released, and not admitted. Anything that might reach the window
/// (a `COUNT` or open-ended rule, an RDATE inside it) is admitted.
pub fn parse_ics_window(
    input: &str,
    default_zone: &str,
    window: Option<(NaiveDate, NaiveDate)>,
    meter: &mut AdmissionMeter,
    out: &mut Vec<CalEvent>,
) -> Result<(), AdmissionError> {
    parse_ics_window_checked(input, default_zone, window, meter, out, || Ok(()))
}

/// The windowed parser with a cooperative check between logical content
/// lines. Blocking sources use this to enforce their total deadline even for
/// one large document; ordinary callers use [`parse_ics_window`].
pub fn parse_ics_window_checked(
    input: &str,
    default_zone: &str,
    window: Option<(NaiveDate, NaiveDate)>,
    meter: &mut AdmissionMeter,
    out: &mut Vec<CalEvent>,
    check: impl FnMut() -> Result<(), AdmissionError>,
) -> Result<(), AdmissionError> {
    parse_ics_window_reported(
        input,
        default_zone,
        window,
        meter,
        out,
        check,
        &mut IcsReport::default(),
    )
}

/// Windowed parser that reports bounded structural and per-event diagnostics.
/// Existing lenient entry points retain their historical output behavior.
pub fn parse_ics_window_reported(
    input: &str,
    default_zone: &str,
    window: Option<(NaiveDate, NaiveDate)>,
    meter: &mut AdmissionMeter,
    out: &mut Vec<CalEvent>,
    mut check: impl FnMut() -> Result<(), AdmissionError>,
    report: &mut IcsReport,
) -> Result<(), AdmissionError> {
    report.diagnostics.clear();
    report.overflow = 0;
    report.document_failure = false;
    let timezone_bindings = prescan_timezones(input, &mut check, report)?;
    let mut cur: Option<Builder> = None;
    let mut components: Vec<ComponentName> = Vec::new();
    let mut event_depth: Option<usize> = None;
    let mut cal_name = String::new();
    let mut saw_calendar = false;
    let mut closed_calendar = false;

    for raw in LogicalLines::new(input) {
        check()?;
        if raw.len() > MAX_LINE_BYTES {
            // Classify from the first physical line without unfolding.
            let hint = raw.name_hint();
            let in_event = cur.is_some();
            let in_alarm = event_depth.is_some_and(|depth| {
                components.len() > depth
                    && components
                        .last()
                        .is_some_and(|part| matches!(part, ComponentName::Alarm))
            });
            let retained = match hint.as_deref() {
                _ if !in_event => hint.as_deref() == Some("X-WR-CALNAME"),
                // Inside an event, a line whose name is itself folded can't be
                // classified, and an oversized BEGIN/END would desynchronize
                // the component stack if skipped — refuse rather than guess.
                None | Some("BEGIN") | Some("END") => true,
                Some(n) if event_depth.is_none_or(|depth| components.len() <= depth) => {
                    is_retained_property(n)
                }
                Some(n) => in_alarm && n == "TRIGGER",
            };
            if retained {
                return Err(AdmissionError::new(if in_event {
                    AdmissionLimit::EventBytes
                } else {
                    AdmissionLimit::LineBytes
                }));
            }
            // Not a value we keep: skip it without allocating.
            continue;
        }
        let text = raw.text();
        if text.trim().is_empty() {
            continue;
        }
        let Some(line) = parse_line(&text) else {
            if !components.is_empty() {
                report.record(IcsDiagnostic::MalformedLine);
                if let Some(builder) = cur.as_mut()
                    && event_depth.is_some_and(|depth| components.len() == depth)
                    && raw.name_hint().as_deref().is_some_and(is_shape_property)
                {
                    builder.quarantined = true;
                }
            }
            continue;
        };
        match line.name.as_str() {
            "BEGIN" => {
                let component = ComponentName::parse(&line.value);
                let depth_exceeded = if cur.is_some() && !matches!(component, ComponentName::Event)
                {
                    event_depth.is_some_and(|depth| {
                        components.len().saturating_sub(depth) >= MAX_COMPONENT_DEPTH
                    })
                } else {
                    components.len() >= MAX_COMPONENT_DEPTH
                };
                if depth_exceeded {
                    report.record(IcsDiagnostic::Truncated);
                    return Err(AdmissionError::new(AdmissionLimit::Nesting));
                }
                if matches!(component, ComponentName::Calendar) {
                    saw_calendar = true;
                }
                if matches!(component, ComponentName::Event) {
                    if cur.is_some() {
                        report.record(IcsDiagnostic::NestedEvent);
                    }
                    meter.begin_event();
                    meter.charge_event(2 * std::mem::size_of::<CalEvent>(), 0)?;
                    cur = Some(Builder::default());
                    event_depth = Some(components.len() + 1);
                }
                components.push(component);
                if matches!(component, ComponentName::Event) {
                    continue;
                }
                if cur.is_some() {
                    continue;
                }
            }
            "END" => {
                let matching_depth = components
                    .iter()
                    .rposition(|component| component.matches(&line.value));
                let matched = matching_depth.is_some();
                if !matched {
                    report.record(IcsDiagnostic::MismatchedEnd);
                }
                let ending_event = line.value.eq_ignore_ascii_case("VEVENT");
                if ending_event && event_depth.is_some_and(|depth| components.len() > depth) {
                    report.record(IcsDiagnostic::UnclosedChild);
                }
                if ending_event {
                    if let Some(depth) = event_depth {
                        components.truncate(depth.saturating_sub(1));
                    } else if let Some(depth) = matching_depth {
                        components.truncate(depth);
                    }
                    event_depth = None;
                    match cur.take() {
                        Some(b) if b.quarantined => {
                            report.record(IcsDiagnostic::InvalidEvent);
                            meter.abandon_event();
                        }
                        Some(b) if window.is_some_and(|w| !b.may_occur_in(w, default_zone)) => {
                            meter.abandon_event();
                        }
                        Some(b) if b.start.is_some() => {
                            let extra =
                                cal_name.len() + if b.uid.is_empty() { ENTRY_OVERHEAD } else { 0 };
                            meter.charge_event(extra, 0)?;
                            meter.admit_event()?;
                            if let Some(mut e) = b.finish(default_zone, report) {
                                e.calendar = cal_name.clone();
                                out.push(e);
                            } else {
                                report.record(IcsDiagnostic::InvalidEvent);
                            }
                        }
                        Some(_) => {
                            report.record(IcsDiagnostic::EventWithoutStart);
                            meter.abandon_event();
                        }
                        None => meter.abandon_event(),
                    }
                    continue;
                }
                if let Some(depth) = matching_depth {
                    if matches!(components[depth], ComponentName::Calendar)
                        && line.value.eq_ignore_ascii_case("VCALENDAR")
                    {
                        closed_calendar = true;
                    }
                    components.truncate(depth);
                }
                continue;
            }
            _ => {}
        }
        if line.name == "X-WR-CALNAME" && cur.is_none() {
            cal_name = line.value;
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        let child_components = event_depth.is_some_and(|depth| components.len() > depth);
        if child_components {
            let in_alarm = event_depth.is_some_and(|depth| {
                components.len() > depth
                    && components
                        .last()
                        .is_some_and(|part| matches!(part, ComponentName::Alarm))
            });
            if in_alarm
                && line.name == "TRIGGER"
                && let Some(mins) = parse_trigger_minutes(&line.value)
            {
                meter.charge_event(ENTRY_OVERHEAD, 1)?;
                b.reminders.push(Reminder {
                    minutes_before: mins,
                });
            }
            continue;
        }
        b.property(&line, meter, report, &timezone_bindings)?;
    }
    if cur.is_some() || !components.is_empty() {
        report.record(IcsDiagnostic::Truncated);
        if components
            .iter()
            .any(|part| !matches!(part, ComponentName::Calendar | ComponentName::Event))
        {
            report.record(IcsDiagnostic::UnclosedChild);
        }
    }
    // Report absent envelopes, and calendars that began but never closed.
    if !saw_calendar || !closed_calendar {
        report.record(IcsDiagnostic::MissingEnvelope);
    }
    // A document that ends mid-event keeps nothing of it, as before.
    meter.abandon_event();
    Ok(())
}

const MAX_TIMEZONE_BINDINGS: usize = 64;
const MAX_TZID_BYTES: usize = 128;

/// Bind at most 64 VTIMEZONE definitions. Definitions beyond the cap and
/// names over 128 bytes are skipped and reported as truncated metadata.
fn prescan_timezones(
    input: &str,
    check: &mut impl FnMut() -> Result<(), AdmissionError>,
    report: &mut IcsReport,
) -> Result<BTreeMap<String, Option<String>>, AdmissionError> {
    let mut bindings = BTreeMap::new();
    let mut stack = Vec::new();
    let mut timezone_depth = None;
    let mut definition: Option<(String, Option<String>)> = None;
    for raw in LogicalLines::new(input) {
        check()?;
        if raw.len() > MAX_LINE_BYTES {
            if timezone_depth.is_some() {
                report.record(IcsDiagnostic::Truncated);
            }
            continue;
        }
        let text = raw.text();
        let Some(line) = parse_line(&text) else {
            continue;
        };
        match line.name.as_str() {
            "BEGIN" => {
                let component = ComponentName::parse(&line.value);
                let pushed = stack.len() < MAX_COMPONENT_DEPTH;
                if pushed {
                    stack.push(component);
                }
                if pushed && matches!(component, ComponentName::Timezone) {
                    timezone_depth = Some(stack.len());
                    definition = Some((String::new(), None));
                }
            }
            "END" => {
                let ending_timezone = line.value.eq_ignore_ascii_case("VTIMEZONE");
                if ending_timezone {
                    if let (Some((tzid, location)), Some(_)) = (definition.take(), timezone_depth) {
                        if !tzid.is_empty() && tzid.len() <= MAX_TZID_BYTES {
                            if bindings.len() < MAX_TIMEZONE_BINDINGS {
                                let canonical = resolve_tzid(&tzid)
                                    .or_else(|| location.as_deref().and_then(resolve_tzid))
                                    .map(|zone| zone.name().to_string());
                                if canonical.is_none() {
                                    report.record(IcsDiagnostic::UnmappedTimeZone);
                                }
                                bindings.entry(tzid).or_insert(canonical);
                            } else {
                                report.record(IcsDiagnostic::Truncated);
                            }
                        } else {
                            report.record(IcsDiagnostic::Truncated);
                        }
                    }
                    timezone_depth = None;
                }
                if let Some(depth) = stack.iter().rposition(|part| part.matches(&line.value)) {
                    stack.truncate(depth);
                }
            }
            "TZID" | "X-LIC-LOCATION" => {
                if timezone_depth == Some(stack.len())
                    && let Some((tzid, location)) = &mut definition
                {
                    if line.value.len() > MAX_TZID_BYTES {
                        report.record(IcsDiagnostic::Truncated);
                    } else if line.name == "TZID" {
                        if tzid.is_empty() {
                            *tzid = line.value;
                        }
                    } else if location.is_none() {
                        *location = Some(line.value);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(bindings)
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
    duration_seen: bool,
    rrules: Vec<RRule>,
    rdates: Vec<EventTime>,
    exdates: Vec<EventTime>,
    reminders: Vec<Reminder>,
    organizer: String,
    categories: String,
    last_modified: i64,
    extra: BTreeMap<String, String>,
    seen_singletons: BTreeSet<&'static str>,
    quarantined: bool,
}

fn is_shape_property(name: &str) -> bool {
    matches!(
        name,
        "DTSTART" | "DTEND" | "DURATION" | "RRULE" | "RDATE" | "EXDATE" | "UID"
    )
}

fn singleton_property(name: &str) -> Option<&'static str> {
    match name {
        "UID" => Some("UID"),
        "SUMMARY" => Some("SUMMARY"),
        "DESCRIPTION" => Some("DESCRIPTION"),
        "LOCATION" => Some("LOCATION"),
        "URL" => Some("URL"),
        "STATUS" => Some("STATUS"),
        "DTSTART" => Some("DTSTART"),
        "DTEND" => Some("DTEND"),
        "DURATION" => Some("DURATION"),
        "ORGANIZER" => Some("ORGANIZER"),
        "LAST-MODIFIED" => Some("LAST-MODIFIED"),
        "DTSTAMP" => Some("DTSTAMP"),
        _ => None,
    }
}

/// The calendar date a time falls on (its local date; UTC for an instant).
fn day_of(t: &EventTime) -> NaiveDate {
    match t {
        EventTime::Date { date } => *date,
        EventTime::Zoned { local, .. } => local.date(),
        EventTime::Instant { at } => at.date_naive(),
    }
}

impl Builder {
    /// Whether any occurrence of this event can touch `[from, to]`. Errs on
    /// the side of "yes": only provably-outside events are excluded.
    fn may_occur_in(&self, (from, to): (NaiveDate, NaiveDate), default_zone: &str) -> bool {
        let Some(start) = self.start.as_ref().map(day_of) else {
            return false;
        };
        // Two days of slack each side: the same instant can land two calendar
        // days apart between the extreme zones (UTC+14 vs UTC-12), so a
        // narrower margin could exclude an event the expansion would place
        // inside the window.
        let slack = chrono::Duration::days(2);
        let lo = from.checked_sub_signed(slack).unwrap_or(NaiveDate::MIN);
        let hi = to.checked_add_signed(slack).unwrap_or(NaiveDate::MAX);
        // The event's own length in days, so a long occurrence that starts
        // before the window but ends inside it still counts.
        let end = self.end.as_ref().map(day_of).unwrap_or_else(|| {
            let extra = self.duration.map_or(0, |d| d.num_days()).max(0) + 1;
            start
                .checked_add_signed(chrono::Duration::days(extra))
                .unwrap_or(NaiveDate::MAX)
        });
        let span = chrono::Duration::days((end - start).num_days().max(0));
        let reaches =
            |d: NaiveDate| d <= hi && d.checked_add_signed(span).unwrap_or(NaiveDate::MAX) >= lo;
        if self.rdates.iter().any(|t| reaches(day_of(t))) || reaches(start) {
            return true;
        }
        if self.rrules.is_empty() || start > hi {
            return false;
        }
        // A rule only ends provably before the window through UNTIL.
        let home = default_zone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let zone = self
            .start
            .as_ref()
            .map(|start| super::recur::series_zone(start, home));
        self.rrules.iter().any(|r| {
            super::recur::until_bound(r, zone)
                .is_none_or(|u| u.date().checked_add_signed(span).unwrap_or(NaiveDate::MAX) >= lo)
        })
    }

    /// Record one property, charging `meter` for what it retains first.
    fn property(
        &mut self,
        line: &ContentLine,
        meter: &mut AdmissionMeter,
        report: &mut IcsReport,
        timezone_bindings: &BTreeMap<String, Option<String>>,
    ) -> Result<(), AdmissionError> {
        let mut normalized;
        let line = if matches!(line.name.as_str(), "DTSTART" | "DTEND" | "RDATE" | "EXDATE")
            && let Some(tzid) = line.params.get("TZID")
            && !tzid.is_empty()
        {
            match timezone_bindings.get(tzid) {
                Some(Some(name)) => {
                    normalized = line.clone();
                    normalized.params.insert("TZID".to_string(), name.clone());
                    &normalized
                }
                Some(None) => line,
                None => match resolve_tzid(tzid) {
                    Some(zone) => {
                        normalized = line.clone();
                        normalized
                            .params
                            .insert("TZID".to_string(), zone.name().to_string());
                        &normalized
                    }
                    None => {
                        self.quarantined = true;
                        report.record(IcsDiagnostic::UnknownTimeZone);
                        line
                    }
                },
            }
        } else {
            line
        };
        let text = line.value.len() + ENTRY_OVERHEAD;
        let tz = line.params.get("TZID").map_or(0, String::len);
        if let Some(name) = singleton_property(&line.name)
            && !self.seen_singletons.insert(name)
        {
            report.record(IcsDiagnostic::DuplicateProperty(name));
            if matches!(name, "UID" | "DTSTART" | "DTEND" | "DURATION") {
                self.quarantined = true;
            }
        }
        match line.name.as_str() {
            "UID" => {
                meter.charge_event(text, 0)?;
                self.uid = line.value.clone();
            }
            "SUMMARY" => {
                meter.charge_event(text, 0)?;
                self.summary = line.value.clone();
            }
            "DESCRIPTION" => {
                meter.charge_event(text, 0)?;
                self.description = line.value.clone();
            }
            "LOCATION" => {
                meter.charge_event(text, 0)?;
                self.location = line.value.clone();
            }
            "URL" => {
                meter.charge_event(text, 0)?;
                self.url = line.value.clone();
            }
            "CATEGORIES" => {
                meter.charge_event(text, 0)?;
                self.categories = line.value.clone();
            }
            "ORGANIZER" => {
                meter.charge_event(text, 0)?;
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
                    "CONFIRMED" => Some(EventStatus::Confirmed),
                    _ => {
                        report.record(IcsDiagnostic::InvalidEvent);
                        Some(EventStatus::Confirmed)
                    }
                }
            }
            "DTSTART" => {
                meter.charge_event(tz + ENTRY_OVERHEAD, 0)?;
                self.start = parse_time(line, "");
                if self.start.is_none() {
                    report.record(IcsDiagnostic::InvalidEvent);
                }
            }
            "DTEND" => {
                meter.charge_event(tz + ENTRY_OVERHEAD, 0)?;
                self.end = parse_time(line, "");
                if self.end.is_none() {
                    self.quarantined = true;
                }
            }
            "DURATION" => {
                self.duration_seen = true;
                self.duration = parse_duration(&line.value);
                if self.duration.is_none() {
                    self.quarantined = true;
                }
            }
            "RRULE" => {
                // A rule's BY* lists are at most one entry per comma, each at
                // most 8 bytes — charged before the rule is parsed.
                let parts = line.value.matches(',').count() + 1;
                meter.charge_event(
                    std::mem::size_of::<RRule>()
                        .saturating_add(parts.saturating_mul(8))
                        .saturating_add(ENTRY_OVERHEAD),
                    parts.saturating_add(1),
                )?;
                match RRule::parse(&line.value) {
                    Ok(r) => self.rrules.push(r),
                    Err(_) => self.quarantined = true,
                }
            }
            "RDATE" | "EXDATE" => {
                let n = line
                    .value
                    .split(',')
                    .filter(|v| !v.trim().is_empty())
                    .count();
                meter.charge_event(
                    n.saturating_mul(std::mem::size_of::<EventTime>() + tz + ENTRY_OVERHEAD),
                    n,
                )?;
                match multi_time(line) {
                    Some(times) if line.name == "RDATE" => self.rdates.extend(times),
                    Some(times) => self.exdates.extend(times),
                    None => self.quarantined = true,
                }
            }
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
                meter.charge_event(n.len() + text, 1)?;
                self.extra.insert(n.to_string(), line.value.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self, default_zone: &str, report: &mut IcsReport) -> Option<CalEvent> {
        if self.quarantined {
            return None;
        }
        let start = self.start?;
        if self.end.is_some() && self.duration_seen {
            return None;
        }
        // RFC 5545: DTEND is optional. With a DURATION, add it; with neither, a
        // dated event lasts one day and a timed event is a point in time.
        let end = if let Some(end) = self.end.clone() {
            end
        } else {
            match &start {
                EventTime::Date { date } => {
                    let default_end = date.succ_opt()?;
                    match self.duration {
                        None => EventTime::Date { date: default_end },
                        Some(duration) => {
                            let seconds = duration.num_seconds();
                            if seconds > 0 && seconds % 86_400 == 0 {
                                match date.checked_add_signed(duration) {
                                    Some(date) => EventTime::Date { date },
                                    None => {
                                        report.record(IcsDiagnostic::InvalidEvent);
                                        EventTime::Date { date: default_end }
                                    }
                                }
                            } else {
                                report.record(IcsDiagnostic::InvalidEvent);
                                EventTime::Date { date: default_end }
                            }
                        }
                    }
                }
                EventTime::Zoned { local, zone } => {
                    let local = local.checked_add_signed(self.duration.unwrap_or_default())?;
                    EventTime::Zoned {
                        local,
                        zone: zone.clone(),
                    }
                }
                EventTime::Instant { at } => {
                    let at = at.checked_add_signed(self.duration.unwrap_or_default())?;
                    EventTime::Instant { at }
                }
            }
        };
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
        if validate_event(&e).is_err() {
            return None;
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
fn multi_time(line: &ContentLine) -> Option<Vec<EventTime>> {
    let mut one = ContentLine {
        name: line.name.clone(),
        params: line.params.clone(),
        value: String::new(),
    };
    let mut out = Vec::new();
    for v in line.value.split(',') {
        if v.trim().is_empty() {
            continue;
        }
        one.value.clear();
        one.value.push_str(v.trim());
        out.push(parse_time(&one, "")?);
    }
    Some(out)
}

/// Parse an RFC 5545 DURATION (`PT1H30M`, `P1D`).
pub fn parse_duration(v: &str) -> Option<chrono::Duration> {
    let normalized = v.trim().to_ascii_uppercase();
    let mut chars = normalized.chars().peekable();
    let negative = match chars.peek() {
        Some('-') => {
            chars.next()?;
            true
        }
        Some('+') => {
            chars.next()?;
            false
        }
        _ => false,
    };
    if chars.next()? != 'P' {
        return None;
    }

    let mut in_time = false;
    let mut saw_component = false;
    let mut saw_time_component = false;
    let mut week_component = false;
    let mut last_rank = None;
    let mut total_seconds = 0i64;
    let mut number = 0i64;
    let mut has_number = false;

    for c in chars {
        if c.is_ascii_digit() {
            number = number
                .checked_mul(10)?
                .checked_add(i64::from(c as u8 - b'0'))?;
            has_number = true;
            continue;
        }
        if c == 'T' {
            if in_time || week_component || has_number {
                return None;
            }
            in_time = true;
            continue;
        }
        if !has_number {
            return None;
        }
        let (rank, scale) = match (in_time, c) {
            (false, 'W') if !saw_component && last_rank.is_none() => {
                week_component = true;
                (0, 604_800)
            }
            (false, 'D') if !week_component => (1, 86_400),
            (true, 'H') if !week_component => (2, 3_600),
            (true, 'M') if !week_component => (3, 60),
            (true, 'S') if !week_component => (4, 1),
            _ => return None,
        };
        if last_rank.is_some_and(|previous| rank <= previous) {
            return None;
        }
        total_seconds = total_seconds.checked_add(number.checked_mul(scale)?)?;
        last_rank = Some(rank);
        saw_component = true;
        saw_time_component |= in_time;
        number = 0;
        has_number = false;
    }
    if has_number || !saw_component || (in_time && !saw_time_component) {
        return None;
    }
    if negative {
        total_seconds = total_seconds.checked_neg()?;
    }
    chrono::Duration::try_seconds(total_seconds)
}

#[cfg(test)]
#[path = "ics_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ics_structure_tests.rs"]
mod structure_tests;

#[cfg(test)]
#[path = "ics_tz_tests.rs"]
mod tz_tests;
