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
    AgentInfo, Effects, ForgeStatus, OnboardingWizard, Outcome, ProbeRequest, ProbeResult, WriteOp,
};
use crate::panes::Panes;
use crate::session::Session;
use crate::welcome::{TourOutcome, WelcomeTour};
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

/// The loop's onboarding state, one local: the wizard slot, the pane it is
/// suspended behind (`gh auth login` / agent setup) as `(pane id, is_login)`,
/// and the full-screen welcome tour (first run / the palette). The tour and
/// the wizard are never open together: the tour hands off to the wizard.
#[derive(Default)]
pub(crate) struct OnboardingUi {
    pub ui: Option<OnboardingWizard>,
    pub wait: Option<(u32, bool)>,
    pub tour: Option<WelcomeTour>,
    /// Unix seconds when the current wizard/tour flow began; pane spawn failures
    /// recorded at or after it veto the success line.
    pub started_at: Option<i64>,
}

impl OnboardingUi {
    /// The tour or wizard owns keyboard/paste input (open and not suspended).
    pub(crate) fn active(&self) -> bool {
        (self.ui.is_some() || self.tour.is_some()) && self.wait.is_none()
    }

    /// Open the welcome tour (palette `setup-wizard`), replacing an open wizard.
    pub(crate) fn open_tour(&mut self, cfg: &Config) {
        self.ui = None;
        self.started_at = Some(thegn_core::util::now());
        self.tour = Some(WelcomeTour::new(cfg));
    }

    /// Route a bracketed paste to the wizard; `false` when it isn't active.
    /// The tour has no text fields: it swallows a paste so the text never
    /// lands in the pane hidden behind it.
    pub(crate) fn paste(&mut self, text: &str) -> bool {
        if self.wait.is_some() {
            return false;
        }
        if self.tour.is_some() {
            return true;
        }
        match self.ui.as_mut() {
            Some(w) => {
                w.handle_paste(text);
                true
            }
            None => false,
        }
    }

    /// Render the tour (full screen) or the wizard overlay, unless suspended
    /// behind the wizard's login/agent tab.
    pub(crate) fn render(&self, surface: &mut termwiz::surface::Surface, screen: Rect) {
        if self.wait.is_some() {
            return;
        }
        if let Some(t) = &self.tour {
            t.render(surface, screen);
        } else if let Some(w) = &self.ui {
            w.render(surface, screen);
        }
    }
}

/// Startup arming, replacing the run.rs first-launch block: open the welcome
/// tour on first run (no `ui_state` marker), or the setup wizard when
/// `thegn setup` requested it; otherwise apply a remembered keymap preset
/// (returns `true` → caller rebuilds the keymap) or arm the one-time keymap
/// picker. The picker is skipped while either is armed — both have their own
/// keymap choice.
pub(crate) fn startup(
    cfg: &mut Config,
    active_menu: &mut Option<crate::menu::MenuOverlay>,
    db: Option<&thegn_core::db::Db>,
) -> (OnboardingUi, bool) {
    // Headless/e2e launches set THEGN_SKIP_ONBOARDING to start straight in the
    // normal UI: a first-run wizard (or the keymap-preset picker below) is modal
    // and would otherwise swallow driven keystrokes. Skips both.
    if std::env::var_os("THEGN_SKIP_ONBOARDING").is_some() {
        return (OnboardingUi::default(), false);
    }
    let requested = crate::onboarding::setup_requested();
    let (first_run, remembered_preset) = startup_db_state(db);
    let wizard = requested.then(|| OnboardingWizard::new(cfg));
    let tour = (first_run && !requested).then(|| WelcomeTour::new(cfg));
    let mut rebuild = false;
    if !first_run && !requested && active_menu.is_none() {
        *active_menu = existing_user_wire_offer(db, cfg);
    }
    if cfg.keymap_preset.is_empty() || cfg.keymap_preset == "default" {
        match remembered_preset {
            Some(remembered) if remembered != "default" => {
                cfg.keymap_preset = remembered;
                rebuild = true;
            }
            Some(_) => {} // remembered "default" — no overlay, no picker
            None if wizard.is_none() && tour.is_none() => {
                *active_menu = Some(crate::menu::keymap_preset_menu())
            }
            None => {}
        }
    }
    (
        OnboardingUi {
            ui: wizard,
            wait: None,
            tour,
            started_at: Some(thegn_core::util::now()),
        },
        rebuild,
    )
}

