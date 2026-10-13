//! `thegn share <action>` — expose a worktree-local port at a public URL.
//!
//! The non-interactive surface over [`thegn_svc::share`]. `start` runs in the
//! foreground (like `bore local`/`ngrok` themselves): it spawns the tunnel
//! client, prints the URL, records the share in the DB so the panel/restore can
//! see it, and blocks until interrupted. `list` reads the persisted shares;
//! `stop` removes a record.
//!
//! The in-process host supervisor (live respawn on restart, badge, panel) builds
//! on the same `[share]` config + `thegn_svc::share` seam this CLI uses.

use anyhow::{Context, Result, bail};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::share::build_share_spec;
use thegn_core::store::WorktreeAuxStore;
use thegn_svc::share::{self, ShareLaunch, ShareProvider};

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Expose a worktree-local port. Runs in the foreground until interrupted.
    Start {
        /// The local TCP port to expose (e.g. a dev server on 3000).
        port: u16,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Reach intent: public | team | peer (maps to `[share] <reach>`).
        /// Omitted ⇒ the single `[share] provider`.
        #[arg(long)]
        reach: Option<String>,
    },
    /// List shares recorded in the DB.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Remove a recorded share for a worktree port.
    Stop {
        port: u16,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Start {
            port,
            target,
            reach,
        } => start(cfg, port, target.worktree, reach),
        Action::List { json } => list(json),
        Action::Stop { port, target } => stop(port, target.worktree),
    }
}

fn start(cfg: &Config, port: u16, worktree: Option<String>, reach: Option<String>) -> Result<()> {
    let wt_path = resolve_worktree(worktree);
    let wt = wt_path.to_string_lossy().into_owned();
    let label = thegn_core::share::label_for(&wt);
    let reach = match reach.as_deref() {
        Some(s) => match thegn_core::config::ShareReach::from_str_validated(s) {
            Ok(r) => Some(r),
            // Refusal must exit non-zero so scripts/CI see the misuse.
            Err(e) => bail!("share: {e}"),
        },
        None => None,
    };
    let Some(spec) = build_share_spec(&cfg.share, &label, port, reach) else {
        // Refusal must exit non-zero so scripts/CI see the misuse.
        bail!("share: disabled (set [share] provider, or that reach)");
    };
    if spec.visibility == thegn_core::config::ShareVisibility::Public && !cfg.share.allow_public {
        // Refusal must exit non-zero so scripts/CI see the misuse.
        bail!("share: public sharing is disabled (set [share] allow_public = true)");
    }
    let provider = share::for_provider(&spec);
    let kind = provider.kind().to_string();
    let launch = match provider.launch() {
        Ok(l) => l,
        Err(e) => return on_error_exit(spec.on_error, e),
    };
    let db = Db::open().ok(); // best-effort: cache: only the share state rows read/write the DB; the launch errors above exit loudly
    let generation = cli_generation()?;

    match launch {
        ShareLaunch::Process(plan) => {
            let statedir = match share::share_state_dir(&wt_path, &kind, port, generation) {
                Ok(path) => path,
                Err(e) => return on_error_exit(spec.on_error, e),
            };
            let running = match share::start(&plan, &statedir, spec.ready_timeout) {
                Ok(r) => r,
                Err(e) => return on_error_exit(spec.on_error, e),
            };
            let endpoint = display_endpoint(&running.public_url, &kind);
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&endpoint);
            outln!("share: 127.0.0.1:{port} → {}", display_endpoint);
            outln!("share: press Ctrl-C to stop");
            if let Some(db) = &db {
                let url = running.public_url.canonical();
                if let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up")
                {
                    tracing::debug!(target: "thegn::db", %error, "share cache upsert failed");
                }
            }
            // Block until the client exits (Ctrl-C tears down the group).
            // CLI path: `thegn share` runs the tunnel in the foreground by design.
            running.wait();
            if let Some(db) = &db
                && let Err(error) = db.delete_share_generation(&wt, port, generation)
            {
                tracing::debug!(target: "thegn::db", %error, "share cache delete failed");
            }
        }
        ShareLaunch::SidecarServe(serve) => {
            // tailscale serve persists in the VPN sidecar (--bg); no process to
            // hold. Report the URL and exit; `share stop` removes the record.
            // Profile-aware — the sidecar rides the container's real name.
            let sidecar = thegn_core::sandbox::vpn_sidecar_name(
                &thegn_core::sandbox::container_name_with_profile(
                    &wt,
                    Some(&thegn_core::profile::name()),
                ),
            );
            let url = match share::serve_up(&sidecar, &serve) {
                Ok(u) => u,
                Err(e) => return on_error_exit(spec.on_error, e),
            };
            let url = url.canonical();
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&url);
            outln!("share: 127.0.0.1:{port} → {display_endpoint}");
            outln!("share: serve persists in the tailnet; `share stop {port}` to remove");
            if let Some(db) = &db
                && let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up")
            {
                tracing::debug!(target: "thegn::db", %error, "share cache upsert failed");
            }
        }
    }
    Ok(())
}

fn display_endpoint(endpoint: &thegn_svc::share::PublicEndpoint, provider: &str) -> String {
    if provider == "iroh" && endpoint.kind() == thegn_svc::share::PublicEndpointKind::Ticket {
        format!("dumbpipe connect-tcp {}", endpoint.canonical())
    } else {
        endpoint.canonical()
    }
}

fn cli_generation() -> Result<u64> {
    let nanos = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("share: system clock is before the Unix epoch")?
            .as_nanos(),
    )
    .context("share: generation timestamp is out of range")?;
    Ok(nanos ^ (u64::from(std::process::id()).rotate_left(17)))
}

/// `fail` surfaces the error (non-zero exit); `warn` notes it and exits 0.
fn on_error_exit(on_error: thegn_core::config::ShareOnError, e: anyhow::Error) -> Result<()> {
    match on_error {
        thegn_core::config::ShareOnError::Warn => {
            outln!("share: {e} (continuing without a share)");
            Ok(())
        }
        thegn_core::config::ShareOnError::Fail => Err(e),
    }
}

fn list(json: bool) -> Result<()> {
    let Ok(db) = Db::open() else {
        outln!("share: could not open the state DB");
        return Ok(());
    };
    let rows = db.list_shares().unwrap_or_default();
    if json {
        return super::emit_json(&rows);
    }
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
            r.public_url
                .as_deref()
                .map(thegn_svc::lsp::sanitize_for_terminal)
                .as_deref()
                .unwrap_or("—"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iroh_ticket_displays_as_consumer_command() {
        let ticket =
            thegn_svc::share::PublicEndpoint::parse_ticket("TICKET123").expect("valid ticket");
        assert_eq!(
            display_endpoint(&ticket, "iroh"),
            "dumbpipe connect-tcp TICKET123"
        );
        assert_eq!(ticket.canonical(), "TICKET123");
    }
}
