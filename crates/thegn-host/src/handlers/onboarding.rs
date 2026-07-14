//! Onboarding-wizard glue extracted from `run.rs` (pinned by the file-size
//! ratchet): startup arming (first-run detection + the legacy keymap picker),
//! outcome application (config writes, probe spawns, login/agent panes), and
//! probe delivery. The wizard state machine itself is [`crate::onboarding`]
//! and stays pure; everything with I/O lives here. Probes run on
//! `spawn_blocking` and come back on the refresh channel
//! (`RefreshKind::Onboarding`) + a waker pulse — never on the loop.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::hydrate::RefreshKind;
use crate::onboarding::{
    Effects, ForgeStatus, OnboardingWizard, Outcome, ProbeRequest, ProbeResult, WriteOp,
};
use crate::panes::Panes;
use crate::session::Session;
use thegn_core::config::Config;
use thegn_core::store::WorkspaceStore;

/// What `handle_key_event` asked the loop to finish up: a keymap-preset change
/// needs a `rebuild_keymap` (loop-owned), a spawned login/agent tab needs the
/// focus/relayout trio.
#[derive(Default)]
pub(crate) struct Applied {
    pub keymap_changed: bool,
    pub spawned: bool,
}

/// The loop's onboarding state, one local: the wizard slot plus the pane it is
/// suspended behind (`gh auth login` / agent setup) as `(pane id, is_login)`.
#[derive(Default)]
pub(crate) struct OnboardingUi {
    pub ui: Option<OnboardingWizard>,
    pub wait: Option<(u32, bool)>,
}

impl OnboardingUi {
    /// The wizard owns keyboard/paste input (open and not suspended).
    pub(crate) fn active(&self) -> bool {
        self.ui.is_some() && self.wait.is_none()
    }

    /// Route a bracketed paste to the wizard; `false` when it isn't active.
    pub(crate) fn paste(&mut self, text: &str) -> bool {
        if self.wait.is_some() {
            return false;
        }
        match self.ui.as_mut() {
            Some(w) => {
                w.handle_paste(text);
                true
            }
            None => false,
        }
    }

    /// Render the wizard overlay unless suspended behind its login/agent tab.
    pub(crate) fn render(&self, surface: &mut termwiz::surface::Surface, screen: Rect) {
        if let (Some(w), None) = (&self.ui, &self.wait) {
            w.render(surface, screen);
        }
    }
}

/// Startup arming, replacing the run.rs first-launch block: open the wizard on
/// first run (no `ui_state` marker) or when `thegn setup` requested it;
/// otherwise apply a remembered keymap preset (returns `true` → caller
/// rebuilds the keymap) or arm the one-time keymap picker. The picker is
/// skipped while the wizard is armed — the wizard has its own keymap step.
pub(crate) fn startup(
    cfg: &mut Config,
    active_menu: &mut Option<crate::menu::MenuOverlay>,
) -> (OnboardingUi, bool) {
    let requested = crate::onboarding::setup_requested();
    // Only a healthy DB with no marker counts as first run — a broken DB must
    // not re-open the wizard on every launch.
    let first_run = thegn_core::db::Db::open()
        .map(|db| {
            db.get_ui_state("", crate::onboarding::UI_STATE_KEY)
                .ok()
                .flatten()
                .is_none()
        })
        .unwrap_or(false);
    let wizard = (requested || first_run).then(|| OnboardingWizard::new(cfg));
    let mut rebuild = false;
    if cfg.keymap_preset.is_empty() || cfg.keymap_preset == "default" {
        match thegn_core::db::Db::open()
            .ok()
            .and_then(|db| db.get_ui_state("", "keymap_preset").ok().flatten())
        {
            Some(remembered) if remembered != "default" => {
                cfg.keymap_preset = remembered;
                rebuild = true;
            }
            Some(_) => {} // remembered "default" — no overlay, no picker
            None if wizard.is_none() => *active_menu = Some(crate::menu::keymap_preset_menu()),
            None => {}
        }
    }
    (
        OnboardingUi {
            ui: wizard,
            wait: None,
        },
        rebuild,
    )
}