/// `ui_state` key recording that the existing-user wiring offer was shown.
const WIRE_OFFER_KEY: &str = "agent_wire_offer";
/// The confirm tag of the wiring offer (arg: comma-joined agent kinds).
pub(crate) const WIRE_OFFER_TAG: &str = "agent-wire";

/// The one-time offer to an existing user (the tour is already done) whose
/// detected agents are not wired for thegn. Marks itself shown before it
/// returns, so it appears once whatever the answer; nothing is written until
/// the user confirms. `None` when already offered, no DB, or nothing to offer.
fn existing_user_wire_offer(
    db: Option<&thegn_core::db::Db>,
    cfg: &Config,
) -> Option<crate::menu::MenuOverlay> {
    let db = db?;
    if !matches!(db.get_ui_state("", WIRE_OFFER_KEY), Ok(None)) {
        return None;
    }
    let kinds = crate::cmd::mcp_proxy_cmd::unwired_detected_agents(cfg);
    let menu = wire_offer_menu(&kinds)?;
    if db.set_ui_state("", WIRE_OFFER_KEY, "1").is_err() {
        return None; // cannot remember it: never nag on every launch
    }
    Some(menu)
}

/// The confirm menu for wiring `kinds`; `None` for an empty list.
fn wire_offer_menu(kinds: &[String]) -> Option<crate::menu::MenuOverlay> {
    if kinds.is_empty() {
        return None;
    }
    Some(crate::menu::confirm_menu(
        "Set up your agents for thegn?",
        format!(
            "{} {} detected but not wired. Writes marked MCP entries and lifecycle hooks to \
             their settings; undo any time with `thegn mcp wire --remove`. Asked once.",
            kinds.join(" + "),
            if kinds.len() == 1 { "is" } else { "are" }
        ),
        WIRE_OFFER_TAG,
        kinds.join(","),
        false,
    ))
}

/// Run a confirmed wiring offer; `None` when `tag` is not the offer's.
pub(crate) fn apply_wire_offer(tag: &str, arg: &str) -> Option<String> {
    if tag != WIRE_OFFER_TAG {
        return None;
    }
    let kinds: Vec<String> = arg
        .split(',')
        .filter(|k| !k.is_empty())
        .map(String::from)
        .collect();
    Some(match crate::cmd::mcp_proxy_cmd::wire_kinds(&kinds) {
        Ok(lines) => format!("agents wired for thegn ({} writes)", lines.len()),
        Err(e) => format!("agent wiring failed: {e}"),
    })
}

/// Read startup markers from the already-open handle. A missing DB or a failed
/// marker read is not treated as first run, matching the broken-DB fallback.
fn startup_db_state(db: Option<&thegn_core::db::Db>) -> (bool, Option<String>) {
    let Some(db) = db else {
        return (false, None);
    };
    let first_run = matches!(
        db.get_ui_state("", crate::onboarding::UI_STATE_KEY),
        Ok(None)
    );
    let remembered_preset = db.get_ui_state("", "keymap_preset").ok().flatten();
    (first_run, remembered_preset)
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
    user_themes: &[thegn_core::theme_user::UserTheme],
    session: &mut Session,
    panes: &mut Panes,
    center: Rect,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) -> Applied {
    let mut applied = Applied::default();
    if ob.tour.is_some() {
        handle_tour_key(
            key,
            mods,
            ob,
            model,
            cfg,
            user_themes,
            refresh_tx,
            waker,
            &mut applied,
        );
        return applied;
    }
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
            user_themes,
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
                user_themes,
                session,
                panes,
                center,
                refresh_tx,
                waker,
                &mut applied,
            );
            ob.ui = None;
            // A dismissed wizard drops any live theme preview back to the saved
            // theme. A completed wizard already persisted `theme.preset` (the
            // live preview matches it, and the config watcher reconciles the
            // in-memory `cfg`), so leave the previewed palette in place.
            if !completed {
                crate::chrome::set_palette(onboarding_palette(cfg, &cfg.theme.preset, user_themes));
            }
            persist_done(completed);
            model.status = if completed {
                completion_status(
                    "setup complete — re-run any time with `thegn setup`",
                    ob.started_at,
                    &model.panel.notifications,
                )
            } else {
                "setup dismissed — re-run any time with `thegn setup`".into()
            };
        }
    }
    applied
}

