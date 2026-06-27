//! "The bouncer" — run a launched agent inside its sealed `agent_profile`
//! container, route its built-in `bash`/`read`/`edit`/`write` tools back to
//! superzej over a bind-mounted unix-socket ACP channel, and gate the
//! consequential ones behind an interactive allow/deny overlay.
//!
//! The async wiring (spawning the agent, the approval mpsc, opening the overlay)
//! lives in [`crate::run`]; the model-traffic relay lives in [`crate::relay`].
//! This module is the **pure, testable core**:
//!
//! - [`gated_kind`] — which inbound ACP requests need approval (shell/edit/write;
//!   reads + MCP frames pass through).
//! - [`ApprovalQueue`] — the single-active FIFO gate the overlay resolves against.
//! - [`acp_socket_path`] / [`proxy_socket_path`] — per-agent unix-socket paths,
//!   bind-mounted path-preserving into the sealed container.
//! - [`proxy_reach`] — how a sandboxed agent reaches the host model proxy
//!   (loopback / OCI gateway / full-seal unix relay), derived from its backend
//!   and resolved network.
//! - [`agent_env_plan`] — the env vars + socket mounts to inject so the in-sandbox
//!   pi extension wires its provider + tool override correctly.
//!
//! All of it is exercised by the unit tests at the bottom; the live path (a real
//! podman sealed run + the interactive overlay) is the only piece this can't drive.
// A complete, unit-tested policy surface: some accessors (`gated_kind`,
// `is_idle`) document the contract + back the tests even where the loop reads
// the equivalent state another way. Mirrors `menu.rs`'s module-level allow.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use superzej_core::config::Network;
use superzej_core::sandbox::{Backend, Mount};
use superzej_svc::acp::client::AcpInbound;

/// The class of a gated tool call — drives the overlay's verb + glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// `terminal/create` — the agent wants to run a shell command.
    Shell,
    /// `superzej/edit` — an in-place edit of a worktree file.
    Edit,
    /// `superzej/write` — a full-content write of a worktree file.
    Write,
}

impl ApprovalKind {
    /// Imperative phrase for the overlay title ("the agent wants to …").
    pub fn verb(self) -> &'static str {
        match self {
            ApprovalKind::Shell => "run a shell command",
            ApprovalKind::Edit => "edit a file",
            ApprovalKind::Write => "write a file",
        }
    }

    /// A short glyph for the overlay chrome.
    pub fn glyph(self) -> &'static str {
        match self {
            ApprovalKind::Shell => "$",
            ApprovalKind::Edit => "✎",
            ApprovalKind::Write => "✚",
        }
    }
}

/// Whether an inbound ACP request is gated in bouncer mode, and the human detail
/// to show (the command, or the file path). Reads (`fs/read_text_file`), MCP
/// frames, notifications and lifecycle messages are **not** gated — only the
/// three consequential tools the sealed agent routes back to the host.
pub fn gated_kind(inbound: &AcpInbound) -> Option<(ApprovalKind, String)> {
    match inbound {
        AcpInbound::TerminalCreateRequest { command, .. } => {
            Some((ApprovalKind::Shell, command.clone()))
        }
        AcpInbound::SuperzejEditRequest { path, .. } => Some((ApprovalKind::Edit, path.clone())),
        AcpInbound::SuperzejWriteRequest { path, .. } => Some((ApprovalKind::Write, path.clone())),
        _ => None,
    }
}

