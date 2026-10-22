//! Raising calendar reminders as ordinary notifications.
//!
//! Deliberately thin: by going through the normal notification path,
//! reminders inherit the whole `[[notifications.rules]]` engine — routing,
//! `min_priority`, DND quiet hours, sounds, desktop toasts — with zero new
//! config keys. That reuse is why there is no bespoke reminder delivery here.

use termwiz::terminal::TerminalWaker;
use thegn_core::calendar::DueReminder;
use thegn_core::calendar::display::{DisplayText, Field};
use thegn_core::db::Db;
use thegn_core::notification::NotificationKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReminderClaim {
    Won,
    AlreadyClaimed,
    Failed,
}

fn claim_reminder(db: &Db, source_ref: &str, now_ms: i64) -> ReminderClaim {
    match db.claim_calendar_reminder(source_ref, now_ms) {
        Ok(true) => ReminderClaim::Won,
        Ok(false) => ReminderClaim::AlreadyClaimed,
        Err(error) => {
            tracing::warn!(target: "thegn::calendar", %error, "calendar reminder claim failed; not delivering");
            ReminderClaim::Failed
        }
    }
}

/// Raise one due reminder.
///
/// Idempotence comes from the durable claim ledger, independent of inbox
/// lifecycle. Delivery is at-most-once: a crash after claiming but before
/// delivery drops that one reminder rather than risking a duplicate alert.
pub(crate) fn raise_reminder(r: &DueReminder, waker: &TerminalWaker) {
    // Already on the background lane (see
    // `hydrate_calendar::spawn_reminder_check`), so this runs inline — nesting
    // another `spawn_bg` would take a second permit from the eight-permit lane
    // for no gain.
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(target: "thegn::calendar", %error, "calendar reminder database open failed; not delivering");
            return;
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    raise_reminder_on(&db, r, now_ms, |db, source_ref, message| {
        let kind = NotificationKind::CalendarReminder.as_str();
        if let Some(state) = crate::notify::global() {
            let _ = crate::notify::record(db, &state, kind, source_ref, message, "");
            let _ = waker.wake(); // best-effort: waker pulse: a routed reminder must repaint the inbox
            return;
        }
        // best-effort: the inbox is a cache, and a reminder that fails to record
        // must not take down the compositor.
        if crate::automation_events::emit(db, kind, source_ref, message, "").is_ok() {
            let _ = waker.wake(); // best-effort: a waker pulse: an input nudge must never fail the calling path
        }
    });
}

