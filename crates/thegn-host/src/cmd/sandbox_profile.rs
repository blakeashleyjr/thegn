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
        #[arg(id = "isolation", value_name = "PROFILE")]
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
    thegn_core::sandbox_pin::refuse_inside_sandbox(std::env::var_os("THEGN_SANDBOX").is_some())?;
    match action {
        Action::Show { json, target } => show(cfg, target.get(), json),
        Action::Set {
            profile,
            json,
            target,
        } => {
            let profile = parse_pin(&profile)?;
            let worktree = target_path(target.get())?;
            let db = Db::open()?;
            db.set_worktree_sandbox_profile(&worktree.to_string_lossy(), Some(profile))?;
            report(cfg, &worktree, json, true)
        }
        Action::Clear { json, target } => {
            let worktree = target_path(target.get())?;
            let db = Db::open()?;
            db.set_worktree_sandbox_profile(&worktree.to_string_lossy(), None)?;
            report(cfg, &worktree, json, true)
        }
    }
}

fn show(cfg: &Config, target: Option<String>, json: bool) -> Result<()> {
    let worktree = target_path(target)?;
    report(cfg, &worktree, json, false)
}

fn report(cfg: &Config, worktree: &Path, json: bool, changed: bool) -> Result<()> {
    let db = Db::open()?;
    let repo = db
        .repo_root_for(&worktree.to_string_lossy())?
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{} is not a registered worktree", worktree.display()))?;
    let selection =
        db.effective_env_selection(&worktree.to_string_lossy(), &repo.to_string_lossy())?;
    let location = db.location_for(&worktree.to_string_lossy())?;
    let loc = thegn_core::remote::GitLoc::from_db(&worktree.to_string_lossy(), location.as_deref());
    let trusted = crate::handlers::repo_trust::resolve_env_trusted(
        cfg,
        &repo,
        &loc,
        &worktree.to_string_lossy(),
        &selection,
    )?;
    let profile = trusted.environment.sandbox.profile;
    let base_profile = cfg
        .resolve_env(&repo, &loc, worktree, &selection)
        .sandbox
        .profile;
    let source = profile_source(cfg, &repo, &selection, profile, profile != base_profile);
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
    zone_raised: bool,
) -> String {
    if zone_raised {
        return "zone floor".into();
    }
    if selection.profile.is_some() {
        return "pinned for this worktree".into();
    }
    if let Some(name) = selection.name.as_deref()
        && cfg
            .env
            .get(name)
            .and_then(|env| env.sandbox.profile)
            .is_some()
    {
        return format!("[env.{name}]");
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
        .and_then(|value| SandboxProfile::from_str_validated(value).ok())
        == Some(profile)
    {
        return "repository .thegn.*".into();
    }
    if cfg.sandbox.profile == profile {
        return "global [sandbox] profile".into();
    }
    "built-in default".into()
}

fn target_path(target: Option<String>) -> Result<PathBuf> {
    let requested = super::resolve_worktree(target);
    let db = Db::open()?;
    let worktree = match std::fs::canonicalize(&requested) {
        Ok(worktree) => worktree,
        Err(error) => {
            let requested_key = requested.to_string_lossy();
            if db
                .repo_root_for(&requested_key)?
                .is_some_and(|root| !root.is_empty())
            {
                PathBuf::from(requested_key.as_ref())
            } else {
                return Err(anyhow::anyhow!(
                    "cannot resolve worktree path {}: {error}",
                    requested.display()
                ));
            }
        }
    };
    let registered = db
        .repo_root_for(&worktree.to_string_lossy())?
        .is_some_and(|root| !root.is_empty());
    if !registered {
        bail!("{} is not a registered worktree", worktree.display());
    }
    Ok(worktree)
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
        let db = Db::open().unwrap();
        db.put_worktree("repo/main", &path, &path, "main", None, None)
            .unwrap();
        run(
            &Config::default(),
            Action::Set {
                profile: "sealed".into(),
                json: true,
                target: super::super::target::WorktreeTarget {
                    worktree: Some(format!("{path}/.")),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            db.worktree_sandbox_profile(&path).unwrap(),
            Some(SandboxProfile::Sealed)
        );
    }

    #[test]
    fn sandbox_profile_cli_refuses_an_unregistered_canonical_path() {
        let _state = crate::testenv::IsolatedState::new();
        let worktree = tempfile::tempdir().unwrap();
        let requested = format!("{}/.", worktree.path().display());
        let error = run(
            &Config::default(),
            Action::Show {
                json: false,
                target: super::super::target::WorktreeTarget {
                    worktree: Some(requested),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("is not a registered worktree"));
    }

    #[test]
    fn sandbox_profile_cli_accepts_an_exact_registered_remote_path() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        db.put_worktree(
            "repo/remote",
            "/repo",
            "/remote-only/wt",
            "remote",
            None,
            None,
        )
        .unwrap();

        let resolved = super::target_path(Some("/remote-only/wt".into())).unwrap();
        assert_eq!(resolved, std::path::PathBuf::from("/remote-only/wt"));
    }

    #[test]
    fn sandbox_profile_cli_reports_the_deciding_profile_source() {
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
                false,
            ),
            "[env.dev]"
        );
        assert_eq!(
            profile_source(
                &cfg,
                repo.path(),
                &EnvSelection::named("dev").with_profile(SandboxProfile::Hardened),
                SandboxProfile::Hardened,
                false,
            ),
            "pinned for this worktree"
        );
    }

    #[test]
    fn sandbox_profile_cli_refuses_inside_a_sandbox() {
        let _state = crate::testenv::EnvVarGuard::set(&[("THEGN_SANDBOX", "1")]);
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
