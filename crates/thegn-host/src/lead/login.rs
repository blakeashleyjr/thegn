//! `thegn lead login [--mcp <server>]`: log the Lead's agent, or one of its
//! MCP servers, in once on the host (operator only, interactive).
//!
//! The Lead keeps a private harness home and a private MCP dir (`lead::launch`),
//! so a login done in the operator's own `~/.claude` never reaches it, and the
//! sandbox's read-only root refuses an OAuth flow inside. This verb runs the
//! agent binary (or the named `[mcp_servers.<server>]` argv) on the host, with
//! host network and a fresh throwaway config/cache dir (never a dir the Lead
//! can write), waits on it, then installs only the resulting credentials.
//! Nothing is minted and no sandbox is built: it is a host-trust act, the same
//! class as the `mcp_proxy` upstreams, and it is refused anywhere but an
//! operator's own terminal (a Lead, a worker, a pipeline stage, a pipe).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use thegn_core::config::Config;

/// The process facts a login is gated on, gathered once so the refusals are
/// pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoginEnv {
    /// Whether stdin is a terminal.
    pub stdin_tty: bool,
    /// Whether `THEGN_CALLER` is set (a Lead, a worker or an agent caller).
    pub caller_set: bool,
    /// Whether `THEGN_STAGE_LAUNCH` is set (a pipeline stage).
    pub stage_launch_set: bool,
}

impl LoginEnv {
    fn current() -> Self {
        use std::io::IsTerminal as _;
        Self {
            stdin_tty: std::io::stdin().is_terminal(),
            caller_set: std::env::var_os("THEGN_CALLER").is_some(),
            stage_launch_set: std::env::var_os("THEGN_STAGE_LAUNCH").is_some(),
        }
    }
}

/// Why this process may not run a login, if it may not. Pure.
pub(crate) fn refusal(env: &LoginEnv) -> Option<&'static str> {
    if env.caller_set {
        return Some(
            "`thegn lead login` is refused for a thegn caller (THEGN_CALLER is set): run it from \
             your own terminal, outside the Lead and any worker",
        );
    }
    if env.stage_launch_set {
        return Some(
            "`thegn lead login` is refused inside a pipeline stage (THEGN_STAGE_LAUNCH is set)",
        );
    }
    if !env.stdin_tty {
        return Some(
            "`thegn lead login` is interactive: run it from a terminal (stdin is not a TTY)",
        );
    }
    None
}

/// `server` must be a member of the Lead's `[mcp_proxy.profiles]` entry, and
/// a configured `[mcp_servers]` entry. Pure.
pub(crate) fn check_mcp_member(cfg: &Config, server: &str) -> anyhow::Result<()> {
    let lead = cfg
        .lead
        .as_ref()
        .context("[lead] is not configured (or is clamped off on this channel)")?;
    let profile = lead.mcp_profile.as_str();
    let member = cfg
        .mcp_proxy
        .profiles
        .get(profile)
        .is_some_and(|members| members.iter().any(|m| m == server));
    if !member {
        bail!(
            "MCP server {server:?} is not in the Lead's profile [mcp_proxy.profiles] {profile}: \
             `thegn lead login --mcp` logs in only the Lead's own servers"
        );
    }
    if !cfg.mcp_servers.contains_key(server) {
        bail!(
            "MCP server {server:?} is in [mcp_proxy.profiles] {profile} but has no [mcp_servers.{server}] entry"
        );
    }
    Ok(())
}

/// The Lead agent's login program and its env overlay, run in a fresh,
/// throwaway config dir: never the Lead's private harness home, which the
/// sandboxed Lead can write (a planted hook or MCP server there would run on
/// the host, unsandboxed). The tempdir is held until the login ends.
pub(crate) struct AgentLogin {
    /// The agent's binary (resolved on `PATH`), run with no arguments.
    pub prog: String,
    /// The agent's secret-expanded env plus `CLAUDE_CONFIG_DIR=<tmp>`.
    pub env: Vec<(String, String)>,
    /// The fresh config dir the login writes into.
    pub tmp: tempfile::TempDir,
}

