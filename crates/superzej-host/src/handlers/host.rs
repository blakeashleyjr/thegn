//! Loop-side host state: drains [`HostUiEvent`]s from off-thread `ensure_ready`
//! drivers into a per-host view map (panel/sidebar/chip data), raises the
//! install-consent confirm modal, and routes the modal's answer back to the
//! parked driver. Everything here runs ON the loop and is I/O-free.

use std::collections::{HashMap, VecDeque};

use superzej_core::host::HostFailure;

use crate::agent::ProvisionStepView;
use crate::host_flow::{HostUiEvent, failure_reason, resolve_consent};
use crate::menu::{self, MenuChoice, MenuOverlay};

/// The loop's live view of one host (UI-facing; the DB row is the durable one).
#[derive(Default)]
pub(crate) struct HostView {
    pub steps: Vec<ProvisionStepView>,
    pub failed: Option<HostFailure>,
    pub ready: bool,
    pub provisioning: bool,
}

/// What one drain pass asks of the loop.
#[derive(Default)]
pub(crate) struct HostDrainOutcome {
    pub dirty: bool,
    /// Raise this consent modal (only handed out when the loop reports no
    /// other overlay is up; otherwise it stays queued).
    pub consent: Option<(MenuOverlay, String)>,
    /// A host just reached Ready: clear failed-materialize marks so blocked
    /// tabs re-probe on the next turn.
    pub any_ready: bool,
}

pub(crate) struct HostRuntime {
    rx: tokio::sync::mpsc::UnboundedReceiver<HostUiEvent>,
    pub state: HashMap<String, HostView>,
    /// Consent asks parked while another overlay was up — never dropped.
    pending_consent: VecDeque<(String, String)>, // (host, runtime)
}

impl HostRuntime {
    pub(crate) fn new(rx: tokio::sync::mpsc::UnboundedReceiver<HostUiEvent>) -> HostRuntime {
        HostRuntime {
            rx,
            state: HashMap::new(),
            pending_consent: VecDeque::new(),
        }
    }

    /// Drain all pending events. `overlay_open` gates whether a queued consent
    /// modal may be handed to the loop this turn.
    pub(crate) fn drain(&mut self, overlay_open: bool) -> HostDrainOutcome {
        let mut out = HostDrainOutcome::default();
        while let Ok(ev) = self.rx.try_recv() {
            out.dirty = true;
            match ev {
                HostUiEvent::Progress { host, steps } => {
                    let v = self.state.entry(host).or_default();
                    v.steps = steps;
                    v.provisioning = true;
                    v.failed = None;
                }
                HostUiEvent::NeedsConsent { host, runtime } => {
                    if !self.pending_consent.iter().any(|(h, _)| h == &host) {
                        self.pending_consent.push_back((host, runtime));
                    }
                }
                HostUiEvent::Done { host, result } => {
                    let v = self.state.entry(host).or_default();
                    v.provisioning = false;
                    match result {
                        Ok(()) => {
                            v.ready = true;
                            v.failed = None;
                            out.any_ready = true;
                        }
                        Err(f) => {
                            v.ready = false;
                            v.failed = Some(f);
                        }
                    }
                }
            }
        }
        if !overlay_open && let Some((host, runtime)) = self.pending_consent.pop_front() {
            out.consent = Some((consent_menu(&host, &runtime), host));
            out.dirty = true;
        }
        out
    }

    /// One-line status for a host (panel rows / sidebar note) — merged onto
    /// the display snapshots by [`crate::host_ui::merge_live`].
    pub(crate) fn status_line(&self, host: &str) -> Option<String> {
        let v = self.state.get(host)?;
        if let Some(f) = &v.failed {
            return Some(failure_reason(f));
        }
        if v.ready {
            return Some("ready".into());
        }
        if v.provisioning {
            let active = v
                .steps
                .iter()
                .find(|s| s.state == crate::agent::ProvisionState::Active);
            return Some(match active {
                Some(s) => match &s.detail {
                    Some(d) => format!("{} — {d}", s.label),
                    None => s.label.clone(),
                },
                None => "provisioning".into(),
            });
        }
        None
    }
}

