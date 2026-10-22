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
//! The built-in backends are bore, frp, tailscale, and iroh, all dispatched by
//! [`BuiltinProvider`].

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use thegn_core::config::{
    BoreConfig, FrpConfig, FrpProxyType, IrohShareConfig, TailscaleShareConfig, expand_env_ref,
};
use thegn_core::share::{ShareParams, ShareSpec};

use crate::vpn::{OciRuntime, exec_in};

pub type ConflictHook = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct StopHandle {
    child: Arc<Mutex<ShareChild>>,
}

impl fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StopHandle").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    Terminated,
    AlreadyExited,
    Failed(String),
}

struct ShareChild {
    child: Child,
    pid: u32,
    grouped: bool,
    stop_requested: bool,
    #[cfg(test)]
    escalation_count: Arc<std::sync::atomic::AtomicUsize>,
    reaped: bool,
}

impl StopHandle {
    /// Request termination without waiting for the supervisor to reap.
    pub fn stop(&self) -> StopOutcome {
        self.stop_with_grace(SHARE_STOP_GRACE)
    }

    /// Kill the child immediately when the host is exiting.
    pub fn stop_now(&self) -> StopOutcome {
        let mut state = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reaped {
            return StopOutcome::AlreadyExited;
        }
        if state.grouped {
            let exited = crate::plugin::proc::leader_exited_nowait(state.pid);
            crate::plugin::proc::kill_group(state.pid);
            return match exited {
                Ok(true) => StopOutcome::AlreadyExited,
                Ok(false) => StopOutcome::Terminated,
                Err(error) => StopOutcome::Failed(error.to_string()),
            };
        }
        match state.child.try_wait() {
            Ok(Some(_)) => {
                state.reaped = true;
                StopOutcome::AlreadyExited
            }
            Err(_) => match state.child.kill() {
                Ok(()) => StopOutcome::Terminated,
                Err(error) => StopOutcome::Failed(error.to_string()),
            },
            Ok(None) => match state.child.kill() {
                Ok(()) => StopOutcome::Terminated,
                Err(error) => StopOutcome::Failed(error.to_string()),
            },
        }
    }

    fn stop_with_grace(&self, grace: Duration) -> StopOutcome {
        let mut state = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reaped {
            return StopOutcome::AlreadyExited;
        }
        if state.grouped {
            match crate::plugin::proc::leader_exited_nowait(state.pid) {
                Ok(true) => {
                    crate::plugin::proc::kill_group(state.pid);
                    return StopOutcome::AlreadyExited;
                }
                Err(error) => return StopOutcome::Failed(error.to_string()),
                Ok(false) => {}
            }
            crate::plugin::proc::term_group(state.pid);
            if !state.stop_requested {
                state.stop_requested = true;
                #[cfg(test)]
                state.escalation_count.fetch_add(1, Ordering::Relaxed);
                let child = Arc::clone(&self.child);
                let _escalation =
                    thegn_core::thread_name::spawn_named("thegn-share-esc", move || {
                        std::thread::sleep(grace);
                        let state = child
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if !state.reaped
                            && let Ok(false) = crate::plugin::proc::leader_exited_nowait(state.pid)
                        {
                            crate::plugin::proc::kill_group(state.pid);
                        }
                    });
            }
            return StopOutcome::Terminated;
        }
        match state.child.try_wait() {
            Ok(Some(_)) => {
                state.reaped = true;
                StopOutcome::AlreadyExited
            }
            Err(error) => StopOutcome::Failed(error.to_string()),
            Ok(None) => match state.child.kill() {
                Ok(()) => StopOutcome::Terminated,
                Err(error) => StopOutcome::Failed(error.to_string()),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareStartCancelled;

impl fmt::Display for ShareStartCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("share: start cancelled")
    }
}

impl std::error::Error for ShareStartCancelled {}

#[cfg(test)]
mod tests;

/// A validated public address. Tickets remain opaque data; callers decide how
/// to display or copy them and must not treat them as command text.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicEndpoint {
    kind: EndpointKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicEndpointKind {
    Url,
    SocketAddr,
    Ticket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EndpointKind {
    Url(String),
    SocketAddr(String),
    Ticket(String),
}

impl fmt::Debug for PublicEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PublicEndpoint(<redacted>)")
    }
}

impl PublicEndpoint {
    const MAX_URL_BYTES: usize = 2048;
    const MAX_TICKET_BYTES: usize = 512;