/// The configured Lead agent's login: its binary alone (the first word of
/// its interactive command, no shell and no flags, so none of the Lead's
/// permission-bypass flags run on the host), with a fresh config dir.
pub(crate) fn agent_login(cfg: &Config) -> anyhow::Result<AgentLogin> {
    let lead = cfg
        .lead
        .as_ref()
        .context("[lead] is not configured (or is clamped off on this channel)")?;
    let eff = thegn_core::agent_task::effective_agent(cfg, &lead.agent, None)
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    if eff.harness != "claude" {
        bail!("`thegn lead login` supports claude only for now");
    }
    let command = eff
        .interactive_command()
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    let word = command
        .split_whitespace()
        .next()
        .context("[lead] agent has an empty command")?;
    let prog = thegn_core::util::which_path(word)
        .with_context(|| format!("[lead] agent: {word:?} was not found on PATH"))?;
    let tmp = tempfile::Builder::new()
        .prefix("thegn-lead-login-")
        .tempdir()
        .context("creating a fresh config dir for the login")?;
    let mut env = eff.expanded_env();
    env.push((
        "CLAUDE_CONFIG_DIR".to_string(),
        tmp.path().to_string_lossy().into_owned(),
    ));
    Ok(AgentLogin { prog, env, tmp })
}

/// The Lead's private harness home as a held handle (created owner-only).
fn harness_home_dir(
    cfg: &Config,
    state_home: &Path,
) -> anyhow::Result<thegn_core::fsperm::PrivateDir> {
    let (_, dir) = crate::lead::launch::harness_home_env(cfg, state_home)
        .context("the Lead agent has no private home to log in to")?;
    let rel = dir
        .strip_prefix(state_home)
        .context("the Lead's private harness home is outside $XDG_STATE_HOME")?;
    thegn_core::fsperm::ensure_private_dir_beneath(state_home, rel)
        .context("creating the Lead's private harness home")
}

/// Read `from/name` as a regular file, never following a symlink; `None` when
/// it is absent or not a regular file.
fn read_regular(from: &Path, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let path = from.join(name);
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(bytes))
}

/// Copy the login's `.credentials.json` (required) into the Lead's private
/// home `to`, and its `.claude.json` only when `to` has none. Nothing else is
/// copied and no symlink is followed.
fn install_login_files(from: &Path, to: &thegn_core::fsperm::PrivateDir) -> anyhow::Result<()> {
    let Some(creds) = read_regular(from, ".credentials.json")? else {
        bail!("no credentials were written: run /login before exiting");
    };
    to.write_owner_only_atomic(".credentials.json", &creds)
        .context("installing the Lead's credentials")?;
    if !to
        .entry_exists_nofollow(".claude.json")
        .context("checking the Lead's .claude.json")?
        && let Some(state) = read_regular(from, ".claude.json")?
    {
        to.write_owner_only_atomic(".claude.json", &state)
            .context("installing the Lead's .claude.json")?;
    }
    Ok(())
}

/// The argv and env overlay of `[mcp_servers.<server>]` for a login: the
/// placeholder-expanded argv, the env with its secret refs resolved by the
/// same function the daemon's `mcp_proxy.profile_env` uses, package caches in
/// a fresh tempdir (the Lead can write its own caches, so a planted package
/// there must never run on the host), and `MCP_REMOTE_CONFIG_DIR` in the same
/// fresh tempdir (the Lead can write its own `mcp-auth`, so a planted symlink,
/// lock or client info there must never steer the host login). The token data
/// is installed afterwards by `install_mcp_auth`. The tempdir is held until
/// the login ends.
fn mcp_login(
    cfg: &Config,
    server: &str,
) -> anyhow::Result<(Vec<String>, BTreeMap<String, String>, tempfile::TempDir)> {
    check_mcp_member(cfg, server)?;
    let srv = cfg
        .mcp_servers
        .get(server)
        .with_context(|| format!("no [mcp_servers.{server}] entry"))?;
    let spec = thegn_core::mcp::proxy::partition::expand_spec(
        server,
        srv,
        &thegn_core::mcp::proxy::partition::WorktreeContext::default(),
    )
    .map_err(|withheld| anyhow::anyhow!("MCP server {server:?}: {}", withheld.reason))?;
    if spec.argv.is_empty() {
        bail!("[mcp_servers.{server}] has no command");
    }
    let mut env = crate::mcp_proxy::resolve_server_env(server, &spec.env)
        .map_err(|e| anyhow::anyhow!("MCP server {server:?}: {e}"))?;
    let tmp = tempfile::Builder::new()
        .prefix("thegn-lead-login-")
        .tempdir()
        .context("creating fresh package caches for the login")?;
    let at = |sub: &str| tmp.path().join(sub).to_string_lossy().into_owned();
    env.insert("npm_config_cache".to_string(), at("npm"));
    env.insert("UV_CACHE_DIR".to_string(), at("uv"));
    env.insert("BUN_INSTALL_CACHE_DIR".to_string(), at("bun"));
    env.insert("MCP_REMOTE_CONFIG_DIR".to_string(), at("mcp-auth"));
    Ok((spec.argv, env, tmp))
}

