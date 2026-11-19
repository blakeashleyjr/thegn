//! `thegn repo forget` — drop workspaces from a STOPPED instance's registry.
//!
//! Built for splitting one instance into two: copy the state database, then
//! forget from each copy the workspaces the other one owns. It is the
//! registry half of the UI's keep-files "remove workspace"
//! ([`thegn_core::db_workspace_forget`]) plus the rows that would drive
//! background work on the forgotten workspace's worktrees. It deletes
//! database rows only: files on disk, git worktrees and branches are never
//! touched. Dry run by default; `--apply` backs the database up first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::db_workspace_forget::{ForgetWorkspace, WorkspaceForgetCounts};
use thegn_core::models::WorkspaceRow;
use thegn_core::store::WorkspaceStore;
use thegn_core::{outln, repo};

use crate::handlers::sidebar_persist::SIDEBAR_SCOPE;

/// Parsed `repo forget` arguments.
#[derive(Debug, Clone, Default)]
pub struct ForgetArgs {
    /// Workspaces to forget.
    pub paths: Vec<String>,
    /// Forget every registered workspace except these.
    pub keep: Vec<String>,
    pub apply: bool,
    pub json: bool,
    pub copy_from: Option<PathBuf>,
}

/// One workspace the command forgets (or would forget).
#[derive(Debug, Clone, Serialize)]
pub struct ForgottenWorkspace {
    pub repo_path: String,
    pub name: String,
    pub slug: Option<String>,
    #[serde(flatten)]
    pub counts: WorkspaceForgetCounts,
}

/// One workspace the command leaves registered.
#[derive(Debug, Clone, Serialize)]
pub struct KeptWorkspace {
    pub repo_path: String,
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub provider_worktrees: Vec<String>,
}

/// The `--json` document; the human table renders the same data.
#[derive(Debug, Clone, Serialize)]
pub struct ForgetReport {
    pub applied: bool,
    pub backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
    pub forgotten: Vec<ForgottenWorkspace>,
    pub kept: Vec<KeptWorkspace>,
}

/// Inputs to [`forget`] that production reads from the environment.
pub struct ForgetEnv<'a> {
    /// The directory holding `thegn.db`; backups go under it.
    pub state_dir: &'a Path,
    /// UTC stamp naming the backup directory.
    pub stamp: &'a str,
    /// `Some(reason)` when this instance is running.
    pub running: &'a dyn Fn() -> Option<String>,
    /// A backup the caller already took; `None` makes [`forget`] take one.
    pub backup: Option<&'a Path>,
}

