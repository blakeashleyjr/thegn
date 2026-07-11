//! ACP (Agent Client Protocol) arming gate.
//!
//! Only the thegn-managed pi agent speaks ACP: its command references
//! `~/.thegn/pi`, so thegn seeds it the `thegn-acp` extension that binds
//! `ACP_PORT` on session start. For a plain shell (or claude/codex/aider, or a
//! vanilla pi) nothing ever binds the port, so arming the connect supervisor for
//! them yields a permanent `Connection refused` and a false `⚠ agent error` chip
//! (plus a spurious "N error(s) in thegn.log" badge). This module is the gate:
//! [`speaks_acp`] decides whether to arm ACP, and [`resolve_agent_channel`]
//! resolves the reach-back channel (extracted from `run.rs::attach_agent_pane`).

use thegn_core::config::Config;

/// How thegn reaches a launched agent's ACP server: a TCP loopback port (the
/// non-sandboxed path) or a bind-mounted unix socket (the sealed-bouncer path,
/// which crosses the container netns without network). `None` ⇒ no channel was
/// reserved (the agent runs without ACP servicing).
pub(crate) enum AgentChannel {
    Tcp(u16),
    Unix(std::path::PathBuf),
    None,
}

/// ACP wiring for a launched pane: the reach-back channel plus the two
/// lifecycle-scoped side effects the connect supervisor captures — a proxy key to
/// revoke on disconnect, and a model relay whose `Drop` tears down the socket.
/// All `None` when the agent doesn't speak ACP.
pub(crate) struct AcpArming {
    pub channel: AgentChannel,
    pub revoke_key: Option<String>,
    pub relay: Option<crate::relay::RelayHandle>,
}

/// Whether `agent_name` is the thegn-managed pi — the only agent that loads
/// the `thegn-acp` extension and therefore answers on the ACP channel. Keyed
/// on the same `.thegn/pi` command marker the sandbox provisioner uses to
/// decide it must install the managed pi (see `agent::…` `managed_pi`).
pub(crate) fn speaks_acp(cfg: &Config, agent_name: &str) -> bool {
    cfg.agents
        .iter()
        .find(|a| a.name == agent_name)
        .map(|a| a.command.contains(".thegn/pi"))
        .unwrap_or(false)
}

