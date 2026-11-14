//! `thegn share <action>` — expose a worktree-local port at a public URL.
//!
//! The non-interactive surface over [`thegn_svc::share`]. `start` runs in the
//! foreground (like `bore local`/`ngrok` themselves): it spawns the tunnel
//! client, records a pending row before exposure, prints the URL, and blocks
//! until interrupted. It refuses to start over a port with an existing record.
//! `list` reads the persisted shares; `stop` verifies provider teardown before
//! removing its record.
//!
//! The in-process host supervisor (live respawn on restart, badge, panel) builds
//! on the same `[share]` config + `thegn_svc::share` seam this CLI uses.

use anyhow::{Context, Result, bail};
use thegn_core::config::{Config, ShareProviderKind};
use thegn_core::db::{Db, ShareRow};
use thegn_core::models::ShareOwner;
use thegn_core::outln;
use thegn_core::share::{ShareOwnerState, ShareRecovery, ShareState, build_share_spec};
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
    /// Stop a share after verified teardown; use --forget for record-only repair.
    Stop {
        port: u16,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Only remove the saved record; does not touch the tunnel.
        #[arg(long)]
        forget: bool,
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
        Action::Stop {
            port,
            target,
            forget,
        } => stop(cfg, port, target.worktree, forget),
    }
}

fn start(cfg: &Config, port: u16, worktree: Option<String>, reach: Option<String>) -> Result<()> {
    let db = Db::open().context("share: open the state DB")?;
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
    let launch = provider.launch()?;
    let generation = cli_generation()?;

    match launch {
        ShareLaunch::Process(plan) => {
            let interrupt = crate::platform::InterruptGuard::install();
            begin_share(&db, &wt, port, generation, &kind)?;
            if let Err(error) = record_cli_owner(&db, &wt, port, generation, current_owner()) {
                return Err(abandon_pending_with_error(
                    &db, &wt, port, generation, error,
                ));
            }
            if interrupt.fired() {
                abandon_pending(&db, &wt, port, generation)?;
                return Ok(());
            }
            let statedir = match share::share_state_dir(&wt_path, &kind, port, generation) {
                Ok(statedir) => statedir,
                Err(error) => {
                    let error = error.context("share: could not prepare the provider");
                    return Err(abandon_pending_with_error(
                        &db, &wt, port, generation, error,
                    ));
                }
            };
            if interrupt.fired() {
                abandon_pending(&db, &wt, port, generation)?;
                return Ok(());
            }
            let running = match share::start(&plan, &statedir, spec.ready_timeout) {
                Ok(running) => running,
                Err(error) => {
                    let error = error.context("share: could not start the provider");
                    return Err(abandon_pending_with_error(
                        &db, &wt, port, generation, error,
                    ));
                }
            };
            if interrupt.fired() {
                cleanup_interrupted_share(&db, &wt, port, generation, || running.stop())?;
                return Ok(());
            }
            let endpoint = display_endpoint(&running.public_url, &kind);
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&endpoint);
            outln!("share: 127.0.0.1:{port} → {}", display_endpoint);
            outln!("share: press Ctrl-C to stop");
            let url = running.public_url.canonical();
            if let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up") {
                running.stop();
                let error = error.context(
                    "share: could not record the started provider; child teardown requested",
                );
                return Err(abandon_pending_with_error(
                    &db, &wt, port, generation, error,
                ));
            }
            // Block until the client exits (Ctrl-C tears down the group).
            // CLI path: `thegn share` runs the tunnel in the foreground by design.
            while !running.has_exited() {
                if interrupt.fired() {
                    cleanup_interrupted_share(&db, &wt, port, generation, || running.stop())?;
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            running.wait();
            if !db
                .delete_share_generation(&wt, port, generation)
                .context("share: could not remove the provider record after teardown")?
            {
                bail!("share: provider record changed before removal after teardown");
            }
        }
        ShareLaunch::SidecarServe(serve) => {
            // tailscale serve persists in the VPN sidecar (--bg); no process to
            // hold. Report the URL and exit; `share stop` verifies teardown.
            // Profile-aware — the sidecar rides the container's real name.
            let sidecar = thegn_core::sandbox::vpn_sidecar_name(
                &thegn_core::sandbox::container_name_with_profile(
                    &wt,
                    Some(&thegn_core::profile::name()),
                ),
            );
            begin_share_after_preflight(
                &db,
                &wt,
                port,
                generation,
                &kind,
                thegn_svc::vpn::recorded_sidecar(&sidecar).map(|_| ()),
            )?;
            let url = match share::serve_up_for_worktree(&sidecar, &wt_path, &serve) {
                Ok(url) => url,
                Err(error) => match share::serve_down_for_worktree(&sidecar, &wt_path, &serve) {
                    Ok(()) => {
                        let error = error.context("share: serve start failed; teardown verified");
                        return Err(abandon_pending_with_error(
                            &db, &wt, port, generation, error,
                        ));
                    }
                    Err(teardown_error) => {
                        if let Err(record_error) = thegn_svc::vpn::recorded_sidecar(&sidecar) {
                            let error = error.context(format!(
                                "share: serve start failed before exposure ({record_error:#}); teardown could not be verified ({teardown_error:#})"
                            ));
                            return Err(abandon_pending_with_error(
                                &db, &wt, port, generation, error,
                            ));
                        }
                        let failed_record =
                            db.upsert_share(&wt, port, generation, &kind, None, "failed");
                        let mut message = format!(
                            "share: serve start failed ({error:#}); teardown could not be verified ({teardown_error:#}); the share may still be reachable; run `thegn share stop {port}` to retry the teardown"
                        );
                        if let Err(record_error) = failed_record {
                            message.push_str(&format!(
                                "; could not persist failed state: {record_error:#}"
                            ));
                        }
                        return Err(anyhow::anyhow!(message));
                    }
                },
            };
            let url = url.canonical();
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&url);
            outln!("share: 127.0.0.1:{port} → {display_endpoint}");
            outln!(
                "share: serve persists in the tailnet; `thegn share stop {port}` to tear it down"
            );
            if let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up") {
                return match share::serve_down_for_worktree(&sidecar, &wt_path, &serve) {
                    Ok(()) => {
                        let error = error.context(
                            "share: could not record the started provider; teardown verified",
                        );
                        Err(abandon_pending_with_error(
                            &db, &wt, port, generation, error,
                        ))
                    }
                    Err(teardown_error) => {
                        let failed_record =
                            db.upsert_share(&wt, port, generation, &kind, None, "failed");
                        let mut message = format!(
                            "share: could not record the started provider ({error:#}); teardown could not be verified ({teardown_error:#}); the share may still be reachable; run `thegn share stop {port}` to retry the teardown"
                        );
                        if let Err(record_error) = failed_record {
                            message.push_str(&format!(
                                "; could not persist failed state: {record_error:#}"
                            ));
                        }
                        Err(anyhow::anyhow!(message))
                    }
                };
            }
        }
    }
    Ok(())
}

