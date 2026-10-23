//! Loop-side host state: drains [`HostUiEvent`]s from off-thread `ensure_ready`
//! drivers into a per-host view map (panel/sidebar/chip data), raises the
//! install-consent confirm modal, and routes the modal's answer back to the
//! parked driver. Everything here runs ON the loop and is I/O-free.

use std::collections::{HashMap, VecDeque};

use thegn_core::host::HostFailure;
use thegn_core::store::HostStore;

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
             machine — thegn never does this silently. Granting persists for this \
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
fn spawn_host_db(what: &'static str, host: String, f: fn(&thegn_core::db::Db, &str)) {
    let job = move || match thegn_core::db::Db::open() {
        Ok(db) => f(&db, &host),
        Err(e) => thegn_core::msg::warn(&format!("host {what}: state db: {e}")),
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
        .map(|d| thegn_core::time_policy::saturating_i64(u128::from(d.as_secs())))
        .unwrap_or(0)
}

/// Off-loop removal of a runtime-added host: drop the placement-capacity index,
/// the DB row (def + cached state + inventory + events), but only when the host
/// is DB-added and has no live placement tenants. Config-defined hosts and hosts
/// with running sandboxes are refused and reported, never silently dropped — mirrors
/// `thegn host rm`'s guardrails without the interactive `--force`/drain path.
fn host_remove_refusal(name: &str, is_db_def: bool, live_tenants: usize) -> Option<String> {
    if !is_db_def {
        Some(format!(
            "host {name}: config-defined — remove it from config.toml"
        ))
    } else if live_tenants > 0 {
        Some(format!(
            "host {name}: {live_tenants} live placement(s) — drain via `thegn host rm` first"
        ))
    } else {
        None
    }
}

fn remove_host_def(
    host: String,
    refresh_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>>,
    host_ui: Option<crate::host_flow::HostUiTx>,
) {
    let job = move || {
        let report = |message: String, priority| {
            if let Some(tx) = &refresh_tx
                && let Err(error) =
                    tx.send(crate::hydrate::RefreshKind::Toast { message, priority })
            {
                tracing::debug!(target: "thegn::host", %error, "host removal toast receiver is gone");
            }
            if let Some(ui) = &host_ui
                && let Err(error) = ui.waker.wake()
            {
                tracing::debug!(target: "thegn::host", %error, "failed to wake after host removal");
            }
        };
        let outcome = (|| -> Result<String, String> {
            let db = thegn_core::db::Db::open().map_err(|error| format!("host remove: {error}"))?;
            let id = thegn_core::host::HostId::parse(&host)
                .ok_or_else(|| format!("host remove: invalid host id {host}"))?;
            let name = id.config_name().unwrap_or("").to_string();
            let defs = db
                .host_defs()
                .map_err(|error| format!("host remove: {error}"))?;
            use thegn_core::store::PlacementStore;
            let tenants = db
                .tenants_of(&id)
                .map_err(|error| format!("host remove: {error}"))?;
            if let Some(refusal) = host_remove_refusal(
                &name,
                defs.iter().any(|(defined, _)| defined == &name),
                tenants.len(),
            ) {
                return Err(refusal);
            }
            let _ = db.capacity_delete(&id); // best-effort: cache write: the capacity row is bookkeeping; the host_delete below reports the real failure
            db.host_delete(&id)
                .map_err(|error| format!("host remove: {error}"))?;
            Ok(format!("host {name} removed"))
        })();
        match outcome {
            Ok(message) => report(message, thegn_core::notification::Priority::Info),
            Err(message) => report(message, thegn_core::notification::Priority::Alert),
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(job);
    } else {
        job();
    }
}

/// Parse + persist an add-host target (the System ▸ Hosts panel's `n` key),
/// then queue the durable write; the pending-DB drain queues a panel refresh
/// after success. Returns the immediate status line or a parse error.
pub(crate) fn queue_host_add_from_input(
    text: &str,
    cfg: &thegn_core::config::Config,
    pending_db_tx: &tokio::sync::mpsc::UnboundedSender<crate::run::run_pending_db::PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) -> String {
    match crate::handlers::wizard::add_host_from_input(
        text,
        cfg,
        crate::run::run_pending_db::HostAddThen::Panel,
        pending_db_tx,
        waker,
    ) {
        Ok(()) => "add host: saving…".into(),
        Err(error) => format!("add host: {error}"),
    }
}

/// A System ▸ Hosts section key press. `p`/`r`/`c`/`x` act on the cursor host
/// (delegated to [`crate::host_ui::panel_key`]); `m` opens the discoverable
/// action menu; `n` opens the add-host text input. Returns whether the key was
/// claimed (so the loop can mark the frame dirty).
#[allow(clippy::too_many_arguments)]
pub(crate) fn section_key(
    key: termwiz::input::KeyCode,
    cursor: usize,
    model: &mut crate::chrome::FrameModel,
    cfg: &thegn_core::config::Config,
    host_ui: &crate::host_flow::HostUiTx,
    active_menu: &mut Option<MenuOverlay>,
    host_input: &mut Option<(menu::InputOverlay, crate::run::HostInputKind)>,
) -> bool {
    use termwiz::input::KeyCode::Char;
    match key {
        Char('n') => {
            *host_input = Some((
                menu::InputOverlay::new(
                    "add host — user@host[:port]  (dumbpipe:<ticket> <user> for iroh)",
                    "",
                ),
                crate::run::HostInputKind::AddHost,
            ));
            model.status = "add host: type a target, Enter to add (Esc cancels)".into();
            true
        }
        Char('m') => {
            if let Some(h) = model.panel.hosts.get(cursor) {
                *active_menu = Some(menu::host_actions_menu(&h.name, &h.id, h.is_db_def));
            }
            true
        }
        _ => crate::host_ui::panel_key(key, cursor, model, cfg, host_ui, active_menu),
    }
}

/// Intercept a menu choice that answers a live consent ask, one of the System ▸
/// Hosts panel confirms (`host-grant` / `host-rm`), or a host action-menu item
/// (`host-provision` / `host-reprobe` / `host-remove` — the `m` menu). Returns a
/// status line when consumed. `pending` marks "the consent modal for this host
/// is (or was) up"; `host_ui` is `None` only in unit tests (no fake waker), in
/// which case the provision/re-probe items are inert.
pub(crate) fn intercept_menu_choice(
    choice: &MenuChoice,
    pending: &mut Option<String>,
    cfg: &thegn_core::config::Config,
    host_ui: Option<&crate::host_flow::HostUiTx>,
    refresh_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>>,
) -> Option<String> {
    match choice {
        // Host action-menu provision / re-probe: spawn the same off-loop drive
        // as the `p`/`r` keys.
        MenuChoice::Confirm {
            tag: tag @ ("host-provision" | "host-reprobe"),
            arg,
        } => {
            let name = arg.strip_prefix("host:").unwrap_or(arg);
            host_ui.map(|ui| crate::host_ui::drive_host(name, *tag == "host-reprobe", cfg, ui))
        }
        // Host action-menu "remove host": drop a runtime-added `[host.*]` def.
        // Tenant-safe + DB-only — validated in the off-loop job, which reports
        // refusals and failures through a toast.
        MenuChoice::Confirm {
            tag: "host-remove",
            arg,
        } => {
            let name = arg.strip_prefix("host:").unwrap_or(arg).to_string();
            remove_host_def(arg.clone(), refresh_tx.cloned(), host_ui.cloned());
            Some(format!("host {name}: removing…"))
        }
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
                let Some(id) = thegn_core::host::HostId::parse(host) else {
                    return;
                };
                // Seed the row when the host was never provisioned, so the
                // UPDATE-based grant has something to land on.
                if !matches!(db.host_get(&id), Ok(Some(_))) {
                    let name = id.config_name().unwrap_or("").to_string();
                    let _ = db.host_checkpoint(
                        // best-effort: cache write: seeds the row so the consent grant below lands
                        &id,
                        &name,
                        "",
                        &thegn_core::host_machine::HostState::Unknown,
                        None,
                        unix_now(),
                    );
                }
                if let Err(e) = db.host_set_consent(&id, true, unix_now()) {
                    thegn_core::msg::warn(&format!("host grant failed: {e}"));
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
                let Some(id) = thegn_core::host::HostId::parse(host) else {
                    return;
                };
                if let Err(e) = db.host_delete(&id) {
                    thegn_core::msg::warn(&format!("host rm-cache failed: {e}"));
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

/// Stop all ingress shares on the active worktree (`Action::StopWorktreeShare`),
/// extracted from the size-capped `run.rs`. Durable cleanup runs on the
/// critical DB writer so stopped tunnels cannot be resurrected on restart.
pub(crate) fn stop_worktree_share(
    model: &mut crate::chrome::FrameModel,
    share_supervisor: &mut crate::share::ShareSupervisor,
    session: &crate::session::Session,
) {
    let Some(wt) = session.active_group().map(|g| g.path.clone()) else {
        return;
    };
    // DB cleanup keys off the ports the SUPERVISOR actually stopped —
    // `model.shares` is a render mirror that can lag a worktree switch, and
    // deleting by it left the real rows behind (a "stopped" tunnel then
    // resurrected on the next launch).
    let stopped = share_supervisor.stop(&wt, None);
    let status = crate::share::stop_status(&stopped, None);
    queue_stopped_share_deletes(wt.clone(), stopped.removed);
    queue_stopped_share_failures(wt, stopped.failed.clone());
    model.shares = crate::run::current_share_views(share_supervisor, session);
    model.status = status;
}

pub(crate) fn queue_stopped_share_deletes(worktree: String, stopped: Vec<(u16, u64)>) {
    if stopped.is_empty() {
        return;
    }
    crate::db_task::persist_critical(move |db| {
        use thegn_core::store::WorktreeAuxStore as _;
        for (port, generation) in stopped {
            match db.delete_share_generation(&worktree, port, generation) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(target: "thegn::db", %worktree, port, generation, "stale stopped share delete left another generation intact")
                }
                Err(error) => {
                    tracing::warn!(target: "thegn::db", %error, "stopped share cache delete failed")
                }
            }
        }
    });
}

pub(crate) fn queue_stopped_share_failures(
    worktree: String,
    failed: Vec<crate::share::StopFailure>,
) {
    if failed.is_empty() {
        return;
    }
    crate::db_task::persist_critical(move |db| {
        for failure in failed {
            crate::share::apply_share_persist(
                db,
                crate::share::stop_failure_persist(&worktree, &failure),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::host::HostStep;

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
        // host_ui = None (no fake waker in tests); the paths exercised here don't
        // spawn a drive, so config + host_ui are inert.
        let cfg = thegn_core::config::Config::default();
        let mut pending = Some("host:a".to_string());
        let s = intercept_menu_choice(
            &MenuChoice::Confirm {
                tag: "host-install",
                arg: "host:a".into(),
            },
            &mut pending,
            &cfg,
            None,
            None,
        )
        .expect("consumed");
        assert!(s.contains("granted"));
        assert!(pending.is_none());

        let mut pending = Some("host:a".to_string());
        let s = intercept_menu_choice(&MenuChoice::Dismiss, &mut pending, &cfg, None, None)
            .expect("consumed");
        assert!(s.contains("declined"));
        assert!(pending.is_none());

        // A dismiss with no pending consent is someone else's dismiss.
        let mut none = None;
        assert!(intercept_menu_choice(&MenuChoice::Dismiss, &mut none, &cfg, None, None).is_none());
        // Unrelated confirms pass through.
        assert!(
            intercept_menu_choice(
                &MenuChoice::Confirm {
                    tag: "git-op",
                    arg: String::new()
                },
                &mut Some("host:a".into()),
                &cfg,
                None,
                None,
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

    #[test]
    fn host_remove_refusal_covers_config_and_live_tenants() {
        assert!(
            host_remove_refusal("lab", false, 0)
                .unwrap()
                .contains("config-defined")
        );
        assert!(
            host_remove_refusal("lab", true, 2)
                .unwrap()
                .contains("2 live placement(s)")
        );
        assert_eq!(host_remove_refusal("lab", true, 0), None);
    }

    #[test]
    fn stop_worktree_share_does_not_block_on_a_held_write_lock() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        use thegn_core::store::WorktreeAuxStore as _;

        let state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        db.upsert_share("/isolated/wt", 4312, 17, "bore", None, "failed")
            .unwrap();
        drop(db);
        let lock = rusqlite::Connection::open(state.state_home().join("thegn/thegn.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let session = crate::session::Session {
            id: "test".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "isolated/wt",
                crate::session::GroupKind::Home,
                "/isolated/wt",
            )],
            active: 0,
        };
        let mut supervisor = crate::share::ShareSupervisor::new();
        crate::share::seed_failed_instance_for_test(&mut supervisor, "/isolated/wt", 4312, 17);
        let mut model = crate::chrome::FrameModel::default();
        let started = std::time::Instant::now();
        stop_worktree_share(&mut model, &mut supervisor, &session);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert_eq!(model.status, "Stopped 1 share(s)");

        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        crate::db_task::persist_critical(move |db| {
            let rows = db.list_shares().map(|rows| {
                rows.into_iter()
                    .filter(|row| row.worktree == "/isolated/wt")
                    .collect::<Vec<_>>()
            });
            if read_tx.send(rows).is_err() {
                tracing::debug!(target: "thegn::host", "share read-back receiver closed");
            }
        });
        assert!(
            read_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }
}