    pub fn parse_url(value: &str) -> Option<Self> {
        if value.len() > Self::MAX_URL_BYTES
            || value.chars().any(|c| c.is_control() || c.is_whitespace())
            || value.contains('#')
        {
            return None;
        }
        let (scheme, rest) = value.split_once("://")?;
        if !matches!(scheme, "http" | "https") || rest.is_empty() || rest.contains('@') {
            return None;
        }
        let authority = rest.split('/').next()?;
        if authority.is_empty() || authority.contains('?') || authority.contains('#') {
            return None;
        }
        let (host, port) = if authority.starts_with('[') {
            let close = authority.find(']')?;
            let ip = match authority[1..close].parse::<IpAddr>() {
                Ok(ip) => ip,
                Err(_) => return None,
            };
            if !ip.is_ipv6() {
                return None;
            }
            let tail = &authority[close + 1..];
            if tail.is_empty() {
                (format!("[{ip}]"), None)
            } else {
                let port = match tail.strip_prefix(':')?.parse::<u16>() {
                    Ok(port) => port,
                    Err(_) => return None,
                };
                if port == 0 {
                    return None;
                }
                (format!("[{ip}]"), Some(port))
            }
        } else {
            let (host, port) = match authority.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') => {
                    let port = match port.parse::<u16>() {
                        Ok(port) => port,
                        Err(_) => return None,
                    };
                    if port == 0 {
                        return None;
                    }
                    (host, Some(port))
                }
                Some(_) => return None,
                None => (authority, None),
            };
            if host.is_empty() || host.len() > 253 || !host.is_ascii() {
                return None;
            }
            if let Ok(ip) = host.parse::<IpAddr>() {
                (ip.to_string(), port)
            } else {
                (validate_dns_host(host)?, port)
            }
        };
        let path = &rest[authority.len()..];
        if path.contains('@') || !valid_percent_encoding(path) {
            return None;
        }
        let port =
            port.filter(|p| !((*p == 80 && scheme == "http") || (*p == 443 && scheme == "https")));
        let canonical = format!(
            "{scheme}://{host}{}{}",
            port.map(|p| format!(":{p}")).unwrap_or_default(),
            path
        );
        Some(Self {
            kind: EndpointKind::Url(canonical),
        })
    }

    pub fn parse_ticket(value: &str) -> Option<Self> {
        if value.is_empty()
            || value.len() > Self::MAX_TICKET_BYTES
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        {
            return None;
        }
        Some(Self {
            kind: EndpointKind::Ticket(value.to_owned()),
        })
    }

    fn parse_fixed(value: &str) -> Option<Self> {
        if let Some(url) = Self::parse_url(value) {
            return Some(url);
        }
        if value.len() > 260 || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        let (host, port) = value.rsplit_once(':')?;
        let port = match port.parse::<u16>() {
            Ok(port) => port,
            Err(_) => return None,
        };
        if port == 0 {
            return None;
        }
        if host.is_empty() || host.contains('@') || host.contains('/') {
            return None;
        }
        let canonical_host = if host.starts_with('[') {
            let inner = host.strip_prefix('[')?.strip_suffix(']')?;
            match inner.parse::<IpAddr>() {
                Ok(ip) if ip.is_ipv6() => format!("[{ip}]"),
                _ => return None,
            }
        } else if let Ok(ip) = host.parse::<IpAddr>() {
            match ip {
                IpAddr::V4(_) => ip.to_string(),
                IpAddr::V6(_) => return None,
            }
        } else {
            validate_dns_host(host)?
        };
        Some(Self {
            kind: EndpointKind::SocketAddr(format!("{canonical_host}:{port}")),
        })
    }

    pub fn canonical(&self) -> String {
        match &self.kind {
            EndpointKind::Url(url) => url.clone(),
            EndpointKind::SocketAddr(addr) => addr.clone(),
            EndpointKind::Ticket(ticket) => ticket.clone(),
        }
    }

    pub fn as_url(&self) -> Option<&str> {
        match &self.kind {
            EndpointKind::Url(url) => Some(url),
            _ => None,
        }
    }

    pub fn kind(&self) -> PublicEndpointKind {
        match &self.kind {
            EndpointKind::Url(_) => PublicEndpointKind::Url,
            EndpointKind::SocketAddr(_) => PublicEndpointKind::SocketAddr,
            EndpointKind::Ticket(_) => PublicEndpointKind::Ticket,
        }
    }
}

/// Validate an ASCII DNS name without accepting browser-numeric shorthands or
/// malformed IDNA A-labels that a browser could interpret differently.
fn validate_dns_host(host: &str) -> Option<String> {
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return None;
    }
    let labels = host.split('.').collect::<Vec<_>>();
    if !labels.iter().all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            && !(bytes.len() >= 4
                && bytes[2] == b'-'
                && bytes[3] == b'-'
                && !label.starts_with("xn--"))
    }) {
        return None;
    }
    let last = labels.iter().rev().find(|label| !label.is_empty())?;
    let numeric = last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .or_else(|| last.strip_prefix("0X"))
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if numeric {
        return None;
    }
    let canonical = host.to_ascii_lowercase();
    if canonical.split('.').any(|label| label.starts_with("xn--")) {
        match url::Host::parse(host) {
            Ok(url::Host::Domain(domain)) if domain == canonical => {}
            _ => return None,
        }
    }
    Some(canonical)
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

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
    /// Find `marker` in a line and capture the next bounded opaque ticket.
    AfterMarkerRaw { marker: String },
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
                .finish(),
        }
    }
}

impl UrlRule {
    /// Apply the rule to one output line, returning the URL if this line matches.
    /// `Fixed` never matches a line (it's resolved at spawn time, see [`start`]).
    pub fn apply(&self, line: &str) -> Option<PublicEndpoint> {
        match self {
            UrlRule::AfterMarker { marker, scheme } => {
                if marker.is_empty() || line.matches(marker).count() != 1 {
                    return None;
                }
                let rest = line.split_once(marker)?.1.trim_start();
                let token = rest.split_whitespace().next()?.trim_end_matches(['.', ',']);
                if !matches!(scheme.as_str(), "http" | "https") {
                    return None;
                }
                let authority = token.split('/').next()?;
                let port = if authority.starts_with('[') {
                    authority.split_once(']')?.1.strip_prefix(':')?
                } else {
                    authority.rsplit_once(':')?.1
                };
                if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                PublicEndpoint::parse_url(&format!("{scheme}://{token}"))
            }
            UrlRule::AfterMarkerRaw { marker } => {
                if marker.is_empty() || line.matches(marker).count() != 1 {
                    return None;
                }
                let rest = line.split_once(marker)?.1.trim_start();
                let token = rest.split_whitespace().next()?;
                PublicEndpoint::parse_ticket(token)
            }
            UrlRule::Fixed(_) => None,
        }
    }

    /// The config-derived URL, if this is a `Fixed` rule.
    pub fn fixed(&self) -> Option<PublicEndpoint> {
        match self {
            UrlRule::Fixed(u) => PublicEndpoint::parse_fixed(u),
            UrlRule::AfterMarker { .. } | UrlRule::AfterMarkerRaw { .. } => None,
        }
    }
}

/// Secret-bearing share values are zeroized on drop and never formatted (THE-325).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(zeroize::Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