/// Feed one key to the wizard and apply its [`Outcome`] on the event loop:
/// run the effects (config writes, probe spawn, login/agent pane), dismiss +
/// persist the first-run marker on close. Config writes are the same small
/// in-place `toml_edit` files the env wizard writes on the loop.
#[allow(clippy::too_many_arguments)] // one call site; a ctx struct would cost more run.rs lines
pub(crate) fn handle_key_event(
    key: &KeyCode,
    mods: Modifiers,
    ob: &mut OnboardingUi,
    model: &mut FrameModel,
    cfg: &mut Config,
    session: &mut Session,
    panes: &mut Panes,
    center: Rect,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) -> Applied {
    let mut applied = Applied::default();
    let Some(w) = ob.ui.as_mut() else {
        return applied;
    };
    match w.handle_key(key, mods) {
        Outcome::Pending => {}
        Outcome::Do(effects) => run_effects(
            effects,
            &mut ob.wait,
            model,
            cfg,
            session,
            panes,
            center,
            refresh_tx,
            waker,
            &mut applied,
        ),
        Outcome::Close { completed, effects } => {
            run_effects(
                effects,
                &mut ob.wait,
                model,
                cfg,
                session,
                panes,
                center,
                refresh_tx,
                waker,
                &mut applied,
            );
            ob.ui = None;
            persist_done();
            model.status = if completed {
                "setup complete — re-run any time with `thegn setup`".into()
            } else {
                "setup dismissed — re-run any time with `thegn setup`".into()
            };
        }
    }
    applied
}

/// Mark the wizard as seen so it stops auto-opening.
fn persist_done() {
    if let Ok(db) = thegn_core::db::Db::open() {
        // best-effort: a failed persist just re-offers the wizard next launch.
        let _ = db.set_ui_state("", crate::onboarding::UI_STATE_KEY, "1");
    }
}

#[allow(clippy::too_many_arguments)]
fn run_effects(
    effects: Effects,
    wait: &mut Option<(u32, bool)>,
    model: &mut FrameModel,
    cfg: &mut Config,
    session: &mut Session,
    panes: &mut Panes,
    center: Rect,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    applied: &mut Applied,
) {
    let path = Config::path();
    let mut wrote = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for op in effects.writes {
        let r = match op {
            WriteOp::Set { key, value } => thegn_core::config_write::set_key(&path, &key, &value),
            WriteOp::SetArray { key, items } => {
                thegn_core::config_write::set_string_array(&path, &key, &items)
            }
            WriteOp::Secret { name, key, token } => crate::secret::store(&name, &token)
                .and_then(|sref| thegn_core::config_write::set_key(&path, &key, &sref)),
            WriteOp::Host { name, ssh } => {
                thegn_core::config_write::upsert_host(&path, &name, &ssh)
            }
            WriteOp::KeymapPreset(preset) => {
                model.status = super::apply_keymap_preset(&preset, cfg);
                applied.keymap_changed = true;
                Ok(())
            }
        };
        match r {
            Ok(()) => wrote += 1,
            Err(e) => errs.push(e.to_string()),
        }
    }
    // Never swallow a failed write — saving config IS the wizard's primary path.
    if !errs.is_empty() {
        model.status = format!("setup: save failed — {}", errs.join("; "));
    } else if wrote > 0 && !applied.keymap_changed {
        model.status = format!(
            "setup: saved {wrote} setting{} to {}",
            if wrote == 1 { "" } else { "s" },
            path.display()
        );
    }
    if let Some(req) = effects.probe {
        spawn_probe(req, refresh_tx.clone(), waker.clone());
    }
    if effects.login {
        spawn_wait_tab(
            "gh auth login",
            true,
            wait,
            model,
            session,
            panes,
            center,
            applied,
        );
    }
    if effects.agent_setup {
        spawn_wait_tab(
            "thegn agent setup",
            false,
            wait,
            model,
            session,
            panes,
            center,
            applied,
        );
    }
}

/// Spawn `command` into a new center tab and suspend the wizard until the
/// pane exits (`wait`); the run loop resumes + re-probes on exit.
#[allow(clippy::too_many_arguments)]
fn spawn_wait_tab(
    command: &str,
    is_login: bool,
    wait: &mut Option<(u32, bool)>,
    model: &mut FrameModel,
    session: &mut Session,
    panes: &mut Panes,
    center: Rect,
    applied: &mut Applied,
) {
    match crate::actions::open_command_tab_id(session, panes, command, None, center) {
        Some(id) => {
            *wait = Some((id, is_login));
            applied.spawned = true;
            model.status = format!("`{command}` opened — the wizard resumes when it exits");
        }
        None => model.status = format!("setup: couldn't spawn `{command}`"),
    }
}

