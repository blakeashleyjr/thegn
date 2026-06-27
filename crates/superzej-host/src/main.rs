//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod agent;
mod apps;
mod borders;
mod center;
mod chrome;
mod clipboard;
mod cmd;
mod compositor;
mod copymode;
mod desktop_notify;
mod emulator;
mod focus;
mod font;
mod gitmut;
mod hover;
mod hydrate;
mod input;
mod keyhint;
mod keymap;
mod layer;
mod layout;
mod layout_spec;
mod logotype;
mod lsp;
mod mem;
mod menu;
mod metrics;
mod mousefilter;
mod palette;
mod pane;
mod panel;
mod panes;
mod perf;
mod pins;
mod profile;
mod proxy_daemon;
mod queries;
mod recorder;
mod render_plan;
mod run;
mod sandbox_events;
mod search;
mod search_everywhere;
mod seg;
mod sequence;
mod session;
mod sidebar;
mod task;
mod telemetry;
#[cfg(test)]
mod testenv;
mod testkit;
mod toast;
mod wire;
mod wizard;

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
    /// Cross-provider CI/CD inspection: runs, jobs, logs, trigger/rerun/cancel.
    Ci {
        #[command(subcommand)]
        action: cmd::ci::Action,
    },
    /// Theme interactive switcher.
    Theme {
        #[command(subcommand)]
        action: cmd::theme::Action,
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
    /// Report per-worktree disk usage (checkout + reclaimable `target/`).
    Disk {
        /// Scan only this worktree (defaults to all known worktrees).
        #[arg(long)]
        worktree: Option<String>,
        /// Scan every known worktree (the default when no `--worktree` is given).
        #[arg(long)]
        all: bool,
    },
    /// Reclaim a worktree's `target/` build artifacts (keeps the checkout).
    Clean {
        /// Clean this worktree (defaults to the current one).
        #[arg(long)]
        worktree: Option<String>,
        /// Clean every known worktree (except the active one).
        #[arg(long)]
        all: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// List git repos discovered under repo_roots.
    Repos,
    /// List recently opened repos (history).
    Recent { count: Option<i64> },
    /// Inspect the effective (layered) configuration.
    Config {
        #[command(subcommand)]
        action: cmd::config::Action,
    },
    /// Inspect and select named execution environments (`[env.<name>]`).
    Env {
        #[command(subcommand)]
        action: cmd::env::Action,
    },
    /// Print the exact sandbox argv for a worktree (for debugging).
    SandboxArgv {
        /// Path to the worktree (defaults to the current directory).
        worktree: Option<String>,
    },
    /// Push, list, dismiss, or read notifications (plugin/script API).
    Notify {
        #[command(subcommand)]
        action: cmd::notify::Action,
    },
    /// Tail or query the szhost log file (plugin/script API).
    Logs {
        #[command(subcommand)]
        action: cmd::logs::Action,
    },
}

fn main() -> anyhow::Result<()> {
    // Cap glibc's per-thread arena count before the runtime spawns any threads,
    // so the host can't sprawl across dozens of never-trimmed arenas (an audit
    // traced ~2.5 GB RSS to ~131 of them). No-op off glibc. See `mem`.
    mem::tune_allocator();

    // Strip any inherited GIT_DIR/GIT_WORK_TREE/etc. before anything else (and
    // before the tokio runtime spawns threads — env mutation must be
    // single-threaded). superzej targets git explicitly with `-C <dir>`, so it
    // never needs an ambient GIT_DIR; leaving one in place would propagate to
    // every pane shell, agent, and sandbox we spawn and let a child `git
    // worktree add` leak `core.worktree` into the shared main `.git/config`.
    superzej_core::util::scrub_git_env();

    let mut cli = Cli::parse();
    if let Some(lvl) = cli.log_level.as_deref() {
        cli.overrides.push(format!("log.level={lvl}"));
    }

    // A subcommand runs synchronously and exits; no subcommand launches the
    // interactive compositor (the default).
    if let Some(command) = cli.command.take() {
        return run_subcommand(&cli, command);
    }

    // Manual runtime instead of #[tokio::main]: dropping a Runtime blocks on
    // every in-flight spawn_blocking task, so quitting would wait out whatever
    // hydration is mid-flight (git/tokei/podman subprocesses — easily 100ms+).
    // shutdown_background detaches those; exit is as instant as launch.
    //
    // Bounded over the `Runtime::new()` default (which sizes the worker pool to
    // ncpu and lets the blocking pool grow to 512): the host is I/O-bound, not
    // compute-bound, so a small worker pool keeps latency snappy, and a tight
    // blocking cap + short keep-alive stop the on-demand hydration/git threads
    // from sprawling (each thread is a glibc arena → RSS). Tunable, but these
    // defaults cut the steady-state thread count from ~60 without hurting feel.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(8)
        .max_blocking_threads(32)
        .thread_keep_alive(std::time::Duration::from_secs(3))
        .thread_name("szhost-rt")
        .build()?;
    let result = rt.block_on(run::main(cli));
    rt.shutdown_background();
    // termwiz opens /dev/tty without O_CLOEXEC; child pane shells inherit that
    // FD and keep the outer PTY open after szhost exits, preventing the parent
    // from seeing EOF. process::exit is the correct terminal-emulator exit: it
    // kills the whole process group atomically, matching what alacritty/kitty do.
    let code: i32 = match &result {
        Ok(()) => 0,
        Err(_) => 1,
    };
    std::process::exit(code);
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
        Command::Ci { action } => cmd::ci::run(&cfg, action),
        Command::Theme { action } => {
            let p = superzej_core::config::Config::path();
            cmd::theme::run(&cfg, action, p)
        }
        Command::Diff {
            worktree,
            base,
            stat,
            file,
        } => cmd::diff::run(worktree, base, stat, file),
        Command::List => cmd::list::run(&cfg),
        Command::Disk { worktree, all } => cmd::disk::disk(&cfg, worktree, all),
        Command::Clean {
            worktree,
            all,
            force,
        } => cmd::disk::clean(&cfg, worktree, all, force),
        Command::Repos => cmd::repos::repos(&cfg),
        Command::Recent { count } => cmd::repos::recent(count),
        Command::Config { action } => cmd::config::run(&cfg, action, config_path),
        Command::Env { action } => cmd::env::run(&cfg, action),
        Command::Notify { action } => cmd::notify::run(action),
        Command::Logs { action } => cmd::logs::run(&cfg, action),
        Command::SandboxArgv { worktree } => {
            let wt = worktree
                .or_else(|| std::env::current_dir().ok()?.to_str().map(str::to_string))
                .unwrap_or_default();
            match crate::agent::launch_spec(&cfg, &wt, None, "shell") {
                Ok(spec) => {
                    superzej_core::outln!("{}", spec.argv.join(" "));
                }
                Err(e) => {
                    superzej_core::msg::die(&format!("launch_spec failed: {e:#}"));
                }
            }
            Ok(())
        }
    }
}
