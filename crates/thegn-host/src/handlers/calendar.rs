//! Raising calendar reminders as ordinary notifications.
//!
//! Deliberately thin: by going through the normal notification path,
//! reminders inherit the whole `[[notifications.rules]]` engine — routing,
//! `min_priority`, DND quiet hours, sounds, desktop toasts — with zero new
//! config keys. That reuse is why there is no bespoke reminder delivery here.

use termwiz::terminal::TerminalWaker;
use thegn_core::calendar::DueReminder;
use thegn_core::db::Db;
use thegn_core::notification::NotificationKind;
use thegn_core::store::NotificationStore;

/// Raise one due reminder.
///
/// Idempotent across restarts: the reminder's
/// `(event, occurrence, lead time)` identity is stored as the notification's
/// `source_ref`, so re-checking after a restart finds the existing row and
/// does nothing rather than re-nagging.
pub(crate) fn raise_reminder(r: &DueReminder, waker: &TerminalWaker) {
    let source_ref = r.source_ref();
    let message = message_for(r);
    // Already on the background lane (see
    // `hydrate_calendar::spawn_reminder_check`), so this runs inline — nesting
    // another `spawn_bg` would take a second permit from the eight-permit lane
    // for no gain.
    let Ok(db) = Db::open() else { return };
    let kind = NotificationKind::CalendarReminder.as_str();
    // The restart guard. A `SELECT 1` rather than a new table.
    if db.has_notification(kind, &source_ref).unwrap_or(false) {
        return;
    }
    // best-effort: the inbox is a cache, and a reminder that fails to record
    // must not take down the compositor.
    if db.put_notification(kind, &source_ref, &message, "").is_ok() {
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    }
}

/// `standup in 10m — Room 3`
fn message_for(r: &DueReminder) -> String {
    let when = match r.starts_in_mins {
        m if m <= 0 => "now".to_string(),
        1 => "in 1m".to_string(),
        m if m < 60 => format!("in {m}m"),
        m if m % 60 == 0 => format!("in {}h", m / 60),
        m => format!("in {}h{:02}", m / 60, m % 60),
    };
    let mut msg = format!("{} {when}", r.title);
    // The most actionable detail available — a meeting URL beats a room name.
    let detail = if !r.url.trim().is_empty() {
        r.url.trim()
    } else {
        r.location.trim()
    };
    if !detail.is_empty() {
        msg.push(' ');
        msg.push_str(crate::caps::active_glyphs().emdash);
        msg.push(' ');
        msg.push_str(detail);
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rem(title: &str, starts_in: i64, location: &str, url: &str) -> DueReminder {
        DueReminder {
            event_id: "ics:work/e1".into(),
            title: title.into(),
            location: location.into(),
            url: url.into(),
            occurrence_start_ms: 1_800_000_000_000,
            trigger_mins: 10,
            starts_in_mins: starts_in,
        }
    }

    #[test]
    fn the_message_reads_naturally_at_every_lead_time() {
        assert_eq!(message_for(&rem("standup", 10, "", "")), "standup in 10m");
        assert_eq!(message_for(&rem("standup", 1, "", "")), "standup in 1m");
        assert_eq!(message_for(&rem("standup", 60, "", "")), "standup in 1h");
        assert_eq!(message_for(&rem("standup", 90, "", "")), "standup in 1h30");
        // Already started, or the check ran a moment late.
        assert_eq!(message_for(&rem("standup", 0, "", "")), "standup now");
        assert_eq!(message_for(&rem("standup", -2, "", "")), "standup now");
    }

    #[test]
    fn a_join_link_wins_over_a_room_name() {
        // The point of the toast is to be actionable.
        assert_eq!(
            message_for(&rem("1:1", 5, "Room 3", "https://meet.example/x")),
            format!(
                "1:1 in 5m {} https://meet.example/x",
                crate::caps::active_glyphs().emdash
            )
        );
        assert_eq!(
            message_for(&rem("1:1", 5, "Room 3", "")),
            format!("1:1 in 5m {} Room 3", crate::caps::active_glyphs().emdash)
        );
        // Whitespace-only fields are not a detail.
        assert_eq!(message_for(&rem("1:1", 5, "  ", " ")), "1:1 in 5m");
    }

    #[test]
    fn the_message_is_ascii_on_an_ascii_terminal() {
        // It lands in the inbox and the desktop toast, both of which render
        // through the same caps path — a hard-coded em dash would be mojibake.
        let msg = crate::caps::test_override::with_unicode(
            thegn_core::termcaps::UnicodeLevel::Ascii,
            || message_for(&rem("1:1", 5, "Room 3", "")),
        );
        assert!(msg.is_ascii(), "{msg:?}");
        assert!(msg.contains("Room 3"));
    }
}