/// A single-line summary for the overlay body: the detail flattened to one line
/// and truncated so a long command / path can't blow out the centered layer.
pub fn summary(detail: &str) -> String {
    const MAX: usize = 72;
    let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX {
        let kept: String = flat.chars().take(MAX - 1).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

/// One pending approval: the worktree it came from, what it wants to do, the
/// summary shown, and an opaque reply handle the loop resolves with the user's
/// allow/deny decision. Generic over the reply so the queue logic is testable
/// without tokio.
#[derive(Debug)]
pub struct ApprovalRequest<R> {
    pub worktree: String,
    pub kind: ApprovalKind,
    pub detail: String,
    pub reply: R,
}

/// A FIFO approval gate: at most one request is *active* (shown in the overlay)
/// at a time; the rest queue in arrival order. The loop opens the overlay for
/// the active request and, on the user's pick, [`resolve`](Self::resolve)s it —
/// promoting the next queued request (if any) to active.
#[derive(Debug)]
pub struct ApprovalQueue<R> {
    active: Option<ApprovalRequest<R>>,
    queued: VecDeque<ApprovalRequest<R>>,
}

impl<R> Default for ApprovalQueue<R> {
    fn default() -> Self {
        Self {
            active: None,
            queued: VecDeque::new(),
        }
    }
}

impl<R> ApprovalQueue<R> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing pending and nothing showing.
    pub fn is_idle(&self) -> bool {
        self.active.is_none() && self.queued.is_empty()
    }

    /// The request currently shown in the overlay, if any.
    pub fn active(&self) -> Option<&ApprovalRequest<R>> {
        self.active.as_ref()
    }

    /// Add a request. Returns `true` when it became the active request (the gate
    /// was idle), signalling the caller to open the overlay; `false` when it was
    /// queued behind an already-open one.
    pub fn enqueue(&mut self, req: ApprovalRequest<R>) -> bool {
        if self.active.is_none() {
            self.active = Some(req);
            true
        } else {
            self.queued.push_back(req);
            false
        }
    }

    /// Resolve the active request: returns it (so the caller can send the user's
    /// decision on its `reply`) and promotes the next queued request to active.
    /// The newly-active request (if any) is then available via [`active`](Self::active).
    pub fn resolve(&mut self) -> Option<ApprovalRequest<R>> {
        let done = self.active.take();
        self.active = self.queued.pop_front();
        done
    }
}

/// The directory holding per-agent control sockets. Prefer the runtime dir
/// (tmpfs, short path — unix socket paths cap at ~108 bytes) and fall back to the
/// state home. Bind-mounted path-preserving into the sealed container, so the
/// in-container path equals the host path.
fn socket_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(superzej_core::util::xdg_state_home)
        .join("superzej")
}

/// A stable short token for a worktree path (hashed to keep socket paths short).
fn short_token(worktree: &str) -> String {
    let mut h = DefaultHasher::new();
    worktree.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Host path of the agent's ACP control socket (bind-mounted into the sealed
/// container at the same path).
pub fn acp_socket_path(worktree: &str) -> PathBuf {
    socket_dir().join(format!("acp-{}.sock", short_token(worktree)))
}

/// Host path of the agent's model-proxy relay socket (the full-seal egress).
pub fn proxy_socket_path(worktree: &str) -> PathBuf {
    socket_dir().join(format!("proxy-{}.sock", short_token(worktree)))
}

/// How a sandboxed agent reaches the host's model proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyReach {
    /// A reachable base URL (loopback for host/bwrap which share the netns, or
    /// the OCI backend's host-gateway alias under NAT).
    Url(String),
    /// Full network seal (`network=none`): no IP egress. The proxy is relayed
    /// over a bind-mounted unix socket; the extension dials it directly.
    Unix,
}

/// Resolve [`ProxyReach`] from the agent's placement. `sandbox` is the resolved
/// `(backend, network)` of the agent container, or `None` for a host pane.
pub fn proxy_reach(listen: &str, sandbox: Option<(Backend, Network)>) -> ProxyReach {
    let port = listen.rsplit(':').next().unwrap_or("8383");
    match sandbox {
        // Host pane: loopback is the host's own.
        None => ProxyReach::Url(format!("http://{listen}")),
        // Sealed: no IP egress at all — relay over a unix socket.
        Some((_, Network::None)) => ProxyReach::Unix,
        // OCI under NAT: the container reaches the host via the gateway alias.
        Some((backend, _)) if backend.is_oci() => {
            let gw = match backend {
                Backend::Docker | Backend::Smol => "host.docker.internal",
                _ => "host.containers.internal",
            };
            ProxyReach::Url(format!("http://{gw}:{port}"))
        }
        // bwrap shares the host network namespace, so loopback works.
        Some(_) => ProxyReach::Url(format!("http://{listen}")),
    }
}

