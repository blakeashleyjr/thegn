//! The new-worktree wizard's launch path, extracted from `run.rs` (which is
//! pinned by the file-size ratchet): open the pure form instantly, decorate
//! its host rows with `[host.*]` readiness badges, and kick the speculative
//! create worker off-loop.

use superzej_core::store::HostStore;
use termwiz::terminal::TerminalWaker;
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
    template: Option<&superzej_core::config::WorktreeTemplate>,
    cfg: &superzej_core::config::Config,
    create_gen: &mut u64,
    create_tx: &tokio_mpsc::UnboundedSender<wizard::CreateEvent>,
    waker: &TerminalWaker,
    creating: &mut Option<wizard::CreationProgress>,
    wizard_cmd_tx: &mut Option<std::sync::mpsc::Sender<wizard::WizardCmd>>,
    wizard_ui: &mut Option<wizard::NewWorktreeWizard>,
    model: &mut FrameModel,
) {
    if wizard_ui.is_some() || creating.is_some() {
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
    *creating = Some(wizard::CreationProgress::new(*create_gen, w.candidate()));
    *wizard_cmd_tx = Some(cmd_tx);
    *wizard_ui = Some(w);
}

/// Parse + persist an add-host input ("user@host[:port]" or
/// "dumbpipe:<ticket> <user>"), then merge the new def into the LIVE config so
/// the re-opened wizard lists it immediately. Returns the host name.
pub(crate) fn add_host_from_input(
    text: &str,
    cfg: &mut superzej_core::config::Config,
) -> Result<String, String> {
    let mut parts = text.split_whitespace();
    let target = parts.next().ok_or("empty input")?;
    let iroh_user = parts.next();
    let (name, hc) = superzej_core::host_config::parse_host_target(target, iroh_user)?;
    if cfg.host.contains_key(&name)
        && !superzej_core::db::Db::open()
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
    let db = superzej_core::db::Db::open().map_err(|e| format!("state db: {e}"))?;
    db.put_host_def(&name, &hc, now)
        .map_err(|e| e.to_string())?;
    superzej_core::host_config::merge_db_hosts(cfg);
    Ok(name)
}