/// A file the provider needs materialized on disk (0600) before spawn and
/// referenced by `args`/`cwd` — e.g. a generated `frpc.toml`. Mirrors
/// `crate::vpn::SidecarFile`. `dest` is relative to the per-share state dir.
#[derive(Clone, PartialEq, Eq)]
pub struct SharePlanFile {
    pub dest: String,
    pub contents: SecretString,
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
    pub env: Vec<(String, SecretString)>,
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
    /// Scan one output line for a validated public endpoint.
    pub fn match_url(&self, line: &str) -> Option<PublicEndpoint> {
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

mod sealed {
    /// Only this crate implements [`super::ShareProvider`].
    pub trait Sealed {}
}

/// The internal share dispatcher. It is sealed: only the built-in backends
/// (bore, frp, tailscale, iroh) implement it, and share providers cannot be
/// added by plugins. Adding one is core/service work with its own issue.
pub trait ShareProvider: sealed::Sealed {
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

impl sealed::Sealed for BuiltinProvider<'_> {}

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
            ShareParams::Bore(b) => Ok(ShareLaunch::Process(plan_bore(self.spec, b)?)),
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

fn plan_bore(spec: &ShareSpec, b: &BoreConfig) -> Result<SharePlan> {
    if let Some(flag) = bore_secret_flag(&b.extra_args) {
        bail!(
            "bore: extra_args must not carry the secret ({flag}); set [share.bore] secret = \"env:BORE_SECRET\" or \"file:PATH\" instead"
        );
    }

    let secret = expand_env_ref(&b.secret);
    let mut env = vec![("RUST_LOG".into(), "info".into())];
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        env.push(("BORE_SECRET".into(), SecretString::new(secret)));
    }
    Ok(SharePlan {
        program: "bore".into(),
        args: bore_args(spec.local_port, b),
        // bore logs the "listening at" line via `tracing` at info on stderr;
        // make sure it's emitted regardless of an inherited RUST_LOG.
        env,
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
    })
}

fn bore_secret_flag(extra_args: &[String]) -> Option<&'static str> {
    extra_args.iter().find_map(|arg| {
        if arg == "--secret" || arg.starts_with("--secret=") {
            Some("--secret")
        } else if arg == "-s" || (arg.starts_with("-s") && !arg.starts_with("--")) {
            Some("-s")
        } else {
            None
        }
    })
}

/// `bore local <port> --to <relay> [--port N] [--local-host H] …`.
fn bore_args(local_port: u16, b: &BoreConfig) -> Vec<String> {
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
            contents: toml.into(),
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
/// prints a ticket; we preserve it as opaque data for display/copy consumers.
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
        },
    }
}

// ── sidecar-serve seam (tailscale; smoke-tested) ─────────────────────────────

/// Bring a tailscale serve/funnel up inside the worktree's VPN `sidecar` and
/// return the resulting public URL (derived from the node's MagicDNS name).
/// Errors with guidance if no sidecar with tailscale is reachable.
pub fn serve_up(sidecar: &str, serve: &ServePlan) -> Result<PublicEndpoint> {
    let recorded = crate::vpn::recorded_sidecar(sidecar)?;
    serve_up_with(&recorded, serve, |runtime, target, argv| {
        exec_in(runtime, target, argv)
    })
}

fn serve_up_with(
    recorded: &crate::vpn::RecordedSidecar,
    serve: &ServePlan,
    mut exec: impl FnMut(&OciRuntime, &str, &[String]) -> Result<(bool, String)>,
) -> Result<PublicEndpoint> {
    let guidance = "share: tailscale ingress needs an active VPN sidecar with tailscale (set [sandbox.vpn] provider = \"tailscale\" on this worktree)";
    match exec(&recorded.runtime, &recorded.id, &serve.up_argv) {
        Ok((true, _)) => {
            let dns = serve_dns_name(&recorded.runtime, &recorded.id, &mut exec)
                .with_context(|| guidance)?;
            let suffix = if serve.port == 443 {
                String::new()
            } else {
                format!(":{}", serve.port)
            };
            let url = format!("{}://{dns}{suffix}", serve.scheme);
            PublicEndpoint::parse_url(&url)
                .ok_or_else(|| anyhow::anyhow!("share: invalid tailscale public URL"))
        }
        Ok((false, output)) => bail!(
            "{guidance}: tailscale serve command failed: {}",
            output.trim()
        ),
        Err(error) => Err(error).with_context(|| guidance),
    }
}

/// Tear a tailscale serve/funnel down and verify it is absent in status JSON.
/// Status uses `{"TCP":{"443":{"HTTPS":true}},"Web":{"host.ts.net:443":{..}},"AllowFunnel":{"host.ts.net:443":true}}`.
/// A failed down command is accepted only when readable status confirms the port is absent.
pub fn serve_down(sidecar: &str, serve: &ServePlan) -> Result<()> {
    let recorded = crate::vpn::recorded_sidecar(sidecar).context("share: teardown unverified")?;
    serve_down_with(
        std::slice::from_ref(&recorded.runtime),
        serve,
        |rt, argv| exec_in(rt, &recorded.id, argv),
    )
}

fn serve_down_with(
    runtimes: &[OciRuntime],
    serve: &ServePlan,
    mut exec: impl FnMut(&OciRuntime, &[String]) -> Result<(bool, String)>,
) -> Result<()> {
    let status_argv = vec![
        "tailscale".to_string(),
        "serve".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];
    for rt in runtimes {
        match exec(rt, &serve.down_argv) {
            Ok((true, _)) | Ok((false, _)) => {}
            Err(error) => {
                tracing::debug!(%error, "tailscale serve down command failed; checking status")
            }
        }
        if let Ok((true, out)) = exec(rt, &status_argv)
            && let Ok(false) = serve_status_has_port(&out, serve.port)
        {
            return Ok(());
        }
    }
    bail!("share: teardown unverified; no runtime verified tailscale serve is absent")
}