/// The env vars + socket mounts to inject for a launched agent so the in-sandbox
/// pi extension wires its model provider and (in bouncer mode) its tool override.
#[derive(Debug, Default, Clone)]
pub struct AgentEnvPlan {
    /// `(key, value)` env vars — injected into `env_overrides` for a sandbox
    /// (exported inside the container) or the pane env for a host pane.
    pub vars: Vec<(String, String)>,
    /// Path-preserving rw mounts (the ACP socket dir, and the proxy relay socket
    /// dir under full seal) for a sandboxed agent.
    pub mounts: Vec<Mount>,
    /// Host path superzej connects to for ACP. `Some` ⇒ unix-socket transport
    /// (bouncer + sandbox); `None` ⇒ the caller uses the TCP port.
    pub acp_socket: Option<PathBuf>,
    /// Host path superzej must serve the model-proxy relay on (full seal only).
    pub proxy_relay_socket: Option<PathBuf>,
}

/// Build the [`AgentEnvPlan`] for launching `choice` in `worktree`.
///
/// `sandbox` is the resolved `(backend, network)` of the agent container, or
/// `None` for a host pane. `proxy_key` is the per-worktree virtual key already
/// minted for spend attribution (or `None`). Pure — no DB, no I/O — so the whole
/// wiring decision is unit-tested.
pub fn agent_env_plan(
    cfg: &superzej_core::config::Config,
    worktree: &str,
    sandbox: Option<(Backend, Network)>,
    proxy_key: Option<&str>,
) -> AgentEnvPlan {
    let mut plan = AgentEnvPlan::default();
    let lp = &cfg.llm_proxy;
    let sandboxed = sandbox.is_some();

    // Lower plane: point the agent's model traffic at the proxy. The pi extension
    // registers the provider AT INIT from these env vars.
    if lp.route_agent {
        plan.vars
            .push(("SUPERZEJ_PROXY_API".into(), lp.agent_api.clone()));
        plan.vars
            .push(("SUPERZEJ_PROXY_MODEL".into(), lp.agent_model.clone()));
        if let Some(k) = proxy_key {
            plan.vars.push(("SUPERZEJ_PROXY_KEY".into(), k.to_string()));
        }
        match proxy_reach(&lp.listen, sandbox) {
            ProxyReach::Url(url) => {
                plan.vars.push(("SUPERZEJ_PROXY_BASE_URL".into(), url));
            }
            ProxyReach::Unix => {
                // Full seal: the extension dials the proxy over this socket. The
                // base URL is a placeholder host the relay services; the unix
                // path is the real channel.
                let sock = proxy_socket_path(worktree);
                let sock_s = sock.to_string_lossy().into_owned();
                plan.vars.push((
                    "SUPERZEJ_PROXY_BASE_URL".into(),
                    "http://proxy.superzej.internal".into(),
                ));
                plan.vars
                    .push(("SUPERZEJ_PROXY_UNIX".into(), sock_s.clone()));
                if let Some(dir) = sock.parent() {
                    plan.mounts.push(dir_mount(dir));
                }
                plan.proxy_relay_socket = Some(sock);
            }
        }
    }

    // Upper plane: the bouncer override. Only meaningful when the agent is
    // sandboxed (the gate's value is the sealed boundary). On a host pane it's a
    // no-op: pi keeps running its tools in-process.
    if lp.bouncer && sandboxed {
        let sock = acp_socket_path(worktree);
        let sock_s = sock.to_string_lossy().into_owned();
        plan.vars.push(("SUPERZEJ_BOUNCER".into(), "1".into()));
        plan.vars.push(("ACP_SOCKET".into(), sock_s));
        if let Some(dir) = sock.parent() {
            plan.mounts.push(dir_mount(dir));
        }
        plan.acp_socket = Some(sock);
    }

    plan
}

