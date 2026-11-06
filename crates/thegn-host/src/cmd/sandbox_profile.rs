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
    let source = if selection.profile.is_some() {
        "pinned for this worktree"
    } else if selection.name.is_some() {
        "[env.<name>] or configuration"
    } else if cfg.sandbox.profile != SandboxProfile::default() {
        "config"
    } else {
        "default"
    };
    let row = ProfileRow {
        worktree: worktree.to_string_lossy().into_owned(),
        profile: profile.as_str().into(),
        source: source.into(),
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

fn target_path(target: Option<String>) -> PathBuf {
    super::resolve_worktree(target)
}

#[cfg(test)]
mod tests {
    use super::{Action, run};
    use thegn_core::config::Config;

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
