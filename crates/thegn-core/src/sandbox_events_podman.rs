//! The podman transport of the sandbox seam's container-events op (THE-79).
//!
//! The only file in the change that names the vendor binary: the `podman
//! events` argv, the vendor JSON mapping, and the PATH probe all live here
//! behind the [`ContainerEvents`] seam (`sandbox_events.rs`). A future
//! docker/Apple events adapter is a sibling file plus a profile-table flip —
//! no other site ever names a container runtime (the `runtime-leak` ratchet).
//!
//! Rootless Podman may also leave its `pause` process after an isolated
//! `XDG_RUNTIME_DIR` is removed. That process belongs to Podman's rootless
//! user namespace and can outlive every client by design; thegn owns and
//! reaps its own `events` clients, but cannot safely stop that namespace.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::db::Db;
use crate::sandbox::Backend;
use crate::sandbox_events::{
    ContainerEventSink, ContainerEvents, EventKind, RawEvent, persist, register_event_child,
};

struct PodmanEvents {
    /// `backend_prefix(backend)`: plain `["podman"]`, or
    /// `["sudo", "-n", "podman"]` for rootful — so rootful containers' events
    /// stream through the same code (design §2.6 delta 1).
    prefix: Vec<String>,
}

/// Build the podman events transport for `backend`.
pub fn transport(backend: Backend) -> Box<dyn ContainerEvents> {
    Box::new(PodmanEvents {
        prefix: crate::sandbox::backend_prefix(backend),
    })
}

/// The `podman events` argv for `kind` — verbatim from the pre-seam host
/// module (`sandbox_events.rs:76-85, 146-156`).
fn events_argv(kind: EventKind) -> &'static [&'static str] {
    match kind {
        EventKind::Exec => &[
            "events",
            "--format",
            "json",
            "--filter",
            crate::sandbox_manage::OWNED_LABEL_FILTER,
            "--filter",
            "event=exec",
            "--filter",
            "event=die",
        ],
        EventKind::Network => &[
            "events",
            "--format",
            "json",
            "--filter",
            crate::sandbox_manage::OWNED_LABEL_FILTER,
            "--filter",
            "event=network",
        ],
    }
}

impl ContainerEvents for PodmanEvents {
    fn id(&self) -> &'static str {
        "podman"
    }

    /// The old `have("podman")`, relocated into the impl file: probe the
    /// transport's runtime binary (the last prefix element, after a possible
    /// `sudo`).
    fn available(&self) -> bool {
        match self.prefix.last() {
            Some(bin) => crate::util::have(bin),
            None => false,
        }
    }

    // off-loop: runs on the events-stream thread the host spawned; the
    // blocking read + reap on EOF is exactly the point (audit run.rs:825).
    fn subscribe(self: Box<Self>, kind: EventKind, sink: &mut dyn ContainerEventSink) {
        let prefix = &self.prefix;
        let Some(program) = prefix.first() else {
            return;
        };
        let mut command = Command::new(program);
        command
            .args(&prefix[1..])
            .args(events_argv(kind))
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::sandbox::proc::configure_event_process_group(&mut command);
        let Ok(mut child) = command.spawn() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            drop(register_event_child(child));
            return;
        };
        // Account this watcher's footprint to thegn for as long as it runs.
        let _proc = crate::proc_registry::register(
            crate::proc_registry::GROUP_WATCHER,
            "podman events",
            child.id(),
        );
        let child_guard = register_event_child(child);
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let Some(ev) = parse_podman_event(&line, kind) else {
                continue;
            };
            // Per-event open stays off the event loop; holding a DB handle for
            // the stream lifetime is deferred separately.
            let Ok(db) = Db::open() else {
                continue;
            };
            if persist(&db, &ev) > 0 {
                sink.on_batch(1);
            }
        }
        // Drop kills the complete group and reaps on EOF. The process-wide
        // registry also uses this guard at host shutdown.
        drop(child_guard);
    }
}

