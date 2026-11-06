//! `thegn sandbox profile` — inspect or change the worktree isolation pin.

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use thegn_core::config::{Config, SandboxProfile};
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::sandbox_pin::{explain, parse_pin};
use thegn_core::store::WorkspaceStore;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    Show {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    Set {
        profile: String,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    Clear {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
}

#[derive(Serialize)]
struct ProfileRow {
    worktree: String,
    profile: String,
    source: String,
    explanation: &'static str,
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    if std::env::var_os("THEGN_SANDBOX").is_some() {
        bail!(
            "sandbox profile pins are host-owned; this command cannot change them from inside a sandbox"
        );
    }
    match action {
        Action::Show { json, target } => show(cfg, target.get(), json),
        Action::Set {
            profile,
            json,
            target,
        } => {
            let profile = parse_pin(&profile)?;
            let worktree = target_path(target.get());
            let db = Db::open()?;
            db.set_worktree_sandbox_profile(&worktree.to_string_lossy(), Some(profile))?;
            report(cfg, &worktree, json, true)
        }
        Action::Clear { json, target } => {
            let worktree = target_path(target.get());
            let db = Db::open()?;
            db.set_worktree_sandbox_profile(&worktree.to_string_lossy(), None)?;
            report(cfg, &worktree, json, true)
        }
    }
}

fn show(cfg: &Config, target: Option<String>, json: bool) -> Result<()> {
    let worktree = target_path(target);
    report(cfg, &worktree, json, false)
}

fn report(cfg: &Config, worktree: &Path, json: bool, changed: bool) -> Result<()> {
    let repo = thegn_core::repo::main_worktree(worktree).unwrap_or_else(|| worktree.to_path_buf());
    let selection = crate::sandbox_profile_ui::selection_for(&worktree.to_string_lossy(), &repo)?;
    let loc = thegn_core::remote::GitLoc::for_worktree(worktree);
    let env = cfg.resolve_env(&repo, &loc, worktree, &selection);
    let profile = env.sandbox.profile;
    let source = profile_source(cfg, &repo, &selection, profile);
    let row = ProfileRow {
        worktree: worktree.to_string_lossy().into_owned(),
        profile: profile.as_str().into(),
        source,
        explanation: explain(profile).one_liner,
    };
    if json {
        super::emit_json(&row)?;
    } else {
        outln!(
            "{}: {} ({}) — {}",
            row.worktree,
            row.profile,
            row.source,
            row.explanation
        );
        if changed {
            outln!("Applies to new panes; restart open panes to use the new profile.");
        }
    }
    Ok(())
}

fn profile_source(
    cfg: &Config,
    repo: &Path,
    selection: &thegn_core::sandbox_pin::EnvSelection,
    profile: SandboxProfile,
) -> String {
    if selection.profile.is_some() {
        return "pinned for this worktree".into();
    }
    let mut sources = Vec::new();
    if let Some(name) = selection.name.as_deref()
        && cfg
            .env
            .get(name)
            .and_then(|env| env.sandbox.profile)
            .is_some_and(|configured| configured == profile)
    {
        sources.push(format!("[env.{name}]"));
    }
    let repo_profile = thegn_core::config::discover_repo_overlay(repo)
        .selected()
        .and_then(|candidate| {
            thegn_core::config::parse_overlay_value(&candidate.body, candidate.format).ok()
        })
        .and_then(|value| {
            value
                .get("sandbox")?
                .get("profile")?
                .as_str()
                .map(str::to_string)
        });
    if repo_profile
        .as_deref()
        .and_then(|value| thegn_core::sandbox_pin::parse_pin(value).ok())
        .is_some_and(|configured| configured == profile)
    {
        sources.push("repository .thegn.*".into());
    }
    if cfg.sandbox.profile == profile {
        sources.push("global [sandbox] profile".into());
    }
    if sources.is_empty() {
        "built-in default".into()
    } else {
        sources.join(" + ")
    }
}

fn target_path(target: Option<String>) -> PathBuf {
    super::resolve_worktree(target)
}

#[cfg(test)]
mod tests {
    use super::{Action, profile_source, run};
    use thegn_core::config::{Config, SandboxProfile};
    use thegn_core::db::Db;
    use thegn_core::sandbox_pin::EnvSelection;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn sandbox_profile_cli_set_persists_the_worktree_override() {
        let _state = crate::testenv::IsolatedState::new();
        let worktree = tempfile::tempdir().unwrap();
        let path = worktree.path().to_string_lossy().into_owned();
        run(
            &Config::default(),
            Action::Set {
                profile: "sealed".into(),
                json: true,
                target: super::super::target::WorktreeTarget {
                    worktree: Some(path.clone()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let db = Db::open().unwrap();
        assert_eq!(
            db.worktree_sandbox_profile(&path).unwrap(),
            Some(SandboxProfile::Sealed)
        );
    }

    #[test]
    fn sandbox_profile_cli_reports_every_matching_profile_source() {
        let cfg: Config =
            toml::from_str("[sandbox]\nprofile='sealed'\n[env.dev.sandbox]\nprofile='sealed'\n")
                .unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".thegn.toml"),
            "[sandbox]\nprofile='sealed'\n",
        )
        .unwrap();
        assert_eq!(
            profile_source(
                &cfg,
                repo.path(),
                &EnvSelection::named("dev"),
                SandboxProfile::Sealed,
            ),
            "[env.dev] + repository .thegn.* + global [sandbox] profile"
        );
        assert_eq!(
            profile_source(
                &cfg,
                repo.path(),
                &EnvSelection::named("dev").with_profile(SandboxProfile::Hardened),
                SandboxProfile::Hardened,
            ),
            "pinned for this worktree"
        );
    }

    #[test]
    fn sandbox_profile_cli_refuses_inside_a_sandbox() {
        let _state = crate::testenv::EnvGuard::set(&[("THEGN_SANDBOX", "1")]);
        let error = run(
            &Config::default(),
            Action::Show {
                json: false,
                target: super::super::target::WorktreeTarget::default(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("sandbox profile pins are host-owned"));
    }
}