/// The status line for a finished flow: `success` unless a pane failed to spawn
/// (an unread `process_failed` notification recorded since `started_at`), in
/// which case the failure text (real error + log path) replaces the success claim.
fn completion_status(
    success: &str,
    started_at: Option<i64>,
    notifications: &[thegn_core::notification::Notification],
) -> String {
    let Some(since) = started_at else {
        return success.to_string();
    };
    match notifications.iter().find(|n| {
        n.kind == thegn_core::notification::NotificationKind::ProcessFailed
            && !n.read
            && n.created_at_ms >= since
    }) {
        Some(n) => format!("setup finished, but a pane failed to start: {}", n.message),
        None => success.to_string(),
    }
}

/// Feed one key to the welcome tour: live theme preview, probe spawns, and on
/// close the marker, the palette revert (unless a theme was saved), the writes
/// and the optional hand-off to the setup wizard.
#[allow(clippy::too_many_arguments)] // mirrors handle_key_event's loop context
fn handle_tour_key(
    key: &KeyCode,
    mods: Modifiers,
    ob: &mut OnboardingUi,
    model: &mut FrameModel,
    cfg: &mut Config,
    user_themes: &[thegn_core::theme_user::UserTheme],
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    applied: &mut Applied,
) {
    let Some(t) = ob.tour.as_mut() else {
        return;
    };
    match t.handle_key(key, mods) {
        TourOutcome::Pending => {}
        TourOutcome::Do(fx) => {
            if let Some(name) = fx.preview_theme {
                crate::chrome::set_palette(onboarding_palette(cfg, &name, user_themes));
            }
            for req in fx.probes {
                spawn_probe(req, refresh_tx.clone(), waker.clone());
            }
        }
        TourOutcome::Close {
            completed,
            writes,
            open_setup,
        } => {
            ob.tour = None;
            persist_done(completed);
            let saves_theme = writes
                .iter()
                .any(|w| matches!(w, WriteOp::Set { key, .. } if key == "theme.preset"));
            // Unsaved preview → back to the configured palette. A saved theme
            // keeps the preview; the config watcher reconciles `cfg`.
            if !saves_theme {
                crate::chrome::set_palette(onboarding_palette(cfg, &cfg.theme.preset, user_themes));
            }
            let n = writes.len();
            model.status = match (completed, n) {
                (false, _) => {
                    "welcome tour closed, nothing saved — reopen it from the palette".into()
                }
                (true, 0) => {
                    "welcome tour done, nothing to save — reopen it from the palette".into()
                }
                (true, _) => format!(
                    "welcome tour: saving {n} setting{}…",
                    if n == 1 { "" } else { "s" }
                ),
            };
            save_tour_writes(writes, model, cfg, refresh_tx, waker, applied);
            if open_setup {
                ob.started_at = Some(thegn_core::util::now());
                ob.ui = Some(OnboardingWizard::new(cfg));
            }
        }
    }
}

