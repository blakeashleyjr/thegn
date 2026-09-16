//! Per-worktree ingress sharing — expose a worktree-local port at a public URL.
//!
//! The *inbound* sibling of [`crate::vpn`]. This module owns provider
//! *knowledge*: given a resolved [`ShareSpec`], build the (pure, testable)
//! [`SharePlan`] — the tunnel-client program, args, env, and the rule for
//! scraping the public URL out of its output — resolve its secrets, then spawn
//! and watch the child. The lifecycle (restart, persistence, UI) lives in the
//! host, exactly as [`crate::vpn`] hands its plan back to `thegn_core::sandbox`.
//!
//! Division of labor mirrors the other svc seams: the pure plan builder and URL
//! matcher are unit-tested here (`tests`); the subprocess execution
//! ([`start`]) is the I/O seam, exercised by `test/smoke.sh`.
//!
//! `bore` (<https://github.com/ekzhang/bore>) is the first and only backend; the
//! [`ShareProvider`] seam keeps room for rathole/zrok/ngrok/iroh later.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thegn_core::config::{
    BoreConfig, FrpConfig, FrpProxyType, IrohShareConfig, TailscaleShareConfig, expand_env_ref,
};
use thegn_core::share::{ShareParams, ShareSpec};

use crate::vpn::{OciRuntime, exec_in};

#[cfg(test)]
mod tests;

/// How to derive the public URL/address from a started share.
#[derive(Clone, PartialEq, Eq)]
pub enum UrlRule {
    /// Find `marker` in an output line, take the whitespace-delimited token after
    /// it as a `host:port`, and format it into `scheme://host:port`. Used by
    /// providers that *print* their address (bore, dumbpipe).
    AfterMarker { marker: String, scheme: String },
    /// The address is known up front from config (frp derives it from
    /// `subdomain`/`server_addr`; the client never prints it). `start` returns it
    /// as soon as the child is confirmed alive.
    Fixed(String),
    /// Find `marker` in a line and capture the next whitespace token verbatim
    /// (no host:port shape required), substituting it into `template`'s `{}`.
    /// Used for opaque addresses like a dumbpipe ticket.
    AfterMarkerRaw { marker: String, template: String },
}

impl fmt::Debug for UrlRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AfterMarker { .. } => f
                .debug_struct("UrlRule::AfterMarker")
                .field("marker", &"<redacted>")
                .field("scheme", &"<redacted>")
                .finish(),
            Self::Fixed(_) => f
                .debug_tuple("UrlRule::Fixed")
                .field(&"<redacted>")
                .finish(),
            Self::AfterMarkerRaw { .. } => f
                .debug_struct("UrlRule::AfterMarkerRaw")
                .field("marker", &"<redacted>")
                .field("template", &"<redacted>")
                .finish(),
        }
    }
}

impl UrlRule {
    /// Apply the rule to one output line, returning the URL if this line matches.
    /// `Fixed` never matches a line (it's resolved at spawn time, see [`start`]).
    pub fn apply(&self, line: &str) -> Option<String> {
        match self {
            UrlRule::AfterMarker { marker, scheme } => {
                let rest = line.split_once(marker)?.1.trim_start();
                let token = rest.split_whitespace().next()?.trim_end_matches(['.', ',']);
                // Require a host:port shape so stray log lines don't false-match.
                let (host, port) = token.rsplit_once(':')?;
                if host.is_empty() || port.is_empty() || port.parse::<u16>().is_err() {
                    return None;
                }
                Some(format!("{scheme}://{token}"))
            }
            UrlRule::AfterMarkerRaw { marker, template } => {
                let rest = line.split_once(marker)?.1.trim_start();
                let token = rest.split_whitespace().next()?;
                if token.is_empty() {
                    return None;
                }
                Some(template.replace("{}", token))
            }
            UrlRule::Fixed(_) => None,
        }
    }

    /// The config-derived URL, if this is a `Fixed` rule.
    pub fn fixed(&self) -> Option<&str> {
        match self {
            UrlRule::Fixed(u) => Some(u),
            UrlRule::AfterMarker { .. } | UrlRule::AfterMarkerRaw { .. } => None,
        }
    }
}