/// A pane from `spawn_wait_tab` exited: resume the wizard, and after a login
/// pane re-probe gh auth so the Forge step reflects the fresh state.
pub(crate) fn on_pane_exit(
    exited: &[u32],
    ob: &mut OnboardingUi,
    model: &mut FrameModel,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) -> bool {
    let Some((id, is_login)) = ob.wait else {
        return false;
    };
    if !exited.contains(&id) {
        return false;
    }
    ob.wait = None;
    if is_login && let Some(w) = ob.ui.as_mut() {
        w.forge_recheck();
        spawn_probe(ProbeRequest::Forge, refresh_tx.clone(), waker.clone());
        model.status = "login window closed — re-checking gh auth".into();
    }
    ob.ui.is_some()
}

/// Deliver a probe answer. Host reachability lands in the status bar (its step
/// has already advanced); the rest updates the live wizard.
pub(crate) fn apply_probe(
    ob: &mut OnboardingUi,
    result: ProbeResult,
    model: &mut FrameModel,
) -> bool {
    if let ProbeResult::Host { name, ok, detail } = &result {
        model.status = if *ok {
            format!("host '{name}' reachable — {detail}")
        } else {
            format!("host '{name}' unreachable — {detail}")
        };
        return true;
    }
    match ob.ui.as_mut() {
        Some(w) => {
            w.apply_probe(result);
            true
        }
        None => false,
    }
}

/// Run a wizard probe off-thread and deliver the result on the refresh
/// channel + waker pulse (the standard off-thread-producer contract).
pub(crate) fn spawn_probe(
    req: ProbeRequest,
    tx: UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let result = match req {
            ProbeRequest::Forge => ProbeResult::Forge(probe_forge()),
            ProbeRequest::Sandbox(chain) => ProbeResult::Sandbox(probe_sandbox(&chain)),
            ProbeRequest::Host { name, ssh } => probe_host(name, &ssh),
        };
        if tx.send(RefreshKind::Onboarding(Box::new(result))).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// `gh auth status`: spawn failure = not installed; non-zero = not
/// authenticated; success mines the "Logged in to …" line for the summary.
fn probe_forge() -> ForgeStatus {
    // off-loop: inside spawn_blocking
    #[expect(clippy::disallowed_methods)]
    let out = std::process::Command::new("gh")
        .args(["auth", "status"])
        .output();
    match out {
        Err(_) => ForgeStatus::NotInstalled,
        Ok(o) if o.status.success() => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let who = text
                .lines()
                .find(|l| l.contains("Logged in to"))
                .map(|l| l.trim().trim_start_matches('✓').trim().to_string())
                .unwrap_or_else(|| "logged in".into());
            ForgeStatus::Authenticated(who)
        }
        Ok(_) => ForgeStatus::NotAuthenticated,
    }
}

/// PATH presence per chain entry ("host"/"none" need no binary and are
/// always available).
fn probe_sandbox(chain: &[String]) -> Vec<(String, bool)> {
    use thegn_core::placement::{Placement, RuntimeProbe};
    chain
        .iter()
        .map(|name| {
            let ok = match thegn_core::sandbox::Backend::parse(name) {
                Some(thegn_core::sandbox::Backend::None) => true,
                Some(b) => Placement::Local.probe_runtime(b.binary()) == RuntimeProbe::Present,
                None => false,
            };
            (name.clone(), ok)
        })
        .collect()
}

/// Reachability of `user@box[:port]` via the control-plane `remote_home` probe
/// (BatchMode ssh; a password prompt fails fast instead of hanging).
fn probe_host(name: String, ssh: &str) -> ProbeResult {
    let (host, port) = match ssh.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().unwrap_or(22))
        }
        _ => (ssh.to_string(), 22),
    };
    let target = thegn_core::remote::SshTarget {
        host,
        port,
        forward_agent: false,
    };
    match thegn_core::remote::remote_home(&target) {
        Some(home) => ProbeResult::Host {
            name,
            ok: true,
            detail: format!("$HOME = {home}"),
        },
        None => ProbeResult::Host {
            name,
            ok: false,
            detail: "ssh probe failed (BatchMode; check keys/agent)".into(),
        },
    }
}
