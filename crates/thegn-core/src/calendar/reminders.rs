//! Which reminders have come due — pure, so the check costs nothing and can
//! ride the ticker instead of needing a timer thread of its own.
//!
//! A dedicated "sleep until the next reminder" thread would be more precise,
//! but it is a second always-on thread and one refactor away from a spin. The
//! ticker already wakes; worst-case lateness of one coarse slot is irrelevant
//! for a "10 minutes before" reminder.

use chrono::{DateTime, Utc};

use super::{CalEvent, GapPolicy};

/// A reminder that should fire now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueReminder {
    /// `"<source>/<uid>"`.
    pub event_id: String,
    pub title: String,
    pub location: String,
    pub url: String,
    /// When the occurrence starts, unix ms.
    pub occurrence_start_ms: i64,
    /// The reminder's configured lead time.
    pub trigger_mins: u32,
    /// Whole minutes until the occurrence starts; negative if it has begun.
    pub starts_in_mins: i64,
}

impl DueReminder {
    /// A restart-durable identity for this exact reminder firing.
    ///
    /// Encoded into the notification's existing `source_ref` so a dedupe is a
    /// `SELECT 1` and no new schema is needed. All three parts matter: the same
    /// event can have several lead times, and a recurring event fires once per
    /// occurrence.
    pub fn source_ref(&self) -> String {
        format!(
            "cal:{}@{}+{}",
            self.event_id, self.occurrence_start_ms, self.trigger_mins
        )
    }
}

/// Reminders whose trigger moment falls in `(last_checked_ms, now_ms]`.
///
/// The half-open window is what makes this idempotent across ticks: a reminder
/// fires on the one tick that straddles its trigger, not on every tick from
/// then until the event starts.
///
/// `default_reminders` applies to events whose source supplied none of their
/// own; an event with explicit reminders uses only those.
pub fn due(
    events: &[CalEvent],
    home: chrono_tz::Tz,
    default_reminders: &[super::Reminder],
    last_checked_ms: i64,
    now_ms: i64,
) -> Vec<DueReminder> {
    // A backwards or absurd window (a suspend/resume, a wall-clock jump) would
    // otherwise replay hours of reminders at once. Clamp to one hour of
    // catch-up: anything older has stopped being worth raising.
    let since = last_checked_ms.clamp(now_ms - 3_600_000, now_ms);
    let mut out = Vec::new();
    for e in events {
        // A cancelled meeting should not nag.
        if e.status == super::EventStatus::Cancelled {
            continue;
        }
        let Some(start) = e.start.instant_in(home, GapPolicy::ShiftForward) else {
            continue;
        };
        let start_ms = start.timestamp_millis();
        let reminders = if e.reminders.is_empty() {
            default_reminders
        } else {
            &e.reminders[..]
        };
        for r in reminders {
            let trigger = start_ms - (r.minutes_before as i64) * 60_000;
            if trigger > since && trigger <= now_ms {
                out.push(DueReminder {
                    event_id: e.id().0,
                    title: e.title.clone(),
                    location: e.location.clone(),
                    url: e.url.clone(),
                    occurrence_start_ms: start_ms,
                    trigger_mins: r.minutes_before,
                    starts_in_mins: (start_ms - now_ms) / 60_000,
                });
            }
        }
    }
    out.sort_by_key(|d| (d.occurrence_start_ms, d.trigger_mins));
    out
}

/// The soonest upcoming occurrence, for a "next event" readout.
pub fn next_event(
    events: &[CalEvent],
    home: chrono_tz::Tz,
    now: DateTime<Utc>,
) -> Option<&CalEvent> {
    events
        .iter()
        .filter(|e| e.status != super::EventStatus::Cancelled)
        .filter_map(|e| {
            e.start
                .instant_in(home, GapPolicy::ShiftForward)
                .filter(|s| *s >= now)
                .map(|s| (s, e))
        })
        .min_by_key(|(s, _)| *s)
        .map(|(_, e)| e)
}

#[cfg(test)]
#[path = "reminders_tests.rs"]
mod tests;