pub fn run(cfg: &Config, args: ForgetArgs) -> Result<()> {
    let running = || instance_running_reason(cfg);
    // Refuse before opening the database at all.
    if let Some(reason) = running() {
        bail!(refusal(&reason));
    }
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    let dest = state_dir.join("thegn.db");
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let (reader, copied_from) = if let Some(source) = args.copy_from.as_ref() {
        if dest.exists() {
            if let (Ok(source), Ok(canonical_dest)) =
                (std::fs::canonicalize(source), std::fs::canonicalize(&dest))
                && source == canonical_dest
            {
                bail!(
                    "--copy-from source and destination are the same file at {}; Nothing was changed.",
                    source.display()
                );
            }
            bail!(
                "this instance already has a state database at {}; --copy-from only seeds an empty instance (move it aside first). Nothing was changed.",
                dest.display()
            );
        }
        let source = std::fs::canonicalize(source)
            .with_context(|| format!("canonicalize source database {}", source.display()))?;
        let mut shm = source.clone().into_os_string();
        shm.push("-shm");
        let mut wal = source.clone().into_os_string();
        wal.push("-wal");
        if std::path::Path::new(&shm).exists() || std::path::Path::new(&wal).exists() {
            bail!(
                "the instance that owns {} looks running (its -wal/-shm files exist); stop that instance (thegn daemon stop and quit its UI, with that instance's environment) and retry. Nothing was changed.",
                source.display()
            );
        }
        let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.clone());
        if source == canonical_dest {
            bail!(
                "--copy-from source and destination are the same file at {}; Nothing was changed.",
                source.display()
            );
        }
        let Some(reader) = Db::open_read_only_current_at(&source)? else {
            bail!("no state database at {}", source.display());
        };
        (reader, Some(source))
    } else {
        let Some(reader) = Db::open_read_only_current()? else {
            bail!(
                "no state database at {}; nothing was changed",
                dest.display()
            )
        };
        (reader, None)
    };
    let version = reader.user_version()?;
    if version != thegn_core::db::SCHEMA_VERSION {
        bail!(
            "state database is schema v{version}, this build expects v{}; use the matching build. Nothing was changed.",
            thegn_core::db::SCHEMA_VERSION
        );
    }
    let mut report = if args.apply {
        let backup = if let Some(source) = copied_from.as_ref() {
            reader
                .backup_to(&dest)
                .context("copy failed; nothing was forgotten")?;
            source.clone()
        } else {
            let backup = state_dir
                .join(format!("forget-backup-{stamp}"))
                .join("thegn.db");
            reader
                .backup_to(&backup)
                .context("database backup failed; nothing was forgotten")?;
            backup
        };
        drop(reader);
        let db = Db::open()?;
        forget(
            &db,
            &args,
            &ForgetEnv {
                state_dir: &state_dir,
                stamp: &stamp,
                running: &running,
                backup: Some(&backup),
            },
        )?
    } else {
        forget(
            &reader,
            &args,
            &ForgetEnv {
                state_dir: &state_dir,
                stamp: &stamp,
                running: &running,
                backup: None,
            },
        )?
    };
    report.copied_from = copied_from
        .as_ref()
        .map(|source| source.display().to_string());
    if args.json {
        return super::emit_json(&report);
    }
    print_report(&report, args.apply && copied_from.is_some());
    Ok(())
}

fn refusal(reason: &str) -> String {
    format!(
        "refusing to forget workspaces while this instance is running ({reason}); \
         quit thegn and stop its daemon (`thegn daemon stop`), then retry. Nothing was changed."
    )
}

/// Is this instance running? The interactive UI holds the profile singleton
/// lock ([`thegn_core::profile::instance_running`], what `thegn open` asks);
/// the pane daemon answers on this state dir's control socket (the
/// `probe_stop_target` health + scope probe `thegn daemon stop` and
/// `thegn doctor` use). A socket that answers but cannot be verified counts as
/// running: a live process would re-persist the rows this command deletes.
fn instance_running_reason(cfg: &Config) -> Option<String> {
    if thegn_core::profile::instance_running() {
        return Some("a thegn UI holds this profile's singleton lock".to_string());
    }
    daemon_running_reason(cfg)
}

fn daemon_running_reason(cfg: &Config) -> Option<String> {
    use crate::daemon::client::{StopProbe, probe_stop_target};
    use thegn_svc::control::client::{ControlAddr, ControlClient};
    let socket = crate::daemon::socket_path(&cfg.daemon);
    let shown = socket.display().to_string();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return Some(format!("could not probe the pane daemon: {error}")),
    };
    let probe = runtime.block_on(async {
        let client = ControlClient::new(ControlAddr::Unix(socket));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            probe_stop_target(&client, &crate::daemon::scope_aliases()),
        )
        .await
    });
    match probe {
        Ok(StopProbe::NotRunning | StopProbe::Foreign(_)) => None,
        Ok(StopProbe::Ours) => Some(format!("the pane daemon answers at {shown}")),
        Ok(StopProbe::Unverified(reason)) => Some(format!(
            "something answers at the pane daemon socket {shown}: {reason}"
        )),
        Err(_) => Some(format!(
            "the pane daemon socket {shown} did not answer within 5s"
        )),
    }
}

