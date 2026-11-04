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

#[derive(Debug, Default, PartialEq)]
struct Classified {
    ours: Vec<PathBuf>,
    unknown: Vec<PathBuf>,
    foreign: Vec<(PathBuf, String)>,
    unverified: Vec<(PathBuf, String)>,
}

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

#[derive(Debug, PartialEq, Eq)]
enum ShutdownResult {
    Stopped,
    Refused(String),
    StillRunning,
}

/// Ask every verified listener to shut down, then wait (under the ONE `deadline`) for each to disappear.
/// Results come back in `ours` order, one per listener.
async fn shutdown_ours<R, RF, P, PF>(
    ours: &[PathBuf],
    deadline: tokio::time::Instant,
    mut request: R,
    probe: P,
) -> Vec<(PathBuf, ShutdownResult)>
where
    R: FnMut(PathBuf) -> RF,
    RF: Future<Output = std::result::Result<(), String>>,
    P: FnMut(PathBuf) -> PF,
    PF: Future<Output = bool>,
{
    let mut refused = HashMap::new();
    let mut waiting = Vec::new();
    for path in ours {
        match probe_before_deadline(deadline, request(path.clone())).await {
            Some(Ok(())) => waiting.push(path.clone()),
            Some(Err(error)) => {
                refused.insert(path.clone(), error);
            }
            None => {
                refused.insert(
                    path.clone(),
                    "stop deadline reached before shutdown".to_string(),
                );
            }
        }
    }
    let survivors = wait_until_stopped(
        waiting,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        probe,
    )
    .await
    .into_iter()
    .collect::<HashSet<_>>();
    ours.iter()
        .map(|path| {
            let result = if let Some(error) = refused.remove(path) {
                ShutdownResult::Refused(error)
            } else if survivors.contains(path) {
                ShutdownResult::StillRunning
            } else {
                ShutdownResult::Stopped
            };
            (path.clone(), result)
        })
        .collect()
}

/// The failure labels `stop_verdict` reports: a refused listener as `"<label> (<error>)"`, a survivor as `"<label>"`.
fn shutdown_failures(
    results: &[(PathBuf, ShutdownResult)],
    label: impl Fn(&std::path::Path) -> String,
) -> Vec<String> {
    results
        .iter()
        .filter_map(|(path, result)| match result {
            ShutdownResult::Stopped => None,
            ShutdownResult::Refused(error) => Some(format!("{} ({})", label(path), error)),
            ShutdownResult::StillRunning => Some(label(path)),
        })
        .collect()
}

async fn classify_targets<F, Fut>(
    targets: Vec<PathBuf>,
    deadline: tokio::time::Instant,
    mut probe: F,
) -> Classified
where
    F: FnMut(PathBuf) -> Fut,
    Fut: Future<Output = crate::daemon::client::StopProbe>,
{
    let mut classified = Classified::default();
    let mut targets = targets.into_iter();
    while let Some(target) = targets.next() {
        if tokio::time::Instant::now() >= deadline {
            classified.unknown.push(target);
            classified.unknown.extend(targets);
            break;
        }
        match tokio::time::timeout_at(deadline, probe(target.clone())).await {
            Ok(crate::daemon::client::StopProbe::NotRunning) => {}
            Ok(crate::daemon::client::StopProbe::Ours) => classified.ours.push(target),
            Ok(crate::daemon::client::StopProbe::Foreign(scope)) => {
                classified.foreign.push((target, scope));
            }
            Ok(crate::daemon::client::StopProbe::Unverified(reason)) => {
                classified.unverified.push((target, reason));
            }
            Err(_) => {
                classified.unknown.push(target);
                classified.unknown.extend(targets);
                break;
            }
        }
    }
    classified
}