/// Copy the login's mcp-remote token data from `from` (the fresh
/// `MCP_REMOTE_CONFIG_DIR`) into the Lead's private MCP dir's `mcp-auth`:
/// only `*_tokens.json` and `*_client_info.json` regular files inside real
/// `mcp-remote-*` directories. No lock, verifier or log is copied and no
/// symlink is followed.
fn install_mcp_auth(from: &Path, state_home: &Path) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", from.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("mcp-remote-") {
            continue;
        }
        let sub = entry.path();
        if !std::fs::symlink_metadata(&sub).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let rel = Path::new("thegn")
            .join(thegn_core::lead_sandbox::HARNESS_HOME_DIR)
            .join("mcp")
            .join("mcp-auth")
            .join(name);
        let dest = thegn_core::fsperm::ensure_private_dir_beneath(state_home, &rel)
            .context("creating the Lead's private mcp-auth dir")?;
        for file in std::fs::read_dir(&sub).with_context(|| format!("reading {}", sub.display()))? {
            let file = file.with_context(|| format!("reading {}", sub.display()))?;
            let file_name = file.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if !(file_name.ends_with("_tokens.json") || file_name.ends_with("_client_info.json")) {
                continue;
            }
            if let Some(bytes) = read_regular(&sub, file_name)? {
                dest.write_owner_only_atomic(file_name, &bytes)
                    .with_context(|| format!("installing the Lead's {name}/{file_name}"))?;
            }
        }
    }
    Ok(())
}

/// Inherit stdin/stdout/stderr so an OAuth URL or a login prompt reaches the
/// operator.
fn inherit_stdio(cmd: &mut std::process::Command) {
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
}

/// Run the login. Refused unless this is an operator's own terminal.
pub(crate) fn run(cfg: &Config, mcp: Option<&str>) -> anyhow::Result<()> {
    if let Some(why) = refusal(&LoginEnv::current()) {
        bail!(why);
    }
    if cfg.lead.is_none() {
        bail!(
            "the Lead is not configured: set it up from thegn (Alt i), or add a [lead] section \
             (on the stable channel it also needs `preview = true`)"
        );
    }
    let state_home = thegn_core::util::xdg_state_home();
    match mcp {
        None => {
            let login = agent_login(cfg)?;
            let dest = harness_home_dir(cfg, &state_home)?;
            // No shell, no flags: the agent binary alone, in the fresh dir so
            // no project `.claude/` of the operator's cwd is loaded either.
            let mut cmd = std::process::Command::new(&login.prog);
            cmd.envs(login.env.iter().map(|(k, v)| (k, v)))
                .current_dir(login.tmp.path());
            inherit_stdio(&mut cmd);
            tracing::info!(target: "thegn::lead", "lead login: running the Lead agent on the host");
            thegn_core::outln!("log in with /login, then exit");
            wait_interactive(cmd)?;
            install_login_files(login.tmp.path(), &dest)
        }
        Some(server) => {
            let (argv, env, tmp) = mcp_login(cfg, server)?;
            let (prog, rest) = argv.split_first().context("no MCP server command")?;
            // No shell: the configured argv as is.
            let mut cmd = std::process::Command::new(prog);
            cmd.args(rest).envs(env);
            inherit_stdio(&mut cmd);
            tracing::info!(target: "thegn::lead", server, "lead login: running the Lead MCP server on the host");
            thegn_core::outln!(
                "running MCP server {server:?} for its login; open the URL it prints, then press \
                 Ctrl-C"
            );
            let result = wait_interactive(cmd)
                .and_then(|()| install_mcp_auth(&tmp.path().join("mcp-auth"), &state_home));
            drop(tmp);
            result
        }
    }
}

