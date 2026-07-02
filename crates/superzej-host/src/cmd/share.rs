//! `superzej share <action>` — expose a worktree-local port at a public URL.
//!
//! The non-interactive surface over [`superzej_svc::share`]. `start` runs in the
//! foreground (like `bore local`/`ngrok` themselves): it spawns the tunnel
//! client, prints the URL, records the share in the DB so the panel/restore can
//! see it, and blocks until interrupted. `list` reads the persisted shares;
//! `stop` removes a record.
//!
//! The in-process host supervisor (live respawn on restart, badge, panel) builds
//! on the same `[share]` config + `superzej_svc::share` seam this CLI uses.

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_core::share::build_share_spec;
use superzej_svc::share::{self, ShareLaunch, ShareProvider};

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Expose a worktree-local port. Runs in the foreground until interrupted.
    Start {
        /// The local TCP port to expose (e.g. a dev server on 3000).
        port: u16,
        #[arg(long)]
        worktree: Option<String>,
        /// Reach intent: public | team | peer (maps to `[share] <reach>`).
        /// Omitted ⇒ the single `[share] provider`.
        #[arg(long)]
        reach: Option<String>,
    },
    /// List shares recorded in the DB.
    List,
    /// Remove a recorded share for a worktree port.
    Stop {
        port: u16,
        #[arg(long)]
        worktree: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Start {
            port,
            worktree,
            reach,
        } => start(cfg, port, worktree, reach),
        Action::List => list(),
        Action::Stop { port, worktree } => stop(port, worktree),
    }
}

fn start(cfg: &Config, port: u16, worktree: Option<String>, reach: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree).to_string_lossy().into_owned();
    let label = superzej_core::share::label_for(&wt);
    let reach = match reach.as_deref() {
        Some(s) => match superzej_core::config::ShareReach::from_str_validated(s) {
            Ok(r) => Some(r),
            Err(e) => {
                outln!("share: {e}");
                return Ok(());
            }
        },
        None => None,
    };
    let Some(spec) = build_share_spec(&cfg.share, &label, port, reach) else {
        outln!("share: disabled (set [share] provider, or that reach)");
        return Ok(());
    };
    if spec.visibility == superzej_core::config::ShareVisibility::Public && !cfg.share.allow_public
    {
        outln!("share: public sharing is disabled (set [share] allow_public = true)");
        return Ok(());
    }
    let provider = share::for_provider(&spec);
    let kind = provider.kind().to_string();
    let launch = match provider.launch() {
        Ok(l) => l,
        Err(e) => return on_error_exit(spec.on_error, e),
    };
    let db = Db::open().ok();

    match launch {
        ShareLaunch::Process(plan) => {
            let statedir = share::share_state_dir(&wt, port);
            let running = match share::start(&plan, &statedir, spec.ready_timeout) {
                Ok(r) => r,
                Err(e) => return on_error_exit(spec.on_error, e),
            };
            outln!("share: 127.0.0.1:{port} → {}", running.public_url);
            outln!("share: press Ctrl-C to stop");
            if let Some(db) = &db {
                let _ = db.upsert_share(&wt, port, &kind, Some(&running.public_url), "up");
            }
            // Block until the client exits (Ctrl-C tears down the group).
            let share::RunningShare { mut child, .. } = running;
            // CLI path: `szhost share` runs the tunnel in the foreground by design.
            #[expect(clippy::disallowed_methods)]
            let _ = child.wait();
            if let Some(db) = &db {
                let _ = db.delete_share(&wt, port);
            }
        }
        ShareLaunch::SidecarServe(serve) => {
            // tailscale serve persists in the VPN sidecar (--bg); no process to
            // hold. Report the URL and exit; `share stop` removes the record.
            let sidecar = superzej_core::sandbox::vpn_sidecar_name(
                &superzej_core::sandbox::container_name(&wt),
            );
            let url = match share::serve_up(&sidecar, &serve) {
                Ok(u) => u,
                Err(e) => return on_error_exit(spec.on_error, e),
            };
            outln!("share: 127.0.0.1:{port} → {url}");
            outln!("share: serve persists in the tailnet; `share stop {port}` to remove");
            if let Some(db) = &db {
                let _ = db.upsert_share(&wt, port, &kind, Some(&url), "up");
            }
        }
    }
    Ok(())
}

/// `fail` surfaces the error (non-zero exit); `warn` notes it and exits 0.
fn on_error_exit(on_error: superzej_core::config::ShareOnError, e: anyhow::Error) -> Result<()> {
    match on_error {
        superzej_core::config::ShareOnError::Warn => {
            outln!("share: {e} (continuing without a share)");
            Ok(())
        }
        superzej_core::config::ShareOnError::Fail => Err(e),
    }
}

fn list() -> Result<()> {
    let Ok(db) = Db::open() else {
        outln!("share: could not open the state DB");
        return Ok(());
    };
    let rows = db.list_shares().unwrap_or_default();
    if rows.is_empty() {
        outln!("no shares");
        return Ok(());
    }
    for r in &rows {
        outln!(
            "{:<6} {:<6} {:<10} {}",
            r.local_port,
            r.provider,
            r.state,
            r.public_url.as_deref().unwrap_or("—"),
        );
    }
    Ok(())
}

fn stop(port: u16, worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree).to_string_lossy().into_owned();
    let Ok(db) = Db::open() else {
        outln!("share: could not open the state DB");
        return Ok(());
    };
    db.delete_share(&wt, port)?;
    outln!("share: removed record for port {port}");
    outln!("share: a foreground `share start` keeps running — interrupt it directly");
    Ok(())
}