/// Plan, and with `--apply` perform, the forget. Refuses (error, nothing
/// changed) when the instance is running, when both or neither selection form
/// is given, or when an argument matches no registered workspace.
pub fn forget(db: &Db, args: &ForgetArgs, env: &ForgetEnv<'_>) -> Result<ForgetReport> {
    if let Some(reason) = (env.running)() {
        bail!(refusal(&reason));
    }
    match (args.paths.is_empty(), args.keep.is_empty()) {
        (false, false) => bail!("pass either workspace paths to forget or --keep, not both"),
        (true, true) => {
            bail!("name the workspaces to forget, or pass --keep <PATH> to forget every other one")
        }
        _ => {}
    }
    let rows = db.workspaces()?;
    let worktrees = db.worktrees()?;
    let forget_paths: Vec<String> = if args.keep.is_empty() {
        let mut selected = Vec::new();
        for arg in &args.paths {
            let path = resolve_registered(&rows, arg)?;
            if !selected.contains(&path) {
                selected.push(path);
            }
        }
        selected
    } else {
        let mut keep = Vec::new();
        for arg in &args.keep {
            keep.push(
                resolve_registered(&rows, arg)
                    .with_context(|| {
                        format!(
                            "--keep {arg} must name exactly one registered workspace; refusing so a typo cannot forget everything"
                        )
                    })?,
            );
        }
        rows.iter()
            .map(|row| row.repo_path.clone())
            .filter(|path| !keep.contains(path))
            .collect()
    };

    let mut forgotten = Vec::new();
    let mut kept = Vec::new();
    for row in &rows {
        if forget_paths.contains(&row.repo_path) {
            let slug = db.forget_slug(&row.repo_path)?;
            let counts = db.workspace_forget_counts(&ForgetWorkspace {
                repo_path: &row.repo_path,
                slug: slug.as_deref(),
                sidebar_scope: SIDEBAR_SCOPE,
            })?;
            forgotten.push(ForgottenWorkspace {
                repo_path: row.repo_path.clone(),
                name: row.name.clone(),
                slug,
                counts,
            });
        } else {
            kept.push(KeptWorkspace {
                repo_path: row.repo_path.clone(),
                name: row.name.clone(),
                provider_worktrees: worktrees
                    .iter()
                    .filter(|worktree| {
                        worktree.repo_root == row.repo_path
                            && matches!(
                                thegn_core::remote::GitLoc::from_db(
                                    &worktree.worktree,
                                    Some(worktree.location.as_str())
                                ),
                                thegn_core::remote::GitLoc::Provider { .. }
                            )
                    })
                    .map(|worktree| worktree.worktree.clone())
                    .collect(),
            });
        }
    }

    let mut report = ForgetReport {
        applied: false,
        backup: None,
        copied_from: None,
        forgotten,
        kept,
    };
    if !args.apply || report.forgotten.is_empty() {
        return Ok(report);
    }

    let backup = match env.backup {
        Some(taken) => taken.to_path_buf(),
        None => {
            let backup = env
                .state_dir
                .join(format!("forget-backup-{}", env.stamp))
                .join("thegn.db");
            db.backup_to(&backup)
                .context("database backup failed; nothing was forgotten")?;
            backup
        }
    };
    let backup_text = backup.display().to_string();
    report.backup = Some(backup_text.clone());
    for entry in &mut report.forgotten {
        entry.counts = db
            .forget_workspace_offline(&ForgetWorkspace {
                repo_path: &entry.repo_path,
                slug: entry.slug.as_deref(),
                sidebar_scope: SIDEBAR_SCOPE,
            })
            .with_context(|| {
                format!(
                    "forget {} failed; earlier workspaces in this run were forgotten, restore from {backup_text} to undo",
                    entry.repo_path
                )
            })?;
    }
    report.applied = true;
    Ok(report)
}

