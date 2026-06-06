//! `superzej status` — the per-repo **home tab**: registers the repo, names its
//! tab `{slug}/home`, prints an inventory + key hints, then drops to a shell.
//!
//! This is the landing pane of every repo's home tab (opened by `new-workspace`
//! with the `home-tab` layout, and the first tab of a cold-started session). It
//! runs in the repo root (the tab's cwd), so it can self-register: that keeps
//! the sidebar's repo list and the panel's worktree resolution correct without a
//! separate bootstrap step.

use crate::commands::list;
use crate::config::Config;
use crate::db::Db;
use crate::{repo, util, worktree, zellij};
use anyhow::Result;

pub fn run(cfg: &Config) -> Result<()> {
    // Self-register the repo + this home tab so the sidebar/panel resolve it.
    if let Some(root) = std::env::current_dir()
        .ok()
        .and_then(|c| repo::main_worktree(&c))
    {
        let root_s = root.to_string_lossy().into_owned();
        let name = repo::repo_name(&root);
        let slug = repo::repo_slug(&root);
        let home = repo::home_tab(&slug);
        if let Ok(db) = Db::open() {
            let _ = db.touch_repo(&root_s, &name);
            let _ = db.put_workspace(&root_s, &name);
            // The home tab maps to the repo's main checkout (for the diff/PR panel).
            let branch = worktree::default_branch(&root);
            let _ = db.put_worktree(&home, &root_s, &root_s, &branch, None);
        }
        if zellij::in_zellij() {
            zellij::rename_tab(&home);
        }
    }

    use crate::theme;
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        println!(
            "\x1b[38;2;{}m\u{2726}\x1b[0m \x1b[1m\x1b[38;2;{}msuperzej\x1b[0m \
\x1b[38;2;{}m— terminal-native worktree IDE\x1b[0m\n",
            theme::MAGENTA,
            cfg.accent_rgb(),
            theme::FAINT,
        );
    } else {
        println!("superzej — terminal-native worktree IDE\n");
    }
    list::run(cfg, false)?;
    println!(
        "\n  Keys:  Alt-W new workspace (repo session)   Alt-w new worktree (tab)\n         \
         Alt-t new tab (same worktree)   Alt-h/j/k/l move focus\n         \
         Alt-n new panel   Alt-o switch repo   Alt-d dashboard\n         \
         Alt-s sidebar   Alt-p diff/PR panel\n         \
         Alt-g lazygit   Alt-y yazi   Alt-e editor   Alt-/ diff\n         \
         Alt-X remove worktree + close tab\n"
    );
    // Seed the pane title with the repo's current branch (the home checkout).
    let branch = std::env::current_dir()
        .ok()
        .and_then(|c| util::git_out(&c, &["symbolic-ref", "--quiet", "--short", "HEAD"]))
        .unwrap_or_else(|| "home".to_string());
    util::set_terminal_title(&branch);
    util::exec_shell();
}