/// Spawn `cmd` and wait for it. Ctrl-C is caught for the duration so it ends
/// the child (it shares the terminal's foreground process group) and thegn
/// still reaps it; the disposition is restored on drop.
fn wait_interactive(mut cmd: std::process::Command) -> anyhow::Result<()> {
    let _interrupt = crate::platform::InterruptGuard::install();
    let mut child = cmd.spawn().context("start the login")?;
    #[expect(
        clippy::disallowed_methods,
        reason = "an interactive operator-terminal login owns its child and blocks the CLI on it"
    )]
    let status = child.wait().context("wait for the login")?;
    tracing::info!(target: "thegn::lead", code = ?status.code(), "lead login: ended");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty() -> LoginEnv {
        LoginEnv {
            stdin_tty: true,
            ..LoginEnv::default()
        }
    }

    #[test]
    fn login_is_allowed_only_from_an_operator_terminal() {
        assert_eq!(refusal(&tty()), None);
        let no_tty = LoginEnv::default();
        assert!(refusal(&no_tty).is_some_and(|w| w.contains("not a TTY")));
        let caller = LoginEnv {
            caller_set: true,
            ..tty()
        };
        assert!(refusal(&caller).is_some_and(|w| w.contains("THEGN_CALLER")));
        let stage = LoginEnv {
            stage_launch_set: true,
            ..tty()
        };
        assert!(refusal(&stage).is_some_and(|w| w.contains("THEGN_STAGE_LAUNCH")));
    }

    fn lead_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.agents
            .push(toml::from_str("name = \"lead-agent\"\ncommand = \"claude\"").unwrap());
        cfg.lead = Some(thegn_core::config_lead::LeadConfig {
            agent: "lead-agent".into(),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn login_mcp_refuses_a_server_outside_the_lead_profile() {
        let mut cfg = lead_cfg();
        let profile = cfg.lead.as_ref().unwrap().mcp_profile.clone();
        cfg.mcp_servers.insert(
            "todoist".into(),
            toml::from_str("command = [\"npx\", \"todoist-mcp\"]").unwrap(),
        );
        cfg.mcp_servers.insert(
            "other".into(),
            toml::from_str("command = [\"npx\", \"other-mcp\"]").unwrap(),
        );
        // No profile at all.
        let err = check_mcp_member(&cfg, "todoist").unwrap_err().to_string();
        assert!(
            err.contains(&format!("[mcp_proxy.profiles] {profile}")),
            "{err}"
        );
        cfg.mcp_proxy
            .profiles
            .insert(profile.clone(), vec!["todoist".into()]);
        assert!(check_mcp_member(&cfg, "todoist").is_ok());
        let err = check_mcp_member(&cfg, "other").unwrap_err().to_string();
        assert!(
            err.contains("\"other\" is not in the Lead's profile"),
            "{err}"
        );
        // A profile member with no server entry.
        cfg.mcp_proxy
            .profiles
            .insert(profile, vec!["todoist".into(), "ghost".into()]);
        assert!(check_mcp_member(&cfg, "ghost").is_err());
        // No [lead]: refused.
        cfg.lead = None;
        assert!(check_mcp_member(&cfg, "todoist").is_err());
    }

    #[test]
    fn login_mcp_env_points_at_the_private_mcp_dir() {
        let mut cfg = lead_cfg();
        let profile = cfg.lead.as_ref().unwrap().mcp_profile.clone();
        cfg.mcp_servers.insert(
            "todoist".into(),
            toml::from_str(
                "command = [\"npx\"]\nargs = [\"-y\", \"todoist-mcp\"]\nenv = { A = \"b\" }",
            )
            .unwrap(),
        );
        cfg.mcp_proxy
            .profiles
            .insert(profile, vec!["todoist".into()]);
        let state = tempfile::tempdir().unwrap();
        let (argv, env, tmp) = mcp_login(&cfg, "todoist").unwrap();
        assert_eq!(argv, ["npx", "-y", "todoist-mcp"]);
        assert_eq!(env.get("A").map(String::as_str), Some("b"));
        // The package caches and the mcp-remote config dir are fresh, never
        // the Lead-writable ones.
        let lead_harness = state.path().join("thegn/lead-harness");
        let canon_harness = std::fs::canonicalize(state.path())
            .unwrap()
            .join("thegn/lead-harness");
        for var in [
            "npm_config_cache",
            "UV_CACHE_DIR",
            "BUN_INSTALL_CACHE_DIR",
            "MCP_REMOTE_CONFIG_DIR",
        ] {
            let v = std::path::PathBuf::from(env.get(var).expect(var));
            assert!(
                !v.starts_with(&lead_harness) && !v.starts_with(&canon_harness),
                "{var}: {}",
                v.display()
            );
            assert!(v.starts_with(tmp.path()), "{var}: {}", v.display());
        }
        assert!(!env.contains_key("THEGN_LEAD_MCP_HOME"));
    }

    #[test]
    fn login_mcp_installs_only_regular_token_files() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("mcp-auth");
        let sub = from.join("mcp-remote-0.1");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x_tokens.json"), "tokens").unwrap();
        std::fs::write(sub.join("x_lock.json"), "{\"pid\":1,\"port\":1}").unwrap();
        let secret = root.path().join("secret");
        std::fs::write(&secret, "owner secret").unwrap();
        let linked =
            thegn_core::fsperm::create_symlink(&secret, &sub.join("y_tokens.json"), false).is_ok();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        install_mcp_auth(&from, &state).unwrap();
        let dest = state.join("thegn/lead-harness/mcp/mcp-auth/mcp-remote-0.1");
        assert_eq!(
            std::fs::read_to_string(dest.join("x_tokens.json")).unwrap(),
            "tokens"
        );
        assert!(!dest.join("x_lock.json").exists());
        if linked {
            assert!(std::fs::symlink_metadata(dest.join("y_tokens.json")).is_err());
        }
    }

    #[test]
    fn login_installs_only_the_credentials_and_a_missing_claude_json() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("from");
        std::fs::create_dir(&from).unwrap();
        let to =
            thegn_core::fsperm::ensure_private_dir_beneath(root.path(), Path::new("to")).unwrap();
        // A missing .credentials.json is an error.
        let err = install_login_files(&from, &to).unwrap_err().to_string();
        assert!(err.contains("no credentials were written"), "{err}");
        // A symlinked .credentials.json is refused, not followed.
        let secret = root.path().join("secret");
        std::fs::write(&secret, "owner secret").unwrap();
        if thegn_core::fsperm::create_symlink(&secret, &from.join(".credentials.json"), false)
            .is_ok()
        {
            assert!(install_login_files(&from, &to).is_err());
            assert!(!to.entry_exists_nofollow(".credentials.json").unwrap());
            std::fs::remove_file(from.join(".credentials.json")).unwrap();
        }
        // The credentials are copied; an existing .claude.json is kept; no
        // other file is copied.
        std::fs::write(from.join(".credentials.json"), "creds").unwrap();
        std::fs::write(from.join(".claude.json"), "new").unwrap();
        std::fs::write(from.join("settings.json"), "{\"hooks\":{}}").unwrap();
        std::fs::write(to.path().join(".claude.json"), "old").unwrap();
        install_login_files(&from, &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.path().join(".credentials.json")).unwrap(),
            "creds"
        );
        assert_eq!(
            std::fs::read_to_string(to.path().join(".claude.json")).unwrap(),
            "old"
        );
        assert!(!to.path().join("settings.json").exists());
        // With none at the destination, .claude.json is copied.
        std::fs::remove_file(to.path().join(".claude.json")).unwrap();
        install_login_files(&from, &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.path().join(".claude.json")).unwrap(),
            "new"
        );
    }
}