fn stop_verdict(ours_total: usize, failures: &[String], unknown: &[String]) -> Result<()> {
    if failures.is_empty() && unknown.is_empty() {
        return Ok(());
    }
    let mut details = Vec::new();
    if !failures.is_empty() {
        details.push(format!(
            "{} of {} listeners still live ({})",
            failures.len(),
            ours_total,
            failures.join(", ")
        ));
    }
    if !unknown.is_empty() {
        details.push(format!(
            "{} not confirmed ({})",
            unknown.len(),
            unknown.join(", ")
        ));
    }
    Err(anyhow!(
        "pane daemon stop incomplete: {}",
        details.join("; ")
    ))
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
                move || -> Result<Vec<PathBuf>> {
                    let db = thegn_core::db::Db::open()?;
                    Ok(scopes
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
                        .collect::<Vec<_>>())
                }
            }),
        )
        .await
        {
            Some(Ok(Ok(paths))) => (paths, None),
            Some(Ok(Err(error))) => (Vec::new(), Some(Some(error.to_string()))),
            Some(Err(error)) => (Vec::new(), Some(Some(error.to_string()))),
            None => (Vec::new(), Some(None)),
        };
        let (registry, registry_error) = registry;
        let mut unknown = Vec::new();
        match registry_error {
            Some(Some(error)) => {
                outln!("unknown: listener registry could not be read ({})", error);
                unknown.push("listener registry".to_string());
            }
            Some(None) => {
                outln!("unknown: listener registry not read before the stop deadline");
                unknown.push("listener registry".to_string());
            }
            None => {}
        }
        let configured = crate::daemon::socket_path(&cfg.daemon);
        let legacy_path = thegn_core::config_daemon::legacy_socket_path(
            &thegn_core::util::xdg_state_home().join("thegn"),
        );
        let legacy = (crate::daemon::legacy_socket_state()
            == crate::daemon::LegacySocketState::Live)
            .then_some(legacy_path);
        let targets = stop_targets(registry, configured, legacy);

        let classified = classify_targets(targets, stop_deadline, {
            let scopes = scopes.clone();
            move |path| {
                let scopes = scopes.clone();
                async move {
                    let client = ControlClient::new(ControlAddr::Unix(path));
                    crate::daemon::client::probe_stop_target(&client, &scopes).await
                }
            }
        })
        .await;
        let ours = classified.ours;
        for (path, reason) in classified.unverified {
            let client = ControlClient::new(ControlAddr::Unix(path.clone()));
            let label = crate::daemon::client::socket_label(&client);
            outln!("unverified {}: {}", label, reason);
            unknown.push(label);
        }
        for (path, scope) in classified.foreign {
            let client = ControlClient::new(ControlAddr::Unix(path));
            outln!(
                "skipped {}: belongs to a different state directory (scope {})",
                crate::daemon::client::socket_label(&client),
                scope
            );
        }
        for path in classified.unknown {
            let client = ControlClient::new(ControlAddr::Unix(path));
            let label = crate::daemon::client::socket_label(&client);
            outln!(
                "unknown {}: stop deadline reached before it was checked",
                label
            );
            unknown.push(label);
        }
        if ours.is_empty() && unknown.is_empty() {
            outln!("no thegn pane daemon is running");
            return Ok(());
        }

        let results = shutdown_ours(
            &ours,
            stop_deadline,
            |path| async move {
                ControlClient::new(ControlAddr::Unix(path))
                    .call_raw("POST", "/v1/daemon/shutdown", None)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
            |path| async move {
                let health = ControlClient::new(ControlAddr::Unix(path)).health().await;
                still_live(&health)
            },
        )
        .await;
        for (path, result) in &results {
            let client = ControlClient::new(ControlAddr::Unix(path.clone()));
            let label = crate::daemon::client::socket_label(&client);
            match result {
                ShutdownResult::Refused(error) => {
                    outln!("shutdown refused for {}: {}", label, error);
                }
                ShutdownResult::StillRunning => {
                    outln!(
                        "still running after {}s: {}",
                        crate::daemon::service::SHUTDOWN_TOTAL_DEADLINE.as_secs(),
                        label
                    );
                }
                ShutdownResult::Stopped => outln!("stopped {}", label),
            }
        }
        let failures = shutdown_failures(&results, |path| {
            let client = ControlClient::new(ControlAddr::Unix(path.to_path_buf()));
            crate::daemon::client::socket_label(&client)
        });
        stop_verdict(ours.len(), &failures, &unknown)
    })
}