/// Return whether Tailscale status reports a serve/funnel on `port`.
fn serve_status_has_port(json: &str, port: u16) -> Result<bool> {
    use serde_json::Value;

    if json.trim().is_empty() || json.trim() == "{}" {
        return Ok(false);
    }
    let status: Value =
        serde_json::from_str(json).context("share: parse tailscale serve status")?;
    if !status.is_object() {
        bail!("share: unexpected tailscale serve status shape")
    }
    let port_key = port.to_string();
    let suffix = format!(":{port}");
    let mut found_section = false;
    for section in ["TCP", "Web", "AllowFunnel"] {
        if let Some(value) = status.get(section) {
            let entries = value.as_object().ok_or_else(|| {
                anyhow::anyhow!("share: unexpected tailscale {section} status shape")
            })?;
            found_section = true;
            let has_port = if section == "TCP" {
                entries.contains_key(&port_key)
            } else {
                entries.keys().any(|key| key.ends_with(&suffix))
            };
            if has_port {
                return Ok(true);
            }
        }
    }
    if !found_section {
        bail!("share: unexpected tailscale serve status shape")
    }
    Ok(false)
}

/// Read the node's MagicDNS name from `tailscale status --json` in the sidecar.
fn serve_dns_name(
    rt: &OciRuntime,
    target: &str,
    exec: &mut impl FnMut(&OciRuntime, &str, &[String]) -> Result<(bool, String)>,
) -> Result<String> {
    let argv = vec![
        "tailscale".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];
    let (ok, out) = exec(rt, target, &argv)?;
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
    child: StopHandle,
    readers: Vec<std::thread::JoinHandle<()>>,
    pub public_url: PublicEndpoint,
    endpoint_tracker: EndpointTracker,
    cleanup: Option<ShareCleanup>,
}

type ShareCleanup = (std::path::PathBuf, Vec<String>, Option<Vec<u8>>);

impl fmt::Debug for RunningShare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunningShare")
            .field("child", &self.child)
            .field("public_url", &"<redacted>")
            .finish()
    }
}

impl RunningShare {
    pub fn stop_handle(&self) -> StopHandle {
        self.child.clone()
    }

    /// Check and reap the child if it has exited.
    pub fn has_exited(&self) -> bool {
        let mut state = self
            .child
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reaped {
            return true;
        }
        if state.grouped {
            match crate::plugin::proc::leader_exited_nowait(state.pid) {
                Ok(true) => crate::plugin::proc::kill_group(state.pid),
                Ok(false) => return false,
                Err(error) => {
                    tracing::debug!(%error, "share child exit check failed");
                    return false;
                }
            }
        }
        match state.child.try_wait() {
            Ok(Some(_)) => {
                state.reaped = true;
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::debug!(%error, "share child exit check failed");
                false
            }
        }
    }

    /// Whether the provider announced a different public endpoint after its
    /// first accepted endpoint. This remains sticky for the share lifetime.
    pub fn endpoint_conflict(&self) -> bool {
        self.endpoint_tracker.conflict.load(Ordering::Acquire)
    }

    /// Run `hook` once when a changed endpoint announcement is observed.
    /// Installing after the conflict was observed still runs it immediately.
    pub fn on_endpoint_conflict(&self, hook: ConflictHook) {
        let mut hook_slot = self.endpoint_tracker.conflict_hook.lock().unwrap();
        *hook_slot = Some(hook);
        if self.endpoint_tracker.conflict.load(Ordering::Acquire)
            && let Some(hook) = hook_slot.take()
        {
            drop(hook_slot);
            hook();
        }
    }

    /// Best-effort terminate the child.
    pub fn stop(self) {
        let report = self.stop_verified();
        if report.leaked_readers > 0 || matches!(report.outcome, StopOutcome::Failed(_)) {
            tracing::warn!(?report, "share teardown was incomplete");
        }
    }

    pub fn wait(self) {
        let report = self.wait_verified();
        if report.leaked_readers > 0 || matches!(report.outcome, StopOutcome::Failed(_)) {
            tracing::warn!(?report, "share teardown was incomplete");
        }
    }

    pub fn stop_verified(mut self) -> ShareTeardown {
        let outcome = self.child.stop();
        let outcome = self.reap(outcome);
        let leaked_readers = self.join_readers();
        self.cleanup_files();
        ShareTeardown {
            outcome,
            leaked_readers,
        }
    }

    pub fn wait_verified(mut self) -> ShareTeardown {
        let outcome = self.reap(StopOutcome::AlreadyExited);
        let leaked_readers = self.join_readers();
        self.cleanup_files();
        ShareTeardown {
            outcome,
            leaked_readers,
        }
    }

    fn reap(&self, outcome: StopOutcome) -> StopOutcome {
        let grouped = {
            let state = self
                .child
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.reaped {
                return if matches!(&outcome, StopOutcome::Failed(_)) {
                    outcome
                } else {
                    StopOutcome::AlreadyExited
                };
            }
            state.grouped
        };
        if grouped {
            let pid = self
                .child
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pid;
            if matches!(&outcome, StopOutcome::Failed(_)) {
                crate::plugin::proc::kill_group(pid);
            }
            if let Err(error) = crate::plugin::proc::wait_leader_exit_nowait(pid) {
                tracing::debug!(%error, "share no-wait leader wait failed; using blocking reap");
                crate::plugin::proc::kill_group(pid);
            }
            let mut state = self
                .child
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = state.child.wait() {
                tracing::debug!(%error, "share child reap failed");
                return StopOutcome::Failed(error.to_string());
            }
            state.reaped = true;
            return outcome;
        }
        self.reap_foreground(outcome)
    }