fn begin_share(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
    kind: &str,
) -> Result<()> {
    let rows = store.list_shares().context("share: read the state DB")?;
    if let Some(row) = rows
        .iter()
        .find(|row| row.worktree == wt && row.local_port == port)
    {
        bail!(
            "share: port {port} already has a recorded {} share ({}); stop it with `thegn share stop {port}` (or `thegn share stop {port} --forget` for a stale record) before starting another",
            row.provider,
            row.state
        );
    }
    store
        .upsert_share(wt, port, generation, kind, None, "starting")
        .context("share: could not record the pending share; nothing was started")
}

fn current_owner() -> Option<ShareOwner> {
    let pid = std::process::id();
    match crate::platform::process_start_ticks(pid) {
        Some(start_ticks) => Some(ShareOwner { pid, start_ticks }),
        None => {
            tracing::debug!(
                pid,
                "could not read share CLI start time; owner will be unrecorded"
            );
            None
        }
    }
}

fn record_cli_owner(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
    owner: Option<ShareOwner>,
) -> Result<()> {
    let Some(owner) = owner else {
        return Ok(());
    };
    let recorded = store
        .set_share_owner(wt, port, generation, owner)
        .context("share: could not record the share owner; nothing was started")?;
    if !recorded {
        bail!(
            "share: pending share record changed before its owner was recorded; nothing was started"
        );
    }
    Ok(())
}

fn begin_share_after_preflight(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
    kind: &str,
    preflight: Result<()>,
) -> Result<()> {
    preflight.context("share: provider precondition failed; nothing was started")?;
    begin_share(store, wt, port, generation, kind)
}

