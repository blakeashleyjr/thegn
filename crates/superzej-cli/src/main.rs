// The substrate-agnostic core now lives in its own crate. Re-export its modules
// under `crate::` so the (transitional) zellij-driven command + palette code that
// references `crate::db`, `crate::config`, … keeps resolving unchanged. The
// `#[macro_use]` pulls in core's exported `outln!`/`out!` macros crate-wide.
pub use superzej_core::{
    config, db, diff_highlight, github, keymap, log, models, msg, out, picker, remote, repo,
    sandbox, theme, util, worktree, yazi,
};
// `out` above already brings the `out!` macro into scope (same path, macro
// namespace); add `outln!` so `crate::outln!` call sites resolve too.
pub use superzej_core::outln;

mod cli;
mod commands;
mod palette;
mod zellij;

use clap::Parser;
use cli::{Cli, Command};
use config::Config;

fn main() {
    // Rust installs SIG_IGN for SIGPIPE at startup, so writing to a closed pipe
    // (e.g. `superzej worktrees | head`) surfaces as an EPIPE that `println!`
    // unwraps into a panic. Restore the default disposition so we exit quietly
    // like a normal Unix tool. Safe: single-threaded, before any output.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = Cli::parse();
    
    // Add legacy log level mapping to overrides if provided
    let mut overrides = args.overrides.clone();
    if let Some(lvl) = args.log_level.as_deref() {
        overrides.push(format!("log.level={lvl}"));
    }

    let effective_path = args.config.clone().unwrap_or_else(Config::path);
    let cfg = Config::load_layered(&config::ProcessEnv, &overrides, args.config.clone());

    // Bring up diagnostics now that we have `[log]`. The daemon (whose stdio is
    // nulled) logs only to its file; everything else also writes branded stderr.
    let role = match &args.command {
        Some(Command::Watch { session, .. }) => log::Role::Watch {
            session: session.clone().unwrap_or_else(zellij::ui_session),
        },
        _ => log::Role::Cli,
    };
    log::init(role, &cfg.log);

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
        Command::Menu { tab } => commands::menu::run(&cfg, tab),
        Command::GrantPlugins => commands::grant_plugins::run(),
        Command::ResolveWorktree { session, tab } => commands::resolve::run(session, tab),
        Command::PanelSnapshot { session, tab } => commands::snapshot::run(session, tab),
        Command::Watch {
            session,
            pr_interval,
        } => commands::watch::run(&cfg, session, pr_interval),
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
        Command::Monitor { kind } => commands::monitor::run(&cfg, &kind),
        Command::Files {
            reveal,
            worktree,
            tab,
            session,
            close,
            restore,
        } => commands::files::run(&cfg, reveal, worktree, tab, session, close, restore),
        Command::Dashboard { watch, inner } => commands::dashboard::run(&cfg, watch, inner),
        Command::CloseWorktree {
            delete_branch,
            force,
        } => commands::close_worktree::run(&cfg, delete_branch, force),
        Command::ClosePanel => commands::close_worktree::close_panel(),
        // Deprecated alias.
        Command::ClosePane {
            remove_worktree,
            delete_branch,
            force,
        } => {
            if remove_worktree {
                commands::close_worktree::run(&cfg, delete_branch, force)
            } else {
                commands::close_worktree::close_panel()
            }
        }
        Command::Sidebar { toggle } => commands::panels::sidebar(toggle),
        Command::Panel { toggle } => commands::panels::panel(toggle),
        Command::Pin { action } => commands::pin::run(&cfg, action),
        Command::Pr { action } => commands::pr::run(&cfg, action),
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
        Command::Config { action } => commands::config::run(&cfg, action, effective_path),
        Command::Keys { action } => commands::keys::run(&cfg, action),
        Command::Theme => commands::theme::run(&cfg),
        Command::Stats => commands::stats::run(),
        Command::Activity { ack } => commands::activity::run(ack),
    };

    if let Err(e) = result {
        msg::die(&format!("{e:#}"));
    }
}
