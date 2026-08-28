//! The sandbox seam's optional container-events op (THE-79).
//!
//! This is the vendor-agnostic half of the container-events subscriber: the
//! caps bit ([`EventsCap`]), the transport traits ([`ContainerEvents`] /
//! [`ContainerEventSink`]), the DB write ([`persist`]), and the
//! container-name → worktree mapping. The vendor transport lives in
//! `sandbox_events_podman` — the only file in the change that names the vendor
//! binary — handed out by the [`Backend::events`] factory iff the backend's
//! profile cap is [`EventsCap::Yes`].
//!
//! The subscriber is process-bound and its callers are blocking threads, so
//! this is a **sync** seam (provider-seams spec: sandbox is in the sync set;
//! `test/async-trait-ratchet.txt` stays empty). Thread ownership stays in the
//! host: `subscribe` blocks the *calling* thread and never spawns.

use crate::db::Db;
use crate::sandbox::{Backend, CONTAINER_PREFIX};
use crate::store::{WorkspaceStore, WorktreeAuxStore};

/// The container-events op of the sandbox seam. A kind is either implemented
/// or reserved with a reason — the seam rule (`seam.rs:13-19`), per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventsCap {
    /// Implemented: `Backend::events()` hands out a transport.
    Yes,
    /// A container runtime with a daemon event stream thegn cannot read yet.
    Reserved(&'static str),
    /// No container-runtime event stream exists (process wrappers, host shell).
    No,
}

/// Which event stream to subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// `exec` + `die`: what ran inside each container (the audit panel's
    /// command log).
    Exec,
    /// `network`: per-connection audit rows; only subscribed when
    /// `network_audit` is configured.
    Network,
}

/// One parsed container event, vendor-agnostic and ready to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    /// The container's runtime name (`thegn-{slug}`).
    pub container: String,
    /// Audit kind (`exec`, `die`, `network`).
    pub kind: String,
    /// Free-form detail (the execID, the network name).
    pub detail: Option<String>,
    /// Event time, epoch seconds.
    pub ts: i64,
}

/// Receives the "rows persisted" pulse per parsed batch. The DB write happens
/// inside the transport (it owns the vendor JSON schema); the sink only fans
/// the update out.
pub trait ContainerEventSink: Send {
    fn on_batch(&mut self, count: usize);
}

/// The optional events op of a container-runtime backend (sync, object-safe).
pub trait ContainerEvents: Send {
    /// Vendor transport id (`"podman"`), for logs / doctor notes.
    fn id(&self) -> &'static str;

    /// Cheap offline probe: the transport's binary on PATH (the old
    /// `have("podman")`, relocated into the impl file).
    fn available(&self) -> bool;

    /// Blocking: runs the subscription loop on the CALLER's thread until the
    /// stream ends (EOF ⇒ reap the child and return). Stream failures end the
    /// loop silently — audit is best-effort (the `// audit run.rs:825`
    /// contract). Never called on the event loop.
    fn subscribe(self: Box<Self>, kind: EventKind, sink: &mut dyn ContainerEventSink);
}

/// Write one parsed event to the audit DB. Returns the number of rows written
/// (0 or 1); 0 means "no panel pulse" — exactly the old `process_exec_event` /
/// `process_network_event` `None` shape.
///
/// The container-name → worktree mapping needs the DB anyway, so the caller
/// opens it once per event and hands it in (the old code opened it twice).
pub fn persist(db: &Db, ev: &RawEvent) -> usize {
    // Only thegn-owned containers carry audit rows.
    if !ev.container.starts_with(CONTAINER_PREFIX) {
        return 0;
    }
    let Some(worktree) = worktree_from_container_name(db, &ev.container) else {
        return 0;
    };
    // Audit rows are best-effort (audit run.rs:825): a failed insert must
    // never take down the subscriber — 0 rows just means no update pulse.
    if db
        .insert_container_event(&worktree, ev.ts, &ev.kind, ev.detail.as_deref(), None)
        .is_err()
    {
        return 0;
    }
    // The 7-day prune rides the exec stream only — today's asymmetry is
    // preserved deliberately: the network path never pruned, and one prune
    // per exec-stream event keeps the table bounded without a second timer.
    // The network stream is the only producer of kind `"network"` (the exec
    // stream emits `exec`/`die`), so `!= "network"` is exactly "the exec
    // stream" — a `die` event prunes too, as the pre-seam host code did. A
    // failed prune suppresses the pulse exactly like the old `.ok()?` did
    // (the row itself stays written).
    if ev.kind != "network" && db.prune_container_events(7 * 24 * 3600).is_err() {
        return 0;
    }
    1
}

