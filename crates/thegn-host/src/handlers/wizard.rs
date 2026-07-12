//! The new-worktree wizard's launch path, extracted from `run.rs` (which is
//! pinned by the file-size ratchet): open the pure form instantly, decorate
//! its host rows with `[host.*]` readiness badges, and kick the speculative
//! create worker off-loop.

use termwiz::terminal::TerminalWaker;
use thegn_core::store::HostStore;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use crate::chrome::FrameModel;
use crate::wizard;

/// Launch the new-worktree wizard + speculative-create worker against `root`.
/// `base_override` forks from a chosen branch (item 52); `None` uses the
/// configured/auto-resolved base. Shared by the `NewWorktree` action and the
/// sidebar "fork worktree" outcome so both go through one launch path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn begin_worktree_wizard(
    root: std::path::PathBuf,
    base_override: Option<String>,
    template: Option<&thegn_core::config::WorktreeTemplate>,
    cfg: &thegn_core::config::Config,
    create_gen: &mut u64,
    create_tx: &tokio_mpsc::UnboundedSender<wizard::CreateEvent>,
    waker: &TerminalWaker,
    inflight: &mut crate::handlers::creating::InFlight,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<wizard::WizardCmd>>,
    wizard_ui: &mut Option<wizard::NewWorktreeWizard>,
    model: &mut FrameModel,
) {
    // Only the modal wizard form is single-flight — a committed background
    // creation (its slow remote sandbox provisioning) no longer blocks opening
    // a new wizard, so worktree creation is concurrent.
    if wizard_ui.is_some() {
        model.status = "worktree creation already in progress".into();
        return;
    }
    // Open the wizard instantly (pure prefill) and start the worker, which
    // speculatively creates the worktree under the candidate name while the
    // user reads the form.
    *create_gen += 1;
    let mut w = wizard::NewWorktreeWizard::new(root.clone(), cfg);
    // Host-readiness badges on the env rows (hosts-as-resources): built from
    // the already-hydrated panel snapshot — no I/O on the loop.
    w.set_host_badges(crate::host_ui::wizard_host_badges(cfg, &model.panel.hosts));
    // A template (item 54) seeds the prefix + sandbox/agent selection; its base
    // branch flows through `base_override` below.
    if let Some(tmpl) = template {
        w.apply_template(tmpl);
    }
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let ctx = wizard::WorkerCtx {
        cfg: cfg.clone(),
        repo_root: root,
        candidate: w.candidate(),
        generation: *create_gen,
        db_path: None,
        base_override,
    };
    let tx = create_tx.clone();
    let wk = waker.clone();
    task::spawn_blocking(move || {
        wizard::run_worker(ctx, cmd_rx, tx, move || {
            let _ = wk.wake();
        });
    });
    inflight
        .progress
        .insert(*create_gen, wizard::CreationProgress::new(w.candidate()));
    inflight.wizard_gen = Some(*create_gen);
    *wizard_cmd_tx = Some(cmd_tx);
    *wizard_ui = Some(w);
}

/// Route a wizard outcome that abandons worktree creation to go set up its
/// prerequisite first: `AddHost` opens the add-host input, `SetupEnv` opens the
/// env wizard pre-seeded to the chosen provider kind. Both cancel the
/// speculative-create worker; the user re-runs new-worktree once the host/env
/// exists. Extracted from `run.rs` (pinned by the file-size ratchet).
#[allow(clippy::too_many_arguments)]
pub(crate) fn leave_for_setup(
    outcome: wizard::WizardOutcome,
    cfg: &thegn_core::config::Config,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<wizard::WizardCmd>>,
    wizard_ui: &mut Option<wizard::NewWorktreeWizard>,
    inflight: &mut crate::handlers::creating::InFlight,
    host_input: &mut Option<(crate::menu::InputOverlay, crate::run::HostInputKind)>,
    env_wizard_ui: &mut Option<crate::env_wizard::EnvWizard>,
    model: &mut FrameModel,
) {
    let root = wizard_ui.as_ref().map(|w| w.root().clone());
    if let Some(tx) = wizard_cmd_tx.take() {
        let _ = tx.send(wizard::WizardCmd::Cancel);
    }
    *wizard_ui = None;
    // Drop only THIS wizard's speculative creation; committed background
    // creations keep their own generation and run to completion.
    if let Some(g) = inflight.wizard_gen.take() {
        inflight.progress.remove(&g);
    }
    match outcome {
        wizard::WizardOutcome::AddHost => {
            let Some(repo_root) = root else { return };
            *host_input = Some((
                crate::menu::InputOverlay::new(
                    "add host — user@host[:port], or dumbpipe:<ticket> <user>",
                    "",
                ),
                crate::run::HostInputKind::NewHost { repo_root },
            ));
        }
        wizard::WizardOutcome::SetupEnv(kind) => {
            *env_wizard_ui = Some(crate::env_wizard::EnvWizard::with_kind(cfg, &kind));
            model.status =
                format!("set up {kind}: create the environment, then rerun new worktree");
        }
        _ => {}
    }
}

/// Parse + persist an add-host input (`user@host[:port]` or
/// `dumbpipe:<ticket> <user>`), then merge the new def into the LIVE config so
/// the re-opened wizard lists it immediately. Returns the host name.
pub(crate) fn add_host_from_input(
    text: &str,
    cfg: &mut thegn_core::config::Config,
) -> Result<String, String> {
    let mut parts = text.split_whitespace();
    let target = parts.next().ok_or("empty input")?;
    let iroh_user = parts.next();
    let (name, hc) = thegn_core::host_config::parse_host_target(target, iroh_user)?;
    if cfg.host.contains_key(&name)
        && !thegn_core::db::Db::open()
            .ok()
            .and_then(|db| db.host_defs().ok())
            .is_some_and(|d| d.iter().any(|(n, _)| n == &name))
    {
        return Err(format!("[host.{name}] already exists in config.toml"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let db = thegn_core::db::Db::open().map_err(|e| format!("state db: {e}"))?;
    db.put_host_def(&name, &hc, now)
        .map_err(|e| e.to_string())?;
    thegn_core::host_config::merge_db_hosts(cfg);
    Ok(name)
}
