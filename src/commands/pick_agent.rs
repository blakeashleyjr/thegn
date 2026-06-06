//! `superzej pick-agent` — runs inside a freshly created worktree pane. Shows the
//! agent/tool/shell picker, records the choice, then execs it so the selection
//! becomes the pane's own process.

use crate::config::Config;
use crate::db::{self, Db};
use crate::remote::GitLoc;
use crate::{msg, picker, repo, sandbox, theme, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    branch: Option<String>,
    preset: Option<String>,
    resume: bool,
) -> Result<()> {
    // Resolve the worktree: explicit arg, else (argless worktree-tab layout) the
    // DB row for this tab — which also carries remote worktrees that have no
    // local cwd — else the cwd.
    let worktree = worktree
        .or_else(worktree_for_focused_tab)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    // Where this worktree's git lives — local on disk, or a remote over ssh.
    let loc = GitLoc::for_worktree(Path::new(&worktree));
    if !loc.is_remote() && !Path::new(&worktree).is_dir() {
        msg::die(&format!("pick-agent: worktree '{worktree}' does not exist"));
    }

    // On restart (`--resume`) skip the picker and relaunch the agent this
    // worktree last ran (recorded in the DB); fall back to the picker if none.
    let preset = preset.or_else(|| {
        resume
            .then(|| {
                Db::open()
                    .ok()
                    .and_then(|db| db.worktree_agent(&worktree).ok().flatten())
            })
            .flatten()
    });

    // Branch may be omitted (worktree-tab layout runs us with cwd only); derive
    // it via the location (works local or over ssh).
    let branch = branch.or_else(|| loc.git_out(&["symbolic-ref", "--quiet", "--short", "HEAD"]));

    // The local repo root (for the per-repo overlay + slug), from the DB when the
    // worktree itself is remote, else climbed from the local worktree.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(&worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(&worktree)))
        .unwrap_or_else(|| PathBuf::from(&worktree));

    // Name the tab `{repo_slug}/{branch}` — the globally-unique scheme that lets
    // the panel/resolve key on tab name in the single session.
    if let (true, Some(b)) = (zellij::in_zellij(), branch.as_deref()) {
        zellij::rename_tab(&repo::branch_tab(&repo::repo_slug(&repo_root), b));
    }

    // Choices: agents + tools + a literal shell.
    let mut labels: Vec<String> = cfg.agents.iter().map(|a| a.name.clone()).collect();
    labels.extend(cfg.tools.iter().map(|t| t.name.clone()));
    if !labels.iter().any(|l| l == "shell") {
        labels.push("shell".into());
    }

    let choice = preset.unwrap_or_else(|| {
        let prompt = format!("Run in {}", util::basename(&worktree));
        // Show each choice with its identity glyph; map the display label back
        // to the bare name on selection.
        let display: Vec<String> = labels
            .iter()
            .map(|n| format!("{} {n}", theme::agent_glyph(n)))
            .collect();
        match picker::pick(&prompt, &display, cfg.picker.as_str()) {
            Some(sel) => display
                .iter()
                .position(|d| *d == sel)
                .map(|i| labels[i].clone())
                .unwrap_or(sel),
            None => {
                msg::warn("no selection; dropping to shell");
                "shell".into()
            }
        }
    });

    // Resolve the chosen label to a command string.
    let cmd: String = if choice == "shell" {
        "__shell__".into()
    } else if let Some(c) = cfg.agent_command(&choice) {
        c.to_string()
    } else if let Some(c) = cfg.tool_command(&choice) {
        c.to_string()
    } else {
        msg::warn(&format!("unknown choice '{choice}'; dropping to shell"));
        "__shell__".into()
    };

    // Record the choice for the dashboard (keyed by worktree path).
    if let Ok(db) = Db::open() {
        let _ = db.set_worktree_agent(&worktree, &choice);
    }

    // Tag the pane title with the agent's identity glyph (a bare char — pane
    // titles can't carry ANSI), so the session shows "C feat/foo" etc.
    if zellij::in_zellij() {
        let label = branch
            .as_deref()
            .unwrap_or_else(|| util::basename(&worktree));
        zellij::rename_pane(&format!("{} {label}", theme::agent_glyph(&choice)));
    }

    // Local worktrees: cd in so a host fallback lands in the right place. Remote
    // worktrees have no local dir — the transport cd's on the remote.
    if !loc.is_remote() {
        std::env::set_current_dir(&worktree)?;
    }
    std::env::set_var("SUPERZEJ_WORKTREE", &worktree);
    // Seed the pane's window title with the branch (the worktree); the chosen
    // program overrides it as usual.
    util::set_terminal_title(
        branch
            .as_deref()
            .unwrap_or_else(|| util::basename(&worktree)),
    );
    std::env::set_var("SUPERZEJ_BRANCH", branch.unwrap_or_default());

    // Wrap the chosen program in the worktree's sandbox/container (and/or the
    // mosh/ssh transport for a remote worktree). Only this interactive process is
    // sandboxed — the worktree dir is bind-mounted, so host-side git reads keep
    // working. Falls back to the plain host shell when, for a *local* worktree,
    // the sandbox is disabled / resolves to `none` / can't be brought up.
    let sb = cfg.repo_sandbox(&repo_root);
    let inner = if cmd == "__shell__" {
        "${SHELL:-/bin/sh} -l".to_string()
    } else {
        cmd.clone()
    };
    let cname = sandbox::container_name(&worktree);
    if let Some(spec) = sandbox::resolve(&sb, &loc, &cname) {
        match sandbox::ensure(&spec) {
            Ok(()) => {
                let argv = sandbox::enter_argv(&spec, &inner);
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                util::exec_command(refs[0], &refs[1..]); // diverges on success
            }
            Err(e) => msg::warn(&format!("sandbox: {e}; running on host")),
        }
    }

    if cmd == "__shell__" {
        util::exec_shell();
    } else {
        util::exec_shell_cmd(&cmd);
    }
}

/// The worktree path recorded for the focused tab (the argless worktree-tab
/// layout path). `None` outside a session or when the tab isn't a known worktree.
fn worktree_for_focused_tab() -> Option<String> {
    if !zellij::in_zellij() {
        return None;
    }
    let tab = zellij::focused_tab_name()?;
    Db::open()
        .ok()
        .and_then(|db| db.worktree_for_tab(&db::session(), &tab).ok().flatten())
}
