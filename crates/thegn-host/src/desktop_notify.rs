//! Desktop notification delivery (items 421/430).
//!
//! Consumes [`DesktopNotification`]s from the event bus and shells out to the
//! platform notifier (`notify-send` on Linux) on a dedicated OS thread, so the
//! event loop is never blocked on the notifier subprocess. Notifications below
//! the configured minimum urgency are dropped here — they still live in the
//! in-app inbox and as sidebar badges.

#[cfg(any(unix, windows))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use thegn_core::event_bus::{DesktopNotification, NotificationUrgency};

const ACTIVE_POLL: Duration = Duration::from_millis(25);
static DISPATCHER_LIVE: AtomicBool = AtomicBool::new(false);

type OwnedChild = crate::platform::DesktopChild;

/// Handle for the one desktop dispatcher. Dropping the handle requests stop;
/// the explicit shutdown path additionally waits up to the caller's deadline.
pub(crate) struct DispatcherHandle {
    stop: Arc<AtomicBool>,
    bus: Option<thegn_core::event_bus::EventBus>,
    done: tokio::sync::oneshot::Receiver<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DispatcherHandle {
    /// Signal stop without waiting. Call this before awaiting any other
    /// shutdown owner so the shared application deadline applies uniformly.
    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(bus) = &self.bus {
            bus.close_desktop_receivers();
        }
    }

    /// Await worker completion without blocking the Tokio executor. If
    /// quarantine has made ownership uncertain past the deadline, dropping the
    /// join handle detaches this original worker while it retains the Child.
    pub(crate) async fn shutdown_until(mut self, deadline: tokio::time::Instant) {
        self.request_stop();
        match tokio::time::timeout_at(deadline, &mut self.done).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: "thegn::desktop_notify", %error, "desktop dispatcher worker ended without completion receipt");
            }
            Err(_) => {
                // The original worker retains any quarantined Child and remains
                // the only owner. Detaching this one worker keeps the UI bounded.
                tracing::error!(target: "thegn::desktop_notify", "desktop dispatcher shutdown deadline elapsed");
            }
        }
        // The worker sends completion immediately before returning. Joining
        // here would needlessly block the executor; dropping a completed
        // handle is sufficient.
        drop(self.join.take());
    }
}

impl Drop for DispatcherHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(bus) = &self.bus {
            bus.close_desktop_receivers();
        }
        // Dropping the join handle detaches only the fixed dispatcher worker;
        // its permit and any owned Child remain with that worker.
    }
}

/// Spawn the desktop-notification dispatcher thread.
///
/// `rx` is the event bus' desktop channel; `enabled` gates delivery entirely;
/// `min_urgency` is the threshold below which toasts are suppressed. The thread
/// exits when the sender side of `rx` is dropped or the returned handle shuts it
/// down. A process-wide permit prevents a quarantined worker from being bypassed
/// by a replacement dispatcher.
pub(crate) fn spawn(
    bus: thegn_core::event_bus::EventBus,
    rx: std::sync::mpsc::Receiver<DesktopNotification>,
    enabled: bool,
    min_urgency: NotificationUrgency,
) -> DispatcherHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    if DISPATCHER_LIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        drop(rx);
        drop(done_tx);
        return DispatcherHandle {
            stop,
            bus: None,
            done: done_rx,
            join: None,
        };
    }

    let worker_stop = Arc::clone(&stop);
    let qos = if enabled {
        crate::platform::qos::Qos::Utility
    } else {
        crate::platform::qos::Qos::Background
    };
    let join = std::thread::Builder::new()
        .name("desktop-notify".into())
        .spawn(move || {
            crate::platform::qos::set_self(qos);
            let healthy = run_dispatcher(rx, enabled, min_urgency, worker_stop);
            if healthy {
                DISPATCHER_LIVE.store(false, Ordering::Release);
            }
            let _ = done_tx.send(());
        })
        .ok();
    if join.is_none() {
        DISPATCHER_LIVE.store(false, Ordering::Release);
    }
    DispatcherHandle {
        stop,
        bus: Some(bus),
        done: done_rx,
        join,
    }
}

fn run_dispatcher(
    rx: mpsc::Receiver<DesktopNotification>,
    enabled: bool,
    min_urgency: NotificationUrgency,
    stop: Arc<AtomicBool>,
) -> bool {
    run_dispatcher_with(rx, enabled, min_urgency, stop, deliver)
}