    fn reap_foreground(&self, outcome: StopOutcome) -> StopOutcome {
        // Foreground CLI and non-Unix fallback retain polling. Supervised Unix
        // shares use WNOWAIT above.
        loop {
            {
                let mut state = self
                    .child
                    .child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.reaped {
                    return if matches!(&outcome, StopOutcome::Failed(_)) {
                        outcome
                    } else {
                        StopOutcome::AlreadyExited
                    };
                }
                match state.child.try_wait() {
                    Ok(Some(_)) => {
                        state.reaped = true;
                        return outcome;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::debug!(%error, "share child wait failed"),
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn join_readers(&mut self) -> usize {
        let deadline = Instant::now() + SHARE_READER_JOIN;
        while Instant::now() < deadline && self.readers.iter().any(|reader| !reader.is_finished()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut leaked = 0;
        for reader in self.readers.drain(..) {
            if !reader.is_finished() {
                leaked += 1;
                continue;
            }
            if reader.join().is_err() {
                tracing::debug!("share reader panicked");
            }
        }
        leaked
    }

    fn cleanup_files(&mut self) {
        if let Some((dir, files, manifest)) = self.cleanup.take() {
            cleanup_state_files(&dir, &files, manifest.as_deref());
        }
    }
}

const SHARE_STOP_GRACE: Duration = Duration::from_secs(3);
const SHARE_READER_JOIN: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareTeardown {
    pub outcome: StopOutcome,
    pub leaked_readers: usize,
}

#[derive(Clone, Default)]
struct EndpointTracker {
    accepted: Arc<OnceLock<String>>,
    conflict: Arc<AtomicBool>,
    conflict_hook: Arc<Mutex<Option<ConflictHook>>>,
}

impl EndpointTracker {
    fn observe(&self, endpoint: &PublicEndpoint) {
        let canonical = endpoint.canonical();
        if let Some(accepted) = self.accepted.get() {
            if accepted != &canonical {
                self.signal_conflict();
            }
        } else if self.accepted.set(canonical.clone()).is_err()
            && self
                .accepted
                .get()
                .is_some_and(|accepted| accepted != &canonical)
        {
            self.signal_conflict();
        }
    }

    fn signal_conflict(&self) {
        if !self.conflict.swap(true, Ordering::AcqRel) {
            self.run_conflict_hook();
        }
    }

    fn run_conflict_hook(&self) {
        let hook = self.conflict_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Create or verify the per-share state directory. The path component is
/// bounded and includes a SHA-256 digest of canonical native path bytes,
/// provider, port, and generation. Its manifest detects digest collisions and
/// prevents reusing another share's directory.
pub fn share_state_dir(
    worktree: &std::path::Path,
    provider: &str,
    port: u16,
    generation: u64,
) -> Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|h| h.join(".local/state"))
                .unwrap_or_default()
        });
    if base.as_os_str().is_empty() {
        anyhow::bail!("share: set XDG_STATE_HOME or HOME; no safe state directory is available");
    }
    share_state_dir_at(&base, worktree, provider, port, generation)
}

fn share_state_dir_at(
    base: &std::path::Path,
    worktree: &std::path::Path,
    provider: &str,
    port: u16,
    generation: u64,
) -> Result<std::path::PathBuf> {
    let canonical = match std::fs::canonicalize(worktree) {
        Ok(path) => path,
        Err(_) => std::path::absolute(worktree).context("share: absolutize worktree identity")?,
    };
    let path_bytes = canonical.as_os_str().as_encoded_bytes();
    let repo_bytes = repository_identity(&canonical);
    let mut identity = format!(
        "share-state-v1\n{}\n{}\n{provider}\n{port}\n{generation}\n",
        path_bytes.len(),
        repo_bytes.len()
    );
    for byte in repo_bytes {
        identity.push_str(&format!("{byte:02x}"));
    }
    identity.push('\n');
    for byte in path_bytes {
        identity.push_str(&format!("{byte:02x}"));
    }
    let digest = Sha256::digest(identity.as_bytes());
    let digest = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let prefix = canonical
        .file_name()
        .unwrap_or(canonical.as_os_str())
        .to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .take(24)
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let prefix = if prefix.is_empty() {
        "worktree"
    } else {
        &prefix
    };
    let dir = base.join("thegn/share").join(format!("{prefix}-{digest}"));
    let parent = dir.parent().context("share: state dir has no parent")?;
    thegn_core::fsperm::ensure_private_dir(parent)
        .with_context(|| format!("share: state parent {}", parent.display()))?;
    let created = match std::fs::create_dir(&dir) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("share: create state dir {}", dir.display()));
        }
    };
    let metadata = std::fs::symlink_metadata(&dir).context("share: inspect state dir")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("share: refusing non-directory state path");
    }
    let manifest = dir.join("owner.manifest");
    if created {
        thegn_core::fsperm::ensure_private_dir(&dir)
            .with_context(|| format!("share: state dir {}", dir.display()))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest)
            .context("share: create owner manifest")?;
        file.write_all(identity.as_bytes())
            .context("share: write owner manifest")?;
        file.sync_all().context("share: sync owner manifest")?;
    } else {
        let manifest_meta = match std::fs::symlink_metadata(&manifest) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Another creator may have won mkdir and be writing its manifest.
                let mut found = false;
                for _ in 0..50 {
                    match std::fs::symlink_metadata(&manifest) {
                        Ok(_) => {
                            found = true;
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => return Err(e).context("share: inspect owner manifest"),
                    }
                }
                if !found {
                    anyhow::bail!("share: existing state directory has no owner manifest");
                }
                std::fs::symlink_metadata(&manifest).context("share: inspect owner manifest")?
            }
            Err(error) => return Err(error).context("share: inspect owner manifest"),
        };
        if !manifest_meta.file_type().is_file() {
            anyhow::bail!("share: refusing non-regular owner manifest");
        }
        let mut matches = false;
        for _ in 0..50 {
            let mut existing = String::new();
            thegn_core::fsperm::open_regular_file_nofollow(&manifest)
                .context("share: open owner manifest")?
                .read_to_string(&mut existing)?;
            if existing == identity {
                matches = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if !matches {
            anyhow::bail!("share: state directory ownership manifest mismatch");
        }
        thegn_core::fsperm::ensure_private_dir(&dir)
            .with_context(|| format!("share: state dir {}", dir.display()))?;
    }
    Ok(dir)
}

