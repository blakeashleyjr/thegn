//! Daemon-side helpers for the sessions.fork operation.
//!
//! The service owns the orchestration and PTY spawn; this module keeps the
//! filesystem handoff and live-source conversion small and auditable. Recipes
//! never leave the live session table.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use thegn_core::control_wire::EventFrame;
use thegn_core::session_fork::{DaemonRecipe, ForkSource};
use thegn_core::store::IntentStore;
use thegn_svc::control::{ControlError, ControlResult, ForkSpec, OpenSpec, SessionInfo};
use tokio::sync::mpsc;

use super::service::{DaemonService, Lookup, SessionEntry};
use super::session::{LiveMeta, SessionActor, SessionMeta, SessionMsg};

pub(crate) fn source_geometry(live: &LiveMeta) -> (u16, u16) {
    (live.rows.max(1), live.cols.max(1))
}

/// The common daemon-owned spawn path for an opened or forked session. Keeping
/// PTY creation, actor registration, and teardown ownership here prevents fork
/// from growing a second subtly different version of the open path.
pub(crate) struct SpawnRequest {
    pub id: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub worktree: Option<String>,
    pub program: String,
    pub recipe: Option<DaemonRecipe>,
    pub forked_from: Option<String>,
    pub handoff: Option<PathBuf>,
}

pub(crate) async fn spawn_session(
    service: &DaemonService,
    request: SpawnRequest,
) -> ControlResult<SessionInfo> {
    if request.argv.is_empty() {
        cleanup_handoff(request.handoff.as_deref());
        return Err(ControlError::Conflict("empty argv".into()));
    }
    let (pane_tx, pane_rx) = mpsc::channel(256);
    let cwd = request.cwd.as_ref().map(std::path::PathBuf::from);
    let pty = crate::pane_pty::open_pty(
        0,
        &request.argv,
        cwd.as_deref(),
        &request.env,
        request.rows,
        request.cols,
        pane_tx,
        None,
        None,
    )
    .map_err(|error| {
        cleanup_handoff(request.handoff.as_deref());
        ControlError::Internal(error)
    })?;
    let meta = SessionMeta {
        id: request.id.clone(),
        worktree: request.worktree.clone(),
        program: request.program,
        cwd: request.cwd,
        created_at_ms: super::now_ms(),
        pid: pty.pid,
        forked_from: request.forked_from,
    };
    let live = Arc::new(Mutex::new(LiveMeta {
        rows: request.rows,
        cols: request.cols,
        ..Default::default()
    }));
    let (msg_tx, msg_rx) = mpsc::channel(64);
    let actor = SessionActor::new(
        meta.clone(),
        live.clone(),
        pty,
        request.rows,
        request.cols,
        service.events.clone(),
        service.idle_tx.clone(),
        service.sessions.clone(),
        service.tombs.clone(),
        service.db.clone(),
        service.config.clone(),
        request.handoff,
    );
    let info = {
        let live = live.lock().expect("live meta lock");
        meta.info(&live, None)
    };
    // Insert before spawning: actor teardown removes its own entry, so an
    // instantly-exiting child must never race registration and leave a phantom.
    service.sessions.lock().await.insert(
        request.id,
        SessionEntry {
            msg_tx,
            meta,
            live,
            recipe: request.recipe,
        },
    );
    tokio::spawn(actor.run(pane_rx, msg_rx));
    service.emit(EventFrame::Sessions);
    Ok(info)
}