fn run_dispatcher_with<F>(
    rx: mpsc::Receiver<DesktopNotification>,
    enabled: bool,
    min_urgency: NotificationUrgency,
    stop: Arc<AtomicBool>,
    launch: F,
) -> bool
where
    F: Fn(&DesktopNotification) -> Option<OwnedChild>,
{
    if !enabled {
        while !stop.load(Ordering::Acquire) && rx.recv().is_ok() {}
        return true;
    }

    let mut active: Option<OwnedChild> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            if let Some(mut child) = active.take()
                && let Err(error) = child.terminate_and_wait()
            {
                drop(rx);
                quarantine(child, error);
                return false;
            }
            return true;
        }

        if let Some(mut child) = active.take() {
            match child.poll_exit() {
                Ok(true) => match child.terminate_and_wait() {
                    Ok(()) => {}
                    Err(error) => {
                        drop(rx);
                        quarantine(child, error);
                        return false;
                    }
                },
                Ok(false) => {
                    active = Some(child);
                    std::thread::sleep(ACTIVE_POLL);
                }
                Err(error) => {
                    drop(rx);
                    quarantine(child, error);
                    return false;
                }
            }
            if active.is_none() && stop.load(Ordering::Acquire) {
                return true;
            }
            continue;
        }

        match rx.recv() {
            Ok(notif) if !stop.load(Ordering::Acquire) => {
                if notif.urgency.meets(min_urgency) {
                    active = launch(&notif);
                }
            }
            Ok(_) => {}
            Err(_) => return true,
        }
    }
}

fn quarantine(mut child: OwnedChild, error: std::io::Error) {
    tracing::warn!(target: "thegn::desktop_notify", %error, "desktop helper wait ownership uncertain; quarantining dispatcher");
    // Keep ownership in this fixed worker. This can block until the direct
    // child settles, but never signals a PID after identity became uncertain.
    if let Err(reap_error) = child.reap_owned() {
        tracing::error!(target: "thegn::desktop_notify", %reap_error, "quarantined desktop helper could not be reaped");
    }
}

#[cfg(any(unix, windows))]
fn spawn_owned(command: &mut Command, helper: &'static str) -> Option<OwnedChild> {
    match crate::platform::DesktopChild::spawn(command) {
        Ok(child) => Some(child),
        Err(error) => {
            tracing::debug!(target: "thegn::desktop_notify", helper, %error, "desktop helper launch failed");
            None
        }
    }
}

