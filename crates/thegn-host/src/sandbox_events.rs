//! Background subscription to the sandbox backend's container events for
//! audit logging (THE-79: the vendor transport moved behind the sandbox seam —
//! this module is a thin orchestrator with zero vendor knowledge).
//!
//! Selection: the configured (or auto-detected) backend hands out an events
//! transport iff its profile cap is `Yes` (`thegn_core::sandbox_events`). Two
//! event streams are subscribed on dedicated threads:
//!
//! - **Exec events** (`event=exec`, `event=die`): logged to `container_events`
//!   so the panel audit log shows what commands ran inside each container.
//! - **Network events** (`event=network`): logged when `network_audit = true`
//!   is configured.
//!
//! The transports' subscription loops block their calling thread and pulse
//! the sink per persisted batch; the sink forwards the count through a
//! channel so the event loop can refresh the panel dirty flag without
//! polling.

use thegn_core::config::{SandboxBackend, SandboxConfig};
use thegn_core::sandbox::Backend;
use thegn_core::sandbox_events::{ContainerEventSink, ContainerEvents, EventKind, EventsCap};

use tokio::sync::mpsc as tokio_mpsc;

/// Update type sent to the event loop: tells it to refresh the audit panel.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SandboxEventBatch {
    /// Number of new events written to the DB.
    pub count: usize,
}

/// Start the background container-events subscriber for the configured
/// sandbox backend.
///
/// Silently does nothing when the selection yields no transport (auto with no
/// events-capable chain entry, a reserved or `No` cap, or the transport's
/// binary not on PATH) — audit is best-effort.
pub fn spawn(cfg: &SandboxConfig, tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>) {
    let Some(backend) = select_backend(cfg) else {
        return;
    };
    // Honest one-line note when the selected backend can never stream events
    // (reserved cap) — debug level, off the hot path.
    if let EventsCap::Reserved(reason) = backend.profile().events {
        tracing::debug!(backend = %backend.label(), reason, "container events: reserved");
    }
    let Some(events) = backend.events() else {
        return;
    };
    // The old PATH-presence gate on the vendor binary, relocated into the
    // transport: no subscriber threads when the runtime binary is missing.
    if !events.available() {
        return;
    }
    // Exec events.
    subscribe_thread("sandbox-events-exec", events, EventKind::Exec, tx.clone());
    // Network events (optional).
    if cfg.network_audit {
        let Some(net_events) = backend.events() else {
            return;
        };
        subscribe_thread("sandbox-events-net", net_events, EventKind::Network, tx);
    }
}

/// Spawn one named subscriber thread running the transport's blocking
/// subscription loop to scope end.
fn subscribe_thread(
    name: &str,
    events: Box<dyn ContainerEvents>,
    kind: EventKind,
    tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>,
) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // Housekeeping: blocks on the container event stream for the
            // process lifetime, writing audit rows nobody waits on. Declared
            // FIRST — the thread-qos ratchet.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut sink = BatchForwarder { tx };
            events.subscribe(kind, &mut sink);
        })
        .ok();
}

/// Forwards the transports' persisted-rows pulses to the event loop channel.
struct BatchForwarder {
    tx: tokio_mpsc::UnboundedSender<SandboxEventBatch>,
}

impl ContainerEventSink for BatchForwarder {
    fn on_batch(&mut self, count: usize) {
        // best-effort: the consumer may be gone (shutdown) — a dropped update
        // pulse must never take down the subscriber thread.
        let _ = self.tx.send(SandboxEventBatch { count });
    }
}

/// Pure selection (no I/O): the explicit config kind resolves through
/// `Backend::from_config`; `auto` walks `backend_chain` and picks the FIRST
/// entry whose events cap is `Yes` — mirroring how chain resolution picks a
/// runtime. `None` only when an explicit `auto` has no events-capable chain
/// entry; an explicit kind always resolves (a reserved/`No` cap is answered
/// downstream, after the honest `Reserved` note).
fn select_backend(cfg: &SandboxConfig) -> Option<Backend> {
    match cfg.backend {
        SandboxBackend::Auto => cfg
            .backend_chain
            .iter()
            .find_map(|name| Backend::parse(name).filter(|b| b.profile().events == EventsCap::Yes)),
        explicit => Backend::from_config(explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(backend: SandboxBackend, chain: &[&str]) -> SandboxConfig {
        let mut c = SandboxConfig::default();
        c.backend = backend;
        c.backend_chain = chain.iter().map(|s| (*s).to_string()).collect();
        c
    }

    #[test]
    fn auto_walks_the_chain_for_the_first_events_capable_entry() {
        // Docker is reserved, so the chain falls through to podman.
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Auto, &["docker", "podman-rootless"])),
            Some(Backend::Podman)
        );
        assert_eq!(
            select_backend(&cfg(
                SandboxBackend::Auto,
                &["bwrap", "podman-rootful", "host"]
            )),
            Some(Backend::PodmanRootful)
        );
        // Nothing events-capable in the chain.
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Auto, &["bwrap", "host"])),
            None
        );
    }

    #[test]
    fn explicit_podman_selects_the_transport() {
        assert_eq!(
            select_backend(&cfg(SandboxBackend::Podman, &[])),
            Some(Backend::Podman)
        );
    }

    #[test]
    fn explicit_docker_stops_at_the_reserved_branch() {
        let backend = select_backend(&cfg(SandboxBackend::Docker, &[]))
            .expect("docker resolves as a backend");
        assert_eq!(backend, Backend::Docker);
        assert!(matches!(backend.profile().events, EventsCap::Reserved(_)));
        // …and the op answers None after the Reserved note.
        assert!(backend.events().is_none());
    }

    #[test]
    fn explicit_none_has_no_transport() {
        let backend =
            select_backend(&cfg(SandboxBackend::None, &[])).expect("none resolves as a backend");
        assert_eq!(backend, Backend::None);
        assert!(backend.events().is_none());
    }
}
