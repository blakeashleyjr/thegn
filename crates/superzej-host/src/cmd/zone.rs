//! `superzej zone <action>` — manage zones: named groups of workspaces inside a
//! profile providing a soft, concurrent firewall (credential sub-vault + egress/
//! budget ceilings). Membership is DB-tracked (never path-inferred); policy is
//! in config (`[zone.<name>]`). See [`superzej_core::zone`].

use anyhow::{Result, bail};
use std::path::PathBuf;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::store::{WorkspaceStore, ZoneDeleteOutcome, ZoneStore};
use superzej_core::{outln, repo, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List zones and their member counts.
    List,
    /// Create a zone.
    Create { name: String },
    /// Rename a zone.
    Rename { name: String, new_name: String },
    /// Delete a zone (refuses if it has members unless `--force`).
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Assign a repo/workspace to a zone (use `none` to unassign).
    Assign {
        /// Zone name, or `none`/`-` to unassign.
        zone: String,
        /// Repo path (defaults to the current directory's repo root).
        repo: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    let db = Db::open()?;
    // Propagate `[zone.<name>.budget]` caps into the proxy budget rows so the
    // per-request rollup enforces them (spend preserved). Cheap; idempotent.
    superzej_core::zone::sync_budget_caps(&cfg.zone, &db);
    match action {
        Action::List => {
            let zones = db.list_zones()?;
            if zones.is_empty() {
                outln!("(no zones)");
            }
            for z in zones {
                outln!("{}  ({} member(s))", z.name, z.member_count);
            }
        }
        Action::Create { name } => {
            db.create_zone(&name, util::now())?;
            outln!("created zone {name}");
        }
        Action::Rename { name, new_name } => {
            let z = find_zone(&db, &name)?;
            db.rename_zone(z, &new_name)?;
            outln!("renamed {name} → {new_name}");
        }
        Action::Rm { name, force } => {
            let z = find_zone(&db, &name)?;
            match db.delete_zone(z, force)? {
                ZoneDeleteOutcome::Deleted => outln!("deleted zone {name}"),
                ZoneDeleteOutcome::RefusedNonEmpty(n) => {
                    bail!("zone {name} still has {n} member(s); reassign them or pass --force")
                }
            }
        }
        Action::Assign { repo, zone } => {
            let root = repo_root_arg(repo);
            let root_s = root.to_string_lossy().to_string();
            if zone.eq_ignore_ascii_case("none") || zone == "-" {
                db.assign_workspace_zone(&root_s, None)?;
                outln!("unassigned {} from its zone", root.display());
            } else {
                let z = find_zone(&db, &zone)?;
                // Ensure a workspaces row exists (a repo not yet opened as a
                // workspace has none, so the assignment would hit 0 rows).
                db.put_workspace(&root_s, &repo::repo_name(&root), "repo")?;
                db.assign_workspace_zone(&root_s, Some(z))?;
                outln!("assigned {} → zone {zone}", root.display());
            }
        }
    }
    Ok(())
}

fn find_zone(db: &Db, name: &str) -> Result<i64> {
    db.list_zones()?
        .into_iter()
        .find(|z| z.name == name)
        .map(|z| z.zone_id)
        .ok_or_else(|| {
            anyhow::anyhow!("no zone named {name:?} (create it with `zone create {name}`)")
        })
}

fn repo_root_arg(path: Option<String>) -> PathBuf {
    let start = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    repo::main_worktree(&start).unwrap_or(start)
}
