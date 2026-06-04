//! `superzej pick-agent` — runs inside a freshly created worktree pane. Shows the
//! agent/tool/shell picker, records the choice, then execs it so the selection
//! becomes the pane's own process.

use crate::config::Config;
use crate::db::Db;
use crate::{msg, picker, util};
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

    // Choices: agents + tools + a literal shell.
    let mut labels: Vec<String> = cfg.agents.iter().map(|a| a.name.clone()).collect();
    labels.extend(cfg.tools.iter().map(|t| t.name.clone()));
    if !labels.iter().any(|l| l == "shell") {
        labels.push("shell".into());
    }

    let choice = preset.unwrap_or_else(|| {
        let prompt = format!("Run in {}", util::basename(&worktree));
        picker::pick(&prompt, &labels, &cfg.picker).unwrap_or_else(|| {
            msg::warn("no selection; dropping to shell");
            "shell".into()
        })
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

    std::env::set_current_dir(&worktree)?;
    std::env::set_var("SUPERZEJ_WORKTREE", &worktree);
    std::env::set_var("SUPERZEJ_BRANCH", branch.unwrap_or_default());

    if cmd == "__shell__" {
        util::exec_shell();
    } else {
        util::exec_shell_cmd(&cmd);
    }
}