/// A file the provider needs materialized on disk (0600) before spawn and
/// referenced by `args`/`cwd` — e.g. a generated `frpc.toml`. Mirrors
/// `crate::vpn::SidecarFile`. `dest` is relative to the per-share state dir.
#[derive(Clone, PartialEq, Eq)]
pub struct SharePlanFile {
    pub dest: String,
    pub contents: String,
}

impl fmt::Debug for SharePlanFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharePlanFile")
            .field("dest", &self.dest)
            .field("contents", &"<redacted>")
            .finish()
    }
}

/// A pure, fully-resolved plan for the tunnel-client child. Built from a
/// [`ShareSpec`] (with secrets already dereferenced); executed by [`start`].
#[derive(Clone, PartialEq, Eq)]
pub struct SharePlan {
    /// The tunnel-client binary (e.g. `bore`, `frpc`, `dumbpipe`).
    pub program: String,
    /// Args after the program. May reference materialized files by the token
    /// `{statedir}`, expanded to the per-share state directory at spawn time.
    pub args: Vec<String>,
    /// Environment overrides (secrets already resolved).
    pub env: Vec<(String, String)>,
    /// Files to materialize 0600 in the per-share state dir before spawn.
    pub files: Vec<SharePlanFile>,
    /// How to recognise/derive the public URL.
    pub url_rule: UrlRule,
}

impl fmt::Debug for SharePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharePlan")
            .field("program", &self.program)
            .field("args", &"<redacted>")
            .field("env", &"<redacted>")
            .field("files", &self.files)
            .field("url_rule", &self.url_rule)
            .finish()
    }
}

impl SharePlan {
    /// Scan one output line for the public URL.
    pub fn match_url(&self, line: &str) -> Option<String> {
        self.url_rule.apply(line)
    }
}

/// How a share is brought up. Most providers spawn a client process; tailscale
/// instead drives `tailscale serve` inside the worktree's existing VPN sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareLaunch {
    /// Spawn a tunnel-client child (bore, frp, dumbpipe) — see [`start`].
    Process(SharePlan),
    /// Run `up_argv` inside the VPN sidecar, derive the URL, and run `down_argv`
    /// on teardown — see [`serve_up`]/[`serve_down`].
    SidecarServe(ServePlan),
}

/// A `tailscale serve`/`funnel` plan executed inside the worktree's VPN sidecar.
/// The public URL is `scheme://<MagicDNS name>:<port>` — the DNS name is read
/// from `tailscale status --json` at bring-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServePlan {
    pub up_argv: Vec<String>,
    pub down_argv: Vec<String>,
    pub scheme: String,
    pub port: u16,
}

/// The share provider seam. One built-in impl ([`BuiltinProvider`]) dispatches
/// on the resolved provider; external providers could plug in here later.
pub trait ShareProvider {
    /// Stable id for logging/bookkeeping.
    fn kind(&self) -> &'static str;
    /// Build the (secrets-resolved) launch. Errors if a required secret/setting
    /// is missing (frp `server_addr`, …).
    fn launch(&self) -> Result<ShareLaunch>;
}

/// Pick the provider implementation for a resolved [`ShareSpec`].
pub fn for_provider(spec: &ShareSpec) -> BuiltinProvider<'_> {
    BuiltinProvider { spec }
}

/// The single built-in provider; dispatches on `spec.params`.
pub struct BuiltinProvider<'a> {
    spec: &'a ShareSpec,
}

impl ShareProvider for BuiltinProvider<'_> {
    fn kind(&self) -> &'static str {
        match self.spec.params {
            ShareParams::Bore(_) => "bore",
            ShareParams::Frp(_) => "frp",
            ShareParams::Tailscale(_) => "tailscale",
            ShareParams::Iroh(_) => "iroh",
        }
    }

    fn launch(&self) -> Result<ShareLaunch> {
        match &self.spec.params {
            ShareParams::Bore(b) => Ok(ShareLaunch::Process(plan_bore(self.spec, b))),
            ShareParams::Frp(f) => Ok(ShareLaunch::Process(plan_frp(self.spec, f)?)),
            ShareParams::Tailscale(t) => {
                Ok(ShareLaunch::SidecarServe(plan_tailscale(self.spec, t)))
            }
            ShareParams::Iroh(i) => Ok(ShareLaunch::Process(plan_iroh(self.spec, i))),
        }
    }
}

