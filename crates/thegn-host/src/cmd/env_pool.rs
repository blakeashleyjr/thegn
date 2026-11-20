//! `thegn env pool-fill`: foreground warm-pool provisioning.
//!
//! This is deliberately a CLI-only provisioning path. The TUI's background
//! reconciler owns its own worker thread; this command keeps the provider work
//! in the foreground and never runs the reconciler.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use clap::Args;
use thegn_core::config::{Config, EnvProviderKind};
use thegn_core::outln;
use thegn_core::store::PoolStore;

#[derive(Debug, Clone, Args)]
pub struct PoolFillArgs {
    #[command(flatten)]
    pub target: super::target::WorktreeTarget,
    /// Set this project's warm-pool size first (0-8; the same setting as the TUI's +/- keys).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=8))]
    pub size: Option<u8>,
    #[arg(long)]
    pub json: bool,
}

/// Fill the warm pool for the selected worktree's project and environment.
pub fn run(cfg: &Config, args: PoolFillArgs) -> Result<()> {
    let wt = super::resolve_worktree(args.target.get())
        .to_string_lossy()
        .into_owned();
    let db = thegn_core::db::Db::open()?;
    let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(&wt));
    let (repo_root, repo, env_name) = crate::lifecycle::pool_context(&db, cfg, &wt, &loc)?;

    let Some(env_config) = cfg.env.get(&env_name) else {
        anyhow::bail!(
            "env {env_name} is not a provider env; the warm pool needs placement = \"provider\""
        );
    };
    if !crate::provision_gate::poolable_env(cfg, &env_name) {
        anyhow::bail!(
            "env {env_name} is not a provider env; the warm pool needs placement = \"provider\""
        );
    }
    if EnvProviderKind::of(&env_config.provider.provider) == EnvProviderKind::Vercel {
        anyhow::bail!(
            "the warm pool is not available for Vercel envs (only an explicit open creates a Vercel sandbox)"
        );
    }

    if let Some(size) = args.size {
        db.set_pool_target(&repo, &env_name, i64::from(size))?;
        if !args.json {
            outln!("warm pool for {repo} ({env_name}): size {size}");
            if size == 0 {
                outln!("the TUI drains extra spares; nothing is destroyed here");
            }
        }
    }

    let target = crate::lifecycle::effective_pool_target(&db, cfg, &repo, &env_name)?;
    if target == 0 {
        if args.json {
            return emit_result(&repo, &env_name, target, 0, 0, &[], &[]);
        }
        outln!("the warm pool is off for {repo} ({env_name}): pass --size N");
        return Ok(());
    }

    let lease = match crate::pool_lease::pool_lease(&repo, &env_name) {
        Ok(lease) => lease,
        Err(crate::pool_lease::LeaseError::Busy) => anyhow::bail!(
            "another warm-pool reconcile for {repo} ({env_name}) is running (the TUI); try again in a few seconds"
        ),
        Err(error) => {
            anyhow::bail!("cannot establish warm-pool reconcile for {repo} ({env_name}): {error}")
        }
    };

    let spares = db.pool_spares_for(&repo, &env_name)?;
    let ready_before = spares.iter().filter(|spare| spare.state == "ready").count();
    let provisioning_before = spares
        .iter()
        .filter(|spare| spare.state == "provisioning")
        .count();
    let needed = pool_deficit(target, ready_before, provisioning_before);
    let mut reserved = Vec::with_capacity(needed);
    let mut failed = Vec::new();
    for _ in 0..needed {
        match crate::provision_gate::reserve_spare(&repo_root, &env_name) {
            Ok(spare) => reserved.push(spare),
            Err(error) => {
                failed.push(format!("{error:#}"));
                break;
            }
        }
    }
    drop(lease);
    drop(db);

    let mut filled = Vec::new();
    for spare in reserved {
        let mut seen = HashSet::new();
        match crate::provision_gate::provision_reserved(
            cfg,
            &repo_root,
            &env_name,
            spare,
            |steps| {
                for step in steps {
                    if step.state == crate::agent::ProvisionState::Active
                        && seen.insert(step.label.clone())
                        && !args.json
                    {
                        outln!("  {}", step.label);
                    }
                }
            },
        ) {
            Ok(name) => filled.push(name),
            Err(error) => failed.push(format!("{error:#}")),
        }
    }

    if args.json {
        emit_result(
            &repo,
            &env_name,
            target,
            ready_before,
            provisioning_before,
            &filled,
            &failed,
        )?;
    } else {
        let names = filled.join(", ");
        outln!(
            "filled {} spare(s) for {repo} ({env_name}): {names}",
            filled.len()
        );
        for error in &failed {
            outln!("! spare failed: {error}");
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} warm-pool spare(s) failed", failed.len())
    }
}

