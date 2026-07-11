//! `[llm_proxy]` config lowering — `LlmProxyConfig` and its inherent impls,
//! split out of the (ratcheted) `config.rs` god-file. The struct is re-exported
//! from `crate::config`, so consumers keep the `config::LlmProxyConfig` path.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{CompressionLevel, RoutingStrategy};

/// `[llm_proxy]` — the AI-traffic chokepoint daemon (`szproxy`). The shell never
/// hard-depends on this; AI is strictly additive, so the default is disabled.
/// When `enabled`, the host launches `szproxy` as a pinned daemon and agents
/// point their `OPENAI_BASE_URL`/`ANTHROPIC_BASE_URL` at `listen`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LlmProxyConfig {
    /// Whether the host should launch + manage the proxy daemon.
    pub enabled: bool,
    /// Address the daemon binds (and agents target).
    pub listen: String,
    /// Backend selection strategy.
    pub routing: RoutingStrategy,
    /// On a budget-cap breach, refuse the request (`true`) or downgrade to a
    /// cheaper tier (`false`). The kill-switch always refuses.
    pub refuse_on_breach: bool,
    /// Path to the proxy's routes document (JSON), passed to `szproxy` as
    /// `SZPROXY_CONFIG`. Empty means no backends are configured yet.
    pub config_path: String,
    /// Streaming: seconds to wait for a backend's first usable output before
    /// falling through (TTFB / empty-completion peek window).
    pub first_byte_timeout_secs: u64,
    /// Streaming: seconds of upstream silence after which a committed stream is
    /// terminated.
    pub idle_timeout_secs: u64,
    /// Streaming: keep-alive cadence (seconds) emitted during upstream silence.
    pub heartbeat_secs: u64,
    /// In-flight token reduction: compress noisy `tool` output before it's sent
    /// upstream (group W). Off by default — AI transforms are opt-in.
    pub token_reduction: bool,
    /// Aggressiveness when `token_reduction` is on.
    pub token_reduction_level: CompressionLevel,
    /// Route a launched agent's model traffic through the proxy at `listen` by
    /// injecting provider config into the agent's environment at spawn. Separate
    /// from `enabled` (which launches `szproxy`): set this to point the agent at
    /// an already-running proxy without launching our own. This governs the
    /// `SUPERZEJ_PROXY_*` vars the pi extension reads — NOT `ANTHROPIC_BASE_URL`
    /// (see `route_claude`).
    pub route_agent: bool,
    /// Additionally route Claude Code / the Anthropic SDK (anything honoring
    /// `ANTHROPIC_BASE_URL`) through the proxy. Off by default: claude talks to
    /// Anthropic directly so a proxy/tunnel hiccup can't break it (a bare
    /// `ANTHROPIC_BASE_URL = http://127.0.0.1:<proxy>` with a down tunnel yields
    /// `ConnectionRefused` and has no upstream fallback). Only meaningful when
    /// `route_agent` is also on. The pi extension routes regardless via
    /// `SUPERZEJ_PROXY_*`; this switch is specifically for the `ANTHROPIC_*` vars.
    pub route_claude: bool,
    /// The pi-side API id for the proxy endpoint. The proxy serves the Anthropic
    /// Messages API (`/v1/messages`); pi's OpenAI client speaks the Responses API,
    /// which the proxy does not implement — so `anthropic-messages` is the default.
    pub agent_api: String,
    /// The model id the agent requests from the proxy (the proxy maps it to a
    /// real backend, e.g. `model-proxy/standard` → its standard route).
    pub agent_model: String,
    /// The upstream provider this scope's traffic is pinned to (V 287 scoped
    /// accounts). Layered like every config key, so a workspace overlay can bind
    /// its worktrees to a dedicated account: the minted per-worktree virtual key
    /// carries this binding, and the proxy leads with that provider's lanes
    /// (others remain failover). Empty ⇒ no pinning.
    pub upstream: String,
    /// "The bouncer": run a launched agent inside its sealed `agent_profile`
    /// container, route its built-in `bash`/`read`/`edit`/`write` tools back
    /// through superzej over a bind-mounted unix-socket ACP channel, and gate
    /// the consequential ones (shell + edit + write) behind an interactive
    /// allow/deny overlay. Off by default — the additive integration (pi runs
    /// its own tools in-process, edits auto-apply) stays the default. When the
    /// resolved `agent_profile` forces no network (`sealed`), the agent's model
    /// traffic is relayed to the proxy over a unix socket too (full egress seal);
    /// otherwise it reaches the proxy via the container gateway.
    pub bouncer: bool,
    /// Base URL an agent running INSIDE a remote/provider sandbox (a sprite VM
    /// that can't reach host loopback) uses to reach `szproxy` — a tunnel/public
    /// endpoint, e.g. `https://proxy.example.ts.net`. When set (and `route_agent`),
    /// superzej injects `ANTHROPIC_BASE_URL` + the per-worktree virtual key into
    /// the provider exec env so ANY agent there (pi, claude code, …) routes
    /// through the proxy by default. Empty ⇒ no remote proxy injection (the
    /// in-sprite agent would talk to the upstream model directly with its key).
    pub remote_base_url: String,
}