/// Apply the tour's writes: the keymap preset on the loop (a `ui_state` write
/// through `db_task`, like the wizard), every config-file write off the loop
/// in one `spawn_blocking`, reported back as a toast. Each op is one
/// `config_write` call, so unrelated keys and comments survive.
fn save_tour_writes(
    writes: Vec<WriteOp>,
    model: &mut FrameModel,
    cfg: &mut Config,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    applied: &mut Applied,
) {
    let mut file_ops = Vec::new();
    for op in writes {
        match op {
            WriteOp::KeymapPreset(preset) => {
                model.status = super::apply_keymap_preset(&preset, cfg);
                applied.keymap_changed = true;
            }
            other => file_ops.push(other),
        }
    }
    if file_ops.is_empty() {
        return;
    }
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        // off-loop: each config_write call locks, reads and rewrites the file
        let path = Config::path();
        let n = file_ops.len();
        let mut errs: Vec<String> = Vec::new();
        for op in file_ops {
            let r = match op {
                WriteOp::Set { key, value } => {
                    thegn_core::config_write::set_key(&path, &key, &value)
                }
                WriteOp::PromoteAgent {
                    name,
                    command,
                    keep,
                } => thegn_core::config_write::promote_agent(&path, &name, &command, &keep),
                WriteOp::WireAgents { kinds } => {
                    crate::cmd::mcp_proxy_cmd::wire_kinds(&kinds).map(|_| ())
                }
                other => Err(anyhow::anyhow!("the welcome tour cannot write {other:?}")),
            };
            if let Err(e) = r {
                errs.push(e.to_string());
            }
        }
        let (message, priority) = if errs.is_empty() {
            (
                format!(
                    "Saved {n} setting{} to {}",
                    if n == 1 { "" } else { "s" },
                    path.display()
                ),
                thegn_core::notification::Priority::Notice,
            )
        } else {
            (
                format!("Welcome tour: save failed — {}", errs.join("; ")),
                thegn_core::notification::Priority::Alert,
            )
        };
        if tx.send(RefreshKind::Toast { message, priority }).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Resolve a token to the value written into config: an empty token stays
/// empty, an already-`ref` token is passed through verbatim (legacy
/// materialization), and a raw token is stored via the secret backend and
/// replaced by its SecretRef (config never holds raw tokens).
fn resolve_token(secret_name: &str, token: &str, is_ref: bool) -> anyhow::Result<String> {
    let t = token.trim();
    if t.is_empty() {
        Ok(String::new())
    } else if is_ref {
        Ok(t.to_string())
    } else {
        crate::secret::store(secret_name, t)
    }
}

/// Mark the wizard as seen so it stops auto-opening.
fn persist_done(completed: bool) {
    crate::db_task::persist(move |db| {
        if let Err(error) = mark_done(db, completed) {
            tracing::debug!(target: "thegn::onboarding", %error, "could not persist onboarding completion");
        }
    });
}

/// The `ui_state` value recording how the first-run surface closed. Any value
/// retires first run — that is the whole contract — but spelling the outcome
/// out keeps "did they finish, or skip off the top?" answerable later.
fn done_marker(completed: bool) -> &'static str {
    if completed { "completed" } else { "skipped" }
}