fn still_live(health: &anyhow::Result<()>) -> bool {
    match health {
        Ok(()) => true,
        Err(error) => !crate::daemon::client::proves_no_listener(error),
    }
}

async fn probe_before_deadline<F: Future>(
    deadline: tokio::time::Instant,
    future: F,
) -> Option<F::Output> {
    tokio::time::timeout_at(deadline, future).await.ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Classified, ShutdownResult, classify_targets, probe_before_deadline, shutdown_failures,
        shutdown_ours, still_live, stop_targets, stop_verdict, wait_until_stopped,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use thegn_core::config::{Config, DaemonConfig};

    #[test]
    fn stop_reports_an_unverified_listener_as_unknown_and_fails() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let _state = crate::testenv::IsolatedState::new();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("daemon.sock");
        let endpoint = thegn_svc::ipc::IpcEndpoint::for_socket_path(&socket);
        let listener = rt.block_on(async {
            match thegn_svc::ipc::IpcListener::bind_exclusive(&endpoint)
                .await
                .unwrap()
            {
                thegn_svc::ipc::BindOutcome::Bound(listener) => listener,
                thegn_svc::ipc::BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
            }
        });
        let accept_task = rt.spawn(async move {
            let mut listener = listener;
            while let Ok(stream) = listener.accept_stream().await {
                drop(stream);
            }
        });
        let config = Config {
            daemon: DaemonConfig {
                socket: socket.display().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        // stop() builds its own runtime, so run it on a plain thread outside this one.
        let result = std::thread::spawn(move || super::stop(&config))
            .join()
            .unwrap();

        rt.block_on(async {
            accept_task.abort();
            assert!(accept_task.await.is_err());
        });
        let error = result.expect_err("an unverified listener must fail daemon stop");
        assert!(
            error.to_string().contains(&socket.display().to_string()),
            "the failure must name the unverified socket: {error}"
        );
        assert!(
            error.to_string().contains("not confirmed"),
            "unverified listeners must be included in the unknown stop verdict: {error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn classify_targets_reports_a_probe_that_outlasts_the_deadline_as_unknown() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let c = PathBuf::from("c");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let classified = classify_targets(
            vec![a, b.clone(), c.clone()],
            deadline,
            move |path| async move {
                if path == *"a" {
                    crate::daemon::client::StopProbe::NotRunning
                } else {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    crate::daemon::client::StopProbe::Ours
                }
            },
        )
        .await;
        assert!(classified.ours.is_empty());
        assert_eq!(classified.unknown, vec![b, c]);
        assert_eq!(tokio::time::Instant::now(), deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn classify_targets_keeps_an_unverified_listener_out_of_not_running() {
        let classified = classify_targets(
            vec![PathBuf::from("a")],
            tokio::time::Instant::now() + Duration::from_secs(1),
            |_path| async { crate::daemon::client::StopProbe::Unverified("500".into()) },
        )
        .await;
        assert_eq!(
            classified.unverified,
            vec![(PathBuf::from("a"), "500".to_string())]
        );
        assert!(classified.ours.is_empty());
        assert!(classified.unknown.is_empty());
        let verdict = stop_verdict(0, &[], &["a".into()]);
        assert!(verdict.is_err());
        assert!(verdict.unwrap_err().to_string().contains("a"));
    }

    #[test]
    fn a_health_error_that_does_not_prove_absence_keeps_the_target_live() {
        let healthy: anyhow::Result<()> = Ok(());
        let timeout = Err(anyhow::Error::new(
            thegn_svc::control::client::ControlBoundError::RequestTimeout,
        ));
        let refused = Err(anyhow::Error::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));

        assert!(still_live(&healthy));
        assert!(still_live(&timeout));
        assert!(!still_live(&refused));
    }

    #[tokio::test(start_paused = true)]
    async fn classify_targets_keeps_ours_and_foreign_before_the_deadline() {
        let ours = PathBuf::from("ours");
        let foreign = PathBuf::from("foreign");
        let classified = classify_targets(
            vec![ours.clone(), foreign.clone()],
            tokio::time::Instant::now() + Duration::from_secs(1),
            move |path| async move {
                if path == *"ours" {
                    crate::daemon::client::StopProbe::Ours
                } else {
                    crate::daemon::client::StopProbe::Foreign("other".into())
                }
            },
        )
        .await;
        assert_eq!(
            classified,
            Classified {
                ours: vec![ours],
                unknown: Vec::new(),
                foreign: vec![(foreign, "other".into())],
                unverified: vec![],
            }
        );
    }

    #[test]
    fn stop_verdict_with_only_unknown_listeners_is_an_error() {
        let result = stop_verdict(0, &[], &["/run/x.sock".into()]);
        match result {
            Err(error) => assert!(error.to_string().contains("/run/x.sock")),
            Ok(()) => panic!("unknown listener must make stop fail"),
        }
        assert!(stop_verdict(0, &[], &[]).is_ok());
        let result = stop_verdict(
            2,
            &["/run/live-a.sock".into()],
            &["/run/unchecked-b.sock".into()],
        );
        match result {
            Err(error) => {
                let message = error.to_string();
                assert!(message.contains("/run/live-a.sock"));
                assert!(message.contains("/run/unchecked-b.sock"));
            }
            Ok(()) => panic!("live and unknown listeners must make stop fail"),
        }
    }

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

    #[tokio::test(start_paused = true)]
    async fn shutdown_ours_reports_current_plus_two_legacy_listeners_individually() {
        let current = PathBuf::from("current");
        let legacy_a = PathBuf::from("legacy_a");
        let legacy_b = PathBuf::from("legacy_b");
        let ours = vec![current.clone(), legacy_a.clone(), legacy_b.clone()];
        let started = tokio::time::Instant::now();
        let results = shutdown_ours(
            &ours,
            started + Duration::from_secs(3),
            |path| async move {
                if path.as_os_str() == "legacy_a" {
                    Err("503 refused".to_string())
                } else {
                    Ok(())
                }
            },
            |path| async move { path.as_os_str() == "legacy_b" },
        )
        .await;

        assert_eq!(
            results,
            vec![
                (current, ShutdownResult::Stopped),
                (legacy_a, ShutdownResult::Refused("503 refused".to_string())),
                (legacy_b, ShutdownResult::StillRunning),
            ]
        );
        let failures = shutdown_failures(&results, |path| path.display().to_string());
        assert_eq!(
            failures,
            vec!["legacy_a (503 refused)".to_string(), "legacy_b".to_string()]
        );
        let error = stop_verdict(3, &failures, &[]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("2 of 3 listeners still live"));
        assert!(message.contains("legacy_a"));
        assert!(message.contains("legacy_b"));
        let elapsed = tokio::time::Instant::now() - started;
        assert!(elapsed >= Duration::from_secs(3));
        assert!(elapsed < Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ours_with_every_listener_gone_has_no_failures() {
        let ours = vec![
            PathBuf::from("current"),
            PathBuf::from("legacy_a"),
            PathBuf::from("legacy_b"),
        ];
        let results = shutdown_ours(
            &ours,
            tokio::time::Instant::now() + Duration::from_secs(3),
            |_path| async { Ok(()) },
            |_path| async { false },
        )
        .await;

        assert!(
            results
                .iter()
                .all(|(_, result)| matches!(result, ShutdownResult::Stopped))
        );
        let failures = shutdown_failures(&results, |path| path.display().to_string());
        assert!(failures.is_empty());
        assert!(stop_verdict(3, &[], &[]).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_shutdown_request_spends_the_one_total_deadline_and_never_reports_success() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let ours = vec![a.clone(), b.clone()];
        let started = tokio::time::Instant::now();
        let results = shutdown_ours(
            &ours,
            started + Duration::from_secs(2),
            |path| async move {
                if path.as_os_str() == "a" {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                Ok(())
            },
            |_path| async { true },
        )
        .await;

        assert_eq!(
            results[0],
            (
                a,
                ShutdownResult::Refused("stop deadline reached before shutdown".to_string())
            )
        );
        assert_eq!(results[1].1, ShutdownResult::StillRunning);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(2)
        );
    }
}