/// Identify the repository shared by linked worktrees. If `.git` cannot be
/// decoded, retain the canonical worktree path as the repository identity;
/// the worktree bytes below still distinguish it losslessly.
fn repository_identity(worktree: &std::path::Path) -> Vec<u8> {
    let dot_git = worktree.join(".git");
    let Ok(metadata) = std::fs::metadata(&dot_git) else {
        return worktree.as_os_str().as_encoded_bytes().to_vec();
    };
    let git_dir = if metadata.is_dir() {
        match std::fs::canonicalize(&dot_git) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::trace!(%error, "could not canonicalize worktree .git directory");
                None
            }
        }
    } else {
        match std::fs::read_to_string(&dot_git) {
            Ok(contents) => match contents.trim().strip_prefix("gitdir:") {
                Some(raw) => match std::fs::canonicalize(worktree.join(raw.trim())) {
                    Ok(path) => Some(path),
                    Err(error) => {
                        tracing::trace!(%error, "could not canonicalize worktree git directory");
                        None
                    }
                },
                None => None,
            },
            Err(_) => None,
        }
    };
    let Some(git_dir) = git_dir else {
        return worktree.as_os_str().as_encoded_bytes().to_vec();
    };
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(raw) => match std::fs::canonicalize(git_dir.join(raw.trim())) {
            Ok(path) => path,
            Err(_) => git_dir.clone(),
        },
        Err(_) => git_dir,
    };
    common_dir.as_os_str().as_encoded_bytes().to_vec()
}

const DIRECT_CLIENT_ENV_KEYS: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SystemRoot",
    "WINDIR",
];

/// Keep only basic runtime locale, path, and temporary-directory settings.
/// HOME, account names, DBUS_*, IROH_SECRET, provider credentials, and OCI
/// connection variables are deliberately excluded. Adding a key requires
/// source review because every direct share client starts with this environment.
fn direct_client_env(
    ambient: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    ambient
        .into_iter()
        .filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                DIRECT_CLIENT_ENV_KEYS
                    .iter()
                    .any(|allowed| key.eq_ignore_ascii_case(allowed))
            })
        })
        .collect()
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
    let cancel = AtomicBool::new(false);
    start_with_ambient(
        plan,
        statedir,
        timeout,
        std::env::vars_os(),
        &cancel,
        ProcessGrouping::Foreground,
    )
}

pub fn start_cancellable(
    plan: &SharePlan,
    statedir: &std::path::Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<RunningShare> {
    start_with_ambient(
        plan,
        statedir,
        timeout,
        std::env::vars_os(),
        cancel,
        ProcessGrouping::OwnGroup,
    )
}

#[derive(Clone, Copy)]
enum ProcessGrouping {
    Foreground,
    OwnGroup,
}

fn start_with_ambient(
    plan: &SharePlan,
    statedir: &std::path::Path,
    timeout: Duration,
    ambient: impl IntoIterator<Item = (OsString, OsString)>,
    cancel: &AtomicBool,
    grouping: ProcessGrouping,
) -> Result<RunningShare> {
    let manifest = read_owner_manifest(statedir)?;
    match start_inner(
        plan,
        statedir,
        timeout,
        manifest.as_deref(),
        ambient,
        cancel,
        grouping,
    ) {
        Ok(mut running) => {
            running.cleanup = Some((
                statedir.to_path_buf(),
                plan.files.iter().map(|f| f.dest.clone()).collect(),
                manifest,
            ));
            Ok(running)
        }
        Err(error) => {
            let files = plan
                .files
                .iter()
                .map(|file| file.dest.clone())
                .collect::<Vec<_>>();
            cleanup_state_files(statedir, &files, manifest.as_deref());
            Err(error)
        }
    }
}

fn cleanup_state_files(statedir: &std::path::Path, files: &[String], manifest: Option<&[u8]>) {
    if let Err(error) = verify_owner_manifest(statedir, manifest) {
        tracing::warn!(%error, "share ownership changed before credential cleanup");
        return;
    }
    let mut credentials_removed = true;
    for file in files {
        if validate_dest(file).is_ok()
            && let Err(error) = std::fs::remove_file(statedir.join(file))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            credentials_removed = false;
            tracing::warn!(%error, "could not remove share credential file");
        }
    }
    if manifest.is_none() || !credentials_removed {
        return;
    }
    remove_owned_path(&statedir.join("owner.manifest"), "share owner manifest");
    if let Err(error) = std::fs::remove_dir(statedir)
        && !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
        )
    {
        tracing::warn!(%error, "could not remove share state directory");
    }
}

fn remove_owned_path(path: &std::path::Path, description: &str) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not remove {description}");
    }
}

fn read_owner_manifest(statedir: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let path = statedir.join("owner.manifest");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("share: inspect owner manifest"),
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!("share: refusing non-regular owner manifest");
    }
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(&path)
        .context("share: open owner manifest")?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .context("share: read owner manifest")?;
    Ok(Some(contents))
}

fn verify_owner_manifest(statedir: &std::path::Path, expected: Option<&[u8]>) -> Result<()> {
    let actual = read_owner_manifest(statedir)?;
    if actual.as_deref() != expected {
        anyhow::bail!("share: state directory ownership manifest changed");
    }
    Ok(())
}

