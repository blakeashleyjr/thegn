mod cli;
mod commands;
mod config;
mod db;
mod diff_highlight;
mod github;
mod models;
mod msg;
mod picker;
mod repo;
mod theme;
mod util;
mod worktree;
mod zellij;

use clap::Parser;
use cli::{Cli, Command};
use config::Config;

fn main() {
    let args = Cli::parse();
    let cfg = Config::load();
    picker::set_accent(&cfg.accent_hex());

    let result = match args.command.unwrap_or(Command::Launch) {
        Command::Launch => commands::launch::run(&cfg),
        Command::Attach { session } => commands::attach::run(&cfg, session),
        Command::NewWorkspace {
            target,
            name,
            from_home,
        } => commands::new_workspace::run(&cfg, target, name, from_home),
        Command::NewWorktree {
            name,
            base,
            in_place,
            repo,
        } => commands::new_worktree::run(&cfg, name, base, in_place, repo),
        // Deprecated alias: `new-pane` -> `new-worktree`.
        Command::NewPane {
            name,
            base,
            dir: _,
            in_place,
        } => commands::new_worktree::run(&cfg, name, base, in_place, None),
        Command::NewPanel { dir, in_place } => commands::new_panel::run(&cfg, &dir, in_place),
        Command::NewTab { session } => commands::new_tab::run(session),
        Command::Workspaces => commands::workspaces::run(),
        Command::Worktrees => commands::worktrees::run(&cfg),
        Command::OpenWorktree { path } => commands::open_worktree::run(path),
        Command::Menu => commands::menu::run(&cfg),
        Command::GrantPlugins => commands::grant_plugins::run(),
        Command::ResolveWorktree { session, tab } => commands::resolve::run(session, tab),
        Command::PanelSnapshot { session, tab } => commands::snapshot::run(session, tab),
        Command::Watch {
            session,
            pr_interval,
        } => commands::watch::run(session, pr_interval),
        Command::RestoreSession => commands::restore::run(),
        Command::PickAgent {
            worktree,
            branch,
            agent,
            in_place: _, // no-op; accepted for worktree-tab layout compatibility
            resume,
        } => commands::pick_agent::run(&cfg, worktree, branch, agent, resume),
        Command::Tool {
            name,
            worktree,
            file,
        } => commands::tool::run(&cfg, &name, worktree, file),
        Command::Dashboard { watch, inner } => commands::dashboard::run(&cfg, watch, inner),
        Command::CloseWorktree {
            delete_branch,
            force,
        } => commands::close_worktree::run(delete_branch, force),
        Command::ClosePanel => commands::close_worktree::close_panel(),
        // Deprecated alias.
        Command::ClosePane {
            remove_worktree,
            delete_branch,
            force,
        } => {
            if remove_worktree {
                commands::close_worktree::run(delete_branch, force)
            } else {
                commands::close_worktree::close_panel()
            }
        }
        Command::Sidebar { toggle } => commands::panels::sidebar(toggle),
        Command::Panel { toggle } => commands::panels::panel(toggle),
        Command::Pr { action } => commands::pr::run(action),
        Command::Diff {
            worktree,
            base,
            stat,
            files,
            file,
        } => commands::diff::run(worktree, base, stat, files, file),
        Command::List { json } => commands::list::run(&cfg, json),
        Command::Repos => commands::repos::run(&cfg),
        Command::Recent { count } => commands::recent::run(count),
        Command::Status => commands::status::run(&cfg),
        Command::Theme => commands::theme::run(&cfg),
        Command::Stats => commands::stats::run(),
        Command::Activity { ack } => commands::activity::run(ack),
    };

    if let Err(e) = result {
        msg::die(&format!("{e:#}"));
    }
}
