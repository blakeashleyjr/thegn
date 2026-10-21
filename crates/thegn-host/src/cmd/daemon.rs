//! `thegn daemon stop` — stop every verified pane daemon known to this state
//! directory. The daemon itself is run by the hidden bare `thegn daemon`
//! (spawned lazily by the first attach / `thegn serve`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Subcommand;
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::control::client::{ControlAddr, ControlClient};

#[derive(Subcommand, Clone)]
pub enum DaemonAction {
    /// Stop every verified pane daemon for this state directory (admin).
    Stop,
}

pub fn run(cfg: &Config, action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Stop => stop(cfg),
    }
}

fn stop_targets(
    registry: Vec<PathBuf>,
    configured: PathBuf,
    legacy_live: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(registry.len() + 2);
    for path in registry.into_iter().chain([configured]).chain(legacy_live) {
        if seen.insert(path.clone()) {
            targets.push(path);
        }
    }
    targets
}

async fn wait_until_stopped<F, Fut>(
    targets: Vec<PathBuf>,
    deadline: Duration,
    mut probe: F,
) -> Vec<PathBuf>
where
    F: FnMut(PathBuf) -> Fut,
    Fut: Future<Output = bool>,
{
    let until = tokio::time::Instant::now() + deadline;
    let mut remaining = targets;
    loop {
        let mut still_live = Vec::with_capacity(remaining.len());
        let mut iter = remaining.into_iter();
        while let Some(target) = iter.next() {
            let now = tokio::time::Instant::now();
            if now >= until {
                still_live.push(target);
                still_live.extend(iter);
                return still_live;
            }
            let time_left = until - now;
            match tokio::time::timeout(time_left, probe(target.clone())).await {
                Ok(true) | Err(_) => still_live.push(target),
                Ok(false) => {}
            }
        }
        if still_live.is_empty() {
            return still_live;
        }
        let now = tokio::time::Instant::now();
        if now >= until {
            return still_live;
        }
        remaining = still_live;
        tokio::time::sleep((until - now).min(Duration::from_millis(100))).await;
    }
}

fn stop(cfg: &Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let stop_deadline =
            tokio::time::Instant::now() + crate::daemon::service::SHUTDOWN_TOTAL_DEADLINE;
        let scopes = crate::daemon::scope_aliases();
        let registry = match probe_before_deadline(
            stop_deadline,
            tokio::task::spawn_blocking({
                let scopes = scopes.clone();
                move || match thegn_core::db::Db::open() {
                    Ok(db) => scopes
                        .iter()
                        .filter_map(|scope| {
                            thegn_svc::control::client::discover(
                                &db,
                                scope,
                                crate::daemon::now_ms(),
                            )
                        })
                        .filter_map(|addr| match addr {
                            ControlAddr::Unix(path) => Some(path),
                            ControlAddr::Tcp { .. } | ControlAddr::HttpOrigin { .. } => None,
                        })
                        .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                }
            }),
        )
        .await
        {
            Some(Ok(paths)) => paths,
            Some(Err(_)) | None => Vec::new(),
        };
        let configured = crate::daemon::socket_path(&cfg.daemon);
        let legacy_path = thegn_core::config_daemon::legacy_socket_path(
            &thegn_core::util::xdg_state_home().join("thegn"),
        );
        let legacy = (crate::daemon::legacy_socket_state()
            == crate::daemon::LegacySocketState::Live)
            .then_some(legacy_path);
        let targets = stop_targets(registry, configured, legacy);

        let mut ours = Vec::new();
        for path in targets {
            let client = ControlClient::new(ControlAddr::Unix(path.clone()));
            let label = crate::daemon::client::socket_label(&client);
            let probe = probe_before_deadline(
                stop_deadline,
                crate::daemon::client::probe_stop_target(&client, &scopes),
            )
            .await;
            match probe {
                None => {
                    outln!(
                        "skipped {}: stop deadline reached while checking listener",
                        label
                    );
                }
                Some(crate::daemon::client::StopProbe::NotRunning) => {}
                Some(crate::daemon::client::StopProbe::Ours) => ours.push(path),
                Some(crate::daemon::client::StopProbe::Foreign(scope)) => {
                    outln!(
                        "skipped {}: belongs to a different state directory (scope {})",
                        label,
                        scope
                    );
                }
            }
        }
        if ours.is_empty() {
            outln!("no thegn pane daemon is running");
            return Ok(());
        }

        let mut refused = HashMap::new();
        let mut waiting = Vec::new();
        for path in &ours {
            let client = ControlClient::new(ControlAddr::Unix(path.clone()));
            match probe_before_deadline(
                stop_deadline,
                client.call_raw("POST", "/v1/daemon/shutdown", None),
            )
            .await
            {
                Some(Ok(_)) => waiting.push(path.clone()),
                Some(Err(error)) => {
                    refused.insert(path.clone(), error.to_string());
                }
                None => {
                    refused.insert(path.clone(), "stop deadline reached before shutdown".into());
                }
            }
        }
        let survivors = wait_until_stopped(
            waiting,
            stop_deadline.saturating_duration_since(tokio::time::Instant::now()),
            |path| async move {
                ControlClient::new(ControlAddr::Unix(path))
                    .health()
                    .await
                    .is_ok()
            },
        )
        .await;
        let survivors = survivors.into_iter().collect::<HashSet<_>>();
        let mut failures = Vec::new();
        for path in &ours {
            let client = ControlClient::new(ControlAddr::Unix(path.clone()));
            let label = crate::daemon::client::socket_label(&client);
            if let Some(error) = refused.get(path) {
                outln!("shutdown refused for {}: {}", label, error);
                failures.push(format!("{} ({})", label, error));
            } else if survivors.contains(path) {
                outln!(
                    "still running after {}s: {}",
                    crate::daemon::service::SHUTDOWN_TOTAL_DEADLINE.as_secs(),
                    label
                );
                failures.push(label);
            } else {
                outln!("stopped {}", label);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "pane daemon stop incomplete: {} of {} listeners still live ({})",
                failures.len(),
                ours.len(),
                failures.join(", ")
            ))
        }
    })
}

