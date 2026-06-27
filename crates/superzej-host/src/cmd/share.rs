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
use superzej_svc::share::{self, ShareProvider};

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Expose a worktree-local port. Runs in the foreground until interrupted.
    Start {
        /// The local TCP port to expose (e.g. a dev server on 3000).
        port: u16,
        #[arg(long)]
        worktree: Option<String>,
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
        Action::Start { port, worktree } => start(cfg, port, worktree),
        Action::List => list(),
        Action::Stop { port, worktree } => stop(port, worktree),
    }
}

fn start(cfg: &Config, port: u16, worktree: Option<String>) -> Result<()> {
    let Some(spec) = build_share_spec(&cfg.share, port) else {
        outln!("share: disabled (set [share] provider = \"bore\")");
        return Ok(());
    };
    let provider = share::for_provider(&spec);
    let kind = provider.kind().to_string();
    let plan = provider.plan()?;

    let wt = resolve_worktree(worktree).to_string_lossy().into_owned();

    let running = match share::start(&plan, spec.ready_timeout) {
        Ok(r) => r,
        Err(e) => {
            // Mirror VPN's `on_error`: `fail` surfaces, `warn` notes and exits 0.
            match spec.on_error {
                superzej_core::config::ShareOnError::Warn => {
                    outln!("share: {e} (continuing without a share)");
                    return Ok(());
                }
                superzej_core::config::ShareOnError::Fail => return Err(e),
            }
        }
    };

    outln!("share: 127.0.0.1:{port} → {}", running.public_url);
    outln!("share: press Ctrl-C to stop");

    let db = Db::open().ok();
    if let Some(db) = &db {
        let _ = db.upsert_share(&wt, port, &kind, Some(&running.public_url), "up");
    }

    // Block until the tunnel client exits (Ctrl-C propagates to the process
    // group and tears down both). Best-effort cleanup of the DB record after.
    let share::RunningShare { mut child, .. } = running;
    let _ = child.wait();
    if let Some(db) = &db {
        let _ = db.delete_share(&wt, port);
    }
    Ok(())
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
