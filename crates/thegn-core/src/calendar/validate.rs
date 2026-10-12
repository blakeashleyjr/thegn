//! Domain checks shared by calendar admission boundaries.

use std::fmt;

use super::{CalEvent, EventTime};

/// A value-free reason that a calendar event cannot be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDefect {
    EmptyUid,
    MixedShape,
    Reversed,
    RecurrenceShape,
    UnknownZone,
}

impl fmt::Display for EventDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyUid => "empty UID",
            Self::MixedShape => "mixed all-day and timed endpoints",
            Self::Reversed => "end precedes start",
            Self::RecurrenceShape => "incompatible recurrence values",
            Self::UnknownZone => "unknown time zone",
        })
    }
}

/// Validate an event before it enters provider output or the calendar cache.
pub fn validate_event(event: &CalEvent) -> Result<(), EventDefect> {
    if event.uid.trim().is_empty() {
        return Err(EventDefect::EmptyUid);
    }

    if matches!(
        (&event.start, &event.end),
        (
            EventTime::Date { .. },
            EventTime::Zoned { .. } | EventTime::Instant { .. }
        ) | (
            EventTime::Zoned { .. } | EventTime::Instant { .. },
            EventTime::Date { .. }
        )
    ) {
        return Err(EventDefect::MixedShape);
    }

    if [&event.start, &event.end].into_iter().any(|time| {
        matches!(time, EventTime::Zoned { zone, .. }
            if !zone.as_str().trim().is_empty() && zone.resolve().is_none())
    }) {
        return Err(EventDefect::UnknownZone);
    }

    super::span_dates(&event.start, &event.end, chrono_tz::UTC)
        .map_err(|_| EventDefect::Reversed)?;

    if let Some(recurrence) = event.recurrence.as_ref().filter(|recurrence| {
        !recurrence.rules.is_empty()
            || !recurrence.rdates.is_empty()
            || !recurrence.exdates.is_empty()
    }) && (super::nominal_span(event).is_none()
        || super::recur::check_values(recurrence, &event.start).is_err())
    {
        return Err(EventDefect::RecurrenceShape);
    }
    Ok(())
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
