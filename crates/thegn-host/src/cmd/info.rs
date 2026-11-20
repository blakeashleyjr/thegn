//! `thegn info` — explain the effective workspace/worktree configuration.

use std::path::{Path, PathBuf};

use anyhow::Result;
use thegn_core::config::{Config, ProcessEnv};
use thegn_core::db::Db;
use thegn_core::info_model::{
    Field, INFO_SCHEMA, InfoReport, InfoTarget, LiveFacts, SessionRow, Source,
};
use thegn_core::store::{WorkspaceStore, ZoneStore};

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Workspace to describe: its slug, name or repo path (default: the one you are in).
    #[arg(long, value_name = "SLUG|PATH")]
    pub workspace: Option<String>,
    /// Worktree to describe: its path or branch (default: the current worktree, if any).
    #[arg(long, value_name = "PATH|BRANCH")]
    pub worktree: Option<String>,
    /// Emit the report as one JSON document.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: Args, config_path: PathBuf) -> Result<()> {
    let here = super::resolve_worktree(None);
    let target = thegn_core::info_facts::resolve_target(
        &Db::open()?,
        args.workspace.as_deref(),
        args.worktree.as_deref(),
        &here,
    )?;
    let (sessions, sessions_source, daemon_note) = daemon_sessions(cfg);
    let live = LiveFacts {
        read_git: true,
        git: None,
        git_source: None,
        activity: None,
        pipeline_stage: None,
        devcontainer: None,
        sessions,
        sessions_source,
        workspace_ahead: None,
    };
    let mut report = gather(cfg, Some(config_path), &target, live)?;
    if let Some(note) = daemon_note {
        report.notes.push(note);
    }
    if args.json {
        super::emit_json(&report)
    } else {
        thegn_core::outln!("{}", thegn_core::info_model::render_text(&report));
        Ok(())
    }
}