/// Deliver one notification via the platform notifier. Best-effort: failures
/// (notifier missing, spawn error) are swallowed — a missing toast must never
/// disrupt the session.
fn deliver(notif: &DesktopNotification) -> Option<OwnedChild> {
    #[cfg(target_os = "linux")]
    return deliver_linux(notif);
    #[cfg(target_os = "macos")]
    return deliver_macos(notif);
    #[cfg(windows)]
    return deliver_windows(notif);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = notif;
        None
    }
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
fn linux_command(notif: &DesktopNotification) -> Command {
    let mut command = Command::new("notify-send");
    command // best-effort: spawn: a missing/broken notifier just skips the notification
        .arg("--app-name=thegn")
        .arg("--urgency")
        .arg(notify_send_urgency(notif.urgency))
        .arg("--")
        .arg(&notif.title)
        .arg(&notif.body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

#[cfg(target_os = "linux")]
fn deliver_linux(notif: &DesktopNotification) -> Option<OwnedChild> {
    if !thegn_core::util::have("notify-send") {
        return None;
    }
    let mut command = linux_command(notif);
    spawn_owned(&mut command, "notify-send")
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
fn deliver_macos(notif: &DesktopNotification) -> Option<OwnedChild> {
    if !thegn_core::util::have("osascript") {
        return None;
    }
    let title = applescript_escape(&notif.title);
    let body = applescript_escape(&notif.body);
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let mut command = Command::new("osascript");
    command // best-effort: spawn: a missing/broken notifier just skips the notification
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    spawn_owned(&mut command, "osascript")
}

/// Windows toast via the WinRT notification API driven from PowerShell — no
/// extra crate/COM plumbing for a best-effort toast, mirroring the
/// `notify-send`/`osascript` subprocess pattern. Runs on the dispatcher
/// thread (never the loop); PowerShell startup latency is acceptable there.
#[cfg(windows)]
fn deliver_windows(notif: &DesktopNotification) -> Option<OwnedChild> {
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
        return None;
    };
    let mut command = Command::new(ps);
    command // best-effort: spawn: a missing/broken notifier just skips the notification
        .args(["-NoProfile", "-Command", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    spawn_owned(&mut command, "powershell")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_dispatcher_drains_without_delivering() {
        let bus = thegn_core::event_bus::EventBus::new();
        let handle = spawn(
            bus.clone(),
            bus.desktop_receiver(),
            false,
            NotificationUrgency::Normal,
        );
        bus.publish_with_notification(&thegn_core::event_bus::Event::TestsFailed {
            worktree: "/wt/app".into(),
            count: 2,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime
            .block_on(handle.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1)));
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

    #[cfg(target_os = "linux")]
    #[test]
    fn notify_send_command_keeps_text_as_argv_data() {
        let notification = DesktopNotification {
            title: "--title \"quoted\" \\\\path".into(),
            body: "line\x01\x7f 'quoted' \\\\path".into(),
            urgency: NotificationUrgency::Critical,
            worktree: "/wt".into(),
        };
        let command = linux_command(&notification);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--app-name=thegn");
        assert_eq!(args[1], "--urgency");
        assert_eq!(args[2], "critical");
        assert_eq!(args[3], "--");
        assert_eq!(args[4], notification.title);
        assert_eq!(args[5], notification.body);
    }

    #[cfg(unix)]
    #[test]
    fn enabled_dispatcher_filters_before_private_fake_launch() {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_launches = Arc::clone(&launches);
        let worker = std::thread::spawn(move || {
            let result = run_dispatcher_with(
                rx,
                true,
                NotificationUrgency::Normal,
                worker_stop,
                move |_| {
                    worker_launches.fetch_add(1, Ordering::Relaxed);
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "exit 0"]);
                    Some(crate::platform::DesktopChild::spawn(&mut command).unwrap())
                },
            );
            let _ = done_tx.send(result);
        });
        tx.send(DesktopNotification {
            title: "critical".into(),
            body: "helper".into(),
            urgency: NotificationUrgency::Critical,
            worktree: "/wt".into(),
        })
        .unwrap();
        tx.send(DesktopNotification {
            title: "low".into(),
            body: "dropped".into(),
            urgency: NotificationUrgency::Low,
            worktree: "/wt".into(),
        })
        .unwrap();
        drop(tx);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(worker);
        assert_eq!(launches.load(Ordering::Relaxed), 1);
    }

    /// A private helper command exercises the same owned-child lifecycle as a
    /// notifier without invoking notify-send, DBus, or a desktop session.
    #[cfg(unix)]
    #[test]
    fn fake_helpers_are_reaped_before_the_next_delivery() {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_launches = Arc::clone(&launches);
        let worker = std::thread::spawn(move || {
            let result =
                run_dispatcher_with(rx, true, NotificationUrgency::Low, worker_stop, move |_| {
                    worker_launches.fetch_add(1, Ordering::Relaxed);
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "exit 0"]);
                    Some(crate::platform::DesktopChild::spawn(&mut command).unwrap())
                });
            let _ = done_tx.send(result);
        });
        let notification = DesktopNotification {
            title: "private fake".into(),
            body: "helper".into(),
            urgency: NotificationUrgency::Normal,
            worktree: "/wt".into(),
        };
        tx.send(notification.clone()).unwrap();
        tx.send(notification).unwrap();
        drop(tx);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(worker);
        assert_eq!(launches.load(Ordering::Relaxed), 2);
    }

    /// A failed helper launch releases the active slot so a later notification
    /// can still launch through the same dispatcher worker.
    #[cfg(unix)]
    #[test]
    fn failed_launch_allows_a_subsequent_delivery() {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let worker = std::thread::spawn(move || {
            let result = run_dispatcher_with(
                rx,
                true,
                NotificationUrgency::Low,
                Arc::new(AtomicBool::new(false)),
                move |_| {
                    let attempt = worker_attempts.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        return None;
                    }
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "exit 0"]);
                    Some(crate::platform::DesktopChild::spawn(&mut command).unwrap())
                },
            );
            let _ = done_tx.send(result);
        });
        let notification = DesktopNotification {
            title: "retry".into(),
            body: "private fake".into(),
            urgency: NotificationUrgency::Normal,
            worktree: "/wt".into(),
        };
        tx.send(notification.clone()).unwrap();
        tx.send(notification).unwrap();
        drop(tx);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(worker);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    /// A non-exiting private helper occupies the sole active slot; the next
    /// notification remains pending and shutdown terminates the owned group.
    #[cfg(unix)]
    #[test]
    fn never_exit_helper_keeps_one_active_and_shutdown_cleans_it() {
        let bus = thegn_core::event_bus::EventBus::new();
        let rx = bus.desktop_receiver();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_launches = Arc::clone(&launches);
        let worker = std::thread::spawn(move || {
            let result =
                run_dispatcher_with(rx, true, NotificationUrgency::Low, worker_stop, move |_| {
                    worker_launches.fetch_add(1, Ordering::Relaxed);
                    let _ = started_tx.send(());
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "sleep 30"]);
                    Some(crate::platform::DesktopChild::spawn(&mut command).unwrap())
                });
            let _ = done_tx.send(result);
        });
        bus.publish_with_notification(&thegn_core::event_bus::Event::TestsFailed {
            worktree: "/wt".into(),
            count: 2,
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..thegn_core::event_bus::MAX_PENDING_DESKTOP + 1 {
            bus.publish_with_notification(&thegn_core::event_bus::Event::TestsFailed {
                worktree: "/wt".into(),
                count: 2,
            });
        }
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        stop.store(true, Ordering::Release);
        bus.close_desktop_receivers();
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(worker);
    }
}
