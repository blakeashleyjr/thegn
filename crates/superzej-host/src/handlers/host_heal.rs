//! Background host-heal: when a `[host.*]` row is parked on `Failed(retryable)`
//! (a network flap survived every in-step retry), re-drive its bring-up on a
//! growing backoff until it recovers — the sticky-failed-state fix.
//!
//! Loop-side and I/O-free: the ticker's `RefreshKind::HostHeal` (15s) calls
//! [`on_heal_tick`] with the already-hydrated snapshots; due hosts get ONE
//! off-thread `ensure_host_ready` each (single-flight is guaranteed by the
//! flight registry + heartbeat arbitration in `host_flow`). Results ride the
//! existing `HostUiEvent` channel — a recovery flips the panel to ready and
//! `HostDrainOutcome::any_ready` clears the sticky `materialize_failed` /
//! `prewarm_failed` sets, so a stuck loading splash self-heals with no `[r]`.

use std::collections::HashMap;

use superzej_core::heal::HealSchedule;

use crate::handlers::host::HostRuntime;
use crate::host_ui::HostSnapshot;

/// How long a spawned heal may stay unaccounted-for before its inflight mark
/// expires (a Deferred/wedged drive must not block healing forever).
const INFLIGHT_TTL_SECS: i64 = 600;

pub(crate) struct HealState {
    schedule: HealSchedule,
    /// Per host-id: (attempts made, unix time of the last attempt — or of
    /// first seeing the failure, for attempt 0).
    attempts: HashMap<String, (u32, i64)>,
    /// Host-ids with a heal drive spawned and not yet observed finished.
    inflight: HashMap<String, i64>,
}

impl Default for HealState {
    fn default() -> Self {
        HealState {
            // The `[remote] heal_backoff_secs` schedule installed at config load.
            schedule: superzej_core::heal::active_schedule(),
            attempts: HashMap::new(),
            inflight: HashMap::new(),
        }
    }
}

impl HealState {
    /// Reconcile finished heals from the loop's live runtime view: an inflight
    /// host that is no longer provisioning either recovered (forget it) or
    /// failed again (bump its attempt counter). Returns hosts that recovered
    /// under our healing (for the status line).
    fn reconcile(&mut self, rt: &HostRuntime, now: i64) -> Vec<String> {
        let mut recovered = Vec::new();
        self.inflight.retain(|id, spawned| {
            match rt.state.get(id) {
                Some(v) if v.provisioning => now.saturating_sub(*spawned) < INFLIGHT_TTL_SECS,
                Some(v) if v.ready => {
                    self.attempts.remove(id);
                    recovered.push(id.clone());
                    false
                }
                Some(_) => {
                    // Finished and not ready: restamp the clock so the next
                    // (already-incremented-at-spawn) attempt waits its full
                    // backoff from the failure, not from the spawn.
                    if let Some(e) = self.attempts.get_mut(id) {
                        e.1 = now;
                    }
                    false
                }
                // No live view (drive deferred/died silently): expire by TTL.
                None => now.saturating_sub(*spawned) < INFLIGHT_TTL_SECS,
            }
        });
        recovered
    }

    /// Pure decision: which snapshot hosts are due a background re-drive now.
    fn due_hosts(&mut self, hosts: &[HostSnapshot], now: i64) -> Vec<String> {
        let mut due = Vec::new();
        for h in hosts {
            if h.state != "failed" || !h.retryable || h.provisioning {
                continue;
            }
            if self.inflight.contains_key(&h.id) {
                continue;
            }
            // First sighting of this failure: stamp attempt 0 and wait out the
            // first backoff step (never heal-storm the moment a host fails —
            // the in-step retry ladder just exhausted itself against it).
            let (attempt, last) = *self.attempts.entry(h.id.clone()).or_insert((0, now));
            if self.schedule.due(attempt, last, now) {
                due.push(h.name.clone());
                self.inflight.insert(h.id.clone(), now);
                let e = self.attempts.entry(h.id.clone()).or_insert((0, now));
                e.0 = e.0.saturating_add(1);
                e.1 = now;
            }
        }
        due
    }
}

/// The `RefreshKind::HostHeal` tick. Cheap when nothing is failed (one Vec
/// scan); spawns at most one off-thread drive per due host. Sets
/// `model.status` and returns true when a heal was spawned or a host
/// recovered (the loop marks the frame dirty).
pub(crate) fn on_heal_tick(
    st: &mut HealState,
    model: &mut crate::chrome::FrameModel,
    rt: &HostRuntime,
    cfg: &superzej_core::config::Config,
    host_ui: &crate::host_flow::HostUiTx,
) -> bool {
    let now = unix_now();
    let recovered = st.reconcile(rt, now);
    let due = st.due_hosts(&model.panel.hosts, now);
    for name in &due {
        spawn_heal(name, cfg, host_ui);
    }
    if let Some(n) = recovered.first() {
        let name = n.strip_prefix("host:").unwrap_or(n);
        model.status = format!("host {name} recovered — retrying blocked worktrees");
        return true;
    }
    if !due.is_empty() {
        model.status = format!("host {}: retrying bring-up…", due.join(", "));
        return true;
    }
    false
}

