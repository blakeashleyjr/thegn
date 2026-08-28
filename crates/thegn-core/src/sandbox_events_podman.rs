//! The podman transport of the sandbox seam's container-events op (THE-79).
//!
//! The only file in the change that names the vendor binary: the `podman
//! events` argv, the vendor JSON mapping, and the PATH probe all live here
//! behind the [`ContainerEvents`] seam (`sandbox_events.rs`). A future
//! docker/Apple events adapter is a sibling file plus a profile-table flip —
//! no other site ever names a container runtime (the `runtime-leak` ratchet).

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::db::Db;
use crate::sandbox::Backend;
use crate::sandbox_events::{ContainerEventSink, ContainerEvents, EventKind, RawEvent, persist};

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
            "label=io.thegn=true",
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
            "label=io.thegn=true",
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
        let Ok(mut child) = Command::new(program)
            .args(&prefix[1..])
            .args(events_argv(kind))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        // Account this watcher's footprint to thegn for as long as it runs. Held to
        // the end of the function so it is dropped after the child is reaped.
        let _proc = crate::proc_registry::register(
            crate::proc_registry::GROUP_WATCHER,
            "podman events",
            child.id(),
        );
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let Some(ev) = parse_podman_event(&line, kind) else {
                continue;
            };
            let Ok(db) = Db::open() else {
                continue;
            };
            if persist(&db, &ev) > 0 {
                sink.on_batch(1);
            }
        }
        // Reap the `podman events` child when the stream ends (EOF on daemon
        // restart / no socket): a dropped `Child` is never waited on, leaving a
        // zombie for the life of the long-running host. best-effort: the exit
        // status carries no action, so a failure is only logged — never fatal
        // (audit run.rs:825).
        if let Err(error) = child.wait() {
            tracing::debug!(%error, "podman events: child reap failed");
        }
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
                "label=io.thegn=true",
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
                "label=io.thegn=true",
                "--filter",
                "event=network",
            ]
        );
    }
}