fn raise_reminder_on(
    db: &Db,
    r: &DueReminder,
    now_ms: i64,
    deliver: impl FnOnce(&Db, &str, &str),
) -> ReminderClaim {
    let source_ref = r.source_ref();
    let message = message_for(r);
    let claim = claim_reminder(db, &source_ref, now_ms);
    if claim == ReminderClaim::Won {
        deliver(db, &source_ref, &message);
    }
    claim
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
    let mut msg = format!(
        "{} {when}",
        DisplayText::new(&r.title, Field::Title).as_str()
    );
    // The most actionable detail available — a meeting URL beats a room name.
    let detail = if !r.url.trim().is_empty() {
        DisplayText::new(&r.url, Field::Url).into_string()
    } else {
        DisplayText::new(&r.location, Field::Location).into_string()
    };
    if !detail.is_empty() {
        msg.push(' ');
        msg.push_str(crate::caps::active_glyphs().emdash);
        msg.push(' ');
        msg.push_str(&detail);
    }
    DisplayText::new(&msg, Field::Reminder).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use thegn_core::store::NotificationStore;

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
    fn hostile_reminders_are_bounded_single_row_and_keep_semantic_values() {
        let raw = format!("\r\nATTACK\x1b\x07\t{}", "界\u{301}".repeat(4096));
        let reminder = rem(&raw, 10, &raw, &raw);
        let message = message_for(&reminder);
        assert!(!message.chars().any(char::is_control));
        assert!(crate::seg::cells(&message) <= 512);
        assert!(message.chars().count() <= 512);
        assert_eq!(reminder.title, raw);
        assert_eq!(reminder.url, raw);
        assert_eq!(message, message.trim());
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

    #[test]
    fn a_reminder_is_claimed_once_and_a_dismissed_row_never_refires() {
        let db = Db::open_memory().unwrap();
        assert_eq!(claim_reminder(&db, "", 4_999), ReminderClaim::Failed);
        let reminder = rem("standup", 10, "", "");
        let source_ref = reminder.source_ref();
        assert_eq!(claim_reminder(&db, &source_ref, 5_000), ReminderClaim::Won);
        assert_eq!(
            claim_reminder(&db, &source_ref, 5_001),
            ReminderClaim::AlreadyClaimed
        );
        let id = db
            .put_notification("calendar_reminder", &source_ref, "standup in 10m", "")
            .unwrap();
        db.delete_notification(id).unwrap();
        assert_eq!(
            claim_reminder(&db, &source_ref, 5_002),
            ReminderClaim::AlreadyClaimed
        );
    }

    #[test]
    fn raise_reminder_on_delivers_once_across_threads_and_connections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reminders.sqlite");
        drop(Db::open_at(&path).unwrap());
        let barrier = Arc::new(Barrier::new(4));
        let deliveries = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            let deliveries = Arc::clone(&deliveries);
            workers.push(
                std::thread::Builder::new()
                    .name("thegn-test-rem".into())
                    .spawn(move || {
                        let db = Db::open_at(&path).unwrap();
                        barrier.wait();
                        raise_reminder_on(&db, &rem("standup", 10, "", ""), 5_000, |_, _, _| {
                            deliveries.fetch_add(1, Ordering::SeqCst);
                        })
                    })
                    .unwrap(),
            );
        }
        let claims: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(deliveries.load(Ordering::SeqCst), 1, "claims: {claims:?}");
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == ReminderClaim::Won)
                .count(),
            1,
            "claims: {claims:?}"
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == ReminderClaim::AlreadyClaimed)
                .count(),
            3,
            "claims: {claims:?}"
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test: waits on its own child test processes"
    )]
    fn raise_reminder_on_delivers_once_across_processes() {
        if let (Some(db_path), Some(out_path)) = (
            std::env::var_os("TG_REMINDER_RACE_CHILD_DB"),
            std::env::var_os("TG_REMINDER_RACE_CHILD_OUT"),
        ) {
            let db_path = std::path::PathBuf::from(db_path);
            let db = Db::open_at(&db_path).unwrap();
            let claim = raise_reminder_on(&db, &rem("standup", 10, "", ""), 5_000, |_, _, _| {});
            let outcome = match claim {
                ReminderClaim::Won => "won",
                ReminderClaim::AlreadyClaimed => "already",
                ReminderClaim::Failed => "failed",
            };
            fs::write(out_path, outcome).unwrap();
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("reminders.sqlite");
        drop(Db::open_at(&db_path).unwrap());
        let exe = std::env::current_exe().unwrap();
        let out_a = temp.path().join("child-a.out");
        let out_b = temp.path().join("child-b.out");
        let mut child_a = Command::new(&exe)
            .args([
                "--exact",
                "handlers::calendar::tests::raise_reminder_on_delivers_once_across_processes",
                "--nocapture",
            ])
            .env("TG_REMINDER_RACE_CHILD_DB", &db_path)
            .env("TG_REMINDER_RACE_CHILD_OUT", &out_a)
            .spawn()
            .unwrap();
        let mut child_b = Command::new(exe)
            .args([
                "--exact",
                "handlers::calendar::tests::raise_reminder_on_delivers_once_across_processes",
                "--nocapture",
            ])
            .env("TG_REMINDER_RACE_CHILD_DB", &db_path)
            .env("TG_REMINDER_RACE_CHILD_OUT", &out_b)
            .spawn()
            .unwrap();
        let output_a = child_a.wait_with_output().unwrap();
        let output_b = child_b.wait_with_output().unwrap();
        assert!(
            output_a.status.success(),
            "child A stdout: {}\nchild A stderr: {}",
            String::from_utf8_lossy(&output_a.stdout),
            String::from_utf8_lossy(&output_a.stderr)
        );
        assert!(
            output_b.status.success(),
            "child B stdout: {}\nchild B stderr: {}",
            String::from_utf8_lossy(&output_b.stdout),
            String::from_utf8_lossy(&output_b.stderr)
        );
        let mut outcomes = [
            fs::read_to_string(out_a).unwrap(),
            fs::read_to_string(out_b).unwrap(),
        ];
        outcomes.sort();
        assert_eq!(outcomes, [String::from("already"), String::from("won")]);
    }

    #[test]
    fn raise_reminder_on_survives_a_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reminders.sqlite");
        let deliveries = std::cell::Cell::new(0);
        {
            let db = Db::open_at(&path).unwrap();
            assert_eq!(
                raise_reminder_on(&db, &rem("standup", 10, "", ""), 5_000, |_, _, _| {
                    deliveries.set(deliveries.get() + 1);
                }),
                ReminderClaim::Won
            );
        }
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            raise_reminder_on(&db, &rem("standup", 10, "", ""), 5_001, |_, _, _| {
                deliveries.set(deliveries.get() + 1);
            }),
            ReminderClaim::AlreadyClaimed
        );
        assert_eq!(deliveries.get(), 1);
    }

    #[test]
    fn raise_reminder_on_ignores_changed_event_text() {
        let db = Db::open_memory().unwrap();
        let deliveries = std::cell::Cell::new(0);
        assert_eq!(
            raise_reminder_on(&db, &rem("standup", 10, "Room 3", ""), 5_000, |_, _, _| {
                deliveries.set(deliveries.get() + 1);
            }),
            ReminderClaim::Won
        );
        assert_eq!(
            raise_reminder_on(
                &db,
                &rem("standup moved", 5, "Room 9", "https://x.test/j"),
                5_001,
                |_, _, _| deliveries.set(deliveries.get() + 1),
            ),
            ReminderClaim::AlreadyClaimed
        );
        assert_eq!(deliveries.get(), 1);
    }

    #[test]
    fn raise_reminder_on_fires_each_occurrence_and_each_lead() {
        let db = Db::open_memory().unwrap();
        let deliveries = std::cell::Cell::new(0);
        let base = rem("standup", 10, "", "");
        let mut next_occurrence = rem("standup", 10, "", "");
        next_occurrence.occurrence_start_ms += 86_400_000;
        let mut other_lead = rem("standup", 5, "", "");
        other_lead.trigger_mins = 5;

        for reminder in [&base, &next_occurrence, &other_lead] {
            assert_eq!(
                raise_reminder_on(&db, reminder, 5_000, |_, _, _| {
                    deliveries.set(deliveries.get() + 1);
                }),
                ReminderClaim::Won
            );
            assert_eq!(
                raise_reminder_on(&db, reminder, 5_001, |_, _, _| {
                    deliveries.set(deliveries.get() + 1);
                }),
                ReminderClaim::AlreadyClaimed
            );
        }
        assert_eq!(deliveries.get(), 3);
    }

    #[test]
    fn raise_reminder_on_claim_failure_delivers_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reminders.sqlite");
        drop(Db::open_at(&path).unwrap());
        let db = Db::open_read_only_at(&path).unwrap().unwrap();
        let deliveries = std::cell::Cell::new(0);
        assert_eq!(
            raise_reminder_on(&db, &rem("standup", 10, "", ""), 5_000, |_, _, _| {
                deliveries.set(deliveries.get() + 1);
            }),
            ReminderClaim::Failed
        );
        assert_eq!(deliveries.get(), 0);
    }
}
