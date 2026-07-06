//! `szhost sprite-exec <id> [--] cmd…` — the self-bridge that gives a sprites
//! (WSS-native) env a CLI exec prefix for the control-plane READ path: the
//! chrome git/gh/fs reads and the persisted worktree [`GitLoc`] all shell out
//! through it (the role `vps-ssh` plays for VPS envs). Interactive panes attach
//! over the provider's native exec API directly (see `agent::native_exec_for`),
//! NOT this bridge — so this stays a non-tty, one-shot command runner.
//!
//! Resolves the sprites provider from the first WSS-native env whose API token
//! resolves, builds it named with THIS id (exec takes the id as an argument),
//! runs the command over the native exec API, relays its captured stdout, and
//! exits with the command's code — so a caller like `git -C /workspace status`
//! reads the output on the child's stdout exactly as if it had run locally.

use anyhow::{Result, anyhow};
use superzej_core::config::Config;
use superzej_core::config_env_tables::wss_native_provider_kind;

/// Run `cmd` inside sprite `id` over the native exec API, relay stdout, and exit
/// with its code. `cmd` arrives already shaped by `GitLoc::provider_command`
/// (`/bin/sh -lc <script>`); an empty `cmd` falls back to a login shell.
pub fn run(cfg: &Config, id: &str, cmd: &[String]) -> Result<()> {
    let argv: Vec<String> = if cmd.is_empty() {
        vec![
            "/bin/sh".into(),
            "-lc".into(),
            "exec ${SHELL:-/bin/sh} -l".into(),
        ]
    } else {
        cmd.to_vec()
    };
    // Any WSS-native env's provider config works — the sandbox id is the routing
    // key, and exec takes it as an argument (like `vps-ssh` resolving by name).
    let provider = cfg
        .env
        .values()
        .filter(|envc| wss_native_provider_kind(&envc.provider.provider))
        .find_map(|envc| crate::provider_factory::provider_for_named(&envc.provider, id))
        .ok_or_else(|| {
            anyhow!("sprite-exec: no WSS-native env with a resolvable API token for id {id:?}")
        })?;
    let rt = tokio::runtime::Runtime::new()?;
    let (code, out) = rt.block_on(async { provider.run_exec(id, &argv, None, &[]).await })?;
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
    std::process::exit(code);
}
