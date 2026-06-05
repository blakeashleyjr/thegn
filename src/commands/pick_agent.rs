//! `superzej pick-agent` — runs inside a freshly created worktree pane. Shows the
//! agent/tool/shell picker, records the choice, then execs it so the selection
//! becomes the pane's own process.

use crate::config::Config;
use crate::db::Db;
use crate::{msg, picker, repo, theme, util, zellij};
use anyhow::Result;
use std::path::Path;

pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    branch: Option<String>,
    preset: Option<String>,
) -> Result<()> {
    let worktree = worktree.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    if !Path::new(&worktree).is_dir() {
        msg::die(&format!("pick-agent: worktree '{worktree}' does not exist"));
    }

    // Branch may be omitted (worktree-tab layout runs us with cwd only); derive
    // it from git, and name the tab after it so resurrected tabs read correctly.
    let branch = branch.or_else(|| {
        util::git_out(
            Path::new(&worktree),
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
    });
    // Name the tab `{repo_slug}/{branch}` — the globally-unique scheme that lets
    // the panel/resolve key on tab name in the single session.
    if let (true, Some(b)) = (zellij::in_zellij(), branch.as_deref()) {
        let slug = repo::main_worktree(Path::new(&worktree))
            .map(|r| repo::repo_slug(&r))
            .unwrap_or_else(|| "repo".to_string());
        zellij::rename_tab(&repo::branch_tab(&slug, b));
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
        match picker::pick(&prompt, &display, &cfg.picker) {
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

    std::env::set_current_dir(&worktree)?;
    std::env::set_var("SUPERZEJ_WORKTREE", &worktree);
    // Seed the pane's window title with the branch (the worktree); the chosen
    // program overrides it as usual.
    util::set_terminal_title(
        branch
            .as_deref()
            .unwrap_or_else(|| util::basename(&worktree)),
    );
    std::env::set_var("SUPERZEJ_BRANCH", branch.unwrap_or_default());

    if cmd == "__shell__" {
        util::exec_shell();
    } else {
        util::exec_shell_cmd(&cmd);
    }
}
