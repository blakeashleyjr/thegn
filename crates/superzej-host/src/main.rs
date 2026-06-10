//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod agent;
mod center;
mod chrome;
mod cmd;
mod compositor;
mod copymode;
mod emulator;
mod hydrate;
mod input;
mod keyhint;
mod keymap;
mod layout;
mod palette;
mod pane;
mod panel;
mod panes;
mod pins;
mod run;
mod sequence;
mod session;
mod sidebar;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Clone)]
#[command(
    name = "superzej",
    version,
    about = "superzej — terminal-native worktree IDE"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Override a config value (e.g. `--set theme.accent=cyan --set drawer.height=15`)
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,

    /// A non-interactive subcommand. With none, launch the compositor.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The non-interactive CLI verbs the single binary still exposes. Bare
/// `superzej` (no subcommand) launches the interactive compositor.
#[derive(Subcommand, Clone)]
pub enum Command {
    /// GitHub PR data + actions for a worktree.
    Pr {
        #[command(subcommand)]
        action: cmd::pr::Action,
    },
    /// GitHub Issue data + actions for a worktree.
    Issue {
        #[command(subcommand)]
        action: cmd::issue::Action,
    },
    /// Emit a syntax-highlighted diff of a worktree against its branch point.
    Diff {
        #[arg(long)]
        worktree: Option<String>,
        /// Diff against this base ref (default: the repo's default branch).
        #[arg(long)]
        base: Option<String>,
        /// Summary (--stat) only.
        #[arg(long)]
        stat: bool,
        /// Full diff of a single file.
        #[arg(long)]
        file: Option<String>,
    },
    /// List managed worktrees.
    List,
    /// List git repos discovered under repo_roots.
    Repos,
    /// List recently opened repos (history).
    Recent { count: Option<i64> },
    /// Inspect the effective (layered) configuration.
    Config {
        #[command(subcommand)]
        action: cmd::config::Action,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    if let Some(lvl) = cli.log_level.as_deref() {
        cli.overrides.push(format!("log.level={lvl}"));
    }

    // A subcommand runs synchronously and exits; no subcommand launches the
    // interactive compositor (the default).
    if let Some(command) = cli.command.take() {
        return run_subcommand(&cli, command);
    }
    run::main(cli).await
}

/// Dispatch a non-interactive verb. Loads the layered config (the verbs that
/// need it) and routes to the ported `cmd` module.
fn run_subcommand(cli: &Cli, command: Command) -> anyhow::Result<()> {
    let cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv,
        &cli.overrides,
        cli.config.clone(),
    );
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(superzej_core::config::Config::path);
    match command {
        Command::Pr { action } => cmd::pr::run(action),
        Command::Issue { action } => cmd::issue::run(action),
        Command::Diff {
            worktree,
            base,
            stat,
            file,
        } => cmd::diff::run(worktree, base, stat, file),
        Command::List => cmd::list::run(&cfg),
        Command::Repos => cmd::repos::repos(&cfg),
        Command::Recent { count } => cmd::repos::recent(count),
        Command::Config { action } => cmd::config::run(&cfg, action, config_path),
    }
}
