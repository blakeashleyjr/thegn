//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod acp_gate;
mod actions;
mod agent;
mod agent_configs;
mod agent_home;
mod agent_pi;
mod agent_ssh;
mod agent_teardown;
mod apps;
mod autoscale;
mod bar_nav;
mod borders;
mod bouncer;
mod bridge_sup;
mod build_cache;
mod caps;
mod center;
mod chrome;
mod cli_help;
mod clipboard;
mod cmd;
mod compositor;
mod copymode;
mod desktop_notify;
mod detail;
mod direnv_warm;
mod emulator;
mod env_ui;
mod env_wizard;
mod escape;
mod fff_backend;
mod fly_reaper;
mod focus;
mod font;
mod forward;
mod frame_write;
mod gitmut;
mod handlers;
mod host_flow;
mod host_provision;
mod host_ui;
mod hover;
mod hydrate;
mod hydrate_feed;
mod hydrate_terminal;
mod input;
mod integrate;
mod iroh_home;
mod keyhint;
mod keymap;
mod kitty_relay;
mod layer;
mod layout;
mod layout_spec;
mod lifecycle;
mod loading;
mod loc_scan;
mod logotype;
mod lsp;
mod managed_tool;
mod mem;
mod menu;
mod merge_driver;
mod metrics;
mod mousefilter;
mod nav;
mod nixcache;
mod notify;
mod palette;
mod pane;
mod panel;
mod panel_util;
mod panes;
mod perf;
mod pi_assets;
mod pins;
mod placement_flow;
mod pr_view;
mod predict;
mod probe;
mod profile;
mod provider_factory;
mod provision_gate;
mod provision_recover;
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
mod sched;
mod search;
mod search_everywhere;
mod secret;
mod seg;
mod sequence;
mod session;
mod share;
mod sidebar;
mod snapshot;
mod sprite_bridge;
mod ssh_shim;
mod subsystem;
mod tabbar_env;
mod task;
mod telemetry;
mod terminal_wizard;
#[cfg(test)]
mod testenv;
mod testkit;
mod toast;
mod vps_bridge;
mod vps_reaper;
mod warmcache;
mod wire;
mod wizard;
mod workspace_create;
mod workspace_picker;

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
    /// Worktree lifecycle + inspection (`wt list|diff|disk|clean`).
    Wt {
        #[command(subcommand)]
        action: cmd::wt::Action,
    },
    /// Repo discovery + history (`repo list|recent`).
    Repo {
        #[command(subcommand)]
        action: cmd::repos::Action,
    },
    /// Focus a repo in the running instance (DB-mailbox intent, ~1s pickup),
    /// or launch the compositor onto it when none is running.
    Open {
        /// Repo path (any dir inside it) or a unique repo basename.
        repo: String,
        /// Only record the pointer / intent; never launch the compositor.
        #[arg(long)]
        no_launch: bool,
    },
    /// Hidden legacy spelling of `wt diff` (kept working forever).
    #[command(hide = true)]
    Diff {
        #[command(flatten)]
        args: cmd::wt::DiffArgs,
    },
    /// Hidden legacy spelling of `wt list` (kept working forever).
    #[command(hide = true)]
    List {
        #[command(flatten)]
        args: cmd::wt::ListArgs,
    },
    /// Drain the local merge queue: fold eligible worktree branches into the
    /// repo's target branch, landing clean ones and deferring conflicts
    /// (`[merge_queue]`, the fold-actor).
    Integrate,
    /// Agent-driven merge queue: assign branches (`merge add`) and drain them one
    /// by one (`merge drain`), dispatching a headless CLI agent to resolve
    /// conflicts / fix the build (`[merge_queue]`).
    Merge {
        #[command(subcommand)]
        action: cmd::merge::Action,
    },
    /// Hidden legacy spelling of `wt disk` (kept working forever).
    #[command(hide = true)]
    Disk {
        #[command(flatten)]
        args: cmd::wt::DiskArgs,
    },
    /// Hidden legacy spelling of `wt clean` (kept working forever).
    #[command(hide = true)]
    Clean {
        #[command(flatten)]
        args: cmd::wt::CleanArgs,
    },
    /// Hidden legacy spelling of `repo list` (kept working forever).
    #[command(hide = true)]
    Repos {
        /// Emit one JSON array of paths instead of plain lines.
        #[arg(long)]
        json: bool,
    },
    /// Hidden legacy spelling of `repo recent` (kept working forever).
    #[command(hide = true)]
    Recent {
        count: Option<i64>,
        /// Emit one JSON array of paths instead of plain lines.
        #[arg(long)]
        json: bool,
    },
    /// Hidden legacy spelling of `repo trust` (kept working forever).
    #[command(hide = true)]
    RepoTrust {
        /// Repo path (default: current directory).
        path: Option<String>,
        /// Approve a pending request by its id.
        #[arg(long)]
        approve: Option<String>,
        /// Revoke a recorded decision by its id.
        #[arg(long)]
        revoke: Option<String>,
    },
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
    /// Manage zones (workspace groups with credential/egress/budget sub-scoping).
    Zone {
        #[command(subcommand)]
        action: cmd::zone::Action,
    },
    /// Inspect the placement engine: per-host resources (declared / reserved /
    /// measured), decision dry-runs, and recorded decision traces.
    Placement {
        #[command(subcommand)]
        action: cmd::placement::Action,
    },
    /// Inspect and provision `[host.<name>]` machines (the once-per-host
    /// lifecycle behind fast remote OCI sandboxes).
    Host {
        #[command(subcommand)]
        action: cmd::host::Action,
    },
    /// Install + configure superzej's managed pi under `~/.superzej/pi` (the
    /// "Agent" picker entry): a pinned binary + the superzej-acp extension.
    Agent {
        #[command(subcommand)]
        action: cmd::agent::Action,
    },
    /// BugStalker debugger: install/pin `bs`, or start a session (`debug run
    /// <program>` / `debug attach <pid>`) — run inside a pane to debug within
    /// its sandbox/placement.
    Debug {
        #[command(subcommand)]
        action: cmd::debug::Action,
    },
    /// User-declared MCP servers (`[mcp_servers.<name>]`): list, emit the
    /// `mcpServers` settings block, or install a server's binary (grant-checked).
    Mcp {
        #[command(subcommand)]
        action: cmd::mcp::Action,
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
    /// Generate shell completions (bash/zsh/fish/elvish/powershell) for the
    /// invoked binary name — `superzej completions zsh > …/_superzej`.
    Completions { shell: clap_complete::Shell },
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
    /// Hidden: the VPS exec bridge (`vps-ssh <name> -- cmd…`) — the CLI prefix a
    /// VPS provider env's panes and control-plane reads run through. Resolves
    /// the instance IP (registry, then API) and execs `ssh` with the managed
    /// key + per-instance known_hosts.
    #[command(hide = true)]
    VpsSsh {
        /// The managed instance name (the resolved sandbox id).
        name: String,
        /// Command to run (empty ⇒ a login shell).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Hidden: the sprites exec bridge (`sprite-exec <id> -- cmd…`) — the
    /// control-plane prefix a sprites (WSS-native) env's chrome git/gh/fs reads
    /// and persisted worktree location run through. Runs the command over the
    /// sprite's native exec API and relays its stdout. Panes attach over the
    /// exec API directly (not this bridge). Not for interactive use.
    #[command(hide = true)]
    SpriteExec {
        /// The sprite sandbox id (the resolved provider id).
        id: String,
        /// Command to run (empty ⇒ a login shell).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
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

    // Parse through the grouped-help wrapper (cli_help) so top-level --help
    // renders commands under semantic headings; behavior is otherwise
    // identical to `Cli::parse()`.
    let matches = cli_help::attach(<Cli as clap::CommandFactory>::command())
        .try_get_matches()
        .unwrap_or_else(|e| e.exit());
    let mut cli =
        <Cli as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
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
    // interactive compositor (the default). `open` is special: with no live
    // instance it falls THROUGH to the interactive launch below.
    if let Some(command) = cli.command.take() {
        let result = if let Command::Open { repo, no_launch } = command {
            let mut cfg = superzej_core::config::Config::load_layered(
                &superzej_core::config::ProcessEnv,
                &cli.overrides,
                cli.config.clone(),
            );
            superzej_core::host_config::merge_db_hosts(&mut cfg);
            match cmd::open::run(&cfg, &repo, no_launch) {
                Ok(cmd::open::OpenOutcome::Delivered) => Ok(()),
                Ok(cmd::open::OpenOutcome::LaunchTui) => Err(None), // fall through
                Err(e) => Err(Some(e)),
            }
        } else {
            run_subcommand(&cli, command).map_err(Some)
        };
        match result {
            Ok(()) => return Ok(()),
            // Typed not-found errors map to the scripting exit-code contract
            // (cmd::EXIT_NOT_FOUND); everything else keeps anyhow's exit 1.
            Err(Some(e)) if e.downcast_ref::<cmd::NotFound>().is_some() => {
                superzej_core::msg::error(&format!("{e:#}"));
                std::process::exit(cmd::EXIT_NOT_FOUND);
            }
            Err(Some(e)) => return Err(e),
            Err(None) => {} // `open` with no live instance: launch the TUI
        }
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
        Ok(()) => cmd::EXIT_OK,
        Err(_) => cmd::EXIT_ERROR,
    };
    std::process::exit(code);
}

/// Dispatch a non-interactive verb. Loads the layered config (the verbs that
/// need it) and routes to the ported `cmd` module.
fn run_subcommand(cli: &Cli, command: Command) -> anyhow::Result<()> {
    let mut cfg = superzej_core::config::Config::load_layered(
        &superzej_core::config::ProcessEnv,
        &cli.overrides,
        cli.config.clone(),
    );
    superzej_core::host_config::merge_db_hosts(&mut cfg);
    let cfg = cfg;
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
        Command::Wt { action } => cmd::wt::run(&cfg, action),
        Command::Repo { action } => cmd::repos::run(&cfg, action),
        // Dispatched before run_subcommand (it may fall through to the TUI);
        // unreachable here, kept for match exhaustiveness.
        Command::Open { repo, no_launch } => cmd::open::run(&cfg, &repo, no_launch).map(|_| ()),
        Command::Diff { args } => cmd::wt::run(&cfg, cmd::wt::Action::Diff(args)),
        Command::List { args } => cmd::wt::run(&cfg, cmd::wt::Action::List(args)),
        Command::Integrate => cmd::integrate::run(&cfg),
        Command::Merge { action } => cmd::merge::run(&cfg, action),
        Command::Disk { args } => cmd::wt::run(&cfg, cmd::wt::Action::Disk(args)),
        Command::Clean { args } => cmd::wt::run(&cfg, cmd::wt::Action::Clean(args)),
        Command::Repos { json } => cmd::repos::repos(&cfg, json),
        Command::RepoTrust {
            path,
            approve,
            revoke,
        } => cmd::repos::trust(&cfg, path, approve, revoke),
        Command::Recent { count, json } => cmd::repos::recent(count, json),
        Command::Config { action } => cmd::config::run(&cfg, action, config_path),
        Command::Env { action } => cmd::env::run(&cfg, action),
        Command::Zone { action } => cmd::zone::run(&cfg, action),
        Command::Placement { action } => cmd::placement::run(&cfg, action),
        Command::Host { action } => cmd::host::run(&cfg, action),
        Command::Agent { action } => cmd::agent::run(&cfg, action),
        Command::Debug { action } => cmd::debug::run(&cfg, action),
        Command::Mcp { action } => cmd::mcp::run(&cfg, action),
        Command::Notify { action } => cmd::notify::run(action),
        Command::Logs { action } => cmd::logs::run(&cfg, action),
        Command::Doctor { json } => cmd::doctor::run(&cfg, json),
        Command::Completions { shell } => {
            // Generate against the same grouped command tree the parser uses,
            // named for the invoked alias (szhost / superzej / sj).
            let mut tree = cli_help::attach(<Cli as clap::CommandFactory>::command());
            let bin = std::env::args()
                .next()
                .and_then(|p| {
                    std::path::Path::new(&p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "szhost".into());
            // Buffer first: generate() panics on write errors, and a consumer
            // like `… | head` closing the pipe early is normal CLI life.
            let mut buf = Vec::new();
            clap_complete::generate(shell, &mut tree, bin, &mut buf);
            use std::io::Write;
            // best-effort: a closed pipe just means the reader got enough.
            let _ = std::io::stdout().write_all(&buf);
            Ok(())
        }
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
        Command::VpsSsh { name, cmd } => vps_bridge::run(&cfg, &name, &cmd),
        Command::SpriteExec { id, cmd } => sprite_bridge::run(&cfg, &id, &cmd),
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
