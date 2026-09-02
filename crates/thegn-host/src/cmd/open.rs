//! `thegn open <repo>` — remote-control (or launch) the compositor onto a
//! workspace.
//!
//! With a live instance: enqueue a `focus_workspace` intent in the DB mailbox
//! (`IntentStore`); the compositor's model refresh claims it within ~1s. With
//! no instance: set the active-workspace pointer and fall through to the
//! normal interactive launch, whose startup resolution (THEGN_SESSION →
//! active-workspace pointer → last-active) lands on the repo with no new
//! startup argument. No IPC either way — the DB is the mailbox.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::{IntentStore, WorkspaceStore};
use thegn_core::{outln, repo};

/// What `run` decided; `LaunchTui` means the caller (main) falls through to
/// the interactive compositor path.
pub enum OpenOutcome {
    /// Delivered to a running instance (or `--no-launch` recorded the pointer).
    Delivered,
    /// No live instance — the caller should launch the compositor.
    LaunchTui,
}

pub fn run(
    cfg: &Config,
    repo_arg: &str,
    no_launch: bool,
    preset: Option<&str>,
) -> Result<OpenOutcome> {
    let db = Db::open()?;
    let target = resolve(cfg, &db, repo_arg)?;
    // Validate `--preset` against config BEFORE any enqueue: a miss lists the
    // configured names and exits 3 (the `open` resolution convention), and no
    // intent is written. The name is validated locally, but resolution to argv
    // still happens only on the receiving compositor — the wire carries the
    // name, never a command.
    if let Some(name) = preset {
        resolve_preset(cfg, name)?;
    }

    if thegn_core::profile::instance_running() {
        let payload = serde_json::to_string(&thegn_core::models::FocusIntent {
            repo: target.clone(),
        })?;
        db.put_intent("focus_workspace", &payload)?;
        enqueue_preset(&db, preset)?;
        match preset {
            Some(name) => {
                outln!("focus + preset '{name}' request sent to the running instance ({target})")
            }
            None => outln!("focus request sent to the running instance ({target})"),
        }
        return Ok(OpenOutcome::Delivered);
    }

    // No live instance: point the startup resolution at the workspace, then
    // launch (unless the caller only wants the pointer recorded). The preset
    // intent is enqueued unconditionally — the launched (or already-launching)
    // compositor claims it after its first frame, so a cold `open --preset`
    // still applies the shape.
    let name = repo::repo_name_from_path(std::path::Path::new(&target));
    // best-effort: recents history; the pointer below is the primary path.
    let _ = db.touch_repo(&target, &name);
    db.set_active_workspace(&target)?;
    enqueue_preset(&db, preset)?;
    if no_launch {
        outln!("active project set to {target}");
        return Ok(OpenOutcome::Delivered);
    }
    Ok(OpenOutcome::LaunchTui)
}

/// Confirm a `--preset` name is configured, or fail with an exit-3
/// [`NotFound`](super::NotFound)
/// listing the known preset names (the `open` miss convention).
fn resolve_preset<'a>(cfg: &'a Config, name: &str) -> Result<&'a thegn_core::config::Preset> {
    cfg.preset(name).ok_or_else(|| {
        anyhow::Error::new(super::NotFound(format!(
            "no preset named '{name}' (known: {})",
            if cfg.presets.is_empty() {
                "none".to_string()
            } else {
                cfg.preset_names().join(", ")
            }
        )))
    })
}

/// Enqueue a name-only `launch_preset` intent (no-op when `--preset` is unset).
fn enqueue_preset(db: &Db, preset: Option<&str>) -> Result<()> {
    if let Some(name) = preset {
        let payload = serde_json::to_string(&thegn_core::models::LaunchPresetIntent {
            name: name.to_string(),
        })?;
        db.put_intent("launch_preset", &payload)?;
    }
    Ok(())
}

/// Resolve the repo argument: an existing path (any dir inside the repo
/// works), else a unique basename match over known + discovered repos.
fn resolve(cfg: &Config, db: &Db, arg: &str) -> Result<String> {
    let expanded = thegn_core::util::expand_tilde(arg);
    let p = std::path::Path::new(&expanded);
    if p.is_dir() {
        if let Some(root) = repo::main_worktree(p).or_else(|| repo::toplevel(p)) {
            return Ok(root.to_string_lossy().into_owned());
        }
        return Err(anyhow::Error::new(super::NotFound(format!(
            "not a git repo: {expanded}"
        ))));
    }

    let mut candidates: Vec<String> = db.known_repos().unwrap_or_default();
    candidates.extend(repo::discover_repos(cfg));
    candidates.sort();
    candidates.dedup();
    // Two spellings can name the SAME directory — macOS `/tmp` is a symlink to
    // `/private/tmp`, so one repo reachable under both paths would otherwise
    // look like two rival matches and fail as "ambiguous". Dedup on the
    // resolved path but keep the original spelling: `canonicalize` yields UNC
    // paths (`\\?\C:\…`) on Windows, so it is only ever a key here, never the
    // value handed back. A path that cannot be resolved keys on itself.
    let mut seen = std::collections::HashSet::new();
    let matches: Vec<&String> = candidates
        .iter()
        .filter(|c| {
            std::path::Path::new(c)
                .file_name()
                .map(|n| n.to_string_lossy() == arg)
                .unwrap_or(false)
        })
        .filter(|c| {
            let key = std::fs::canonicalize(c.as_str())
                .unwrap_or_else(|_| std::path::PathBuf::from(c.as_str()));
            seen.insert(key)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(anyhow::Error::new(super::NotFound(format!(
            "no repo named '{arg}' (known: {})",
            if candidates.is_empty() {
                "none".to_string()
            } else {
                candidates
                    .iter()
                    .filter_map(|c| std::path::Path::new(c).file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )))),
        many => anyhow::bail!(
            "'{arg}' is ambiguous — pass a path: {}",
            many.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
