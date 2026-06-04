mod cli;
mod commands;
mod config;
mod db;
mod models;
mod msg;
mod picker;
mod repo;
mod util;
mod worktree;
mod zellij;

use clap::Parser;
use cli::{Cli, Command};
use config::Config;

fn main() {
    let args = Cli::parse();
    let cfg = Config::load();

    let result = match args.command.unwrap_or(Command::Launch) {
        Command::Launch => commands::launch::run(&cfg),
        Command::Attach { session } => commands::attach::run(session),
        Command::NewWorkspace { target, name } => commands::new_workspace::run(&cfg, target, name),
        Command::NewPane {
            name,
            base,
            dir,
            in_place,
        } => commands::new_pane::run(&cfg, name, base, &dir, in_place),
        Command::PickAgent {
            worktree,
            branch,
            agent,
        } => commands::pick_agent::run(&cfg, worktree, branch, agent),
        Command::Tool { name, worktree } => commands::tool::run(&cfg, &name, worktree),
        Command::Dashboard { watch, inner } => commands::dashboard::run(&cfg, watch, inner),
        Command::ClosePane {
            remove_worktree,
            delete_branch,
            force,
        } => commands::close_pane::run(&cfg, remove_worktree, delete_branch, force),
        Command::List { json } => commands::list::run(&cfg, json),
        Command::Repos => commands::repos::run(&cfg),
        Command::Recent { count } => commands::recent::run(count),
        Command::Status => commands::status::run(&cfg),
    };

    if let Err(e) = result {
        msg::die(&format!("{e:#}"));
    }
}
