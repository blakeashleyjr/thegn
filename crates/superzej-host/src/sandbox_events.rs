//! Background subscription to `podman events` for sandbox audit logging.
//!
//! Two event streams are subscribed simultaneously:
//!
//! - **Exec events** (`event=exec`, `event=die`): logged to `container_events`
//!   with `kind = "exec"` or `kind = "die"` so the panel audit log shows
//!   what commands ran inside each container.
//! - **Network events** (`event=network`): logged with `kind = "network"` when
//!   `network_audit = true` is configured.
//!
//! The subscriber runs on a dedicated OS thread (blocking `podman events`
//! stdout) and fires updates through a channel so the event loop can refresh
//! the panel dirty flag without polling.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use tokio::sync::mpsc as tokio_mpsc;

use superzej_core::sandbox::CONTAINER_PREFIX;

/// Update type sent to the event loop: tells it to refresh the audit panel.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SandboxEventBatch {
    /// Number of new events written to the DB.
    pub count: usize,
}

/// Start the background podman events subscriber.
///
/// `network_audit`: whether to also subscribe to network events.
/// Returns a channel that fires whenever new events are written to the DB.
/// Silently does nothing if `podman` is not available.
pub fn spawn(network_audit: bool, tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>) {
    if !superzej_core::util::have("podman") {
        return;
    }
    let tx = Arc::new(tx);
    // Exec events.
    {
        let tx = Arc::clone(&tx);
        std::thread::Builder::new()
            .name("podman-exec-events".into())
            .spawn(move || subscribe_exec(tx))
            .ok();
    }
    // Network events (optional).
    if network_audit {
        let tx = Arc::clone(&tx);
        std::thread::Builder::new()
            .name("podman-net-events".into())
            .spawn(move || subscribe_network(tx))
            .ok();
    }
}

// ---------------------------------------------------------------------------
// Exec events
// ---------------------------------------------------------------------------

fn subscribe_exec(tx: Arc<tokio_mpsc::UnboundedSender<SandboxEventBatch>>) {
    let Ok(mut child) = Command::new("podman")
        .args([
            "events",
            "--format",
            "json",
            "--filter",
            "label=io.superzej=true",
            "--filter",
            "event=exec",
            "--filter",
            "event=die",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(batch) = process_exec_event(&line) {
            let _ = tx.send(batch);
        }
    }
}

/// Parse a single JSON event line from `podman events` and write to DB.
fn process_exec_event(json: &str) -> Option<SandboxEventBatch> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = v["Name"].as_str()?;
    // Only process superzej-owned containers.
    if !name.starts_with(CONTAINER_PREFIX) {
        return None;
    }
    let worktree = worktree_from_container_name(name)?;
    let kind = v["Status"].as_str().unwrap_or("exec");
    let detail = v["Attributes"]["execID"].as_str().map(|s| s.to_string());
    let ts = v["Time"].as_i64().unwrap_or(0);

    let Ok(db) = superzej_core::db::Db::open() else {
        return None;
    };
    db.insert_container_event(&worktree, ts, kind, detail.as_deref(), None)
        .ok()?;
    db.prune_container_events(7 * 24 * 3600).ok()?;
    Some(SandboxEventBatch { count: 1 })
}

// ---------------------------------------------------------------------------
// Network events
// ---------------------------------------------------------------------------

fn subscribe_network(tx: Arc<tokio_mpsc::UnboundedSender<SandboxEventBatch>>) {
    let Ok(mut child) = Command::new("podman")
        .args([
            "events",
            "--format",
            "json",
            "--filter",
            "label=io.superzej=true",
            "--filter",
            "event=network",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(batch) = process_network_event(&line) {
            let _ = tx.send(batch);
        }
    }
}

fn process_network_event(json: &str) -> Option<SandboxEventBatch> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = v["Name"].as_str()?;
    if !name.starts_with(CONTAINER_PREFIX) {
        return None;
    }
    let worktree = worktree_from_container_name(name)?;
    let detail = v["Attributes"]["network"].as_str().map(|s| s.to_string());
    let ts = v["Time"].as_i64().unwrap_or(0);

    let Ok(db) = superzej_core::db::Db::open() else {
        return None;
    };
    db.insert_container_event(&worktree, ts, "network", detail.as_deref(), None)
        .ok()?;
    Some(SandboxEventBatch { count: 1 })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a container name back to a worktree path.
///
/// Container names are `superzej-{slug}` where the slug is built by
/// `util::slugify`. We can't reverse the slug deterministically, so we look it
/// up in the DB — the worktree path was stored when the container was created.
fn worktree_from_container_name(name: &str) -> Option<String> {
    let db = superzej_core::db::Db::open().ok()?;
    // Map the agent's `-szagent` container and the VPN `-szvpn` sidecar back to
    // their worktree too (strip whichever suffix applies).
    let lookup =
        superzej_core::sandbox::strip_vpn_suffix(superzej_core::sandbox::strip_agent_suffix(name));
    // Linear scan of the worktree list. Fine: there are at most a few dozen.
    let rows = db.worktrees().ok()?;
    rows.into_iter().find_map(|r| {
        if superzej_core::sandbox::container_name(&r.worktree) == lookup {
            Some(r.worktree)
        } else {
            None
        }
    })
}