pub(crate) fn gather(
    cfg: &Config,
    config_path: Option<PathBuf>,
    target: &InfoTarget,
    live: LiveFacts,
) -> Result<InfoReport> {
    let db = Db::open()?;
    let root = Path::new(&target.repo_root);
    let wt = target.worktree.as_deref().map(Path::new).unwrap_or(root);
    let wt_s = wt.to_string_lossy();

    let mut notes = Vec::new();
    let workspace = thegn_core::info_facts::workspace_identity(cfg, &db, root);
    let workspace_stats =
        thegn_core::info_facts::workspace_stats(&db, root, &live, thegn_core::util::now());
    let worktree = target.worktree.as_deref().map_or_else(Vec::new, |path| {
        thegn_core::info_facts::worktree_fields(
            &db,
            root,
            Path::new(path),
            &live,
            thegn_core::util::now(),
        )
    });

    let (environment_result, environment_notes) = resolve_environment(cfg, &db, root, wt);
    notes.extend(environment_notes);
    let (environment, environment_error) = match environment_result {
        Ok(environment) => (Some(environment), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };

    let (settings, mut settings_notes, backends) = match environment.as_ref() {
        Some(env) => {
            let (settings, notes) = thegn_core::info_resolve::settings(
                cfg,
                config_path,
                &db,
                root,
                wt,
                env,
                &ProcessEnv,
            );
            let saved = db
                .worktree_record(&wt_s)?
                .and_then(|row| row.sandbox_backend);
            let (backend_fields, backends) =
                thegn_core::launch_backend::predict(env, saved.as_deref());
            let mut settings = settings;
            settings.extend(backend_fields);
            (settings, notes, backends)
        }
        None => (
            vec![
                Field::unknown(
                    thegn_core::info_model::keys::ENV_NAME,
                    "environment",
                    environment_error
                        .unwrap_or_else(|| "could not resolve the effective environment".into()),
                )
                .warned(),
            ],
            Vec::new(),
            Vec::new(),
        ),
    };
    notes.append(&mut settings_notes);
    notes.push(
        "devcontainer.json is applied at launch after trust and is not reflected here".into(),
    );
    notes.push("sealed-home and compose gates are applied at launch".into());

    let worktree_paths = if target.worktree.is_some() {
        None
    } else {
        match db.worktrees() {
            Ok(rows) => Some(
                rows.into_iter()
                    .filter(|row| row.repo_root == target.repo_root)
                    .map(|row| row.worktree)
                    .collect(),
            ),
            Err(error) => {
                notes.push(format!("sessions: worktree list unreadable: {error:#}"));
                Some(Vec::new())
            }
        }
    };
    let sessions = live.sessions.map(|rows| {
        let paths: Vec<String> = if let Some(worktree) = target.worktree.as_deref() {
            vec![worktree.to_string()]
        } else {
            worktree_paths.unwrap_or_default()
        };
        thegn_core::info_facts::sessions_for(
            &rows,
            root,
            &paths,
            target.worktree.as_deref().map(Path::new),
        )
    });

    let settings_scope = target
        .worktree
        .as_deref()
        .and_then(|path| {
            db.worktree_record(path)
                .ok()
                .flatten()
                .map(|row| row.branch)
        })
        .map(|branch| format!("worktree {branch}"))
        .unwrap_or_else(|| match target.worktree.as_deref() {
            Some(path) => format!("worktree {path}"),
            None => "workspace root checkout".to_string(),
        });

    Ok(InfoReport {
        schema: INFO_SCHEMA,
        generated_at: thegn_core::util::now(),
        target: target.clone(),
        workspace,
        workspace_stats,
        worktree,
        sessions,
        settings_scope,
        settings,
        backends,
        overrides: thegn_core::info_facts::overrides(
            cfg,
            &db,
            root,
            target.worktree.as_deref().map(Path::new),
        ),
        notes,
    })
}

fn resolve_environment(
    cfg: &Config,
    db: &Db,
    root: &Path,
    worktree: &Path,
) -> (Result<thegn_core::env::Environment>, Vec<String>) {
    let approvals = crate::handlers::repo_trust::approvals_for(db, &root.to_string_lossy());
    let mut notes = Vec::new();
    let environment = crate::handlers::repo_trust::effective_environment_for_worktree(
        cfg,
        Some(db),
        root,
        worktree,
        None,
        &approvals,
    );
    let environment = match environment {
        Ok(mut env) => {
            match db.zone_of_worktree(&worktree.to_string_lossy()) {
                Ok(Some(zone)) => {
                    if let Some(zone_config) = cfg.zone.get(&zone.name) {
                        let dropped = thegn_core::zone::apply_zone_ceilings(
                            &mut env.sandbox,
                            &zone.name,
                            zone_config,
                        );
                        if !dropped.is_empty() {
                            notes.push(format!(
                                "zone {} dropped {} network allowance(s)",
                                zone.name,
                                dropped.len()
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => notes.push(format!("zone: {error:#}")),
            }
            Ok(env)
        }
        Err(error) => Err(error),
    };
    if let Err(error) = &environment {
        notes.push(format!("environment: {error:#}"));
    }
    (environment, notes)
}

pub(crate) fn session_row(s: &thegn_svc::control::SessionInfo) -> SessionRow {
    let program = thegn_core::log_redact::redact_text_line(&s.program);
    let token = program.split_whitespace().next().unwrap_or("");
    let program = Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
        .to_string();
    SessionRow {
        id: s.id.clone(),
        program,
        worktree: s.worktree.clone(),
        attached: s.attached_clients,
    }
}

fn daemon_sessions(cfg: &Config) -> (Option<Vec<SessionRow>>, Option<Source>, Option<String>) {
    match crate::agent::block_on_provider(|| async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            super::session::connect(cfg).await?.sessions().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not answer within 2 s"))?
    }) {
        Ok(sessions) => (
            Some(
                sessions
                    .iter()
                    .filter(|session| session.exited_at_ms.is_none())
                    .map(session_row)
                    .collect(),
            ),
            Some(Source::Daemon),
            None,
        ),
        Err(error) => {
            tracing::debug!(target: "thegn::info", "sessions: {error:#} (no daemon?)");
            (
                None,
                None,
                Some(format!("sessions: {error:#} (no daemon?)")),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn info_args_parse_workspace_and_worktree_targets() {
        let workspace =
            crate::Cli::try_parse_from(["thegn", "info", "--workspace", "thegn", "--json"])
                .unwrap();
        assert!(matches!(
            workspace.command,
            Some(crate::Command::Info(Args { json: true, .. }))
        ));
        let worktree = crate::Cli::try_parse_from(["thegn", "info", "--worktree", "feat"]).unwrap();
        assert!(
            matches!(worktree.command, Some(crate::Command::Info(Args { worktree: Some(path), .. })) if path == "feat")
        );
    }

    #[test]
    fn session_row_redacts_flags_and_keeps_program_basename() {
        let session = thegn_svc::control::SessionInfo {
            id: "s1".into(),
            program: "/usr/bin/codex --token secret".into(),
            worktree: Some("/repo/wt".into()),
            attached_clients: 2,
            ..Default::default()
        };
        let row = session_row(&session);
        assert_eq!(row.program, "codex");
        assert_eq!(row.attached, 2);
    }

    #[test]
    fn gather_reports_effective_environment_and_schema() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let db = Db::open().unwrap();
        db.put_workspace(&root, "demo", "repo").unwrap();
        db.put_worktree("demo/feat", &root, &root, "feat", None, None)
            .unwrap();

        let report = gather(
            &Config::default(),
            None,
            &InfoTarget {
                repo_root: root.clone(),
                worktree: Some(root),
            },
            LiveFacts::default(),
        )
        .unwrap();
        assert_eq!(report.schema, 1);
        assert!(
            report
                .settings
                .iter()
                .any(|field| field.key == thegn_core::info_model::keys::SANDBOX_SELECTED_BACKEND)
        );
        assert!(
            report
                .settings
                .iter()
                .any(|field| field.key == thegn_core::info_model::keys::ENV_NAME)
        );
    }

    #[test]
    fn gather_environment_matches_trusted_environment_resolution() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let db = Db::open().unwrap();
        db.put_workspace(&root, "demo", "repo").unwrap();
        db.put_worktree("demo/feat", &root, &root, "feat", None, None)
            .unwrap();
        let cfg = Config::default();
        let approvals = crate::handlers::repo_trust::approvals_for(&db, &root);
        let expected = crate::handlers::repo_trust::effective_environment_for_worktree(
            &cfg,
            Some(&db),
            Path::new(&root),
            Path::new(&root),
            None,
            &approvals,
        )
        .unwrap();
        let (actual, _) = resolve_environment(&cfg, &db, Path::new(&root), Path::new(&root));
        let actual = actual.unwrap();
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.placement.label(), expected.placement.label());
        assert_eq!(
            serde_json::to_value(&actual.sandbox).unwrap(),
            serde_json::to_value(&expected.sandbox).unwrap()
        );
    }
}