/// A path-preserving rw bind mount for a control-socket directory.
fn dir_mount(dir: &std::path::Path) -> Mount {
    let s = dir.to_string_lossy().into_owned();
    Mount {
        host: s.clone(),
        dest: s,
        ro: false,
        cache: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::acp::types::Id;
    use superzej_core::config::Config;

    fn shell_req(cmd: &str) -> AcpInbound {
        AcpInbound::TerminalCreateRequest {
            id: Id::Number(1),
            command: cmd.into(),
            cwd: None,
            env: None,
        }
    }

    #[test]
    fn gated_kind_gates_shell_edit_write_but_not_reads() {
        assert_eq!(
            gated_kind(&shell_req("ls")).map(|(k, _)| k),
            Some(ApprovalKind::Shell)
        );
        assert_eq!(
            gated_kind(&AcpInbound::SuperzejEditRequest {
                id: Id::Number(1),
                path: "a.rs".into(),
                edits: serde_json::Value::Null,
            })
            .map(|(k, _)| k),
            Some(ApprovalKind::Edit)
        );
        assert_eq!(
            gated_kind(&AcpInbound::SuperzejWriteRequest {
                id: Id::Number(1),
                path: "b.rs".into(),
                content: "x".into(),
            })
            .map(|(k, _)| k),
            Some(ApprovalKind::Write)
        );
        // Reads are auto-served — never gated.
        assert!(
            gated_kind(&AcpInbound::FsReadRequest {
                id: Id::Number(1),
                path: "c.rs".into()
            })
            .is_none()
        );
        // MCP frames pass through.
        assert!(
            gated_kind(&AcpInbound::McpMessage {
                connection_id: "x".into(),
                inner: serde_json::Value::Null,
            })
            .is_none()
        );
    }

    #[test]
    fn gated_kind_carries_command_and_path_detail() {
        assert_eq!(gated_kind(&shell_req("rm -rf /")).unwrap().1, "rm -rf /");
    }

    #[test]
    fn summary_flattens_and_truncates() {
        assert_eq!(summary("git   status\n--short"), "git status --short");
        let long = "x".repeat(200);
        let s = summary(&long);
        assert!(s.chars().count() <= 72);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn approval_queue_is_fifo_and_single_active() {
        let mut q: ApprovalQueue<u32> = ApprovalQueue::new();
        assert!(q.is_idle());

        // First request becomes active (signals "open the overlay").
        assert!(q.enqueue(ApprovalRequest {
            worktree: "/wt".into(),
            kind: ApprovalKind::Shell,
            detail: "a".into(),
            reply: 1,
        }));
        // Second + third queue behind it.
        assert!(!q.enqueue(ApprovalRequest {
            worktree: "/wt".into(),
            kind: ApprovalKind::Edit,
            detail: "b".into(),
            reply: 2,
        }));
        assert!(!q.enqueue(ApprovalRequest {
            worktree: "/wt".into(),
            kind: ApprovalKind::Write,
            detail: "c".into(),
            reply: 3,
        }));
        assert!(!q.is_idle());
        assert_eq!(q.active().map(|r| r.reply), Some(1));

        // Resolving returns the finished one and promotes the next, in order.
        assert_eq!(q.resolve().map(|r| r.reply), Some(1));
        assert_eq!(q.active().map(|r| r.reply), Some(2));
        assert_eq!(q.resolve().map(|r| r.reply), Some(2));
        assert_eq!(q.active().map(|r| r.reply), Some(3));
        assert_eq!(q.resolve().map(|r| r.reply), Some(3));
        assert!(q.is_idle());
        assert!(q.resolve().is_none());
    }

    #[test]
    fn proxy_reach_picks_the_right_channel() {
        let listen = "127.0.0.1:8383";
        // Host pane → loopback.
        assert_eq!(
            proxy_reach(listen, None),
            ProxyReach::Url("http://127.0.0.1:8383".into())
        );
        // Podman under NAT → podman gateway alias.
        assert_eq!(
            proxy_reach(listen, Some((Backend::Podman, Network::Nat))),
            ProxyReach::Url("http://host.containers.internal:8383".into())
        );
        // Docker under NAT → docker gateway alias.
        assert_eq!(
            proxy_reach(listen, Some((Backend::Docker, Network::Nat))),
            ProxyReach::Url("http://host.docker.internal:8383".into())
        );
        // bwrap shares the host netns → loopback.
        assert_eq!(
            proxy_reach(listen, Some((Backend::Bwrap, Network::Nat))),
            ProxyReach::Url("http://127.0.0.1:8383".into())
        );
        // Sealed (network=none) → unix relay regardless of backend.
        assert_eq!(
            proxy_reach(listen, Some((Backend::Podman, Network::None))),
            ProxyReach::Unix
        );
    }

    #[test]
    fn socket_paths_are_stable_short_and_distinct() {
        let a = acp_socket_path("/home/x/wt-one");
        let b = acp_socket_path("/home/x/wt-one");
        let c = acp_socket_path("/home/x/wt-two");
        assert_eq!(a, b, "same worktree → same socket");
        assert_ne!(a, c, "different worktrees → different sockets");
        // The proxy + acp sockets never collide for one worktree.
        assert_ne!(acp_socket_path("/wt"), proxy_socket_path("/wt"));
        // Comfortably under the unix-socket path cap.
        assert!(a.to_string_lossy().len() < 108);
    }

    fn bouncer_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.llm_proxy.route_agent = true;
        cfg.llm_proxy.bouncer = true;
        cfg
    }

    #[test]
    fn agent_env_plan_host_pane_has_no_bouncer_or_mounts() {
        let plan = agent_env_plan(&bouncer_cfg(), "/wt", None, Some("szk-1"));
        // Proxy still routes (loopback), but no bouncer override on a host pane.
        assert!(
            plan.vars
                .iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == "http://127.0.0.1:8383")
        );
        assert!(plan.vars.iter().all(|(k, _)| k != "SUPERZEJ_BOUNCER"));
        assert!(plan.acp_socket.is_none());
        assert!(plan.mounts.is_empty());
    }

    #[test]
    fn agent_env_plan_nat_sandbox_enables_bouncer_over_gateway() {
        let plan = agent_env_plan(
            &bouncer_cfg(),
            "/wt",
            Some((Backend::Podman, Network::Nat)),
            Some("szk-1"),
        );
        // Model reaches the host via the podman gateway.
        assert!(
            plan.vars.iter().any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL"
                && v == "http://host.containers.internal:8383")
        );
        // Bouncer override is on, with a unix-socket ACP channel + its mount.
        assert!(
            plan.vars
                .iter()
                .any(|(k, v)| k == "SUPERZEJ_BOUNCER" && v == "1")
        );
        let sock = plan.acp_socket.clone().expect("acp socket under bouncer");
        assert!(
            plan.vars
                .iter()
                .any(|(k, v)| k == "ACP_SOCKET" && v == &sock.to_string_lossy())
        );
        let dir = sock.parent().unwrap().to_string_lossy().into_owned();
        assert!(
            plan.mounts
                .iter()
                .any(|m| m.dest == dir && m.host == dir && !m.ro),
            "the socket dir is bind-mounted path-preserving + rw"
        );
        // No proxy relay socket under NAT.
        assert!(plan.proxy_relay_socket.is_none());
    }

    #[test]
    fn agent_env_plan_sealed_sandbox_relays_proxy_over_unix() {
        let plan = agent_env_plan(
            &bouncer_cfg(),
            "/wt",
            Some((Backend::Podman, Network::None)),
            Some("szk-1"),
        );
        // Full seal: proxy over a unix socket, not an IP.
        assert!(plan.vars.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_UNIX"));
        let relay = plan.proxy_relay_socket.clone().expect("proxy relay socket");
        // Both the ACP socket dir and the proxy socket dir are mounted.
        let relay_dir = relay.parent().unwrap().to_string_lossy().into_owned();
        assert!(plan.mounts.iter().any(|m| m.dest == relay_dir));
        // Bouncer override still on.
        assert!(plan.vars.iter().any(|(k, _)| k == "SUPERZEJ_BOUNCER"));
        assert!(plan.acp_socket.is_some());
    }

    #[test]
    fn agent_env_plan_respects_route_agent_off() {
        let mut cfg = Config::default();
        cfg.llm_proxy.route_agent = false;
        cfg.llm_proxy.bouncer = true;
        let plan = agent_env_plan(&cfg, "/wt", Some((Backend::Podman, Network::Nat)), None);
        // No proxy env when route_agent is off…
        assert!(
            plan.vars
                .iter()
                .all(|(k, _)| !k.starts_with("SUPERZEJ_PROXY"))
        );
        // …but the bouncer override is independent and still applies.
        assert!(plan.vars.iter().any(|(k, _)| k == "SUPERZEJ_BOUNCER"));
    }
}
