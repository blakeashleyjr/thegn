//! `superzej new-worktree` — create a git worktree for a repo and open it as a
//! new zellij *tab* (named `{repo_slug}/{branch}`) whose first pane prompts for
//! what to run (the agent picker). `--in-place` runs that picker in the current
//! pane (the worktree-tab layout). `--repo <path>` targets a specific repo (the
//! sidebar's "+ worktree"); otherwise the current tab's repo is used. All tabs
//! live in the one session, so this is always a plain `new-tab` + tab switch.

use crate::config::{Config, SandboxConfig};
use crate::db::Db;
use crate::remote::{self, GitLoc};
use crate::{commands, msg, repo, util, worktree, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(
    cfg: &Config,
    name: Option<String>,
    base: Option<String>,
    in_place: bool,
    repo_arg: Option<String>,
) -> Result<()> {
    // Resolve the target repo root: an explicit `--repo` (sidebar "+ worktree"),
    // else the repo of the current tab's cwd.
    let root = if let Some(r) = repo_arg.as_deref() {
        repo::main_worktree(Path::new(r))
            .unwrap_or_else(|| msg::die(&format!("'{r}' is not inside a git repository")))
    } else {
        // Resurrection guard (cwd path only): if we're already inside a worktree,
        // do nothing — prevents the worktree-tab pane recursing on resurrection.
        if let Ok(wt) = std::env::var("SUPERZEJ_WORKTREE") {
            if Path::new(&wt).is_dir() {
                msg::warn("already inside a superzej worktree; ignoring new-worktree");
                return Ok(());
            }
        }
        let cwd = std::env::current_dir()?;
        repo::main_worktree(&cwd).unwrap_or_else(|| {
            msg::die(
                "not inside a git repository — open a workspace first (superzej new-workspace)",
            )
        })
    };

    let base = base.unwrap_or_else(|| worktree::resolve_base(&root, cfg));

    // A base with no commits (fresh repo on an unborn branch) can't be branched
    // from. Bail cleanly into a shell instead of dumping a raw git error.
    if util::git_out(&root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        msg::warn(&format!(
            "'{base}' has no commits yet — make an initial commit in this repo, then press Alt-w."
        ));
        return fallback(&root, in_place);
    }

    let slug = repo::repo_slug(&root);
    let branch = worktree::branch_name(&root, name.as_deref(), cfg);
    let tab = repo::branch_tab(&slug, &branch);
    let sb = cfg.repo_sandbox(&root);

    // Remote worktrees are created on the remote over ssh; the tab's cwd then
    // stays the *local* repo root (a valid local dir) and pick-agent picks the
    // remote path back up from the DB. Everything else (local create) is as before.
    let remote = sb.remote.is_remote() && sb.remote.mode == "remote";
    let (wt_path, location, cwd): (String, Option<String>, PathBuf) = if remote {
        match create_remote(&root, &branch, &base, &sb) {
            Some((wt, loc)) => (wt, Some(loc), root.clone()),
            None => {
                msg::warn("remote worktree create failed; falling back to a local worktree");
                match local_create(&root, &branch, &base, cfg) {
                    Some((p, path)) => (p, None, path),
                    None => return fallback(&root, in_place),
                }
            }
        }
    } else {
        match local_create(&root, &branch, &base, cfg) {
            Some((p, path)) => (p, None, path),
            None => return fallback(&root, in_place),
        }
    };

    let db = Db::open()?;
    db.put_worktree(
        &tab,
        &root.to_string_lossy(),
        &wt_path,
        &branch,
        location.as_deref(),
    )?;

    if in_place {
        // Local: cd into the new worktree. Remote: no local dir to cd into —
        // pick-agent resolves the location from the DB and runs over the transport.
        if location.is_none() {
            std::env::set_current_dir(&cwd)?;
        }
        if zellij::in_zellij() {
            zellij::rename_tab(&tab);
        }
        return commands::pick_agent::run(cfg, Some(wt_path), Some(branch), None, false);
    }

    if zellij::in_zellij() {
        // Open a new tab (a tab switch in the one session); its layout pane runs
        // pick-agent. cwd is the worktree locally, or the repo root for remote.
        if !zellij::new_tab(&tab, &cwd, Some("worktree-tab")) {
            zellij::new_tab(&tab, &cwd, None);
        }
    } else {
        msg::info(&format!("(not in zellij) worktree ready at {wt_path}"));
    }
    Ok(())
}

/// Create a local git worktree; `None` (with a warning) on failure. Returns the
/// path as both a string (DB key) and a `PathBuf` (tab cwd).
fn local_create(root: &Path, branch: &str, base: &str, cfg: &Config) -> Option<(String, PathBuf)> {
    let path = worktree::worktree_path(root, branch, cfg);
    msg::info(&format!("creating worktree {branch} off {base}"));
    if !worktree::add(root, branch, base, &path, cfg) {
        msg::warn("could not create the worktree (see the git error above).");
        return None;
    }
    let p = path.to_string_lossy().into_owned();
    Some((p, path))
}

/// Create a worktree on the remote over ssh: clone the repo into `remote_dir` if
/// absent, then `git worktree add`. Returns the absolute remote worktree path and
/// its DB location descriptor. `None` on any failure (caller falls back to local).
fn create_remote(
    root: &Path,
    branch: &str,
    base: &str,
    sb: &SandboxConfig,
) -> Option<(String, String)> {
    let r = &sb.remote;
    let Some(origin) = util::git_out(root, &["remote", "get-url", "origin"]) else {
        msg::warn("remote mode needs an 'origin' remote to clone on the remote host");
        return None;
    };
    let ssh = remote::SshTarget {
        host: r.host.clone(),
        port: r.port,
        forward_agent: r.forward_agent,
    };
    let home = remote::remote_home(&ssh)?;
    let dir = expand_remote_tilde(&r.remote_dir, &home);
    let remote_repo = format!("{}/{}", dir.trim_end_matches('/'), repo::repo_name(root));
    let wt = format!("{remote_repo}/.worktrees/{}", util::slugify(branch));

    // One ssh script: ensure the clone, then add the worktree (trying the bare
    // base ref first, then origin/<base> for a freshly-cloned remote).
    let script = format!(
        "set -e; mkdir -p {dir}; \
         if [ ! -e {repo}/.git ]; then git clone {origin} {repo}; fi; \
         git -C {repo} worktree add -b {br} {wt} {base} 2>/dev/null || \
         git -C {repo} worktree add -b {br} {wt} origin/{base}",
        dir = util::sh_quote(&dir),
        repo = util::sh_quote(&remote_repo),
        origin = util::sh_quote(&origin),
        br = util::sh_quote(branch),
        wt = util::sh_quote(&wt),
        base = util::sh_quote(base),
    );
    msg::info(&format!("creating remote worktree {branch} on {}", r.host));
    let mut argv = remote::ssh_base(r.port, r.forward_agent, true);
    argv.push(r.host.clone());
    argv.push(script);
    let ok = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let location = GitLoc::remote_db_string(&r.host, r.port, r.forward_agent, &wt);
    Some((wt, location))
}

/// Expand a leading `~` against the *remote* home (resolved over ssh).
fn expand_remote_tilde(p: &str, home: &str) -> String {
    if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        p.to_string()
    }
}

/// When a worktree can't be created, keep an in-place pane usable by dropping to
/// a shell in the repo root (so it isn't a dead, exited box).
fn fallback(root: &std::path::Path, in_place: bool) -> Result<()> {
    if in_place {
        std::env::set_current_dir(root)?;
        util::exec_shell();
    }
    Ok(())
}