fn emit_result(
    repo: &str,
    env: &str,
    target: usize,
    ready_before: usize,
    provisioning_before: usize,
    filled: &[String],
    failed: &[String],
) -> Result<()> {
    super::emit_json(&serde_json::json!({
        "repo": repo,
        "env": env,
        "target": target,
        "ready_before": ready_before,
        "provisioning_before": provisioning_before,
        "filled": filled,
        "failed": failed.iter().map(|error| serde_json::json!({ "error": error })).collect::<Vec<_>>(),
    }))
}

/// Spares to create: `target - (ready + provisioning)`, never negative.
pub(crate) fn pool_deficit(target: usize, ready: usize, provisioning: usize) -> usize {
    target.saturating_sub(ready.saturating_add(provisioning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use thegn_core::config::{EnvConfig, PlacementMode};
    use thegn_core::store::WorkspaceStore as _;

    #[test]
    fn pool_deficit_table() {
        for (target, ready, provisioning, expected) in [
            (0, 0, 0, 0),
            (3, 3, 0, 0),
            (3, 1, 2, 0),
            (4, 1, 1, 2),
            (2, usize::MAX, 1, 0),
            (usize::MAX, usize::MAX, usize::MAX, 0),
        ] {
            assert_eq!(pool_deficit(target, ready, provisioning), expected);
        }
    }

    #[test]
    fn clap_rejects_pool_size_above_ceiling() {
        assert!(crate::Cli::try_parse_from(["thegn", "env", "pool-fill", "--size", "9"]).is_err());
    }

    #[test]
    fn clap_parses_pool_size_and_json() {
        let cli =
            crate::Cli::try_parse_from(["thegn", "env", "pool-fill", "--size", "2", "--json"])
                .expect("pool-fill should parse");
        assert!(matches!(
            cli.command,
            Some(crate::Command::Env {
                action: crate::cmd::env::Action::PoolFill(PoolFillArgs {
                    size: Some(2),
                    json: true,
                    ..
                })
            })
        ));
    }

    fn refusal_fixture(
        env_name: &str,
        env: EnvConfig,
    ) -> (crate::testenv::IsolatedState, Config, String) {
        let state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().expect("fixture directory");
        let wt = dir.path().to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().expect("fixture database");
        db.put_worktree("pool-fill-test", &wt, &wt, "main", None, None)
            .expect("record fixture worktree");
        db.set_worktree_env(&wt, env_name)
            .expect("select fixture env");
        drop(db);
        let cfg = Config {
            env: [(env_name.to_owned(), env)].into(),
            ..Config::default()
        };
        (state, cfg, wt)
    }

    #[test]
    fn refuses_vercel_without_pool_rows() {
        let (state, cfg, wt) = refusal_fixture(
            "vercel",
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: thegn_core::config::EnvProviderConfig {
                    provider: "vercel".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let error = run(
            &cfg,
            PoolFillArgs {
                target: super::super::target::WorktreeTarget {
                    worktree: Some(wt.clone()),
                    worktree_pos: None,
                },
                size: Some(2),
                json: false,
            },
        )
        .expect_err("Vercel pool fill must refuse");
        assert!(format!("{error:#}").contains("not available for Vercel"));
        let db = thegn_core::db::Db::open().unwrap();
        assert!(db.pool_spares_for(&wt, "vercel").unwrap().is_empty());
        drop(state);
    }

    #[test]
    fn refuses_non_provider_without_pool_rows() {
        let (state, cfg, wt) = refusal_fixture(
            "local",
            EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        let error = run(
            &cfg,
            PoolFillArgs {
                target: super::super::target::WorktreeTarget {
                    worktree: Some(wt.clone()),
                    worktree_pos: None,
                },
                size: Some(2),
                json: false,
            },
        )
        .expect_err("non-provider pool fill must refuse");
        assert!(format!("{error:#}").contains("needs placement = \"provider\""));
        let db = thegn_core::db::Db::open().unwrap();
        assert!(db.pool_spares_for(&wt, "local").unwrap().is_empty());
        drop(state);
    }
}