impl Default for LlmProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:8383".to_string(),
            routing: RoutingStrategy::default(),
            refuse_on_breach: true,
            config_path: String::new(),
            first_byte_timeout_secs: 45,
            idle_timeout_secs: 120,
            heartbeat_secs: 10,
            token_reduction: false,
            token_reduction_level: CompressionLevel::default(),
            route_agent: false,
            route_claude: false,
            agent_api: "anthropic-messages".to_string(),
            agent_model: "model-proxy/standard".to_string(),
            upstream: String::new(),
            bouncer: false,
            remote_base_url: String::new(),
        }
    }
}

impl LlmProxyConfig {
    /// Env vars for an agent/shell running inside a REMOTE/provider sandbox so its
    /// model traffic routes through `szproxy` by default. Empty unless `route_agent`
    /// is on; the loopback URL is then reachable via the reverse tunnel superzej
    /// stands up (empty/`auto` `remote_base_url`) or via an explicit external URL.
    /// Sets the `SUPERZEJ_PROXY_*` vars the pi extension reads. `virtual_key`, when
    /// given, becomes the proxy auth key (else the passthrough master key is used).
    /// Only when `route_claude` is also on does it additionally set
    /// `ANTHROPIC_BASE_URL` (+ the virtual key as `ANTHROPIC_API_KEY`) so claude
    /// code / the Anthropic SDK route through the proxy too; by default they talk
    /// to Anthropic directly (a down proxy tunnel can't break claude).
    pub fn remote_agent_env(&self, virtual_key: Option<&str>) -> Vec<(String, String)> {
        let url = match self.remote_base_url() {
            Some(u) => u,
            None => return Vec::new(),
        };
        let mut v = vec![
            ("SUPERZEJ_PROXY_BASE_URL".to_string(), url.clone()),
            ("SUPERZEJ_PROXY_API".to_string(), self.agent_api.clone()),
            ("SUPERZEJ_PROXY_MODEL".to_string(), self.agent_model.clone()),
        ];
        if self.route_claude {
            v.push(("ANTHROPIC_BASE_URL".to_string(), url));
        }
        if let Some(k) = virtual_key.map(str::trim).filter(|k| !k.is_empty()) {
            v.push(("SUPERZEJ_PROXY_KEY".to_string(), k.to_string()));
            if self.route_claude {
                v.push(("ANTHROPIC_API_KEY".to_string(), k.to_string()));
            }
        }
        v
    }

    /// Env for an agent running ON THE HOST (not a sandbox) so its model traffic
    /// routes through the proxy over the LOCAL `listen` loopback directly — no
    /// tunnel, no relay. Mirrors [`remote_agent_env`](Self::remote_agent_env) but
    /// always targets `http://127.0.0.1:<listen-port>`, so it stays correct even
    /// when `remote_base_url` points at an external endpoint used for *remote*
    /// sandboxes. Sets the `SUPERZEJ_PROXY_*` vars the pi extension reads, and —
    /// only when `route_claude` is also on — `ANTHROPIC_BASE_URL`
    /// (claude/codex/Anthropic SDK). Empty unless `route_agent`; no auth key (the
    /// pi extension falls back to its default), matching the keyless sprite path.
    /// See [`LlmProxyConfig`].
    pub fn local_agent_env(&self) -> Vec<(String, String)> {
        if !self.route_agent {
            return Vec::new();
        }
        let url = format!("http://127.0.0.1:{}", self.listen_port());
        let mut v = vec![
            ("SUPERZEJ_PROXY_BASE_URL".to_string(), url.clone()),
            ("SUPERZEJ_PROXY_API".to_string(), self.agent_api.clone()),
            ("SUPERZEJ_PROXY_MODEL".to_string(), self.agent_model.clone()),
        ];
        if self.route_claude {
            v.push(("ANTHROPIC_BASE_URL".to_string(), url));
        }
        v
    }