/// Map a container name back to a worktree path.
///
/// Container names are `thegn-{slug}` where the slug is built by
/// `util::slugify`. We can't reverse the slug deterministically, so we look it
/// up in the DB — the worktree path was stored when the container was created.
pub fn worktree_from_container_name(db: &Db, name: &str) -> Option<String> {
    // Map the agent's `-tgagent` container and the VPN `-tgvpn` sidecar back to
    // their worktree too (strip whichever suffix applies).
    let lookup = crate::sandbox::strip_vpn_suffix(crate::sandbox::strip_agent_suffix(name));
    // Linear scan of the worktree list. Fine: there are at most a few dozen.
    // Match BOTH name forms: plain `thegn-{slug}` and the profile form
    // `thegn-{profile}-{slug}` — under a non-default profile every container
    // uses the latter, and matching only the plain form dropped every audit
    // event (the TIMELINE stayed permanently empty).
    let profile = crate::profile::name();
    let rows = db.worktrees().ok()?;
    rows.into_iter().find_map(|r| {
        let plain = crate::sandbox::container_name(&r.worktree);
        let profiled = crate::sandbox::container_name_with_profile(&r.worktree, Some(&profile));
        if plain == lookup || profiled == lookup {
            Some(r.worktree)
        } else {
            None
        }
    })
}