/// Orchestrate the control-plane fork. The service implementation is only a
/// boundary; source resolution, history handoff, launch resolution, and
/// lineage persistence all live with the daemon fork code.
pub(crate) fn run<'a>(
    service: &'a DaemonService,
    spec: ForkSpec,
) -> BoxFuture<'a, ControlResult<SessionInfo>> {
    Box::pin(async move {
        let (source, source_tx, source_geometry) = if let Some(harness) = spec.harness.clone() {
            (
                thegn_core::session_fork::ForkSource::HarnessSession {
                    harness,
                    id: spec.session.clone(),
                    agent: spec.agent.clone(),
                    worktree: spec.worktree.clone(),
                },
                None,
                (24, 80),
            )
        } else {
            match service.lookup(&spec.session).await {
                Lookup::Live(tx) => {
                    let sessions = service.sessions.lock().await;
                    let entry = sessions
                        .get(&spec.session)
                        .ok_or_else(|| DaemonService::not_found(&spec.session))?;
                    let source = source(entry).ok_or_else(|| {
                        ControlError::Conflict(format!(
                            "session {} has no retained fork recipe",
                            spec.session
                        ))
                    })?;
                    let live = entry.live.lock().expect("live meta lock");
                    let geometry = source_geometry(&live);
                    (source, Some(tx), geometry)
                }
                Lookup::Dead(_) => {
                    return Err(ControlError::Conflict(format!(
                        "session {} has exited; use sessions.open for a cold start",
                        spec.session
                    )));
                }
                Lookup::Unknown => return Err(DaemonService::not_found(&spec.session)),
            }
        };
        let options = thegn_core::session_fork::ForkOptions {
            cwd: spec.cwd.clone(),
            worktree: spec.worktree.clone(),
            scrollback: spec.scrollback,
            adopt: spec.adopt,
            placement: if spec.tab {
                thegn_core::session_fork::ForkPlacement::NewTab
            } else {
                thegn_core::session_fork::ForkPlacement::Sibling
            },
        };
        let plan = thegn_core::session_fork::ForkRequest { source, options }
            .plan()
            .map_err(|error| ControlError::Conflict(error.to_string()))?;
        let history = if spec.scrollback {
            if let Some(tx) = source_tx {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(SessionMsg::HistoryTail { reply: reply_tx })
                    .await
                    .map_err(|_| DaemonService::not_found(&spec.session))?;
                Some(
                    reply_rx
                        .await
                        .map_err(|_| DaemonService::not_found(&spec.session))?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let child_id = super::service::fresh_id();
        let (argv, cwd, env, worktree, program, recipe) = match &plan {
            thegn_core::session_fork::ForkPlan::Raw {
                argv,
                cwd,
                env,
                worktree,
                ..
            } => (
                thegn_core::sandbox_cpucap::wrap_control_argv(argv.clone(), false),
                cwd.clone(),
                env.clone(),
                worktree.clone(),
                crate::pane::program_name(argv),
                DaemonRecipe::Raw(thegn_core::session_fork::RawLaunchRecipe {
                    argv: argv.clone(),
                    cwd: cwd.clone(),
                    env: env.clone(),
                    worktree: worktree.clone(),
                }),
            ),
            thegn_core::session_fork::ForkPlan::Harness {
                harness,
                native_session_id,
                agent,
                cwd,
                worktree,
                command,
                ..
            } => {
                let agent = agent.clone().unwrap_or_else(|| harness.clone());
                let open = OpenSpec {
                    argv: Vec::new(),
                    cwd: cwd.clone(),
                    env: Vec::new(),
                    rows: source_geometry.0,
                    cols: source_geometry.1,
                    worktree: worktree.clone(),
                    agent: Some(thegn_svc::control::AgentLaunch {
                        agent: agent.clone(),
                        prompt: String::new(),
                        headless: Some(false),
                        bind_worktree: false,
                        resume: None,
                        continue_last: false,
                        stage: None,
                        fork: true,
                        native_session_id: Some(native_session_id.clone()),
                    }),
                    adopt: false,
                    already_capped: false,
                };
                let snapshot = service.config.clone();
                let launch = open.agent.as_ref().expect("fork agent").clone();
                let source_harness = harness.clone();
                let source_command = command.clone();
                let resolved = service
                    .with_db(move |db| {
                        let fresh = crate::config_source::fresh();
                        let cfg = fresh.as_ref().unwrap_or(&snapshot);
                        super::agent_open::resolve_fork(
                            cfg,
                            db,
                            &open,
                            &launch,
                            &source_harness,
                            &source_command,
                        )
                    })
                    .await
                    .map_err(|error| ControlError::Conflict(error.to_string()))?;
                let cwd = resolved
                    .cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned());
                (
                    resolved.argv,
                    cwd.clone(),
                    resolved.env,
                    worktree.clone(),
                    agent.clone(),
                    DaemonRecipe::Agent {
                        harness: harness.clone(),
                        native_session_id: None,
                        agent: Some(agent),
                        cwd: cwd.clone(),
                        worktree: worktree.clone(),
                    },
                )
            }
        };
        // Resolve the complete launch before creating a handoff file. If a
        // fresh-config/provider lookup rejects a native fork, no orphaned
        // context file should remain in the state directory.
        let handoff = match history.as_deref().filter(|text| !text.is_empty()) {
            Some(text) => Some(write_handoff(&child_id, text).map_err(ControlError::Internal)?),
            None => None,
        };
        let handoff_s = handoff
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let env = thegn_core::session_fork::compose_identity_env(
            &env,
            &child_id,
            &service.endpoint,
            plan.lineage(),
            handoff_s.as_deref(),
        );
        let info = spawn_session(
            service,
            SpawnRequest {
                id: child_id.clone(),
                argv,
                cwd,
                env,
                rows: source_geometry.0,
                cols: source_geometry.1,
                worktree: worktree.clone(),
                program,
                recipe: Some(recipe),
                forked_from: Some(plan.lineage().to_string()),
                handoff,
            },
        )
        .await?;
        if spec.adopt {
            let payload = thegn_core::models::AdoptIntent {
                session: child_id.clone(),
                worktree,
                focus: false,
                tab: spec.tab,
            };
            if let Err(error) = service
                .with_db(move |db| {
                    db.put_intent("adopt_session", &serde_json::to_string(&payload)?)?;
                    Ok(())
                })
                .await
            {
                tracing::warn!(target: "thegn::daemon", "adopt intent for {child_id} failed: {error}");
            }
        }
        let record = thegn_core::session_fork::ForkRecord::from_plan(
            &child_id,
            &plan,
            super::now_ms() / 1000,
        );
        let _ = service
            .with_db(move |db| {
                use thegn_core::store::SessionForkStore;
                db.put_session_fork(&record)
            })
            .await;
        Ok(info)
    })
}

/// Convert a live entry into the pure core source. The entry may be a test or
/// legacy stub without a retained recipe; such a session cannot honestly be
/// forked.
pub(crate) fn source(entry: &SessionEntry) -> Option<ForkSource> {
    Some(ForkSource::DaemonSession {
        id: entry.meta.id.clone(),
        recipe: entry.recipe.clone()?,
    })
}

/// Owner-only handoff path for an optional bounded scrollback context.
pub(crate) fn handoff_path(child_id: &str) -> PathBuf {
    thegn_core::util::xdg_state_home()
        .join("thegn")
        .join("forks")
        .join(format!("{child_id}.txt"))
}

/// Write a plain-text handoff before the child is spawned.
pub(crate) fn write_handoff(child_id: &str, text: &str) -> Result<PathBuf> {
    let path = handoff_path(child_id);
    let dir = path.parent().context("fork handoff has no parent")?;

    // Do not follow an attacker-controlled `forks` symlink. The final file is
    // opened with CREATE|EXCL as well, so a pre-existing file or symlink can
    // never be truncated by a scrollback handoff.
    if let Ok(metadata) = std::fs::symlink_metadata(dir)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        anyhow::bail!(
            "fork handoff directory is not a real directory: {}",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    thegn_core::fsperm::restrict_dir_to_owner(dir)
        .with_context(|| format!("restrict {}", dir.display()))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) => {
            return Err(error).with_context(|| format!("create {}", path.display()));
        }
    };
    let result = (|| {
        use std::io::Write;
        file.write_all(text.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        thegn_core::fsperm::restrict_to_owner(&path)
            .with_context(|| format!("restrict {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// Best-effort lifecycle cleanup when the child exits.
pub(crate) fn cleanup_handoff(path: Option<&std::path::Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path); // best-effort: fork context is disposable after child exit
    }
}

/// Return the harness recipe for a normal configured-agent open.
pub(crate) fn agent_recipe(
    cfg: &thegn_core::config::Config,
    launch: &thegn_svc::control::AgentLaunch,
    spec: &thegn_svc::control::OpenSpec,
) -> Option<DaemonRecipe> {
    let harness = super::agent_open::harness_for_agent(cfg, &launch.agent)?;
    Some(DaemonRecipe::Agent {
        harness: harness.id().to_string(),
        native_session_id: launch.native_session_id.clone(),
        agent: Some(launch.agent.clone()),
        cwd: spec.cwd.clone(),
        worktree: spec.worktree.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{cleanup_handoff, handoff_path, source_geometry, write_handoff};
    use crate::daemon::session::LiveMeta;

    #[test]
    fn handoff_path_is_scoped_to_the_forks_directory() {
        let path = handoff_path("child");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("child.txt")
        );
        assert_eq!(
            path.parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str()),
            Some("forks")
        );
    }

    #[test]
    fn cleanup_handoff_removes_a_disposable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("handoff.txt");
        std::fs::write(&path, "context").expect("write handoff");
        cleanup_handoff(Some(&path));
        assert!(!path.exists());
    }

    #[test]
    fn handoff_never_overwrites_a_preexisting_file_link() {
        let state = tempfile::tempdir().expect("state tempdir");
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            state.path().to_str().expect("state path"),
        )]);
        let dir = state.path().join("thegn/forks");
        std::fs::create_dir_all(&dir).expect("fork directory");
        let target = state.path().join("outside.txt");
        std::fs::write(&target, "must remain unchanged").expect("target");
        std::fs::hard_link(&target, dir.join("child.txt")).expect("hard link");

        let error = write_handoff("child", "attacker-controlled context")
            .expect_err("existing link must be rejected");
        assert!(error.to_string().contains("create"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "must remain unchanged"
        );
    }

    #[test]
    fn source_geometry_uses_the_live_resized_dimensions() {
        let live = LiveMeta {
            rows: 41,
            cols: 137,
            ..Default::default()
        };
        assert_eq!(source_geometry(&live), (41, 137));
    }
}