fn start_inner(
    plan: &SharePlan,
    statedir: &std::path::Path,
    timeout: Duration,
    owner_manifest: Option<&[u8]>,
    ambient: impl IntoIterator<Item = (OsString, OsString)>,
    cancel: &AtomicBool,
    grouping: ProcessGrouping,
) -> Result<RunningShare> {
    verify_owner_manifest(statedir, owner_manifest)?;
    if cancel.load(Ordering::Acquire) {
        return Err(ShareStartCancelled.into());
    }
    if matches!(&plan.url_rule, UrlRule::Fixed(_)) && plan.url_rule.fixed().is_none() {
        anyhow::bail!("share: invalid fixed public endpoint");
    }
    materialize_files(plan, statedir)?;
    let sd = statedir.to_string_lossy().into_owned();
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|a| a.replace("{statedir}", &sd))
        .collect();

    let mut cmd = Command::new(&plan.program);
    cmd.env_clear()
        .envs(direct_client_env(ambient))
        .args(&args)
        .current_dir(statedir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &plan.env {
        cmd.env(k, v.expose());
    }
    if matches!(grouping, ProcessGrouping::OwnGroup) {
        crate::plugin::proc::set_process_group(&mut cmd);
    }
    if cancel.load(Ordering::Acquire) {
        return Err(ShareStartCancelled.into());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("share: failed to spawn '{}'", plan.program))?;

    let (tx, rx) = mpsc::sync_channel::<String>(64);
    // URL discovery has a separate bounded signal so noisy diagnostics cannot
    // consume the only startup result.
    let (url_tx, url_rx) = mpsc::sync_channel::<PublicEndpoint>(2);
    let endpoint_tracker = EndpointTracker::default();
    let mut readers = Vec::new();
    for stream in [
        child.stdout.take().map(Streamable::Out),
        child.stderr.take().map(Streamable::Err),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        let url_tx = url_tx.clone();
        let endpoint_tracker = endpoint_tracker.clone();
        let url_rule = plan.url_rule.clone();
        readers.push(thegn_core::thread_name::spawn_named(
            "thegn-share-rd",
            move || {
                drain_stream(stream, tx, url_tx, url_rule, endpoint_tracker);
            },
        ));
    }
    drop(tx);
    drop(url_tx);

    // Config-derived URL: the client never prints it. Confirm it doesn't exit
    // immediately (auth failure, bad config), then return the known address.
    if let Some(url) = plan.url_rule.fixed() {
        let grace = Duration::from_millis(1500).min(timeout);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if cancel.load(Ordering::Acquire) {
                abort_child(child, grouping);
                return Err(ShareStartCancelled.into());
            }
            let exited = if matches!(grouping, ProcessGrouping::OwnGroup)
                && crate::plugin::proc::bounded_group_termination_supported()
            {
                crate::plugin::proc::leader_exited_nowait(child.id())?
            } else {
                child.try_wait()?.is_some()
            };
            if exited {
                abort_child(child, grouping);
                if cancel.load(Ordering::Acquire) {
                    return Err(ShareStartCancelled.into());
                }
                anyhow::bail!("share: '{}' exited early", plan.program);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if cancel.load(Ordering::Acquire) {
            abort_child(child, grouping);
            return Err(ShareStartCancelled.into());
        }
        return Ok(RunningShare {
            readers,
            child: StopHandle {
                child: Arc::new(Mutex::new(ShareChild {
                    pid: child.id(),
                    grouped: matches!(grouping, ProcessGrouping::OwnGroup)
                        && crate::plugin::proc::bounded_group_termination_supported(),
                    stop_requested: false,
                    #[cfg(test)]
                    escalation_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    child,
                    reaped: false,
                })),
            },
            public_url: url,
            endpoint_tracker,
            cleanup: None,
        });
    }

    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Acquire) {
            abort_child(child, grouping);
            return Err(ShareStartCancelled.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            abort_child(child, grouping);
            if cancel.load(Ordering::Acquire) {
                return Err(ShareStartCancelled.into());
            }
            anyhow::bail!(
                "share: '{}' did not report a URL within {}s",
                plan.program,
                timeout.as_secs()
            );
        }
        if let Ok(url) = url_rx.try_recv() {
            if let Err(error) = reject_duplicate_announcement(&url_rx, &url) {
                abort_child(child, grouping);
                return Err(error);
            }
            if endpoint_tracker.conflict.load(Ordering::Acquire) {
                abort_child(child, grouping);
                anyhow::bail!("share: provider reported multiple public endpoints");
            }
            if cancel.load(Ordering::Acquire) {
                abort_child(child, grouping);
                return Err(ShareStartCancelled.into());
            }
            return Ok(RunningShare {
                readers,
                child: StopHandle {
                    child: Arc::new(Mutex::new(ShareChild {
                        pid: child.id(),
                        grouped: matches!(grouping, ProcessGrouping::OwnGroup)
                            && crate::plugin::proc::bounded_group_termination_supported(),
                        stop_requested: false,
                        #[cfg(test)]
                        escalation_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        child,
                        reaped: false,
                    })),
                },
                public_url: url,
                endpoint_tracker,
                cleanup: None,
            });
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(_line) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The priority check may have run before a reader published
                // its URL. If the diagnostic channel then disconnects, give
                // the already-published priority signal one final check.
                if let Ok(url) = url_rx.try_recv() {
                    if let Err(error) = reject_duplicate_announcement(&url_rx, &url) {
                        abort_child(child, grouping);
                        return Err(error);
                    }
                    if endpoint_tracker.conflict.load(Ordering::Acquire) {
                        abort_child(child, grouping);
                        anyhow::bail!("share: provider reported multiple public endpoints");
                    }
                    if cancel.load(Ordering::Acquire) {
                        abort_child(child, grouping);
                        return Err(ShareStartCancelled.into());
                    }
                    return Ok(RunningShare {
                        readers,
                        child: StopHandle {
                            child: Arc::new(Mutex::new(ShareChild {
                                pid: child.id(),
                                grouped: matches!(grouping, ProcessGrouping::OwnGroup)
                                    && crate::plugin::proc::bounded_group_termination_supported(),
                                stop_requested: false,
                                #[cfg(test)]
                                escalation_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                                child,
                                reaped: false,
                            })),
                        },
                        public_url: url,
                        endpoint_tracker,
                        cleanup: None,
                    });
                }
                if cancel.load(Ordering::Acquire) {
                    abort_child(child, grouping);
                    return Err(ShareStartCancelled.into());
                }
                abort_child(child, grouping);
                anyhow::bail!("share: '{}' exited before reporting a URL", plan.program);
            }
        }
    }
}

fn reject_duplicate_announcement(
    url_rx: &mpsc::Receiver<PublicEndpoint>,
    first: &PublicEndpoint,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(50);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match url_rx.recv_timeout(remaining) {
            Ok(next) if next.canonical() != first.canonical() => {
                anyhow::bail!("share: provider reported multiple public endpoints")
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(());
            }
        }
    }
}