// ── pure builders (unit-tested) ──────────────────────────────────────────────

/// The relay used when `[share.bore] to` is left empty.
const BORE_PUBLIC: &str = "bore.pub";

fn plan_bore(spec: &ShareSpec, b: &BoreConfig) -> SharePlan {
    let secret = expand_env_ref(&b.secret);
    SharePlan {
        program: "bore".into(),
        args: bore_args(spec.local_port, b, secret.as_deref()),
        // bore logs the "listening at" line via `tracing` at info on stderr;
        // make sure it's emitted regardless of an inherited RUST_LOG.
        env: vec![("RUST_LOG".into(), "info".into())],
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
    }
}

/// `bore local <port> --to <relay> [--secret S] [--port N] [--local-host H] …`.
fn bore_args(local_port: u16, b: &BoreConfig, secret: Option<&str>) -> Vec<String> {
    let mut a = vec!["local".into(), local_port.to_string()];
    let to = b.to.trim();
    a.push("--to".into());
    a.push(if to.is_empty() {
        BORE_PUBLIC.into()
    } else {
        to.to_string()
    });
    if b.remote_port != 0 {
        a.push("--port".into());
        a.push(b.remote_port.to_string());
    }
    let local_host = b.local_host.trim();
    if !local_host.is_empty() {
        a.push("--local-host".into());
        a.push(local_host.to_string());
    }
    if let Some(s) = secret {
        a.push("--secret".into());
        a.push(s.to_string());
    }
    a.extend(b.extra_args.iter().cloned());
    a
}

/// frp: materialize an `frpc.toml` and derive the public URL from config (frpc
/// never prints it). `https`/`http` → `scheme://<subdomain>.<host>`; `tcp`/`udp`
/// → `<server_addr>:<remote_port>`.
fn plan_frp(spec: &ShareSpec, f: &FrpConfig) -> Result<SharePlan> {
    plan_frp_resolving_token(spec, f, || expand_env_ref(&f.token))
}