/// Write that marker into an open handle: the exact shape the DB task closes
/// over, so a test can round-trip it against [`startup_db_state`].
fn mark_done(db: &thegn_core::db::Db, completed: bool) -> anyhow::Result<()> {
    db.set_ui_state("", crate::onboarding::UI_STATE_KEY, done_marker(completed))?;
    // The tour and wizard own the keymap step but write a preset only when it
    // changes. A run that keeps the default would leave no remembered choice, so
    // the one-time keymap picker would return on the next launch (the marker
    // above already retired first run). Record the default unless a choice is
    // already remembered; an explicit pick is persisted by `apply_keymap_preset`.
    if db.get_ui_state("", "keymap_preset")?.is_none() {
        db.set_ui_state("", "keymap_preset", "default")?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_effects(
    effects: Effects,
    wait: &mut Option<(u32, bool)>,
    model: &mut FrameModel,
    cfg: &mut Config,
    user_themes: &[thegn_core::theme_user::UserTheme],
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
            WriteOp::Host { name, ssh } => {
                thegn_core::config_write::upsert_host(&path, &name, &ssh)
            }
            WriteOp::KeymapPreset(preset) => {
                model.status = super::apply_keymap_preset(&preset, cfg);
                applied.keymap_changed = true;
                Ok(())
            }
            WriteOp::UpsertIssueAccount {
                name,
                provider,
                token,
                token_is_ref,
                team_id,
                workspace_slug,
                base_url,
                email,
                project_key,
            } => resolve_token(&format!("issues-{name}"), &token, token_is_ref).and_then(|tref| {
                thegn_core::config_write::upsert_issue_account(
                    &path,
                    &name,
                    &provider,
                    &tref,
                    &team_id,
                    &workspace_slug,
                    &base_url,
                    &email,
                    &project_key,
                )
            }),
            WriteOp::UpsertForge {
                name,
                kind,
                host,
                token,
                token_is_ref,
            } => resolve_token(&format!("forge-{name}"), &token, token_is_ref).and_then(|tref| {
                thegn_core::config_write::upsert_forge(&path, &name, &kind, &host, &tref)
            }),
            WriteOp::PromoteAgent {
                name,
                command,
                keep,
            } => thegn_core::config_write::promote_agent(&path, &name, &command, &keep),
            WriteOp::WireAgents { kinds } => {
                crate::cmd::mcp_proxy_cmd::wire_kinds(&kinds).map(|_| ())
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
    // Live theme preview: apply the previewed preset to the runtime palette
    // (colors land on the next repaint; the caller sets `dirty`). Not persisted
    // — the actual `theme.preset` write rides `leave_writes` on step advance,
    // and a dismissed wizard reverts through the same loaded-catalog resolver.
    if let Some(name) = effects.preview_theme {
        crate::chrome::set_palette(onboarding_palette(cfg, &name, user_themes));
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
}

fn onboarding_palette(
    cfg: &Config,
    preset: &str,
    user_themes: &[thegn_core::theme_user::UserTheme],
) -> thegn_core::theme::Palette {
    cfg.palette_with_user_themes(preset, user_themes)
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
    if let Some(t) = ob.tour.as_mut() {
        t.apply_probe(result);
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
            ProbeRequest::Agents => ProbeResult::Agents(probe_agents()),
        };
        if tx.send(RefreshKind::Onboarding(Box::new(result))).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// The forge's identity probe (`Forge::whoami` — one probe for onboarding,
/// doctor and the PR queue): not installed / not authenticated / the login.
fn probe_forge() -> ForgeStatus {
    // off-loop: inside spawn_blocking
    let forges = crate::forge_handle::get();
    let loc = thegn_core::remote::GitLoc::Local(
        std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
    );
    match forges.default_forge().whoami(&loc) {
        Ok(login) => ForgeStatus::Authenticated(format!("logged in as {login}")),
        Err(thegn_core::forge::ForgeError::NotInstalled) => ForgeStatus::NotInstalled,
        Err(_) => ForgeStatus::NotAuthenticated,
    }
}

/// Every registered coding-agent harness and where its interactive program is
/// on `PATH` (the welcome tour's Agents chapter). A stat per `PATH` entry per
/// harness, so off the loop.
fn probe_agents() -> Vec<AgentInfo> {
    // off-loop: inside spawn_blocking
    let unwired = crate::cmd::mcp_proxy_cmd::unwired_detected_agents(&Config::default());
    thegn_core::harness::HARNESSES
        .iter()
        .map(|h| {
            let command = h.interactive_command().to_string();
            let program = command.split_whitespace().next().unwrap_or("").to_string();
            let id = h.id().to_string();
            AgentInfo {
                unwired: unwired.contains(&id),
                id,
                name: h.display_name().to_string(),
                path: thegn_core::util::which_path(&program),
                command,
            }
        })
        .collect()
}

/// PATH presence per chain entry ("host"/"none" need no binary and are
/// always available).
/// The wizard's view of the sandbox chain.
///
/// Delegates to the same `support_report` `thegn doctor` renders and the same
/// probes selection uses, so the wizard cannot disagree with what actually
/// happens at pane spawn. It previously called `probe_runtime` directly — a bare
/// PATH check that showed a green dot for a stopped Docker daemon, which is
/// precisely the state that then fails every pane.
fn probe_sandbox(chain: &[String]) -> Vec<thegn_core::sandbox_support::BackendSupport> {
    thegn_core::sandbox_support::support_report(
        chain,
        &thegn_core::placement::Placement::Local,
        None,
    )
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
    let target = thegn_core::remote::SshTarget::plain(host, port, false);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissed_onboarding_restores_a_loaded_user_theme() {
        let mut cfg = Config::default();
        cfg.theme.preset = "local-paper".into();
        let expected = cfg.palette_with_preset("light");
        let user = thegn_core::theme_user::UserTheme::from_palette("local-paper", &expected);

        let restored = onboarding_palette(&cfg, &cfg.theme.preset, &[user]);

        assert_eq!(restored.bg0, expected.bg0);
        assert_eq!(restored.text, expected.text);
        assert_ne!(restored.bg0, cfg.palette().bg0);
    }

    #[test]
    fn startup_db_state_reads_the_passed_handle() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state("", crate::onboarding::UI_STATE_KEY, "1")
            .unwrap();
        db.set_ui_state("", "keymap_preset", "vim").unwrap();
        assert_eq!(startup_db_state(Some(&db)), (false, Some("vim".into())));

        let empty = thegn_core::db::Db::open_memory().unwrap();
        assert_eq!(startup_db_state(Some(&empty)), (true, None));
        assert_eq!(startup_db_state(None), (false, None));
    }

    #[test]
    fn wire_offer_is_one_time_and_only_for_something_to_offer() {
        assert!(wire_offer_menu(&[]).is_none());
        let menu = wire_offer_menu(&["claude".to_string(), "codex".to_string()]).unwrap();
        assert!(matches!(
            menu.items().first().map(|i| &i.choice),
            Some(crate::menu::MenuChoice::Confirm { tag: WIRE_OFFER_TAG, arg })
                if arg == "claude,codex"
        ));
        assert_eq!(apply_wire_offer("other", "claude"), None);
        // Already offered, or no DB: never offered again.
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state("", WIRE_OFFER_KEY, "1").unwrap();
        assert!(existing_user_wire_offer(Some(&db), &Config::default()).is_none());
        assert!(existing_user_wire_offer(None, &Config::default()).is_none());
    }

    /// THE-2610: the tour is the first-run default, and skipping it (Esc / the
    /// skip control) records a marker that stops it auto-opening again.
    #[test]
    fn first_run_opens_the_tour_and_a_skip_never_reopens_it() {
        // `startup` honours THEGN_SKIP_ONBOARDING (the e2e recipes set it) and
        // a unit-test run must not, or this asserts on the wrong surface.
        assert!(
            std::env::var_os("THEGN_SKIP_ONBOARDING").is_none(),
            "unset THEGN_SKIP_ONBOARDING: it suppresses the first-run surface"
        );
        let db = thegn_core::db::Db::open_memory().unwrap();
        let mut cfg = Config::default();
        let mut menu: Option<crate::menu::MenuOverlay> = None;

        let (mut ui, rebuild) = startup(&mut cfg, &mut menu, Some(&db));
        assert!(!rebuild);
        assert!(ui.tour.is_some(), "first run auto-opens the welcome tour");

        // Esc is the skip control's chord: it closes without writing a setting.
        let mut tour = ui.tour.take().expect("the tour was armed");
        assert!(matches!(
            tour.handle_key(&KeyCode::Escape, Modifiers::NONE),
            TourOutcome::Close {
                completed: false,
                ..
            }
        ));

        // The marker the close path persists is the one that retires first run.
        mark_done(&db, false).unwrap();
        assert_eq!(
            db.get_ui_state("", crate::onboarding::UI_STATE_KEY)
                .unwrap()
                .as_deref(),
            Some("skipped")
        );
        // A skip that kept the default still remembers it, so the keymap picker
        // does not take the tour's place on the next launch.
        assert_eq!(
            startup_db_state(Some(&db)),
            (false, Some("default".to_string()))
        );

        let (ui, _) = startup(&mut cfg, &mut menu, Some(&db));
        assert!(ui.tour.is_none(), "a skipped tour never auto-opens again");
        assert!(
            menu.is_none(),
            "a remembered default opens no keymap picker"
        );
    }

    /// A tour that saved an explicit preset keeps it: closing the tour must not
    /// overwrite a remembered choice with the default.
    #[test]
    fn closing_the_tour_keeps_an_explicit_remembered_preset() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.set_ui_state("", "keymap_preset", "vscode").unwrap();
        mark_done(&db, true).unwrap();
        assert_eq!(
            startup_db_state(Some(&db)),
            (false, Some("vscode".to_string()))
        );
    }

    #[test]
    fn completion_status_reports_a_recorded_pane_failure_not_success() {
        use thegn_core::notification::{Notification, NotificationKind};
        let fail = |at: i64, read: bool| Notification {
            id: 1,
            kind: NotificationKind::ProcessFailed,
            source_ref: "pane".into(),
            message: "Shell failed to launch: boom. Log: /tmp/thegn.log".into(),
            created_at_ms: at,
            read,
            worktree_path: String::new(),
        };
        let ok = "setup complete";
        assert_eq!(completion_status(ok, Some(100), &[]), ok);
        assert_eq!(completion_status(ok, Some(100), &[fail(50, false)]), ok);
        assert_eq!(completion_status(ok, Some(100), &[fail(150, true)]), ok);
        assert_eq!(completion_status(ok, None, &[fail(150, false)]), ok);
        let shown = completion_status(ok, Some(100), &[fail(150, false)]);
        assert!(
            shown.contains("boom") && shown.contains("/tmp/thegn.log"),
            "{shown}"
        );
        assert!(!shown.contains("setup complete"));
    }
}
