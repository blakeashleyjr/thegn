//! Desktop notification delivery (items 421/430).
//!
//! Consumes [`DesktopNotification`]s from the event bus and shells out to the
//! platform notifier (`notify-send` on Linux) on a dedicated OS thread, so the
//! event loop is never blocked on the notifier subprocess. Notifications below
//! the configured minimum urgency are dropped here — they still live in the
//! in-app inbox and as sidebar badges.

use std::process::Command;

use superzej_core::event_bus::{DesktopNotification, NotificationUrgency};

/// Spawn the desktop-notification dispatcher thread.
///
/// `rx` is the event bus' desktop channel; `enabled` gates delivery entirely;
/// `min_urgency` is the threshold below which toasts are suppressed. The thread
/// exits when the sender side of `rx` is dropped.
pub fn spawn(
    rx: std::sync::mpsc::Receiver<DesktopNotification>,
    enabled: bool,
    min_urgency: NotificationUrgency,
) {
    if !enabled {
        // Drain-and-drop so the bus never blocks on a full channel, but never
        // deliver. Cheap: the thread parks on recv until the bus is dropped.
        std::thread::Builder::new()
            .name("desktop-notify-drain".into())
            .spawn(move || while rx.recv().is_ok() {})
            .ok();
        return;
    }
    std::thread::Builder::new()
        .name("desktop-notify".into())
        .spawn(move || {
            while let Ok(notif) = rx.recv() {
                if notif.urgency.meets(min_urgency) {
                    deliver(&notif);
                }
            }
        })
        .ok();
}

/// Deliver one notification via the platform notifier. Best-effort: failures
/// (notifier missing, spawn error) are swallowed — a missing toast must never
/// disrupt the session.
fn deliver(notif: &DesktopNotification) {
    #[cfg(target_os = "linux")]
    deliver_linux(notif);
    #[cfg(target_os = "macos")]
    deliver_macos(notif);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = notif;
}

/// Map our urgency to a `notify-send` urgency level.
#[cfg(target_os = "linux")]
fn notify_send_urgency(urgency: NotificationUrgency) -> &'static str {
    match urgency {
        NotificationUrgency::Low => "low",
        NotificationUrgency::Normal => "normal",
        NotificationUrgency::Critical => "critical",
    }
}

#[cfg(target_os = "linux")]
fn deliver_linux(notif: &DesktopNotification) {
    if !superzej_core::util::have("notify-send") {
        return;
    }
    let _ = Command::new("notify-send")
        .arg("--app-name=superzej")
        .arg("--urgency")
        .arg(notify_send_urgency(notif.urgency))
        .arg(&notif.title)
        .arg(&notif.body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(target_os = "macos")]
fn deliver_macos(notif: &DesktopNotification) {
    if !superzej_core::util::have("osascript") {
        return;
    }
    // Escape double quotes for the AppleScript string literals.
    let title = notif.title.replace('"', "'");
    let body = notif.body.replace('"', "'");
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_dispatcher_drains_without_delivering() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(rx, false, NotificationUrgency::Normal);
        // Sending should not panic even though delivery is disabled.
        tx.send(DesktopNotification {
            title: "t".into(),
            body: "b".into(),
            urgency: NotificationUrgency::Critical,
            worktree: String::new(),
        })
        .unwrap();
        // Dropping tx ends the drain thread cleanly.
        drop(tx);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn notify_send_urgency_mapping() {
        assert_eq!(notify_send_urgency(NotificationUrgency::Low), "low");
        assert_eq!(notify_send_urgency(NotificationUrgency::Normal), "normal");
        assert_eq!(
            notify_send_urgency(NotificationUrgency::Critical),
            "critical"
        );
    }

    #[test]
    fn enabled_dispatcher_accepts_events() {
        let (tx, rx) = std::sync::mpsc::channel();
        // notify-send may be absent in CI; deliver() is best-effort and never
        // panics. This exercises the threshold + spawn path.
        spawn(rx, true, NotificationUrgency::Normal);
        tx.send(DesktopNotification {
            title: "Tests Failed".into(),
            body: "2 tests failed".into(),
            urgency: NotificationUrgency::Critical,
            worktree: "/wt/app".into(),
        })
        .unwrap();
        // Below-threshold notification is dropped silently.
        tx.send(DesktopNotification {
            title: "PR Opened".into(),
            body: "#1".into(),
            urgency: NotificationUrgency::Low,
            worktree: String::new(),
        })
        .unwrap();
        drop(tx);
    }
}
