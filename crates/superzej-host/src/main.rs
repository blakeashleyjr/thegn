//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod agent;
mod apps;
mod borders;
mod bouncer;
mod bridge_sup;
mod caps;
mod center;
mod chrome;
mod clipboard;
mod cmd;
mod compositor;
mod copymode;
mod desktop_notify;
mod detail;
mod emulator;
mod focus;
mod font;
mod forward;
mod gitmut;
mod hover;
mod hydrate;
mod input;
mod integrate;
mod keyhint;
mod keymap;
mod kitty_relay;
mod layer;
mod layout;
mod layout_spec;
mod lifecycle;
mod loading;
mod logotype;
mod lsp;
mod mem;
mod menu;
mod metrics;
mod mousefilter;
mod nixcache;
mod notify;
mod palette;
mod pane;
mod panel;
mod panes;
mod perf;
mod pi_assets;
mod pins;
mod predict;
mod probe;
mod profile;
mod proxy_daemon;
mod queries;
mod recorder;
mod relay;
mod render_plan;
mod replay;
mod replay_overlay;
mod revtunnel;
mod run;
mod sandbox_events;
mod search;
mod search_everywhere;
mod seg;
mod sequence;
mod session;
mod share;
mod sidebar;
mod subsystem;
mod tabbar_env;
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

    /// Run under a named **profile** — a whole-process firewall (separate
    /// state/DB, config overlay, credentials): e.g. `--profile work`. Falls back
    /// to `SUPERZEJ_PROFILE`; absent / `default` keeps today's shared paths.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

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
    /// Expose a worktree-local port at a public URL (`[share]`).
    Share {
        #[command(subcommand)]
        action: cmd::share::Action,
    },
    /// Inspect auto port forwards / browser previews (`[forward]`).
    Forward {
        #[command(subcommand)]
        action: cmd::forward::Action,
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
    /// Drain the local merge queue: fold eligible worktree branches into the
    /// repo's target branch, landing clean ones and deferring conflicts
    /// (`[merge_queue]`, the fold-actor).
    Integrate,
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
    /// Install + configure superzej's managed pi under `~/.superzej/pi` (the
    /// "Agent" picker entry): a pinned binary + the superzej-acp extension.
    Agent {
        #[command(subcommand)]
        action: cmd::agent::Action,
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
    /// Report detected terminal capabilities and the resulting feature
    /// degradation (color depth, glyphs, undercurl, mouse).
    Doctor {
        /// Emit machine-readable JSON instead of the text report.
        #[arg(long)]
        json: bool,
    },
    /// Hidden: run the resident bridge agent over stdio. The host spawns this
    /// *inside* a remote env (`ssh … szhost bridge`, `sprite exec … szhost
    /// bridge`); it speaks the framed bridge protocol (git/fs/proc) on stdin/
    /// stdout. Not for interactive use — stdout is the protocol channel.
    #[command(hide = true)]
    Bridge,
    /// Hidden: run the reverse-tunnel agent over stdio. The host spawns this
    /// *inside* a remote env; it binds `127.0.0.1:<port>` in the sandbox and
    /// multiplexes every connection to that port back to the host (which dials the
    /// real target, e.g. the local `szproxy`) over this stdio channel. stdout is
    /// the protocol channel — not for interactive use.
    #[command(hide = true)]
    BridgeRevtunnel {
        /// Loopback port to listen on inside the sandbox.
        port: u16,
    },
    /// Hidden: ssh `ProxyCommand` for a provider env with `connect = "ssh"`. Opens
    /// the provider's TCP-over-WebSocket proxy to the in-sandbox `sshd`
    /// (`127.0.0.1:22`) and relays it on stdin/stdout, so a local `ssh` client
    /// gets a native session over the WSS. stdout is the ssh transport — not for
    /// interactive use.
    #[command(hide = true)]
    SpriteProxy {
        /// Worktree path (defaults to the current dir) — selects the env/sandbox.
        worktree: Option<String>,
    },
}

/// Relay a local `ssh` client's stdin/stdout to an in-sandbox `sshd` over the
/// provider's TCP-over-WebSocket proxy (the `sprite-proxy` ProxyCommand). Pumps
/// until either side closes; stdout carries the ssh transport verbatim.
async fn sprite_proxy_relay(
    provider: superzej_svc::provider::Provider,
    id: String,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Ensure the in-sandbox sshd is listening (idempotent; survives restarts).
    let _ = provider
        .run_exec(
            &id,
            &[
                "/bin/sh".to_string(),
                "-lc".to_string(),
                agent::sprite_sshd_start_script(),
            ],
            None,
            &[],
        )
        .await;
    let mut stream = provider
        .open_proxy(&id, "127.0.0.1", agent::SPRITE_SSHD_PORT)
        .await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => match n {
                Ok(0) | Err(_) => break, // client EOF / error
                Ok(n) => {
                    if stream.tx.send(buf[..n].to_vec()).await.is_err() {
                        break; // relay closed
                    }
                }
            },
            msg = stream.rx.recv() => match msg {
                Some(b) => {
                    stdout.write_all(&b).await?;
                    stdout.flush().await?;
                }
                None => break, // server closed
            },
        }
    }
    Ok(())
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

    // Reroot the process environment for the active profile (H) BEFORE the tokio
    // runtime or any thread starts — a whole-process firewall enforced via
    // `std::env::set_var` of profile-scoped roots (state/DB/logs), which every
    // path/sandbox/token read then honors for free. No-op for the default
    // profile (today's shared paths). Sequencing is load-bearing: a single
    // `Db::open()` before this would touch the wrong (shared) DB. Runs for
    // subcommands too, so `superzej --profile work pr …` uses the work DB.
    superzej_core::profile::reroot(cli.profile.as_deref());

    // A subcommand runs synchronously and exits; no subcommand launches the
    // interactive compositor (the default).
    if let Some(command) = cli.command.take() {
        return run_subcommand(&cli, command);
    }

    // Per-profile advisory singleton (H): one interactive window per named
    // profile. Advisory only — if the profile is already running we warn and
    // continue (per-profile DBs are separate + WAL-safe; a hard refusal would
    // break running szhost inside szhost). No-op for the default profile. The
    // guard is held for the whole process (released on exit/death, never stale).
    let _profile_lock = superzej_core::profile::acquire_singleton();
    if matches!(
        _profile_lock,
        superzej_core::profile::Singleton::AlreadyRunning
    ) {
        superzej_core::msg::warn(&format!(
            "profile {:?} appears to be already running in another window; \
             continuing (windows share the profile's WAL database)",
            superzej_core::profile::name()
        ));
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
        Command::Share { action } => cmd::share::run(&cfg, action),
        Command::Forward { action } => cmd::forward::run(action),
        Command::Diff {
            worktree,
            base,
            stat,
            file,
        } => cmd::diff::run(worktree, base, stat, file),
        Command::List => cmd::list::run(&cfg),
        Command::Integrate => cmd::integrate::run(&cfg),
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
        Command::Agent { action } => cmd::agent::run(action),
        Command::Notify { action } => cmd::notify::run(action),
        Command::Logs { action } => cmd::logs::run(&cfg, action),
        Command::Doctor { json } => cmd::doctor::run(&cfg, json),
        Command::Bridge => {
            // The resident agent: framed protocol over stdio until EOF. stdout is
            // the protocol channel — nothing else may write to it.
            superzej_svc::bridge::serve(std::io::stdin().lock(), std::io::stdout());
            Ok(())
        }
        Command::BridgeRevtunnel { port } => {
            // Reverse-tunnel sandbox endpoint: listen on loopback and mux every
            // accepted connection back to the host over stdin/stdout.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
                let stream = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
                superzej_svc::revtunnel::run_sandbox(stream, listener).await
            })?;
            Ok(())
        }
        Command::SpriteProxy { worktree } => {
            // ssh ProxyCommand: relay the in-sandbox sshd (127.0.0.1:22) over the
            // provider's TCP-over-WebSocket proxy on stdin/stdout.
            let wt = worktree
                .or_else(|| std::env::current_dir().ok()?.to_str().map(str::to_string))
                .ok_or_else(|| anyhow::anyhow!("sprite-proxy: no worktree"))?;
            let (provider, id, _workdir) =
                agent::provider_proxy_target(&cfg, &wt).ok_or_else(|| {
                    anyhow::anyhow!(
                        "sprite-proxy: {wt} is not a provider env (or its API token is unset)"
                    )
                })?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(sprite_proxy_relay(provider, id))
        }
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