/// Perform best-effort startup cleanup, including the supervised group when
/// one was created. Startup reader joins remain outside this teardown path.
fn abort_child(mut child: Child, grouping: ProcessGrouping) {
    let pid = child.id();
    if matches!(grouping, ProcessGrouping::OwnGroup)
        && crate::plugin::proc::bounded_group_termination_supported()
    {
        crate::plugin::proc::kill_group(pid);
    } else if let Err(error) = child.kill() {
        tracing::debug!(%error, "share startup abort kill failed");
    }
    if let Err(error) = child.wait() {
        tracing::debug!(%error, "share startup abort reap failed");
    }
}

/// Longest accepted `SharePlanFile::dest`, in bytes. Well under the common
/// 255-byte NAME_MAX so the sibling temp name (`.<dest>.tmp-…`) also fits.
const MAX_DEST_BYTES: usize = 200;

/// Pure check that `dest` is exactly one plain, non-empty path component: no
/// absolute/prefix/root forms, no `.`/`..`, no `/` or `\`, no control
/// characters, no `:`, no trailing `.`/space, no Windows reserved device
/// stems (CON, NUL, COM1..9, ...), not overlong. Run for every file before any filesystem change.
fn validate_dest(dest: &str) -> Result<()> {
    let bad = |why: &str| anyhow::anyhow!("share: invalid file destination {dest:?}: {why}");
    if dest.is_empty() {
        return Err(bad("empty"));
    }
    if dest.len() > MAX_DEST_BYTES {
        return Err(bad("too long"));
    }
    if dest == "." || dest == ".." {
        return Err(bad("not a plain name"));
    }
    if dest
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(bad("contains a separator or control character"));
    }
    if dest.contains(':') {
        return Err(bad("contains ':' (alternate data stream / drive form)"));
    }
    if dest.ends_with('.') || dest.ends_with(' ') {
        return Err(bad("ends with '.' or space"));
    }
    let stem = dest.split('.').next().unwrap_or(dest).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes().get(3), Some(b'1'..=b'9'))
            && stem.len() == 4);
    if reserved {
        return Err(bad("reserved device name"));
    }
    let mut comps = std::path::Path::new(dest).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => Err(bad("not a single plain component")),
    }
}

/// Write each plan file into `statedir` owner-only (dir 0700, files 0600).
///
/// Every failure is fatal and happens before the provider is spawned; errors
/// name paths only, never file contents.
///
/// Race contract: safety comes from the write mechanism, not from checks.
/// Each file is created as a temp sibling with exclusive creation
/// (`O_CREAT|O_EXCL`, which never follows a symlink), owner-only mode applied
/// at creation, written and fsynced, then `rename`d onto the destination.
/// `rename` replaces a final-component symlink instead of following it, so a
/// symlink planted at the destination, even after the `symlink_metadata`
/// precheck below, is never written THROUGH. The precheck is only a tamper
/// signal (policy) and may race harmlessly.
///
/// Out of scope (THE-327 / share-generation ownership): ancestors of
/// `statedir`, collisions between starts, cleanup on stop.
fn materialize_files(plan: &SharePlan, statedir: &std::path::Path) -> Result<()> {
    if plan.files.is_empty() {
        return Ok(());
    }
    for f in &plan.files {
        validate_dest(&f.dest)?;
    }
    thegn_core::fsperm::ensure_private_dir(statedir)
        .with_context(|| format!("share: state dir {}", statedir.display()))?;
    // Precheck EVERY destination before writing the first, so a bad later
    // entry aborts without leaving earlier credential files behind.
    for f in &plan.files {
        let path = statedir.join(&f.dest);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_file() => {
                thegn_core::fsperm::ensure_single_link_regular_file(&m)
                    .with_context(|| format!("share: inspect hardlinks for {}", path.display()))?;
            }
            Ok(_) => anyhow::bail!(
                "share: refusing {}: existing entry is a symlink or not a regular file",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("share: stat {}", path.display()));
            }
        }
    }
    for f in &plan.files {
        let path = statedir.join(&f.dest);
        thegn_core::fsperm::write_owner_only_atomic(&path, f.contents.expose().as_bytes())
            .with_context(|| format!("share: write {}", path.display()))?;
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
    url_tx: mpsc::SyncSender<PublicEndpoint>,
    url_rule: UrlRule,
    endpoint_tracker: EndpointTracker,
) {
    match stream {
        Streamable::Out(reader) => drain_reader(reader, tx, url_tx, url_rule, endpoint_tracker),
        Streamable::Err(reader) => drain_reader(reader, tx, url_tx, url_rule, endpoint_tracker),
    }
}

fn drain_reader<R: Read>(
    mut reader: R,
    tx: mpsc::SyncSender<String>,
    url_tx: mpsc::SyncSender<PublicEndpoint>,
    url_rule: UrlRule,
    endpoint_tracker: EndpointTracker,
) {
    let mut bytes = [0_u8; 1024];
    let mut line = Vec::with_capacity(MAX_PROVIDER_LINE);
    let mut oversized = false;
    loop {
        let count = match reader.read(&mut bytes) {
            Ok(0) => {
                emit_line(
                    &mut line,
                    &mut oversized,
                    &tx,
                    &url_tx,
                    &url_rule,
                    &endpoint_tracker,
                );
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
            Ok(count) => count,
        };
        for byte in &bytes[..count] {
            if *byte == b'\n' {
                emit_line(
                    &mut line,
                    &mut oversized,
                    &tx,
                    &url_tx,
                    &url_rule,
                    &endpoint_tracker,
                );
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
    url_tx: &mpsc::SyncSender<PublicEndpoint>,
    url_rule: &UrlRule,
    endpoint_tracker: &EndpointTracker,
) {
    if !*oversized && !line.is_empty() {
        let text = bounded_lossy(line);
        if let Some(url) = url_rule.apply(&text) {
            endpoint_tracker.observe(&url);
            let _ = url_tx.try_send(url);
        }
        let _ = tx.try_send(text);
    }
    line.clear();
    *oversized = false;
}