fn cleanup_interrupted_share(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
    stop: impl FnOnce(),
) -> Result<()> {
    stop();
    abandon_pending(store, wt, port, generation)
}

fn abandon_pending(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
) -> Result<()> {
    if !store
        .delete_share_generation(wt, port, generation)
        .context("share: could not remove the pending share record")?
    {
        bail!("share: pending share record changed before removal");
    }
    Ok(())
}

fn abandon_pending_with_error(
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    generation: u64,
    error: anyhow::Error,
) -> anyhow::Error {
    match abandon_pending(store, wt, port, generation) {
        Ok(()) => error,
        Err(abandon_error) => anyhow::anyhow!("{error:#}; {abandon_error:#}"),
    }
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

fn list(json: bool) -> Result<()> {
    let db = Db::open().context("share: open the state DB")?;
    let rows = db.list_shares().context("share: read the state DB")?;
    if json {
        return super::emit_json(&listed_shares(&rows, crate::share::owner_state_of));
    }
    if rows.is_empty() {
        outln!("no shares");
        return Ok(());
    }
    for line in list_lines(&rows, crate::share::owner_state_of) {
        outln!("{line}");
    }
    Ok(())
}

fn stop(cfg: &Config, port: u16, worktree: Option<String>, forget: bool) -> Result<()> {
    let wt = resolve_worktree(worktree).to_string_lossy().into_owned();
    let db = Db::open().context("share: open the state DB")?;
    stop_with_store(cfg, &db, &wt, port, forget)
}

fn stop_with_store(
    cfg: &Config,
    store: &dyn WorktreeAuxStore,
    wt: &str,
    port: u16,
    forget: bool,
) -> Result<()> {
    let rows = store.list_shares().context("share: read the state DB")?;
    let row = rows
        .into_iter()
        .find(|row| row.worktree == wt && row.local_port == port)
        .ok_or_else(|| anyhow::anyhow!("share: no recorded share on port {port}"))?;
    if forget {
        if !store.delete_share_generation(wt, port, row.generation)? {
            bail!("share: record changed before it could be forgotten");
        }
        outln!("share: forgot record for port {port}; tunnel was NOT touched");
        return Ok(());
    }
    if row.provider != "tailscale" {
        if crate::share::owner_state_of(&row) == ShareOwnerState::Exited
            && let Some(owner) = row.owner
        {
            bail!(
                "share: the `thegn share start` that owned port {port} (pid {}) has exited; its {} client may still be running: check for it, then use `thegn share stop {port} --forget` to drop the stale record",
                owner.pid,
                row.provider
            );
        }
        bail!(
            "share: share on port {port} is owned by a running `thegn share start` (provider {}); interrupt that process, or use --forget to drop a stale record",
            row.provider
        );
    }
    let label = thegn_core::share::label_for(wt);
    let share_cfg = thegn_core::config::ShareConfig {
        provider: ShareProviderKind::Tailscale,
        ..cfg.share.clone()
    };
    let spec = build_share_spec(&share_cfg, &label, port, None)
        .ok_or_else(|| anyhow::anyhow!("share: tailscale teardown plan is unavailable"))?;
    let launch = share::for_provider(&spec).launch()?;
    let serve = match launch {
        ShareLaunch::SidecarServe(serve) => serve,
        ShareLaunch::Process(_) => {
            bail!("share: tailscale provider did not produce a sidecar teardown plan")
        }
    };
    let recorded_port = row.public_url.as_deref().and_then(url_port);
    match stop_plan(&row, false, Some(serve.port), recorded_port) {
        StopPlan::TeardownTailscale => {}
        StopPlan::RefusePortMismatch {
            recorded,
            configured,
        } => bail!(
            "share: recorded URL port {recorded:?} differs from configured tailscale port {configured}; nothing was stopped; remove it by hand before using --forget"
        ),
        _ => bail!("share: invalid tailscale stop plan"),
    }
    begin_stop(store, &row)?;
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(
        &thegn_core::sandbox::container_name_with_profile(wt, Some(&thegn_core::profile::name())),
    );
    finish_stop(store, &row, || {
        share::serve_down_for_worktree(&sidecar, std::path::Path::new(wt), &serve)
    })?;
    outln!("share: verified teardown for port {port}");
    Ok(())
}

#[derive(serde::Serialize)]
struct ListedShare<'a> {
    #[serde(flatten)]
    row: &'a ShareRow,
    owner_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<&'static str>,
}