fn plan_frp_resolving_token(
    spec: &ShareSpec,
    f: &FrpConfig,
    resolve_token: impl FnOnce() -> Option<String>,
) -> Result<SharePlan> {
    if !f.extra.is_empty() {
        bail!(
            "frp: invalid extra (raw proxy-field injection is unsupported; remove {} entr{})",
            f.extra.len(),
            if f.extra.len() == 1 { "y" } else { "ies" }
        );
    }
    if spec.local_port == 0 {
        bail!("frp: invalid local_port (must be nonzero)");
    }
    let server = validate_server_addr(&f.server_addr)?;
    let label = validate_dns_label(&spec.label, "worktree label")?;
    let proxy_name = format!("tg-{label}-{}", spec.local_port);
    // The generated name is composed from a 63-byte component, a fixed
    // prefix/separator, and a u16 decimal port (at most five bytes).
    const MAX_GENERATED_PROXY_NAME: usize = 3 + 63 + 1 + 5;
    if proxy_name.len() > MAX_GENERATED_PROXY_NAME {
        bail!(
            "frp: invalid generated proxy name ({} bytes exceeds derived limit {})",
            proxy_name.len(),
            MAX_GENERATED_PROXY_NAME
        );
    }
    let is_web = matches!(f.proxy_type, FrpProxyType::Https | FrpProxyType::Http);
    if !is_web && f.remote_port == 0 {
        bail!("frp: invalid remote_port (zero cannot produce a fixed tcp/udp address)");
    }
    let subdomain = {
        let s = f.subdomain.trim();
        if s.is_empty() {
            format!("{label}-{}", spec.local_port)
        } else {
            s.to_string()
        }
    };
    if is_web {
        // The default is `<label>-<port>`; validate the complete derived DNS
        // label so a 63-byte worktree label is still usable with an explicit
        // subdomain, while an overlong default fails before spawn.
        validate_dns_label(&subdomain, "subdomain")?;
    }
    // Resolve and validate every provider-derived web value before constructing
    // the typed document. This keeps invalid host input ahead of token-bearing
    // serialization, even though the plan remains in memory until returned.
    let web_host = if is_web {
        let host = f.subdomain_host.trim();
        if host.is_empty() {
            bail!("frp: set [share.frp] subdomain_host to derive the https URL");
        }
        validate_dns_name(host, "subdomain_host")?;
        Some(host.to_string())
    } else {
        None
    };
    let proxy_type = f.proxy_type.as_str();

    // Build and immediately reparse a typed frpc.toml document. The same
    // deny-unknown-fields structs own both serializer and parser, so generated
    // text cannot silently acquire an unreviewed table/key.
    let token = resolve_token();
    let document = FrpDocument {
        server_addr: server.clone(),
        server_port: f.server_port,
        auth: token.clone().map(|token| FrpAuth {
            method: "token".into(),
            token,
        }),
        proxies: vec![FrpProxy {
            name: proxy_name,
            proxy_type: proxy_type.into(),
            local_ip: "127.0.0.1".into(),
            local_port: spec.local_port,
            subdomain: is_web.then_some(subdomain.clone()),
            remote_port: (!is_web).then_some(f.remote_port),
        }],
    };
    let toml = toml::to_string(&document)
        .map_err(|_| anyhow::anyhow!("frp: generated config failed serialization"))?;
    let parsed: FrpDocument = toml::from_str(&toml)
        .map_err(|_| anyhow::anyhow!("frp: generated config failed validation"))?;
    if parsed != document {
        bail!("frp: generated config failed validation (fields changed)");
    }
    // Derive the public URL.
    let url = if is_web {
        let host = web_host.expect("web host validated above");
        let scheme = if matches!(f.proxy_type, FrpProxyType::Https) {
            "https"
        } else {
            "http"
        };
        let port_suffix = match (scheme, f.vhost_https_port) {
            ("https", 443) | ("http", 80) | (_, 0) => String::new(),
            (_, p) => format!(":{p}"),
        };
        format!("{scheme}://{subdomain}.{host}{port_suffix}")
    } else {
        format!("{}:{}", url_authority(&server), f.remote_port)
    };

    Ok(SharePlan {
        program: "frpc".into(),
        args: vec!["-c".into(), "{statedir}/frpc.toml".into()],
        env: Vec::new(),
        files: vec![SharePlanFile {
            dest: "frpc.toml".into(),
            contents: toml,
        }],
        url_rule: UrlRule::Fixed(url),
    })
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FrpDocument {
    #[serde(rename = "serverAddr")]
    server_addr: String,
    #[serde(rename = "serverPort")]
    server_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<FrpAuth>,
    proxies: Vec<FrpProxy>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FrpAuth {
    method: String,
    token: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FrpProxy {
    name: String,
    #[serde(rename = "type")]
    proxy_type: String,
    #[serde(rename = "localIP")]
    local_ip: String,
    #[serde(rename = "localPort")]
    local_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    subdomain: Option<String>,
    #[serde(rename = "remotePort", skip_serializing_if = "Option::is_none")]
    remote_port: Option<u16>,
}

/// Validate a Thegn DNS label. This is deliberately an application policy:
/// backend parsers accept a wider set, but generated share names need a
/// portable, bounded representation.
fn validate_dns_label(value: &str, field: &str) -> Result<String> {
    let len = value.len();
    if value.is_empty() {
        bail!("frp: invalid {field} (empty; 0 bytes)");
    }
    if len > 63 {
        bail!("frp: invalid {field} ({} bytes exceeds 63)", len);
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        bail!("frp: invalid {field} (must start with lowercase ASCII alphanumeric)");
    }
    if !bytes[len - 1].is_ascii_lowercase() && !bytes[len - 1].is_ascii_digit() {
        bail!("frp: invalid {field} (must end with lowercase ASCII alphanumeric)");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        bail!("frp: invalid {field} (use lowercase ASCII alphanumeric or '-')");
    }
    Ok(value.to_string())
}

fn validate_dns_name(value: &str, field: &str) -> Result<String> {
    if value.len() > 253 {
        bail!("frp: invalid {field} ({} bytes exceeds 253)", value.len());
    }
    if value.is_empty() || value.ends_with('.') {
        bail!("frp: invalid {field} (empty or trailing dot)");
    }
    for label in value.split('.') {
        if label.is_empty() {
            bail!("frp: invalid {field} (empty DNS component)");
        }
        if label.len() > 63 {
            bail!("frp: invalid {field} (DNS component exceeds 63 bytes)");
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[label.len() - 1].is_ascii_alphanumeric() {
            bail!("frp: invalid {field} (DNS components cannot start/end with '-')");
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        {
            bail!("frp: invalid {field} (use ASCII DNS components)");
        }
    }
    Ok(value.to_string())
}

/// Return the frpc spelling of a server address. `IpAddr` parsing keeps IPv6
/// handling typed; frpc receives the unbracketed address, while URL authority
/// formatting below adds brackets only where RFC 3986 requires them.
fn validate_server_addr(raw: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("frp: set [share.frp] server_addr to your frps host");
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("frp: invalid server_addr (whitespace/control characters)");
    }
    if value.starts_with('[') || value.ends_with(']') {
        let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
            bail!("frp: invalid server_addr (malformed bracketed IPv6 address)");
        };
        let ip = inner
            .parse::<IpAddr>()
            .ok()
            .filter(|ip| ip.is_ipv6())
            .ok_or_else(|| anyhow::anyhow!("frp: invalid server_addr (expected bracketed IPv6)"))?;
        return Ok(ip.to_string());
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    if value.contains(':') {
        bail!("frp: invalid server_addr (unbracketed value is not an IPv6 address)");
    }
    validate_dns_name(value, "server_addr")
}

fn url_authority(server: &str) -> String {
    match server.parse::<IpAddr>() {
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        _ => server.to_string(),
    }
}

/// tailscale: `serve`/`funnel` the worktree port over its existing VPN tunnel.
/// Pure builder — execution drives this inside the VPN sidecar (see [`serve_up`]).
fn plan_tailscale(spec: &ShareSpec, t: &TailscaleShareConfig) -> ServePlan {
    let verb = if t.funnel { "funnel" } else { "serve" };
    let port = if t.https_port == 0 { 443 } else { t.https_port };
    let mut up_argv = vec!["tailscale".to_string(), verb.to_string()];
    if port != 443 {
        up_argv.push(format!("--https={port}"));
    }
    up_argv.push("--bg".to_string());
    up_argv.push(spec.local_port.to_string());

    let down_argv = vec![
        "tailscale".to_string(),
        verb.to_string(),
        format!("--https={port}"),
        "off".to_string(),
    ];
    ServePlan {
        up_argv,
        down_argv,
        scheme: "https".into(),
        port,
    }
}

/// iroh peer share via dumbpipe: `dumbpipe listen-tcp --host 127.0.0.1:<port>`
/// prints a ticket; the consumer runs `dumbpipe connect-tcp <ticket>`. We scrape
/// the ticket and present the full connect command as the share's "address".
fn plan_iroh(spec: &ShareSpec, i: &IrohShareConfig) -> SharePlan {
    let mut args = vec![
        "listen-tcp".to_string(),
        "--host".to_string(),
        format!("127.0.0.1:{}", spec.local_port),
    ];
    args.extend(i.extra_args.iter().cloned());
    SharePlan {
        program: "dumbpipe".into(),
        args,
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarkerRaw {
            marker: "connect-tcp".into(),
            template: "dumbpipe connect-tcp {}".into(),
        },
    }
}

// ── sidecar-serve seam (tailscale; smoke-tested) ─────────────────────────────

/// OCI runtimes to try when driving a worktree's VPN sidecar. We don't track
/// which one started it, so try the likely ones; a wrong runtime fails to find
/// the container and is skipped (mirrors `vpn::deregister`).
fn likely_runtimes() -> Vec<OciRuntime> {
    vec![
        OciRuntime::podman(),
        OciRuntime::docker(),
        OciRuntime::new(vec!["sudo".into(), "-n".into(), "podman".into()]),
    ]
}

/// Bring a tailscale serve/funnel up inside the worktree's VPN `sidecar` and
/// return the resulting public URL (derived from the node's MagicDNS name).
/// Errors with guidance if no sidecar with tailscale is reachable.
pub fn serve_up(sidecar: &str, serve: &ServePlan) -> Result<String> {
    for rt in likely_runtimes() {
        match exec_in(&rt, sidecar, &serve.up_argv) {
            Ok((true, _)) => {
                let dns = serve_dns_name(&rt, sidecar)?;
                let suffix = if serve.port == 443 {
                    String::new()
                } else {
                    format!(":{}", serve.port)
                };
                return Ok(format!("{}://{dns}{suffix}", serve.scheme));
            }
            _ => continue,
        }
    }
    bail!(
        "share: tailscale ingress needs an active VPN sidecar with tailscale \
         (set [sandbox.vpn] provider = \"tailscale\" on this worktree)"
    )
}

/// Best-effort teardown: run `down_argv` in the sidecar (it also dies with it).
pub fn serve_down(sidecar: &str, serve: &ServePlan) {
    for rt in likely_runtimes() {
        if let Ok((true, _)) = exec_in(&rt, sidecar, &serve.down_argv) {
            return;
        }
    }
}

/// Read the node's MagicDNS name from `tailscale status --json` in the sidecar.
fn serve_dns_name(rt: &OciRuntime, sidecar: &str) -> Result<String> {
    let argv = vec![
        "tailscale".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];
    let (ok, out) = exec_in(rt, sidecar, &argv)?;
    if !ok {
        bail!("share: could not read tailscale status in sidecar");
    }
    let json: serde_json::Value =
        serde_json::from_str(&out).context("share: parse tailscale status")?;
    let name = json
        .get("Self")
        .and_then(|s| s.get("DNSName"))
        .and_then(|n| n.as_str())
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("share: tailscale status had no DNSName"))?;
    Ok(name)
}

// ── subprocess seam (smoke-tested) ───────────────────────────────────────────

/// A live share: the running tunnel-client child and its public URL.
pub struct RunningShare {
    pub child: Child,
    pub public_url: String,
}

impl fmt::Debug for RunningShare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunningShare")
            .field("child", &self.child)
            .field("public_url", &"<redacted>")
            .finish()
    }
}

impl RunningShare {
    /// Best-effort terminate the child.
    pub fn stop(mut self) {
        let _ = self.child.kill(); // best-effort: child may already have exited
        let _ = self.child.wait(); // best-effort: reap-or-not is terminal here
    }
}

/// The per-share state directory `$XDG_STATE_HOME/thegn/share/<wt>-<port>/`,
/// where materialized config files (e.g. `frpc.toml`) live. Caller-supplied so
/// both the host supervisor and the CLI key it the same way.
pub fn share_state_dir(worktree: &str, port: u16) -> std::path::PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            home.map(|h| h.join(".local/state"))
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        });
    let slug: String = worktree
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    base.join("thegn/share").join(format!("{slug}-{port}"))
}

