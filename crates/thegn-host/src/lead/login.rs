//! `thegn lead login [--mcp <server>]`: log the Lead's agent, or one of its
//! MCP servers, in once on the host (operator only, interactive).
//!
//! The Lead keeps a private harness home and a private MCP dir (`lead::launch`),
//! so a login done in the operator's own `~/.claude` never reaches it, and the
//! sandbox's read-only root refuses an OAuth flow inside. This verb runs the
//! same agent command (or the named `[mcp_servers.<server>]` argv) on the host,
//! with host network, its env pointed at those private dirs, and waits on it.
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

/// The configured Lead agent's interactive command and the env overlay a
/// login runs it with: the agent's secret-expanded env, then the private
/// harness home (created), exactly as the Lead launch sets them.
pub(crate) fn agent_login(
    cfg: &Config,
    state_home: &Path,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let lead = cfg
        .lead
        .as_ref()
        .context("[lead] is not configured (or is clamped off on this channel)")?;
    let eff = thegn_core::agent_task::effective_agent(cfg, &lead.agent, None)
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    let command = eff
        .interactive_command()
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    let mut env = eff.expanded_env();
    match crate::lead::launch::ensure_harness_home(cfg, state_home)? {
        Some((var, dir)) => env.push((var, dir.to_string_lossy().into_owned())),
        None => bail!(
            "[lead] agent {:?} runs {}, which has no private home to log in to",
            lead.agent,
            eff.harness
        ),
    }
    Ok((command, env))
}

/// The argv and env overlay of `[mcp_servers.<server>]` for a login: the
/// placeholder-expanded argv, the env with its secret refs resolved by the
/// same function the daemon's `mcp_proxy.profile_env` uses, then the private
/// MCP dir's cache and auth env (created).
fn mcp_login(
    cfg: &Config,
    server: &str,
    state_home: &Path,
) -> anyhow::Result<(Vec<String>, BTreeMap<String, String>)> {
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
    let dir = crate::lead::launch::ensure_mcp_dir(state_home)?;
    env.extend(crate::lead::launch::mcp_dir_env(&dir));
    Ok((spec.argv, env))
}

/// Run the login. Refused unless this is an operator's own terminal.
pub(crate) fn run(cfg: &Config, mcp: Option<&str>) -> anyhow::Result<()> {
    if let Some(why) = refusal(&LoginEnv::current()) {
        bail!(why);
    }
    if cfg.lead.is_none() {
        bail!("the Lead is not configured: add a [lead] section naming an [[agents]] entry");
    }
    let state_home = thegn_core::util::xdg_state_home();
    let mut cmd = match mcp {
        None => {
            let (command, env) = agent_login(cfg, &state_home)?;
            let argv = thegn_core::shellinv::exec_argv(&thegn_core::util::shell(), &command);
            let (prog, rest) = argv.split_first().context("no shell to run the agent")?;
            let mut cmd = std::process::Command::new(prog);
            cmd.args(rest).envs(env);
            tracing::info!(target: "thegn::lead", "lead login: running the Lead agent on the host");
            thegn_core::outln!("log in with /login, then exit");
            cmd
        }
        Some(server) => {
            let (argv, env) = mcp_login(cfg, server, &state_home)?;
            let (prog, rest) = argv.split_first().context("no MCP server command")?;
            // No shell: the configured argv as is.
            let mut cmd = std::process::Command::new(prog);
            cmd.args(rest).envs(env);
            tracing::info!(target: "thegn::lead", server, "lead login: running the Lead MCP server on the host");
            thegn_core::outln!(
                "running MCP server {server:?} for its login; open the URL it prints, then press \
                 Ctrl-C"
            );
            cmd
        }
    };
    // Inherit stdin/stdout/stderr so an OAuth URL or a login prompt reaches
    // the operator.
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    wait_interactive(cmd)
}

/// Spawn `cmd` and wait for it. On unix, SIGINT is caught for the duration so
/// Ctrl-C ends the child (it shares the terminal's foreground process group)
/// and thegn still reaps it.
fn wait_interactive(mut cmd: std::process::Command) -> anyhow::Result<()> {
    #[cfg(unix)]
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start the signal runtime")?;
    #[cfg(unix)]
    let _interrupt = {
        let _entered = rt.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("catch Ctrl-C for the login")?
    };
    let mut child = cmd.spawn().context("start the login")?;
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
        let (argv, env) = mcp_login(&cfg, "todoist", state.path()).unwrap();
        assert_eq!(argv, ["npx", "-y", "todoist-mcp"]);
        assert_eq!(env.get("A").map(String::as_str), Some("b"));
        let dir = std::fs::canonicalize(state.path().join("thegn/lead-harness/mcp")).unwrap();
        assert_eq!(
            env.get("THEGN_LEAD_MCP_HOME").map(String::as_str),
            Some(dir.to_string_lossy().as_ref())
        );
        assert_eq!(
            env.get("npm_config_cache").map(String::as_str),
            Some(dir.join("npm").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn login_agent_env_uses_the_launch_harness_home() {
        let cfg = lead_cfg();
        let state = tempfile::tempdir().unwrap();
        let state_home = std::fs::canonicalize(state.path()).unwrap();
        let (command, env) = agent_login(&cfg, &state_home).unwrap();
        assert!(command.starts_with("claude"), "{command}");
        let (var, dir) = crate::lead::launch::harness_home_env(&cfg, &state_home).unwrap();
        assert!(
            env.iter()
                .any(|(k, v)| *k == var && *v == dir.to_string_lossy()),
            "{env:?}"
        );
        assert!(dir.is_dir(), "the login creates the private home");
    }
}
