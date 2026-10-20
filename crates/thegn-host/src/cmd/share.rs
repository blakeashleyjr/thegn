//! `thegn share <action>` — expose a worktree-local port at a public URL.
//!
//! The non-interactive surface over [`thegn_svc::share`]. `start` runs in the
//! foreground (like `bore local`/`ngrok` themselves): it spawns the tunnel
//! client, prints the URL, records the share in the DB so the panel/restore can
//! see it, and blocks until interrupted. `list` reads the persisted shares;
//! `stop` verifies provider teardown before removing its record.
//!
//! The in-process host supervisor (live respawn on restart, badge, panel) builds
//! on the same `[share]` config + `thegn_svc::share` seam this CLI uses.

use anyhow::{Context, Result, bail};
use thegn_core::config::{Config, ShareProviderKind};
use thegn_core::db::{Db, ShareRow};
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
    let launch = match provider.launch() {
        Ok(l) => l,
        Err(e) => return Err(e),
    };
    let generation = cli_generation()?;

    match launch {
        ShareLaunch::Process(plan) => {
            let statedir = match share::share_state_dir(&wt_path, &kind, port, generation) {
                Ok(path) => path,
                Err(e) => return Err(e),
            };
            let running = match share::start(&plan, &statedir, spec.ready_timeout) {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
            let endpoint = display_endpoint(&running.public_url, &kind);
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&endpoint);
            outln!("share: 127.0.0.1:{port} → {}", display_endpoint);
            outln!("share: press Ctrl-C to stop");
            let url = running.public_url.canonical();
            if let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up") {
                running.stop();
                return Err(error)
                    .context("share: could not record the started provider; child stopped");
            }
            // Block until the client exits (Ctrl-C tears down the group).
            // CLI path: `thegn share` runs the tunnel in the foreground by design.
            running.wait();
            if !db
                .delete_share_generation(&wt, port, generation)
                .context("share: could not remove the stopped provider record")?
            {
                bail!("share: stopped provider record changed before removal");
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
            let url = match share::serve_up(&sidecar, &serve) {
                Ok(u) => u,
                Err(e) => return Err(e),
            };
            let url = url.canonical();
            let display_endpoint = thegn_svc::lsp::sanitize_for_terminal(&url);
            outln!("share: 127.0.0.1:{port} → {display_endpoint}");
            outln!(
                "share: serve persists in the tailnet; `thegn share stop {port}` to tear it down"
            );
            if let Err(error) = db.upsert_share(&wt, port, generation, &kind, Some(&url), "up") {
                return match share::serve_down(&sidecar, &serve) {
                    Ok(()) => Err(error).context("share: could not record the started provider; teardown verified"),
                    Err(teardown_error) => Err(error).context(format!(
                        "share: could not record the started provider; teardown failed ({teardown_error}); share is still public; run `thegn share stop {port}`"
                    )),
                };
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

fn list(json: bool) -> Result<()> {
    let db = Db::open().context("share: open the state DB")?;
    let rows = db.list_shares().context("share: read the state DB")?;
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
    match stop_plan(&row, forget, None, None) {
        StopPlan::Forget => {
            if !store.delete_share_generation(wt, port, row.generation)? {
                bail!("share: record changed before it could be forgotten");
            }
            outln!("share: forgot record for port {port}; tunnel was NOT touched");
            return Ok(());
        }
        StopPlan::RefuseProcess if row.provider != "tailscale" => {
            bail!(
                "share: share on port {port} is owned by a running `thegn share start` (provider {}); interrupt that process, or use --forget to drop a stale record",
                row.provider
            );
        }
        _ => {}
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
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(
        &thegn_core::sandbox::container_name_with_profile(wt, Some(&thegn_core::profile::name())),
    );
    finish_stop(store, &row, || share::serve_down(&sidecar, &serve))?;
    outln!("share: stopped and verified port {port}");
    Ok(())
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
    teardown().context("share: verified teardown failed; record retained for retry")?;
    if !store
        .delete_share_generation(&row.worktree, row.local_port, row.generation)
        .context("share: teardown verified but could not remove the record")?
    {
        bail!("share: teardown verified but the record changed before removal");
    }
    Ok(())
}

fn url_port(url: &str) -> Option<u16> {
    let authority = url.split_once("://")?.1.split('/').next()?;
    if let Some((_, host_port)) = authority.rsplit_once(':') {
        return host_port.parse().ok();
    }
    (url.starts_with("https://") && !authority.is_empty()).then_some(443)
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