async fn probe_before_deadline<F: Future>(
    deadline: tokio::time::Instant,
    future: F,
) -> Option<F::Output> {
    match tokio::time::timeout_at(deadline, future).await {
        Ok(output) => Some(output),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{probe_before_deadline, stop_targets, wait_until_stopped};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn stop_targets_collapse_duplicates_and_keep_precedence() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let c = PathBuf::from("c");
        assert_eq!(
            stop_targets(
                vec![a.clone(), b.clone(), a.clone()],
                b.clone(),
                Some(c.clone())
            ),
            vec![a, b, c]
        );
        assert_eq!(
            stop_targets(Vec::new(), PathBuf::from("a"), None),
            vec![PathBuf::from("a")]
        );
    }

    #[test]
    fn stop_targets_add_the_legacy_socket_only_when_live() {
        let configured = PathBuf::from("configured");
        let legacy = PathBuf::from("legacy");
        assert_eq!(
            stop_targets(Vec::new(), configured.clone(), None),
            vec![configured.clone()]
        );
        assert_eq!(
            stop_targets(Vec::new(), configured.clone(), Some(configured.clone())),
            vec![configured]
        );
        assert_eq!(
            stop_targets(
                Vec::new(),
                PathBuf::from("configured"),
                Some(legacy.clone())
            ),
            vec![PathBuf::from("configured"), legacy]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_stopped_returns_survivors_after_one_total_deadline() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let b_probe = b.clone();
        let polls = Arc::new(Mutex::new(HashMap::<PathBuf, usize>::new()));
        let started = tokio::time::Instant::now();
        let survivors = wait_until_stopped(vec![a.clone(), b.clone()], Duration::from_secs(3), {
            let polls = Arc::clone(&polls);
            move |path| {
                let polls = Arc::clone(&polls);
                let is_b = path == b_probe;
                async move {
                    let mut polls = polls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let count = polls.entry(path.clone()).or_insert(0);
                    *count += 1;
                    is_b || *count <= 2
                }
            }
        })
        .await;
        let elapsed = tokio::time::Instant::now() - started;
        assert_eq!(survivors, vec![b]);
        assert!(elapsed >= Duration::from_secs(3));
        assert!(elapsed < Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn probe_before_deadline_shares_one_budget_across_probes() {
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(1);
        assert_eq!(
            probe_before_deadline(deadline, async {
                tokio::time::sleep(Duration::from_millis(800)).await;
                true
            })
            .await,
            Some(true)
        );
        assert_eq!(
            probe_before_deadline(deadline, async {
                tokio::time::sleep(Duration::from_millis(800)).await;
                true
            })
            .await,
            None
        );
        assert_eq!(tokio::time::Instant::now(), deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_stopped_returns_immediately_when_all_stop() {
        let started = tokio::time::Instant::now();
        let survivors = wait_until_stopped(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            Duration::from_secs(3),
            |_path| async { false },
        )
        .await;
        assert!(survivors.is_empty());
        assert!(tokio::time::Instant::now() - started < Duration::from_millis(200));
    }
}
