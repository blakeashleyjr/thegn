//! Desktop notification delivery (items 421/430).
//!
//! Consumes [`DesktopNotification`]s from the event bus and shells out to the
//! platform notifier (`notify-send` on Linux) on a dedicated OS thread, so the
//! event loop is never blocked on the notifier subprocess. Notifications below
//! the configured minimum urgency are dropped here — they still live in the
//! in-app inbox and as sidebar badges.

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::process::Command;

use thegn_core::event_bus::{DesktopNotification, NotificationUrgency};

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
    #[cfg(windows)]
    deliver_windows(notif);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
    if !thegn_core::util::have("notify-send") {
        return;
    }
    let _ = Command::new("notify-send")
        .arg("--app-name=thegn")
        .arg("--urgency")
        .arg(notify_send_urgency(notif.urgency))
        .arg(&notif.title)
        .arg(&notif.body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Escape a string for use inside an AppleScript double-quoted literal.
///
/// Backslash FIRST, then quote — the other order would re-escape the backslashes
/// this very function just introduced.
///
/// The previous approach (replace `"` with `'`) left backslashes untouched, and
/// AppleScript treats `\` as an escape: a title or body ending in one escaped the
/// closing quote and the whole script died with `syntax error: A identifier can't
/// go after this """`. osascript is spawned detached with stderr nulled, so that
/// notification simply never appeared and nothing said why. Notification text is
/// branch names, PR titles and agent output — all places a stray backslash is
/// perfectly ordinary. It also rewrote the user's quotes; escaping keeps the text
/// as written.
#[cfg(any(target_os = "macos", test))]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn deliver_macos(notif: &DesktopNotification) {
    if !thegn_core::util::have("osascript") {
        return;
    }
    let title = applescript_escape(&notif.title);
    let body = applescript_escape(&notif.body);
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Windows toast via the WinRT notification API driven from PowerShell — no
/// extra crate/COM plumbing for a best-effort toast, mirroring the
/// `notify-send`/`osascript` subprocess pattern. Runs on the dispatcher
/// thread (never the loop); PowerShell startup latency is acceptable there.
#[cfg(windows)]
fn deliver_windows(notif: &DesktopNotification) {
    // Single-quoted PowerShell literals: escaping is just ' → ''.
    let title = notif.title.replace('\'', "''");
    let body = notif.body.replace('\'', "''");
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
         ContentType = WindowsRuntime] | Out-Null; \
         $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent(\
         [Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $t = $x.GetElementsByTagName('text'); \
         $t.Item(0).AppendChild($x.CreateTextNode('{title}')) | Out-Null; \
         $t.Item(1).AppendChild($x.CreateTextNode('{body}')) | Out-Null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('thegn')\
         .Show([Windows.UI.Notifications.ToastNotification]::new($x))"
    );
    // The script is PowerShell — don't route through util::shell(), which may
    // resolve to cmd.exe.
    let Some(ps) = thegn_core::util::which_path("pwsh.exe")
        .or_else(|| thegn_core::util::which_path("powershell.exe"))
    else {
        return;
    };
    let _ = Command::new(ps)
        .args(["-NoProfile", "-Command", &script])
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

    /// AppleScript escaping keeps the text intact AND keeps the script parseable.
    ///
    /// Runs on every platform (the helper is `cfg(any(macos, test))`) so Linux CI
    /// guards the macOS notification path too — otherwise this is only checked on
    /// a machine nobody runs CI on.
    #[test]
    fn applescript_escape_survives_quotes_and_backslashes() {
        assert_eq!(applescript_escape("plain"), "plain");
        // Quotes are escaped, not rewritten: the user's text is preserved.
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        // The regression: a trailing backslash used to escape the closing quote
        // and kill the whole script, so the notification silently never showed.
        assert_eq!(applescript_escape(r"path\"), r"path\\");
        // Backslash before quote — escaping in the wrong order would produce
        // `\\"` (an escaped backslash then a BARE quote) and break the string.
        assert_eq!(applescript_escape(r#"a\"b"#), r#"a\\\"b"#);
        // A realistic branch name: nothing exotic, still broke it before.
        assert_eq!(
            applescript_escape(r"fix\windows-paths"),
            r"fix\\windows-paths"
        );

        // Every escape is balanced: no odd run of backslashes can reach the
        // closing quote and swallow it.
        for input in [r"\", r"\\", r#"""#, r#"\""#, r"a\\\", "mixed\\\"x"] {
            let e = applescript_escape(input);
            let trailing = e.len() - e.trim_end_matches('\\').len();
            assert_eq!(trailing % 2, 0, "unbalanced trailing escapes in {e:?}");
        }
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