impl Backend {
    /// The sandbox seam's optional events op. `Some` iff the profile's cap is
    /// `Yes` (podman family); reserved and `No` backends answer `None`.
    pub fn events(self) -> Option<Box<dyn ContainerEvents>> {
        match self.profile().events {
            EventsCap::Yes => Some(crate::sandbox_events_podman::transport(self)),
            EventsCap::Reserved(_) | EventsCap::No => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    // The implemented-or-reserved table, per backend (design §2.1): podman
    // family Yes; docker/apple/smol/wsl Reserved with a non-empty reason; the
    // process wrappers + host shell No.
    #[test]
    fn events_cap_table() {
        for b in Backend::ALL {
            match b {
                Backend::Podman | Backend::PodmanRootful => {
                    assert_eq!(b.profile().events, EventsCap::Yes);
                }
                Backend::Docker | Backend::Apple | Backend::Smol | Backend::Wsl => {
                    match b.profile().events {
                        EventsCap::Reserved(reason) => {
                            assert!(!reason.is_empty(), "{:?}: empty reason", b)
                        }
                        other => panic!("{:?}: expected Reserved, got {other:?}", b),
                    }
                }
                Backend::Bwrap
                | Backend::Systemd
                | Backend::WinAppContainer
                | Backend::WinJobObject
                | Backend::None => {
                    assert_eq!(b.profile().events, EventsCap::No);
                }
            }
        }
    }

    #[test]
    fn podman_factory_hands_out_a_transport() {
        let t = Backend::Podman.events().expect("podman implements events");
        assert_eq!(t.id(), "podman");
        assert!(Backend::PodmanRootful.events().is_some());
    }

    #[test]
    fn factory_answers_none_for_reserved_and_no_backends() {
        for b in Backend::ALL {
            if b.profile().events != EventsCap::Yes {
                assert!(b.events().is_none(), "{:?} must not stream events", b);
            }
        }
        assert!(Backend::Docker.events().is_none());
    }

    #[test]
    fn persist_round_trips_exec_die_network() {
        let db = db();
        db.set_worktree_env("/wt/feat", "x").unwrap();
        let name = crate::sandbox::container_name("/wt/feat");
        let now = crate::util::now();

        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name.clone(),
                    kind: "exec".into(),
                    detail: Some("cargo build".into()),
                    ts: now,
                }
            ),
            1
        );
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name.clone(),
                    kind: "die".into(),
                    detail: None,
                    ts: now + 1
                }
            ),
            1
        );
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name,
                    kind: "network".into(),
                    detail: Some("tcp".into()),
                    ts: now + 2,
                }
            ),
            1
        );

        let rows = db.container_events("/wt/feat", 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, "network");
        assert_eq!(rows[1].kind, "die");
        assert_eq!(rows[2].kind, "exec");
        assert_eq!(rows[2].detail.as_deref(), Some("cargo build"));
        assert_eq!(rows[2].ts, now);
    }

    #[test]
    fn persist_filters_non_thegn_containers() {
        let db = db();
        db.set_worktree_env("/wt/feat", "x").unwrap();
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: "nginx".into(),
                    kind: "exec".into(),
                    detail: None,
                    ts: 1
                }
            ),
            0
        );
        assert!(db.container_events("/wt/feat", 10).unwrap().is_empty());
    }

    #[test]
    fn persist_drops_unresolvable_containers() {
        let db = db();
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: "thegn-nowhere".into(),
                    kind: "exec".into(),
                    detail: None,
                    ts: 1,
                }
            ),
            0
        );
    }

    #[test]
    fn exec_stream_prunes_seven_days() {
        let db = db();
        db.set_worktree_env("/wt/feat", "x").unwrap();
        let now = crate::util::now();
        let name = crate::sandbox::container_name("/wt/feat");
        // An old row (from any source) prunes on the next exec event.
        db.insert_container_event("/wt/feat", now - 8 * 86400, "exec", None, None)
            .unwrap();
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name.clone(),
                    kind: "exec".into(),
                    detail: None,
                    ts: now
                }
            ),
            1
        );
        assert_eq!(
            db.container_events("/wt/feat", 10).unwrap().len(),
            1,
            "the 8-day-old row must be pruned"
        );

        // The network stream never prunes (today's asymmetry, preserved).
        db.insert_container_event("/wt/feat", now - 8 * 86400, "network", None, None)
            .unwrap();
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name,
                    kind: "network".into(),
                    detail: None,
                    ts: now
                }
            ),
            1
        );
        assert_eq!(
            db.container_events("/wt/feat", 10).unwrap().len(),
            3,
            "the network stream must not prune"
        );
    }

    #[test]
    fn die_events_prune_like_exec_events() {
        // The exec stream carries both statuses (`exec`/`die`) and the
        // pre-seam host code pruned on every exec-stream event — a `die`
        // event prunes the 7-day-old rows too.
        let db = db();
        db.set_worktree_env("/wt/feat", "x").unwrap();
        let now = crate::util::now();
        let name = crate::sandbox::container_name("/wt/feat");
        db.insert_container_event("/wt/feat", now - 8 * 86400, "exec", None, None)
            .unwrap();
        assert_eq!(
            persist(
                &db,
                &RawEvent {
                    container: name,
                    kind: "die".into(),
                    detail: None,
                    ts: now,
                }
            ),
            1
        );
        assert_eq!(
            db.container_events("/wt/feat", 10).unwrap().len(),
            1,
            "the 8-day-old row must be pruned by the die event"
        );
    }

    #[test]
    fn worktree_lookup_maps_agent_and_vpn_names() {
        let db = db();
        db.set_worktree_env("/wt/feat", "x").unwrap();
        let plain = crate::sandbox::container_name("/wt/feat");
        // The agent container (`-tgagent`) and the VPN sidecar (`-tgvpn`) map
        // back to their worktree.
        assert_eq!(
            worktree_from_container_name(&db, &format!("{plain}-tgagent")),
            Some("/wt/feat".to_string())
        );
        assert_eq!(
            worktree_from_container_name(&db, &format!("{plain}-tgvpn")),
            Some("/wt/feat".to_string())
        );
    }
}