/// Spawn the tunnel client described by `plan`, materializing its files into
/// `statedir` (0600) and expanding the `{statedir}` token in its args.
///
/// For an `AfterMarker` rule, block until the printed URL appears (or `timeout`
/// elapses). For a `Fixed` rule (URL known from config), confirm the child stays
/// alive through a short grace window, then return the fixed URL. On
/// timeout/early-exit the child is killed and an error returned.
pub fn start(
    plan: &SharePlan,
    statedir: &std::path::Path,
    timeout: Duration,
) -> Result<RunningShare> {
    materialize_files(plan, statedir)?;
    let sd = statedir.to_string_lossy().into_owned();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.replace("{statedir}", &sd))
        .collect();

    let mut cmd = Command::new(&plan.program);
    cmd.args(&args)
        .current_dir(statedir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("share: failed to spawn '{}'", plan.program))?;

    let (tx, rx) = mpsc::sync_channel::<String>(64);
    // URL discovery has a separate bounded signal so noisy diagnostics cannot
    // consume the only startup result.
    let (url_tx, url_rx) = mpsc::sync_channel::<String>(1);
    for stream in [
        child.stdout.take().map(Streamable::Out),
        child.stderr.take().map(Streamable::Err),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        let url_tx = url_tx.clone();
        let url_rule = plan.url_rule.clone();
        std::thread::spawn(move || {
            drain_stream(stream, tx, url_tx, url_rule);
        });
    }
    drop(tx);
    drop(url_tx);

    // Config-derived URL: the client never prints it. Confirm it doesn't exit
    // immediately (auth failure, bad config), then return the known address.
    if let Some(url) = plan.url_rule.fixed() {
        let grace = Duration::from_millis(1500).min(timeout);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                abort_child(child);
                anyhow::bail!("share: '{}' exited early", plan.program);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        return Ok(RunningShare {
            child,
            public_url: url.to_string(),
        });
    }

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            abort_child(child);
            anyhow::bail!(
                "share: '{}' did not report a URL within {}s",
                plan.program,
                timeout.as_secs()
            );
        }
        if let Ok(url) = url_rx.try_recv() {
            return Ok(RunningShare {
                child,
                public_url: url,
            });
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(line) => {
                if let Some(url) = plan.match_url(&line) {
                    return Ok(RunningShare {
                        child,
                        public_url: url,
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The priority check may have run before a reader published
                // its URL. If the diagnostic channel then disconnects, give
                // the already-published priority signal one final check.
                if let Ok(url) = url_rx.try_recv() {
                    return Ok(RunningShare {
                        child,
                        public_url: url,
                    });
                }
                abort_child(child);
                anyhow::bail!("share: '{}' exited before reporting a URL", plan.program);
            }
        }
    }
}

/// Perform best-effort terminal startup cleanup. Process-tree settlement is
/// owned by THE-331; this helper provides no such guarantee.
fn abort_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Write each plan file into `statedir` with 0600 perms (dir 0700).
fn materialize_files(plan: &SharePlan, statedir: &std::path::Path) -> Result<()> {
    if plan.files.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(statedir)
        .with_context(|| format!("share: mkdir {}", statedir.display()))?;
    for f in &plan.files {
        let path = statedir.join(&f.dest);
        std::fs::write(&path, &f.contents)
            .with_context(|| format!("share: write {}", path.display()))?;
        // best-effort: shared-credential files are owner-only everywhere.
        let _ = thegn_core::fsperm::restrict_to_owner(&path); // best-effort: shared-credential files are owner-only everywhere (see above)
    }
    Ok(())
}

enum Streamable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

const MAX_PROVIDER_LINE: usize = 4096;

/// Drain a provider stream for the child's full lifetime without retaining an
/// unbounded line or queue. Oversized lines are discarded through newline;
/// diagnostics use a bounded best-effort queue while URL matches use a
/// separate bounded priority signal.
fn drain_stream(
    stream: Streamable,
    tx: mpsc::SyncSender<String>,
    url_tx: mpsc::SyncSender<String>,
    url_rule: UrlRule,
) {
    match stream {
        Streamable::Out(reader) => drain_reader(reader, tx, url_tx, url_rule),
        Streamable::Err(reader) => drain_reader(reader, tx, url_tx, url_rule),
    }
}

fn drain_reader<R: Read>(
    mut reader: R,
    tx: mpsc::SyncSender<String>,
    url_tx: mpsc::SyncSender<String>,
    url_rule: UrlRule,
) {
    let mut bytes = [0_u8; 1024];
    let mut line = Vec::with_capacity(MAX_PROVIDER_LINE);
    let mut oversized = false;
    loop {
        let count = match reader.read(&mut bytes) {
            Ok(0) => {
                emit_line(&mut line, &mut oversized, &tx, &url_tx, &url_rule);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
            Ok(count) => count,
        };
        for byte in &bytes[..count] {
            if *byte == b'\n' {
                emit_line(&mut line, &mut oversized, &tx, &url_tx, &url_rule);
            } else if !oversized {
                if line.len() == MAX_PROVIDER_LINE {
                    oversized = true;
                    line.clear();
                } else {
                    line.push(*byte);
                }
            }
        }
    }
}

/// Keep both input and lossy UTF-8 output within the same byte bound.
fn bounded_lossy(bytes: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(bytes);
    let mut result = String::with_capacity(lossy.len().min(MAX_PROVIDER_LINE));
    for ch in lossy.chars() {
        if result.len() + ch.len_utf8() > MAX_PROVIDER_LINE {
            break;
        }
        result.push(ch);
    }
    result
}

fn emit_line(
    line: &mut Vec<u8>,
    oversized: &mut bool,
    tx: &mpsc::SyncSender<String>,
    url_tx: &mpsc::SyncSender<String>,
    url_rule: &UrlRule,
) {
    if !*oversized && !line.is_empty() {
        let text = bounded_lossy(line);
        if let Some(url) = url_rule.apply(&text) {
            let _ = url_tx.try_send(url);
        }
        let _ = tx.try_send(text);
    }
    line.clear();
    *oversized = false;
}