/// Resolve the ACP channel + proxy side effects for a launched pane, pushing any
/// pane env (`ACP_PORT`, `THEGN_PROXY_*`) onto `env`.
///
/// GATE: a non-ACP agent (`!speaks_acp`) returns all-`None` with NO env mutation
/// and NO side effects (no minted proxy key, no spawned relay), so the caller's
/// `!matches!(channel, AgentChannel::None)` guard leaves the supervisor unarmed —
/// a plain shell no longer produces a phantom `⚠ agent error`.
pub(crate) fn resolve_agent_channel(
    cfg: &Config,
    agent_name: &str,
    wt_path: &str,
    backend: &str,
    env: &mut Vec<(String, String)>,
) -> AcpArming {
    if !speaks_acp(cfg, agent_name) {
        return AcpArming {
            channel: AgentChannel::None,
            revoke_key: None,
            relay: None,
        };
    }

    // "The bouncer": when on and the agent is sandboxed, the launch path already
    // injected the proxy + tool-override env (and ACP_SOCKET) into the container's
    // `env_overrides`, and thegn reaches the agent over a bind-mounted unix
    // socket (TCP can't cross the sealed netns). Otherwise: the legacy TCP path,
    // wiring ACP_PORT + the proxy env onto the pane process env here.
    let bouncer = cfg.llm_proxy.bouncer && backend != "host";

    let channel: AgentChannel = if bouncer {
        AgentChannel::Unix(crate::bouncer::acp_socket_path(wt_path))
    } else {
        // Reserve a free localhost port and hand it to the agent via `ACP_PORT`.
        // The brief bind-then-drop is the standard ephemeral-port reservation; pi
        // re-binds it. Env is the reliable channel: it crosses `sh -lc` wrapping.
        match std::net::TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .filter(|p| *p != 0)
        {
            Some(port) => {
                env.push(("ACP_PORT".to_string(), port.to_string()));
                AgentChannel::Tcp(port)
            }
            None => AgentChannel::None,
        }
    };

    // Lower plane: route the agent's model through the proxy. In bouncer mode the
    // proxy env rides `env_overrides` (set at launch) and the key is the stable
    // per-worktree id; here (TCP path) we mint a fresh key + push the proxy env
    // onto the pane env. Either way the key is revoked when the agent disconnects.
    let revoke_key: Option<String> = if bouncer {
        // Stable id derived deterministically — matches what the launch path minted.
        cfg.llm_proxy
            .route_agent
            .then(|| crate::proxy_keys::agent_proxy_key_id(wt_path))
    } else if cfg.llm_proxy.route_agent {
        let key =
            crate::proxy_keys::mint_agent_proxy_key(wt_path, cfg.llm_proxy.upstream_binding());
        env.push((
            "THEGN_PROXY_BASE_URL".to_string(),
            format!("http://{}", cfg.llm_proxy.listen),
        ));
        env.push((
            "THEGN_PROXY_API".to_string(),
            cfg.llm_proxy.agent_api.clone(),
        ));
        env.push((
            "THEGN_PROXY_MODEL".to_string(),
            cfg.llm_proxy.agent_model.clone(),
        ));
        if let Some(k) = &key {
            env.push(("THEGN_PROXY_KEY".to_string(), k.clone()));
        }
        key
    } else {
        None
    };

    // Full network seal (sealed `agent_profile` → `network=none`): the agent's
    // only egress is a unix-socket relay to the host proxy, bind-mounted into the
    // container. Started here; torn down when the connection task ends.
    let relay =
        (bouncer && cfg.llm_proxy.route_agent && cfg.sandbox.agent_profile.forces_no_network())
            .then(|| {
                let sock = crate::bouncer::proxy_socket_path(wt_path);
                match crate::relay::spawn(sock, cfg.llm_proxy.listen.clone()) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        tracing::error!(target: "thegn::relay", "proxy relay failed to start: {e}");
                        None
                    }
                }
            })
            .flatten();

    AcpArming {
        channel,
        revoke_key,
        relay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::{Config, NamedCommand};

    fn named(name: &str, command: &str) -> NamedCommand {
        NamedCommand {
            name: name.to_string(),
            command: command.to_string(),
            hints: Vec::new(),
            provider: None,
        }
    }

    fn cfg_with(agents: &[(&str, &str)]) -> Config {
        Config {
            agents: agents.iter().map(|(n, c)| named(n, c)).collect(),
            ..Config::default()
        }
    }

    // The managed pi's command references `~/.thegn/pi` (sets PI_CODING_AGENT_DIR
    // there + runs the pinned binary). A vanilla pi (npx) does NOT.
    const MANAGED_PI: &str = "PI_CODING_AGENT_DIR=\"$HOME/.thegn/pi/agent\" exec \"$HOME/.thegn/pi/node_modules/.bin/pi\"";

    #[test]
    fn speaks_acp_true_only_for_managed_pi() {
        let cfg = cfg_with(&[
            ("shell", "__shell__"),
            ("Agent", MANAGED_PI),
            ("claude", "claude"),
            ("codex", "codex"),
            ("Vanilla Pi", "npx -y @earendil-works/pi-coding-agent"),
        ]);
        assert!(speaks_acp(&cfg, "Agent"));
        assert!(!speaks_acp(&cfg, "shell"));
        assert!(!speaks_acp(&cfg, "claude"));
        assert!(!speaks_acp(&cfg, "codex"));
        assert!(!speaks_acp(&cfg, "Vanilla Pi"));
        assert!(!speaks_acp(&cfg, "nonexistent"));
    }

    // Regression guard for the phantom-agent-error bug: a plain shell must NOT
    // reserve a port, mint a key, spawn a relay, or push any ACP/proxy env.
    #[test]
    fn shell_gets_no_channel_and_no_env() {
        let cfg = cfg_with(&[("shell", "__shell__")]);
        let mut env = Vec::new();
        let arming = resolve_agent_channel(&cfg, "shell", "/w/shell", "bwrap", &mut env);
        assert!(matches!(arming.channel, AgentChannel::None));
        assert!(arming.revoke_key.is_none());
        assert!(arming.relay.is_none());
        assert!(env.is_empty(), "no ACP_PORT / proxy env for a shell");
    }

    // Config::default() has bouncer + route_agent OFF ⇒ the plain TCP path: the
    // managed pi reserves exactly one ACP_PORT and no proxy vars / key / relay.
    #[test]
    fn managed_pi_tcp_reserves_port_and_pushes_env() {
        let cfg = cfg_with(&[("Agent", MANAGED_PI)]);
        let mut env = Vec::new();
        let arming = resolve_agent_channel(&cfg, "Agent", "/w/agent", "bwrap", &mut env);
        match arming.channel {
            AgentChannel::Tcp(p) => assert_ne!(p, 0),
            _ => panic!("expected a reserved TCP port for the managed pi"),
        }
        assert!(
            arming.revoke_key.is_none(),
            "route_agent off ⇒ no proxy key"
        );
        assert!(arming.relay.is_none());
        assert_eq!(
            env.iter().filter(|(k, _)| k == "ACP_PORT").count(),
            1,
            "exactly one ACP_PORT entry"
        );
        assert!(
            !env.iter().any(|(k, _)| k.starts_with("THEGN_PROXY_")),
            "no proxy vars when route_agent is off"
        );
    }
}