/// One background re-drive. `BackgroundSkip` never installs anything without
/// consent; outcomes flow back through the shared `HostUiEvent` channel like
/// every other driver.
fn spawn_heal(
    name: &str,
    cfg: &superzej_core::config::Config,
    host_ui: &crate::host_flow::HostUiTx,
) {
    let Some(binding) = cfg.host_binding(name) else {
        return;
    };
    let ui = host_ui.clone();
    tokio::task::spawn_blocking(move || {
        let _ = crate::host_flow::ensure_host_ready(
            &binding,
            crate::host_flow::ConsentPolicy::BackgroundSkip,
            &mut |_| {},
            Some(&ui),
            &mut |reach| superzej_svc::host::runner_for(reach),
        );
    });
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_flow::HostUiEvent;
    use superzej_core::host::{HostFailure, HostStep};

    /// A state pinned to the BUILT-IN schedule — immune to a `[remote]`
    /// override another test may have installed into the process global.
    fn test_state() -> HealState {
        HealState {
            schedule: HealSchedule::default(),
            attempts: HashMap::new(),
            inflight: HashMap::new(),
        }
    }

    fn failed_snap(name: &str, retryable: bool) -> HostSnapshot {
        HostSnapshot {
            name: name.into(),
            id: format!("host:{name}"),
            state: "failed".into(),
            retryable,
            ..Default::default()
        }
    }

    fn empty_rt() -> HostRuntime {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        HostRuntime::new(rx)
    }

    fn rt_with(events: Vec<HostUiEvent>) -> HostRuntime {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for e in events {
            tx.send(e).unwrap();
        }
        let mut rt = HostRuntime::new(rx);
        rt.drain(false);
        rt
    }

    #[test]
    fn no_failed_hosts_means_no_work() {
        let mut st = test_state();
        let ready = HostSnapshot {
            name: "a".into(),
            id: "host:a".into(),
            state: "ready".into(),
            ..Default::default()
        };
        assert!(st.due_hosts(&[ready], 1000).is_empty());
        assert!(st.attempts.is_empty(), "no tracking for healthy hosts");
    }

    #[test]
    fn first_sighting_waits_out_the_first_backoff_step() {
        let mut st = test_state(); // schedule [15,30,60,300]
        let snaps = [failed_snap("a", true)];
        assert!(
            st.due_hosts(&snaps, 1000).is_empty(),
            "stamped, not due yet"
        );
        assert!(st.due_hosts(&snaps, 1010).is_empty(), "still inside 15s");
        let due = st.due_hosts(&snaps, 1016);
        assert_eq!(due, vec!["a".to_string()], "due after the first step");
        assert!(st.inflight.contains_key("host:a"));
        // Inflight: no duplicate spawn while the drive runs.
        assert!(st.due_hosts(&snaps, 2000).is_empty());
    }

    #[test]
    fn non_retryable_and_provisioning_hosts_are_skipped() {
        let mut st = test_state();
        let mut perm = failed_snap("p", false);
        perm.error = "consent declined".into();
        let mut prov = failed_snap("q", true);
        prov.provisioning = true;
        assert!(st.due_hosts(&[perm, prov], 10_000).is_empty());
    }

    #[test]
    fn reconcile_recovery_resets_and_reports() {
        let mut st = test_state();
        let snaps = [failed_snap("a", true)];
        let _ = st.due_hosts(&snaps, 1000);
        let due = st.due_hosts(&snaps, 1016);
        assert_eq!(due.len(), 1);
        let rt = rt_with(vec![HostUiEvent::Done {
            host: "host:a".into(),
            result: Ok(()),
        }]);
        let recovered = st.reconcile(&rt, 1020);
        assert_eq!(recovered, vec!["host:a".to_string()]);
        assert!(st.inflight.is_empty());
        assert!(st.attempts.is_empty(), "clean slate after recovery");
    }

    #[test]
    fn reconcile_refailure_bumps_attempts_toward_longer_backoff() {
        let mut st = test_state();
        let snaps = [failed_snap("a", true)];
        let _ = st.due_hosts(&snaps, 1000); // stamp (0, 1000)
        assert_eq!(st.due_hosts(&snaps, 1016).len(), 1); // attempt 1 spawned
        let rt = rt_with(vec![HostUiEvent::Done {
            host: "host:a".into(),
            result: Err(HostFailure {
                step: HostStep::Connect,
                error: "still down".into(),
                retryable: true,
            }),
        }]);
        assert!(st.reconcile(&rt, 1020).is_empty());
        assert!(st.inflight.is_empty(), "finished attempt cleared");
        let (attempts, last) = st.attempts["host:a"];
        assert_eq!(attempts, 1, "one attempt made");
        assert_eq!(last, 1020, "clock restamped at the refailure");
        // Next attempt honors the LONGER step (30s at attempt 1), not 15s.
        assert!(st.due_hosts(&snaps, 1040).is_empty());
        assert_eq!(st.due_hosts(&snaps, 1051).len(), 1);
    }

    #[test]
    fn inflight_marks_expire_after_ttl() {
        let mut st = test_state();
        let snaps = [failed_snap("a", true)];
        let _ = st.due_hosts(&snaps, 1000);
        let _ = st.due_hosts(&snaps, 1016); // inflight at 1016
        // A drive that never reports (deferred consent, silent death) expires,
        // so healing can resume. No live view at all in rt.
        let rt = empty_rt();
        let _ = st.reconcile(&rt, 1016 + INFLIGHT_TTL_SECS + 1);
        assert!(st.inflight.is_empty(), "TTL expiry unblocks future heals");
    }
}