/// Resolve an argument to a REGISTERED workspace path: the exact registered
/// path; else, for an existing directory, its repo root (as `thegn open`
/// resolves it) or its canonical path; else the registered workspace that
/// contains the path; else a unique basename or display name.
fn resolve_registered(rows: &[WorkspaceRow], arg: &str) -> Result<String> {
    let expanded = thegn_core::util::expand_tilde(arg);
    let registered = |candidate: &Path| {
        rows.iter()
            .find(|row| Path::new(&row.repo_path) == candidate)
            .map(|row| row.repo_path.clone())
    };
    let given = PathBuf::from(&expanded);
    if let Some(found) = registered(given.as_path()) {
        return Ok(found);
    }
    let mut candidates = vec![given.clone()];
    if given.is_dir() {
        if let Some(root) = repo::main_worktree(&given).or_else(|| repo::toplevel(&given)) {
            candidates.push(root);
        }
        if let Ok(canonical) = std::fs::canonicalize(&given) {
            candidates.push(canonical);
        }
    }
    for candidate in &candidates {
        if let Some(found) = registered(candidate.as_path()) {
            return Ok(found);
        }
    }
    if given.is_absolute() {
        let containing = rows
            .iter()
            .filter(|row| {
                candidates
                    .iter()
                    .any(|candidate| candidate.starts_with(Path::new(&row.repo_path)))
            })
            .max_by_key(|row| row.repo_path.len());
        if let Some(row) = containing {
            return Ok(row.repo_path.clone());
        }
    }
    let mut matches: Vec<&WorkspaceRow> = rows
        .iter()
        .filter(|row| {
            row.name == arg
                || Path::new(&row.repo_path)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == arg)
        })
        .collect();
    matches.dedup_by(|a, b| a.repo_path == b.repo_path);
    match matches.as_slice() {
        [one] => Ok(one.repo_path.clone()),
        [] => Err(anyhow::Error::new(super::NotFound(format!(
            "no registered workspace matches '{arg}' (registered: {})",
            if rows.is_empty() {
                "none".to_string()
            } else {
                rows.iter()
                    .map(|row| row.repo_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )))),
        many => bail!(
            "'{arg}' is ambiguous; pass a path: {}",
            many.iter()
                .map(|row| row.repo_path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn counts_line(counts: &WorkspaceForgetCounts) -> String {
    let parts: Vec<String> = [
        ("worktree rows", counts.worktrees),
        ("tab groups", counts.tab_groups),
        ("pins", counts.pins),
        ("collapse keys", counts.collapse_keys),
        ("merge queue", counts.merge_queue),
        ("PR queue", counts.pr_queue),
        ("dispatches abandoned", counts.dispatches_abandoned),
        ("autopilot runs stopped", counts.autopilot_stopped),
        ("pool spares", counts.pool_spares),
        ("pool targets", counts.pool_targets),
        ("hibernations", counts.hibernations),
        ("forwards", counts.forwards),
        ("shares", counts.shares),
        ("automation proposals", counts.automation_proposals),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(label, count)| format!("{label} {count}"))
    .collect();
    if parts.is_empty() {
        "workspace row only".to_string()
    } else {
        parts.join(", ")
    }
}

fn print_report(report: &ForgetReport, copied: bool) {
    let print_copied = || {
        if copied && let Some(source) = &report.copied_from {
            outln!(
                "copied from: {source} (untouched; delete {} to undo)",
                thegn_core::util::xdg_state_home()
                    .join("thegn/thegn.db")
                    .display()
            );
        }
    };
    if !report.applied {
        if copied {
            print_copied();
        } else if let Some(source) = &report.copied_from {
            outln!(
                "dry run: would copy {source} to {}, then:",
                thegn_core::util::xdg_state_home()
                    .join("thegn/thegn.db")
                    .display()
            );
        } else {
            outln!("dry run: nothing changed (pass --apply to forget)");
        }
    } else {
        if let Some(backup) = &report.backup {
            outln!("backup: {backup}");
        }
        outln!("forgot {} workspace(s)", report.forgotten.len());
        print_copied();
    }
    if report.forgotten.is_empty() {
        outln!("nothing to forget");
    }
    for entry in &report.forgotten {
        outln!("forget  {}  ({})", entry.repo_path, entry.name);
        outln!("        {}", counts_line(&entry.counts));
    }
    for entry in &report.kept {
        outln!("keep    {}  ({})", entry.repo_path, entry.name);
        for worktree in &entry.provider_worktrees {
            outln!(
                "note    {worktree} has a provider sandbox; its ownership record stays in the instance that created it, so take it down there (thegn env down) before using it here"
            );
        }
    }
    outln!("files on disk, git worktrees and branches are never touched");
}

#[cfg(test)]
#[path = "repo_forget_tests.rs"]
mod tests;