/// The per-host install-consent confirm modal. `[y]` resolves the parked
/// driver with a grant, `[n]`/Esc declines (both persisted per-host).
fn consent_menu(host: &str, runtime: &str) -> MenuOverlay {
    let name = host.strip_prefix("host:").unwrap_or(host);
    menu::confirm_menu(
        format!("⚙ install {runtime} on {name}?"),
        format!(
            "{name} has no container runtime. Installing {runtime} modifies a remote \
             machine — superzej never does this silently. Granting persists for this \
             host; declining halts host-backed envs until you grant from System ▸ Hosts \
             or set [host.{name}] install_runtime = \"auto\"."
        ),
        "host-install",
        host.to_string(),
        true,
    )
}

/// Run a small host DB mutation off the loop (falling back to inline when no
/// tokio runtime is up). Failures are warned via `msg::warn` — surfaced in
/// the status bar, never swallowed.
fn spawn_host_db(what: &'static str, host: String, f: fn(&superzej_core::db::Db, &str)) {
    let job = move || match superzej_core::db::Db::open() {
        Ok(db) => f(&db, &host),
        Err(e) => superzej_core::msg::warn(&format!("host {what}: state db: {e}")),
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(job);
    } else {
        job();
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Intercept a menu choice that answers a live consent ask, or one of the
/// System ▸ Hosts panel confirms (`host-grant` / `host-rm` — raised by
/// `host_ui::panel_key`). Returns a status line when the choice was consumed.
/// `pending` is the loop's marker for "the consent modal for this host is
/// (or was) up".
pub(crate) fn intercept_menu_choice(
    choice: &MenuChoice,
    pending: &mut Option<String>,
) -> Option<String> {
    match choice {
        MenuChoice::Confirm {
            tag: "host-install",
            arg,
        } => {
            resolve_consent(arg, true);
            *pending = None;
            let name = arg.strip_prefix("host:").unwrap_or(arg);
            Some(format!("host {name}: install granted"))
        }
        // The panel's explicit consent (re-)grant: persist it, and resolve any
        // provision currently parked on the ask.
        MenuChoice::Confirm {
            tag: "host-grant",
            arg,
        } => {
            resolve_consent(arg, true);
            spawn_host_db("grant", arg.clone(), |db, host| {
                let Some(id) = superzej_core::host::HostId::parse(host) else {
                    return;
                };
                // Seed the row when the host was never provisioned, so the
                // UPDATE-based grant has something to land on.
                if !matches!(db.host_get(&id), Ok(Some(_))) {
                    let name = id.config_name().unwrap_or("").to_string();
                    let _ = db.host_checkpoint(
                        &id,
                        &name,
                        "",
                        &superzej_core::host_machine::HostState::Unknown,
                        None,
                        unix_now(),
                    );
                }
                if let Err(e) = db.host_set_consent(&id, true, unix_now()) {
                    superzej_core::msg::warn(&format!("host grant failed: {e}"));
                }
            });
            let name = arg.strip_prefix("host:").unwrap_or(arg);
            Some(format!("host {name}: install grant recorded"))
        }
        // The panel's rm-cache confirm: drop the row + inventory + events; the
        // next use re-probes and re-provisions from scratch.
        MenuChoice::Confirm {
            tag: "host-rm",
            arg,
        } => {
            spawn_host_db("rm-cache", arg.clone(), |db, host| {
                let Some(id) = superzej_core::host::HostId::parse(host) else {
                    return;
                };
                if let Err(e) = db.host_delete(&id) {
                    superzej_core::msg::warn(&format!("host rm-cache failed: {e}"));
                }
            });
            let name = arg.strip_prefix("host:").unwrap_or(arg);
            Some(format!(
                "host {name}: cached state removed — next use re-provisions"
            ))
        }
        MenuChoice::Dismiss if pending.is_some() => {
            let host = pending.take().expect("checked");
            resolve_consent(&host, false);
            let name = host.strip_prefix("host:").unwrap_or(&host);
            Some(format!(
                "host {name}: install declined — grant later from System ▸ Hosts"
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::host::HostStep;

    fn runtime_with(
        events: Vec<HostUiEvent>,
    ) -> (HostRuntime, tokio::sync::mpsc::UnboundedSender<HostUiEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for e in events {
            tx.send(e).unwrap();
        }
        (HostRuntime::new(rx), tx)
    }

    #[test]
    fn drain_tracks_progress_ready_and_failure() {
        let (mut rt, _tx) = runtime_with(vec![
            HostUiEvent::Progress {
                host: "host:a".into(),
                steps: vec![ProvisionStepView {
                    label: "connect".into(),
                    state: crate::agent::ProvisionState::Active,
                    detail: None,
                }],
            },
            HostUiEvent::Done {
                host: "host:a".into(),
                result: Ok(()),
            },
            HostUiEvent::Done {
                host: "host:b".into(),
                result: Err(HostFailure {
                    step: HostStep::Deliver,
                    error: "stalled".into(),
                    retryable: true,
                }),
            },
        ]);
        let out = rt.drain(false);
        assert!(out.dirty);
        assert!(out.any_ready);
        assert!(rt.state["host:a"].ready);
        assert!(rt.state["host:b"].failed.is_some());
        assert_eq!(rt.status_line("host:a").as_deref(), Some("ready"));
        assert!(rt.status_line("host:b").unwrap().contains("stalled"));
        assert!(rt.status_line("host:absent").is_none());
    }

    #[test]
    fn consent_queues_behind_open_overlays_and_never_drops() {
        let (mut rt, _tx) = runtime_with(vec![HostUiEvent::NeedsConsent {
            host: "host:a".into(),
            runtime: "podman".into(),
        }]);
        let out = rt.drain(true);
        assert!(out.consent.is_none(), "held while an overlay is up");
        let out = rt.drain(false);
        let (menu, host) = out.consent.expect("released when clear");
        assert_eq!(host, "host:a");
        assert!(menu.title.contains("podman"));
        // Drained queue: no duplicate modal next turn.
        assert!(rt.drain(false).consent.is_none());
    }

    #[test]
    fn menu_interception_resolves_grant_and_decline() {
        let mut pending = Some("host:a".to_string());
        let s = intercept_menu_choice(
            &MenuChoice::Confirm {
                tag: "host-install",
                arg: "host:a".into(),
            },
            &mut pending,
        )
        .expect("consumed");
        assert!(s.contains("granted"));
        assert!(pending.is_none());

        let mut pending = Some("host:a".to_string());
        let s = intercept_menu_choice(&MenuChoice::Dismiss, &mut pending).expect("consumed");
        assert!(s.contains("declined"));
        assert!(pending.is_none());

        // A dismiss with no pending consent is someone else's dismiss.
        let mut none = None;
        assert!(intercept_menu_choice(&MenuChoice::Dismiss, &mut none).is_none());
        // Unrelated confirms pass through.
        assert!(
            intercept_menu_choice(
                &MenuChoice::Confirm {
                    tag: "git-op",
                    arg: String::new()
                },
                &mut Some("host:a".into())
            )
            .is_none()
        );
    }

    #[test]
    fn progress_status_line_shows_active_step_detail() {
        let (mut rt, _tx) = runtime_with(vec![HostUiEvent::Progress {
            host: "host:a".into(),
            steps: vec![
                ProvisionStepView {
                    label: "connect".into(),
                    state: crate::agent::ProvisionState::Done,
                    detail: None,
                },
                ProvisionStepView {
                    label: "transfer image".into(),
                    state: crate::agent::ProvisionState::Active,
                    detail: Some("412 MiB / 1.9 GiB".into()),
                },
            ],
        }]);
        rt.drain(false);
        assert_eq!(
            rt.status_line("host:a").as_deref(),
            Some("transfer image — 412 MiB / 1.9 GiB")
        );
    }
}