/// Parse a single JSON line from `podman events` into a vendor-agnostic
/// [`RawEvent`] — the `Name`/`Status`/`Attributes`/`Time` mapping, moved from
/// the host's `process_exec_event`/`process_network_event`. Garbage lines →
/// `None`; the thegn-owned name filter lives in `persist`.
fn parse_podman_event(json: &str, kind: EventKind) -> Option<RawEvent> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let container = v["Name"].as_str()?.to_string();
    let ts = v["Time"].as_i64().unwrap_or(0);
    let (kind, detail) = match kind {
        EventKind::Exec => (
            // The exec stream carries both verbs; an unparsed status keeps the
            // old `"exec"` default.
            v["Status"].as_str().unwrap_or("exec").to_string(),
            v["Attributes"]["execID"].as_str().map(|s| s.to_string()),
        ),
        EventKind::Network => (
            "network".to_string(),
            v["Attributes"]["network"].as_str().map(|s| s.to_string()),
        ),
    };
    Some(RawEvent {
        container,
        kind,
        detail,
        ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command as StdCommand, Stdio as StdStdio};
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};
    use std::{fs, thread};

    static CHILD_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fake_long_lived_child() -> Child {
        let mut command = if cfg!(windows) {
            let mut command = StdCommand::new("cmd");
            command.args(["/C", "ping -n 600 127.0.0.1 >NUL"]);
            command
        } else {
            let mut command = StdCommand::new("sleep");
            command.arg("600");
            command
        };
        command.stdout(StdStdio::null()).stderr(StdStdio::null());
        crate::sandbox::proc::configure_event_process_group(&mut command);
        command.spawn().unwrap()
    }

    #[test]
    fn dropping_event_child_guard_kills_and_reaps_child() {
        let _lock = CHILD_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let guard = register_event_child(fake_long_lived_child());
        let reaped = guard.reap_observer();
        drop(guard);
        assert!(reaped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn subscribe_registers_and_reaps_its_process_group_on_shutdown() {
        let _lock = CHILD_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pid_path = std::env::temp_dir().join(format!(
            "thegn-events-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pid_path_text = pid_path.display().to_string();
        let prefix = if cfg!(windows) {
            vec![
                "powershell".to_owned(),
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                format!(
                    "$PID | Set-Content -NoNewline '{pid_path_text}'; Start-Sleep -Seconds 600"
                ),
            ]
        } else {
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo $$ > '{pid_path_text}'; exec sleep 600"),
            ]
        };
        let subscriber = Box::new(PodmanEvents { prefix });
        let subscriber_thread = crate::thread_name::spawn_named("thegn-ev-test", move || {
            let mut sink = NoopSink;
            subscriber.subscribe(EventKind::Exec, &mut sink);
        });
        let start = Instant::now();
        while !pid_path.exists() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        let pid_text = match fs::read_to_string(&pid_path) {
            Ok(pid_text) => pid_text,
            Err(error) => {
                crate::sandbox_events::shutdown_event_streams(
                    Instant::now() + Duration::from_secs(2),
                );
                subscriber_thread.join().unwrap();
                panic!("fake events process did not write its pid: {error}");
            }
        };
        let pid = pid_text.trim().parse::<u32>().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        crate::sandbox_events::shutdown_event_streams(deadline);
        while !subscriber_thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            subscriber_thread.is_finished(),
            "subscriber did not return after EOF"
        );
        subscriber_thread.join().unwrap();
        assert!(Instant::now() < deadline);
        assert!(
            !process_is_alive(pid),
            "subscribe left its events process alive"
        );
        fs::remove_file(pid_path).unwrap();
    }

    #[test]
    fn shutdown_does_not_wait_past_deadline_when_group_kill_fails() {
        let _lock = CHILD_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut command = if cfg!(windows) {
            let mut command = StdCommand::new("cmd");
            command.args(["/C", "ping -n 3 127.0.0.1 >NUL"]);
            command
        } else {
            let mut command = StdCommand::new("sleep");
            command.arg("1");
            command
        };
        command.stdout(StdStdio::null()).stderr(StdStdio::null());
        let guard = register_event_child(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_millis(30);
        let start = Instant::now();
        crate::sandbox_events::shutdown_event_streams(deadline);
        assert!(start.elapsed() < Duration::from_millis(500));
        drop(guard);
    }

    struct NoopSink;

    impl ContainerEventSink for NoopSink {
        fn on_batch(&mut self, _count: usize) {}
    }

    fn process_is_alive(pid: u32) -> bool {
        if cfg!(windows) {
            let filter = format!("PID eq {pid}");
            StdCommand::new("tasklist")
                .args(["/FI", filter.as_str(), "/NH"])
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
                .unwrap_or(true)
        } else {
            let pid_text = pid.to_string();
            StdCommand::new("kill")
                .args(["-0", pid_text.as_str()])
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
        }
    }

    #[test]
    fn shutdown_kills_and_reaps_each_registered_child_by_deadline() {
        let _lock = CHILD_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let first = register_event_child(fake_long_lived_child());
        let second = register_event_child(fake_long_lived_child());
        let deadline = Instant::now() + Duration::from_secs(2);
        crate::sandbox_events::shutdown_event_streams(deadline);
        assert!(Instant::now() < deadline);
        assert!(first.is_reaped());
        assert!(second.is_reaped());
    }

    #[test]
    fn parse_exec_event_with_exec_id() {
        let line = r#"{"Name":"thegn-wt-feat","Status":"exec","Attributes":{"execID":"abc123"},"Time":1700000000}"#;
        let ev = parse_podman_event(line, EventKind::Exec).unwrap();
        assert_eq!(ev.container, "thegn-wt-feat");
        assert_eq!(ev.kind, "exec");
        assert_eq!(ev.detail.as_deref(), Some("abc123"));
        assert_eq!(ev.ts, 1700000000);
    }

    #[test]
    fn parse_die_event_keeps_the_verbatim_status() {
        let line = r#"{"Name":"thegn-wt-feat","Status":"die","Attributes":{},"Time":1700000001}"#;
        let ev = parse_podman_event(line, EventKind::Exec).unwrap();
        assert_eq!(ev.kind, "die");
        assert_eq!(ev.detail, None);
    }

    #[test]
    fn parse_network_event() {
        let line = r#"{"Name":"thegn-wt-feat","Status":"network","Attributes":{"network":"tcp"},"Time":1700000002}"#;
        let ev = parse_podman_event(line, EventKind::Network).unwrap();
        assert_eq!(ev.kind, "network");
        assert_eq!(ev.detail.as_deref(), Some("tcp"));
        assert_eq!(ev.ts, 1700000002);
    }

    #[test]
    fn garbage_lines_yield_none() {
        assert!(parse_podman_event("not json", EventKind::Exec).is_none());
        // Valid JSON, but not an event object / no container name.
        assert!(parse_podman_event("[1,2,3]", EventKind::Exec).is_none());
        assert!(parse_podman_event(r#"{"Status":"exec"}"#, EventKind::Exec).is_none());
        assert!(parse_podman_event(r#"{"Name":42}"#, EventKind::Network).is_none());
    }

    #[test]
    fn argv_matches_the_moved_filters() {
        assert_eq!(
            events_argv(EventKind::Exec),
            [
                "events",
                "--format",
                "json",
                "--filter",
                crate::sandbox_manage::OWNED_LABEL_FILTER,
                "--filter",
                "event=exec",
                "--filter",
                "event=die",
            ]
        );
        assert_eq!(
            events_argv(EventKind::Network),
            [
                "events",
                "--format",
                "json",
                "--filter",
                crate::sandbox_manage::OWNED_LABEL_FILTER,
                "--filter",
                "event=network",
            ]
        );
    }

    #[test]
    fn create_label_and_events_filter_agree() {
        assert_eq!(
            crate::sandbox_manage::OWNED_LABEL_FILTER,
            format!("label={}", crate::sandbox_manage::OWNED_LABEL)
        );
        assert!(events_argv(EventKind::Exec).contains(&crate::sandbox_manage::OWNED_LABEL_FILTER));
        assert!(
            events_argv(EventKind::Network).contains(&crate::sandbox_manage::OWNED_LABEL_FILTER)
        );
    }
}