fn begin_stop(store: &dyn WorktreeAuxStore, row: &ShareRow) -> Result<()> {
    store
        .upsert_share(
            &row.worktree,
            row.local_port,
            row.generation,
            &row.provider,
            row.public_url.as_deref(),
            ShareState::Stopping.as_str(),
        )
        .context("share: could not record the stopping state; nothing was torn down")
}

fn list_lines(
    rows: &[ShareRow],
    owner_state: impl Fn(&ShareRow) -> ShareOwnerState,
) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let state = if owner_state(row) == ShareOwnerState::Exited {
                format!("{} (owner exited)", row.state)
            } else {
                row.state.clone()
            };
            let hint = ShareState::parse(&row.state)
                .map(|state| state.recovery(owner_state(row)))
                .and_then(ShareRecovery::hint);
            let hint = match hint {
                Some(hint) => format!("  ({hint})"),
                None => String::new(),
            };
            format!(
                "{:<6} {:<6} {:<17} {}{}",
                row.local_port,
                row.provider,
                state,
                row.public_url
                    .as_deref()
                    .map(thegn_svc::lsp::sanitize_for_terminal)
                    .as_deref()
                    .unwrap_or("—"),
                hint,
            )
        })
        .collect()
}

fn listed_shares<'a>(
    rows: &'a [ShareRow],
    owner_state: impl Fn(&ShareRow) -> ShareOwnerState,
) -> Vec<ListedShare<'a>> {
    rows.iter()
        .map(|row| ListedShare {
            row,
            owner_state: owner_state(row).as_str(),
            recovery: ShareState::parse(&row.state)
                .map(|state| state.recovery(owner_state(row)).as_str()),
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum StopPlan {
    Forget,
    TeardownTailscale,
    RefuseProcess,
    RefusePortMismatch {
        recorded: Option<u16>,
        configured: u16,
    },
}

fn stop_plan(
    row: &ShareRow,
    forget: bool,
    plan_port: Option<u16>,
    recorded_port: Option<u16>,
) -> StopPlan {
    if forget {
        return StopPlan::Forget;
    }
    if row.provider != "tailscale" {
        return StopPlan::RefuseProcess;
    }
    let Some(configured) = plan_port else {
        return StopPlan::RefuseProcess;
    };
    if row.public_url.is_none()
        && matches!(
            ShareState::parse(&row.state),
            Some(ShareState::Starting | ShareState::Failed | ShareState::Stopping)
        )
    {
        return StopPlan::TeardownTailscale;
    }
    if recorded_port != Some(configured) {
        return StopPlan::RefusePortMismatch {
            recorded: recorded_port,
            configured,
        };
    }
    StopPlan::TeardownTailscale
}

fn finish_stop(
    store: &dyn WorktreeAuxStore,
    row: &ShareRow,
    teardown: impl FnOnce() -> Result<()>,
) -> Result<()> {
    teardown().context(
        "share: verified teardown failed; record retained for retry; if the sidecar/worktree is gone, confirm the funnel is off and use `--forget`",
    )?;
    if !store
        .delete_share_generation(&row.worktree, row.local_port, row.generation)
        .context("share: teardown verified but could not remove the record")?
    {
        bail!("share: teardown verified but the record changed before removal");
    }
    Ok(())
}

fn url_port(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    let default_port = (scheme == "https").then_some(443);
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (_, suffix) = bracketed.split_once(']')?;
        return match suffix {
            "" => default_port,
            suffix => suffix.strip_prefix(':')?.parse().ok(),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => port.parse().ok(),
        Some(_) => None,
        None => default_port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::{ShareOnError, ShareProviderKind};
    use thegn_core::testenv::IsolatedState;

    fn stored_row(db: &Db, provider: &str, url: Option<&str>) -> ShareRow {
        db.upsert_share("/worktree/demo", 3000, 41, provider, url, "up")
            .expect("insert share row");
        db.list_shares()
            .expect("list share rows")
            .into_iter()
            .find(|row| row.worktree == "/worktree/demo" && row.local_port == 3000)
            .expect("inserted share row")
    }

    fn isolated_db(state: &IsolatedState) -> Db {
        Db::open_at(&state.root().join("share-stop.sqlite")).expect("open isolated DB")
    }

    #[test]
    fn record_cli_owner_stamps_only_this_generation() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        begin_share(&db, "/worktree/demo", 3000, 7, "bore").unwrap();
        let owner = ShareOwner {
            pid: 4242,
            start_ticks: 99,
        };
        record_cli_owner(&db, "/worktree/demo", 3000, 7, Some(owner)).unwrap();
        assert_eq!(db.list_shares().unwrap()[0].owner, Some(owner));
        let result = record_cli_owner(&db, "/worktree/demo", 3000, 8, Some(owner));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("changed before its owner was recorded")
        );
        assert_eq!(db.list_shares().unwrap()[0].owner, Some(owner));
        record_cli_owner(&db, "/worktree/demo", 3000, 7, None).unwrap();
        assert_eq!(db.list_shares().unwrap()[0].owner, Some(owner));
    }

    #[test]
    fn list_rows_mark_an_exited_owner() {
        let rows = vec![
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3000,
                provider: "bore".into(),
                public_url: Some("https://demo.test".into()),
                state: "up".into(),
                created_at: 0,
                generation: 41,
                owner: Some(ShareOwner {
                    pid: 4242,
                    start_ticks: 99,
                }),
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3001,
                provider: "frp".into(),
                public_url: None,
                state: "up".into(),
                created_at: 0,
                generation: 42,
                owner: None,
            },
        ];
        let lines = list_lines(&rows, |row| {
            if row.owner.is_some() {
                ShareOwnerState::Exited
            } else {
                ShareOwnerState::Unrecorded
            }
        });
        assert!(lines[0].contains("up (owner exited)"));
        assert!(lines[0][32..].starts_with("https://demo.test"));
        assert!(lines[0].contains("(needs reconcile)"));
        assert!(!lines[1].contains("owner exited"));
        let json = serde_json::to_value(listed_shares(&rows, |row| {
            if row.owner.is_some() {
                ShareOwnerState::Exited
            } else {
                ShareOwnerState::Unrecorded
            }
        }))
        .unwrap();
        let entries = json.as_array().expect("one JSON array");
        assert_eq!(entries[0]["owner_state"], "exited");
        assert_eq!(entries[0]["owner"]["pid"], 4242);
        assert_eq!(entries[1]["owner_state"], "unrecorded");
        assert!(entries[1].get("owner").is_none());
    }

    #[test]
    fn list_rows_name_the_recovery_action() {
        let owner = ShareOwner {
            pid: 4242,
            start_ticks: 99,
        };
        let rows = [
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3000,
                provider: "tailscale".into(),
                public_url: None,
                state: "stopping".into(),
                created_at: 0,
                generation: 41,
                owner: None,
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3001,
                provider: "tailscale".into(),
                public_url: None,
                state: "unknown".into(),
                created_at: 0,
                generation: 42,
                owner: None,
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3002,
                provider: "tailscale".into(),
                public_url: Some("https://demo.test".into()),
                state: "up".into(),
                created_at: 0,
                generation: 43,
                owner: Some(owner),
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3003,
                provider: "tailscale".into(),
                public_url: Some("https://alive.test".into()),
                state: "up".into(),
                created_at: 0,
                generation: 44,
                owner: Some(owner),
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3004,
                provider: "tailscale".into(),
                public_url: None,
                state: "failed".into(),
                created_at: 0,
                generation: 45,
                owner: None,
            },
            ShareRow {
                worktree: "/worktree/demo".into(),
                local_port: 3005,
                provider: "tailscale".into(),
                public_url: None,
                state: "weird".into(),
                created_at: 0,
                generation: 46,
                owner: None,
            },
        ];
        let owner_state = |row: &ShareRow| match row.local_port {
            3002 => ShareOwnerState::Exited,
            3003 => ShareOwnerState::Alive,
            _ => ShareOwnerState::Unrecorded,
        };
        let lines = list_lines(&rows, owner_state);
        assert!(lines[0].contains("(retry: thegn share stop)"));
        assert!(lines[1].contains("(needs reconcile)"));
        assert!(lines[2].contains("(needs reconcile)"));
        assert!(!lines[3].contains("("));
        assert!(!lines[4].contains("("));
        assert!(!lines[5].contains("("));

        let json = serde_json::to_value(listed_shares(&rows, owner_state)).unwrap();
        let entries = json.as_array().expect("share list is an array");
        assert_eq!(entries[0]["recovery"], "retry_teardown");
        assert_eq!(entries[1]["recovery"], "reconcile");
        assert_eq!(entries[2]["recovery"], "reconcile");
        assert_eq!(entries[3]["recovery"], "keep");
        assert_eq!(entries[4]["recovery"], "keep");
        assert!(entries[5].get("recovery").is_none());
    }

    #[test]
    fn begin_stop_records_stopping_and_keeps_the_row_identity() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        let row = stored_row(&db, "tailscale", Some("https://demo.ts.net:8443"));
        let owner = ShareOwner {
            pid: 4242,
            start_ticks: 99,
        };
        assert!(
            db.set_share_owner("/worktree/demo", 3000, row.generation, owner)
                .unwrap()
        );
        let row = db.list_shares().unwrap().remove(0);

        begin_stop(&db, &row).unwrap();

        let stopping = db.list_shares().unwrap().remove(0);
        assert_eq!(stopping.state, "stopping");
        assert_eq!(stopping.generation, row.generation);
        assert_eq!(stopping.provider, row.provider);
        assert_eq!(stopping.public_url, row.public_url);
        assert_eq!(stopping.owner, row.owner);
    }

    #[test]
    fn failed_teardown_leaves_a_stopping_row_and_a_verified_one_deletes_it() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        let row = stored_row(&db, "tailscale", Some("https://demo.ts.net:8443"));
        begin_stop(&db, &row).unwrap();
        let result = finish_stop(&db, &row, || Err(anyhow::anyhow!("unverified")));
        assert!(result.is_err());
        let stopping = db.list_shares().unwrap().remove(0);
        assert_eq!(stopping.state, "stopping");

        begin_stop(&db, &stopping).unwrap();
        finish_stop(&db, &stopping, || Ok(())).unwrap();
        assert!(db.list_shares().unwrap().is_empty());
    }

    #[test]
    fn stop_names_an_exited_owner_of_a_process_row() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        let row = stored_row(&db, "bore", Some("https://public.example"));
        let owner = ShareOwner {
            pid: std::process::id(),
            start_ticks: i64::MAX as u64,
        };
        assert!(
            db.set_share_owner("/worktree/demo", 3000, row.generation, owner)
                .unwrap()
        );
        let result = stop_with_store(&Config::default(), &db, "/worktree/demo", 3000, false);
        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("has exited"));
        assert!(message.contains(&owner.pid.to_string()));
        assert_eq!(db.list_shares().unwrap().len(), 1);
    }

    #[test]
    fn begin_share_records_a_starting_row_before_exposure() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);

        begin_share(&db, "/worktree/demo", 3000, 7, "tailscale").expect("persist pending share");

        let rows = db.list_shares().expect("read shares");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "starting");
        assert_eq!(rows[0].public_url, None);
        assert_eq!(rows[0].generation, 7);
    }

    #[test]
    fn begin_share_refuses_a_port_that_already_has_a_record() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);
        stored_row(&db, "tailscale", Some("https://demo.ts.net"));

        let result = begin_share(&db, "/worktree/demo", 3000, 9, "tailscale");

        let error = result.expect_err("existing record must block replacement");
        assert!(format!("{error:#}").contains("already has a recorded"));
        let rows = db.list_shares().expect("read retained share");
        assert_eq!(rows.len(), 1);
        let row = rows.into_iter().next().expect("retained share row");
        assert_eq!(row.generation, 41);
        assert_eq!(row.state, "up");
        assert_eq!(row.public_url.as_deref(), Some("https://demo.ts.net"));
    }

    #[test]
    fn sidecar_preflight_refusal_leaves_no_share_row() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);

        let result = begin_share_after_preflight(
            &db,
            "/worktree/demo",
            3000,
            9,
            "tailscale",
            Err(anyhow::anyhow!("vpn: no recorded runtime for sidecar")),
        );

        assert!(
            result.is_err(),
            "missing runtime record refuses before exposure"
        );
        assert!(
            db.list_shares()
                .expect("read shares after refusal")
                .is_empty()
        );
    }

    #[test]
    fn interrupted_process_cleanup_stops_and_removes_its_generation() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);
        begin_share(&db, "/worktree/demo", 3000, 7, "frp").expect("persist pending share");
        db.upsert_share(
            "/worktree/demo",
            3000,
            7,
            "frp",
            Some("https://share.example"),
            "up",
        )
        .expect("mark share exposed");
        let stopped = std::cell::Cell::new(false);

        cleanup_interrupted_share(&db, "/worktree/demo", 3000, 7, || stopped.set(true))
            .expect("stop process share and delete its row");

        assert!(stopped.get(), "interrupted cleanup must stop the child");
        assert!(
            db.list_shares()
                .expect("read shares after cleanup")
                .is_empty()
        );
    }

    #[test]
    fn begin_share_allows_another_port_or_worktree() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);
        stored_row(&db, "tailscale", Some("https://demo.ts.net"));

        begin_share(&db, "/worktree/demo", 3001, 9, "tailscale")
            .expect("different port is independent");
        begin_share(&db, "/worktree/other", 3000, 10, "tailscale")
            .expect("different worktree is independent");

        let rows = db.list_shares().expect("read shares");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| {
            row.worktree == "/worktree/demo" && row.local_port == 3001 && row.state == "starting"
        }));
        assert!(rows.iter().any(|row| {
            row.worktree == "/worktree/other" && row.local_port == 3000 && row.state == "starting"
        }));
    }

    #[test]
    fn abandon_pending_removes_only_this_generation() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);
        begin_share(&db, "/worktree/demo", 3000, 7, "tailscale").expect("persist pending share");

        abandon_pending(&db, "/worktree/demo", 3000, 7).expect("remove matching generation");
        assert!(db.list_shares().expect("read after removal").is_empty());

        begin_share(&db, "/worktree/demo", 3000, 8, "tailscale")
            .expect("persist second pending share");
        let result = abandon_pending(&db, "/worktree/demo", 3000, 7);
        assert!(result.is_err(), "wrong generation must be visible");
        let rows = db.list_shares().expect("read retained generation");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generation, 8);
        assert_eq!(rows[0].state, "starting");
    }

    #[test]
    fn stop_plan_tears_down_a_urlless_pending_tailscale_row() {
        let _state = IsolatedState::new();
        let db = isolated_db(&_state);
        db.upsert_share("/worktree/demo", 3000, 41, "tailscale", None, "starting")
            .expect("insert starting row");
        let row = db
            .list_shares()
            .expect("read starting row")
            .into_iter()
            .next()
            .expect("starting share row");
        assert_eq!(
            stop_plan(&row, false, Some(443), None),
            StopPlan::TeardownTailscale
        );

        db.upsert_share("/worktree/demo", 3000, 41, "tailscale", None, "failed")
            .expect("mark row failed");
        let row = db
            .list_shares()
            .expect("read failed row")
            .into_iter()
            .next()
            .expect("failed share row");
        assert_eq!(
            stop_plan(&row, false, Some(443), None),
            StopPlan::TeardownTailscale
        );

        let stopping_row = ShareRow {
            worktree: "/worktree/demo".to_string(),
            local_port: 3000,
            provider: "tailscale".to_string(),
            public_url: None,
            state: "stopping".to_string(),
            created_at: 0,
            generation: 41,
            owner: None,
        };
        assert_eq!(
            stop_plan(&stopping_row, false, Some(443), None),
            StopPlan::TeardownTailscale
        );

        let up_row = ShareRow {
            worktree: "/worktree/demo".to_string(),
            local_port: 3000,
            provider: "tailscale".to_string(),
            public_url: None,
            state: "up".to_string(),
            created_at: 0,
            generation: 41,
            owner: None,
        };
        assert_eq!(
            stop_plan(&up_row, false, Some(443), None),
            StopPlan::RefusePortMismatch {
                recorded: None,
                configured: 443,
            }
        );
    }

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

    #[test]
    fn stop_deletes_the_row_only_when_teardown_succeeds() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        let row = stored_row(&db, "tailscale", Some("https://demo.ts.net:8443"));
        finish_stop(&db, &row, || Ok(())).expect("verified teardown removes row");
        assert!(db.list_shares().expect("read rows").is_empty());

        let row = stored_row(&db, "tailscale", Some("https://demo.ts.net:8443"));
        let result = finish_stop(&db, &row, || Err(anyhow::anyhow!("unverified")));
        assert!(result.is_err(), "failed teardown must be visible");
        assert_eq!(db.list_shares().expect("read rows after failure").len(), 1);
    }

    #[test]
    fn stop_refuses_a_process_row() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        for provider in ["bore", "frp", "iroh"] {
            let row = stored_row(&db, provider, Some("https://public.example"));
            assert_eq!(stop_plan(&row, false, None, None), StopPlan::RefuseProcess);
            let result = stop_with_store(&Config::default(), &db, "/worktree/demo", 3000, false);
            assert!(
                result.is_err(),
                "process provider must refuse verified stop"
            );
            assert_eq!(db.list_shares().expect("read retained row").len(), 1);
            assert!(
                db.delete_share_generation("/worktree/demo", 3000, row.generation)
                    .expect("remove test row")
            );
        }
        assert!(db.list_shares().expect("read rows").is_empty());
        for provider in ["bore", "frp", "iroh", "tailscale"] {
            let row = ShareRow {
                worktree: "/worktree/demo".to_string(),
                local_port: 3000,
                provider: provider.to_string(),
                public_url: None,
                state: "up".to_string(),
                created_at: 0,
                generation: 41,
                owner: None,
            };
            assert_eq!(stop_plan(&row, true, None, None), StopPlan::Forget);
        }
    }

    #[test]
    fn stop_forget_removes_only_the_record() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        let row = stored_row(&db, "bore", Some("https://public.example"));
        stop_with_store(&Config::default(), &db, "/worktree/demo", 3000, true)
            .expect("forget record");
        assert!(db.list_shares().expect("read rows").is_empty());
        assert_eq!(stop_plan(&row, true, None, None), StopPlan::Forget);
    }

    #[test]
    fn stop_refuses_when_configured_port_differs_from_recorded_url() {
        let state = IsolatedState::new();
        let db = isolated_db(&state);
        stored_row(&db, "tailscale", Some("https://demo.ts.net:8443"));
        let cfg = Config {
            share: thegn_core::config::ShareConfig {
                provider: ShareProviderKind::Tailscale,
                tailscale: thegn_core::config::TailscaleShareConfig {
                    https_port: 443,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Config::default()
        };
        let result = stop_with_store(&cfg, &db, "/worktree/demo", 3000, false);
        assert!(result.is_err(), "port drift must refuse before teardown");
        assert_eq!(db.list_shares().expect("read retained row").len(), 1);

        let row = ShareRow {
            worktree: "/worktree/demo".to_string(),
            local_port: 3000,
            provider: "tailscale".to_string(),
            public_url: Some("https://demo.ts.net:8443".to_string()),
            state: "up".to_string(),
            created_at: 0,
            generation: 41,
            owner: None,
        };
        assert_eq!(
            stop_plan(
                &row,
                false,
                Some(443),
                url_port(row.public_url.as_deref().unwrap_or(""))
            ),
            StopPlan::RefusePortMismatch {
                recorded: Some(8443),
                configured: 443,
            }
        );
        assert_eq!(
            stop_plan(&row, false, Some(8443), Some(8443)),
            StopPlan::TeardownTailscale
        );
        assert_eq!(url_port("https://demo.ts.net"), Some(443));
        assert_eq!(url_port("https://[fd7a::1]"), Some(443));
        assert_eq!(url_port("https://[fd7a::1]:8443/path"), Some(8443));
        assert_eq!(url_port("http://[fd7a::1]"), None);
    }

    #[test]
    fn explicit_start_failure_is_never_downgraded() {
        let state = IsolatedState::new();
        let cfg = Config {
            share: thegn_core::config::ShareConfig {
                provider: ShareProviderKind::Frp,
                on_error: ShareOnError::Warn,
                ..Default::default()
            },
            ..Config::default()
        };
        let path = state.root().to_string_lossy().into_owned();
        let result = start(&cfg, 3000, Some(path), None);
        assert!(
            result.is_err(),
            "explicit launch errors must remain nonzero under warn"
        );
        assert!(
            Db::open()
                .expect("open isolated state DB")
                .list_shares()
                .expect("read rows")
                .is_empty()
        );
    }
}
