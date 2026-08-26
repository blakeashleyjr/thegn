//! `thegn project <action>` — manage projects: named groups of workspaces
//! (repos) worked on together as one feature spanning several repos. This is the
//! grouping layer ABOVE workspaces; it carries NO policy (assigning a project
//! never re-scopes credentials/egress/budget/sandbox — that is `thegn zone`'s
//! job). Membership is DB-tracked, never path-inferred.
//!
//! NB: this is distinct from a *tracker* "project" (`[issues] project_key` /
//! `project_id`, provider-side issue-tracker data). `thegn project` groups repos.
//! See [`thegn_core::project`] for the pure feature-branch/plan logic.

use anyhow::{Result, bail};
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::{ProjectDeleteOutcome, ProjectStore, WorkspaceStore};
use thegn_core::{outln, repo, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List projects and their member counts.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Create a project.
    Create { name: String },
    /// Rename a project.
    Rename { name: String, new_name: String },
    /// Delete a project (refuses if it has members unless `--force`).
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Assign a repo/workspace to a project (use `none` to unassign).
    Assign {
        /// Project name, or `none`/`-` to unassign.
        project: String,
        /// Repo path (defaults to the current directory's repo root).
        repo: Option<String>,
    },
}

pub fn run(_cfg: &Config, action: Action) -> Result<()> {
    let db = Db::open()?;
    match action {
        Action::List { json } => {
            let projects = db.list_projects()?;
            if json {
                #[derive(serde::Serialize)]
                struct ProjectJson<'a> {
                    name: &'a str,
                    members: i64,
                    position: i64,
                }
                let out: Vec<ProjectJson> = projects
                    .iter()
                    .map(|p| ProjectJson {
                        name: &p.name,
                        members: p.member_count,
                        position: p.position,
                    })
                    .collect();
                return super::emit_json(&out);
            }
            if projects.is_empty() {
                outln!("(no projects)");
            }
            for p in projects {
                outln!("{}  ({} member(s))", p.name, p.member_count);
            }
        }
        Action::Create { name } => {
            db.create_project(&name, util::now())?;
            outln!("created project {name}");
        }
        Action::Rename { name, new_name } => {
            let p = find_project(&db, &name)?;
            db.rename_project(p, &new_name)?;
            outln!("renamed {name} → {new_name}");
        }
        Action::Rm { name, force } => {
            let p = find_project(&db, &name)?;
            match db.delete_project(p, force)? {
                ProjectDeleteOutcome::Deleted => outln!("deleted project {name}"),
                ProjectDeleteOutcome::RefusedNonEmpty(n) => {
                    bail!("project {name} still has {n} member(s); reassign them or pass --force")
                }
            }
        }
        Action::Assign { repo, project } => {
            let root = repo_root_arg(repo);
            let root_s = root.to_string_lossy().to_string();
            if project.eq_ignore_ascii_case("none") || project == "-" {
                db.assign_workspace_project(&root_s, None)?;
                outln!("unassigned {} from its project", root.display());
            } else {
                let p = find_project(&db, &project)?;
                // Ensure a workspaces row exists (a repo not yet opened as a
                // workspace has none, so the assignment would hit 0 rows).
                db.put_workspace(&root_s, &repo::repo_name(&root), "repo")?;
                db.assign_workspace_project(&root_s, Some(p))?;
                outln!("assigned {} → project {project}", root.display());
            }
        }
    }
    Ok(())
}

fn find_project(db: &Db, name: &str) -> Result<i64> {
    db.list_projects()?
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.project_id)
        .ok_or_else(|| {
            anyhow::anyhow!("no project named {name:?} (create it with `project create {name}`)")
        })
}

fn repo_root_arg(path: Option<String>) -> PathBuf {
    let start = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    repo::main_worktree(&start).unwrap_or(start)
}