    /// The proxy base URL an in-remote agent should use, or `None` if remote
    /// routing is off. `remote_base_url = "auto"` ⇒ the in-sandbox reverse tunnel
    /// at `http://127.0.0.1:<proxy-port>` (superzej stands the tunnel up); an
    /// explicit URL is used verbatim. `None` unless `route_agent` + a value set.
    pub fn remote_base_url(&self) -> Option<String> {
        if !self.route_agent {
            return None;
        }
        let url = self.remote_base_url.trim();
        // `route_agent` alone is the single switch: an empty (or explicit "auto")
        // `remote_base_url` resolves to the in-sandbox reverse tunnel at
        // `http://127.0.0.1:<proxy-port>`. An explicit URL is used verbatim.
        if url.is_empty() || url == "auto" {
            Some(format!("http://127.0.0.1:{}", self.listen_port()))
        } else {
            Some(url.to_string())
        }
    }

    /// The loopback port the in-sandbox reverse tunnel should listen on (so the
    /// injected `ANTHROPIC_BASE_URL` resolves), or `None` unless `route_agent` +
    /// `remote_base_url = "auto"`. The host starts a tunnel on this port that
    /// dials the real `szproxy`.
    pub fn remote_tunnel_port(&self) -> Option<u16> {
        // The tunnel is needed whenever the resolved base URL is the loopback
        // (empty or "auto" under `route_agent`); an explicit external URL needs no
        // tunnel.
        let url = self.remote_base_url.trim();
        (self.route_agent && (url.is_empty() || url == "auto")).then(|| self.listen_port())
    }

    /// The upstream provider binding for minted virtual keys, or `None` when
    /// unset. Resolved from the layered config, so a workspace overlay's value
    /// scopes that workspace's account.
    pub fn upstream_binding(&self) -> Option<&str> {
        let u = self.upstream.trim();
        (!u.is_empty()).then_some(u)
    }

    /// The port from `listen` (e.g. `127.0.0.1:8383` → 8383; 8383 on parse fail).
    pub fn listen_port(&self) -> u16 {
        self.listen
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8383)
    }

    /// The launch spec for the `szproxy` daemon — `(program, args, env)` — or
    /// `None` when the proxy is disabled. The host feeds this to its process
    /// supervisor (e.g. as a `restart = "always"` pinned daemon). `SZPROXY_LISTEN`
    /// and `SZPROXY_CONFIG` mirror the standalone env knobs the daemon reads.
    ///
    /// Launching is gated on `enabled` ONLY — orthogonal to `route_agent`. This
    /// lets `route_agent` point agents at an EXTERNAL proxy already listening on
    /// `listen` (e.g. a separate model-proxy) without superzej trying to bind the
    /// same port and colliding. Run superzej's own szproxy with `enabled = true`.
    pub fn launch_spec(&self) -> Option<(String, Vec<String>, BTreeMap<String, String>)> {
        if !self.enabled {
            return None;
        }
        let mut env = BTreeMap::new();
        env.insert("SZPROXY_LISTEN".to_string(), self.listen.clone());
        if !self.config_path.is_empty() {
            env.insert("SZPROXY_CONFIG".to_string(), self.config_path.clone());
        }
        env.insert(
            "SZPROXY_FIRST_BYTE_TIMEOUT".to_string(),
            self.first_byte_timeout_secs.to_string(),
        );
        env.insert(
            "SZPROXY_STREAM_IDLE_TIMEOUT".to_string(),
            self.idle_timeout_secs.to_string(),
        );
        env.insert(
            "SZPROXY_STREAM_HEARTBEAT_INTERVAL".to_string(),
            self.heartbeat_secs.to_string(),
        );
        env.insert(
            "SZPROXY_COMPRESS".to_string(),
            if self.token_reduction { "1" } else { "0" }.to_string(),
        );
        env.insert(
            "SZPROXY_COMPRESS_LEVEL".to_string(),
            self.token_reduction_level.as_str().to_string(),
        );
        env.insert(
            "SZPROXY_ROUTING".to_string(),
            self.routing.as_str().to_string(),
        );
        Some(("szproxy".to_string(), Vec::new(), env))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_binding_trims_and_empties_to_none() {
        let mut c = LlmProxyConfig::default();
        assert_eq!(c.upstream_binding(), None);
        c.upstream = "  ".into();
        assert_eq!(c.upstream_binding(), None);
        c.upstream = " nano-gpt ".into();
        assert_eq!(c.upstream_binding(), Some("nano-gpt"));
    }

    #[test]
    fn budget_period_lengths() {
        use crate::store::budget_period_len_ms;
        assert_eq!(budget_period_len_ms("daily"), 86_400_000);
        assert_eq!(budget_period_len_ms("weekly"), 604_800_000);
        assert_eq!(budget_period_len_ms("monthly"), 2_592_000_000);
        assert_eq!(budget_period_len_ms("bogus"), 2_592_000_000);
    }
}
