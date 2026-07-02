//! Per-worktree ingress sharing — expose a worktree-local port at a public URL.
//!
//! The *inbound* sibling of [`crate::vpn`]. This module owns provider
//! *knowledge*: given a resolved [`ShareSpec`], build the (pure, testable)
//! [`SharePlan`] — the tunnel-client program, args, env, and the rule for
//! scraping the public URL out of its output — resolve its secrets, then spawn
//! and watch the child. The lifecycle (restart, persistence, UI) lives in the
//! host, exactly as [`crate::vpn`] hands its plan back to `superzej_core::sandbox`.
//!
//! Division of labor mirrors the other svc seams: the pure plan builder and URL
//! matcher are unit-tested here (`tests`); the subprocess execution
//! ([`start`]) is the I/O seam, exercised by `test/smoke.sh`.
//!
//! `bore` (<https://github.com/ekzhang/bore>) is the first and only backend; the
//! [`ShareProvider`] seam keeps room for rathole/zrok/ngrok/iroh later.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use superzej_core::config::{
    BoreConfig, FrpConfig, FrpProxyType, IrohShareConfig, TailscaleShareConfig, expand_env_ref,
};
use superzej_core::share::{ShareParams, ShareSpec};

use crate::vpn::{OciRuntime, exec_in};

#[cfg(test)]
mod tests;

/// How to derive the public URL/address from a started share.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePlanFile {
    pub dest: String,
    pub contents: String,
}

/// A pure, fully-resolved plan for the tunnel-client child. Built from a
/// [`ShareSpec`] (with secrets already dereferenced); executed by [`start`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
    let server = f.server_addr.trim();
    if server.is_empty() {
        bail!("frp: set [share.frp] server_addr to your frps host");
    }
    let is_web = matches!(f.proxy_type, FrpProxyType::Https | FrpProxyType::Http);
    let subdomain = {
        let s = f.subdomain.trim();
        if s.is_empty() {
            format!("{}-{}", spec.label, spec.local_port)
        } else {
            s.to_string()
        }
    };
    let proxy_type = f.proxy_type.as_str();

    // Build frpc.toml.
    let mut toml = String::new();
    toml.push_str(&format!("serverAddr = \"{server}\"\n"));
    toml.push_str(&format!("serverPort = {}\n", f.server_port));
    if let Some(tok) = expand_env_ref(&f.token) {
        toml.push_str("auth.method = \"token\"\n");
        toml.push_str(&format!("auth.token = \"{tok}\"\n"));
    }
    toml.push_str("\n[[proxies]]\n");
    toml.push_str(&format!(
        "name = \"sz-{}-{}\"\n",
        spec.label, spec.local_port
    ));
    toml.push_str(&format!("type = \"{proxy_type}\"\n"));
    toml.push_str("localIP = \"127.0.0.1\"\n");
    toml.push_str(&format!("localPort = {}\n", spec.local_port));
    if is_web {
        toml.push_str(&format!("subdomain = \"{subdomain}\"\n"));
    } else if f.remote_port != 0 {
        toml.push_str(&format!("remotePort = {}\n", f.remote_port));
    }
    for line in &f.extra {
        toml.push_str(line);
        toml.push('\n');
    }

    // Derive the public URL.
    let url = if is_web {
        let host = f.subdomain_host.trim();
        if host.is_empty() {
            bail!("frp: set [share.frp] subdomain_host to derive the https URL");
        }
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
        format!("{server}:{}", f.remote_port)
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
#[derive(Debug)]
pub struct RunningShare {
    pub child: Child,
    pub public_url: String,
}

impl RunningShare {
    /// Best-effort terminate the child.
    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The per-share state directory `$XDG_STATE_HOME/superzej/share/<wt>-<port>/`,
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
    base.join("superzej/share").join(format!("{slug}-{port}"))
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

    let (tx, rx) = mpsc::channel::<String>();
    for stream in [
        child.stdout.take().map(Streamable::Out),
        child.stderr.take().map(Streamable::Err),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let reader: Box<dyn BufRead> = match stream {
                Streamable::Out(s) => Box::new(BufReader::new(s)),
                Streamable::Err(s) => Box::new(BufReader::new(s)),
            };
            for line in reader.lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    // Config-derived URL: the client never prints it. Confirm it doesn't exit
    // immediately (auth failure, bad config), then return the known address.
    if let Some(url) = plan.url_rule.fixed() {
        let grace = Duration::from_millis(1500).min(timeout);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait()? {
                let tail: Vec<String> = rx.try_iter().collect();
                anyhow::bail!(
                    "share: '{}' exited early ({status}): {}",
                    plan.program,
                    tail.join("; ")
                );
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
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "share: '{}' did not report a URL within {}s",
                plan.program,
                timeout.as_secs()
            );
        }
        match rx.recv_timeout(remaining) {
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
                let _ = child.wait();
                anyhow::bail!("share: '{}' exited before reporting a URL", plan.program);
            }
        }
    }
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

enum Streamable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}
