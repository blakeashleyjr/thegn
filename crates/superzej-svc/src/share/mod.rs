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
//! matcher are unit-tested here ([`tests`]); the subprocess execution
//! ([`start`]) is the I/O seam, exercised by `test/smoke.sh`.
//!
//! `bore` (<https://github.com/ekzhang/bore>) is the first and only backend; the
//! [`ShareProvider`] seam keeps room for rathole/zrok/ngrok/iroh later.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use superzej_core::config::{BoreConfig, expand_env_ref};
use superzej_core::share::{ShareParams, ShareSpec};

#[cfg(test)]
mod tests;

/// How to derive the public URL from a line of the tunnel client's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRule {
    /// Find `marker` in a line, take the whitespace-delimited token after it as a
    /// `host:port`, and format it into `scheme://host:port`.
    AfterMarker { marker: String, scheme: String },
}

impl UrlRule {
    /// Apply the rule to one output line, returning the public URL if it matches.
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
        }
    }
}

/// A pure, fully-resolved plan for the tunnel-client child. Built from a
/// [`ShareSpec`] (with secrets already dereferenced); executed by [`start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePlan {
    /// The tunnel-client binary (e.g. `bore`).
    pub program: String,
    /// Args after the program.
    pub args: Vec<String>,
    /// Environment overrides (secrets already resolved).
    pub env: Vec<(String, String)>,
    /// How to recognise the public URL in the child's output.
    pub url_rule: UrlRule,
}

impl SharePlan {
    /// Scan one output line for the public URL.
    pub fn match_url(&self, line: &str) -> Option<String> {
        self.url_rule.apply(line)
    }
}

/// The share provider seam. One built-in impl ([`BuiltinProvider`]) dispatches
/// on the resolved provider; external providers could plug in here later.
pub trait ShareProvider {
    /// Stable id for logging/bookkeeping.
    fn kind(&self) -> &'static str;
    /// Build the (secrets-resolved) plan. Never errors today (bore's secret is
    /// optional), but returns `Result` so future backends can require secrets.
    fn plan(&self) -> Result<SharePlan>;
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
        }
    }

    fn plan(&self) -> Result<SharePlan> {
        match &self.spec.params {
            ShareParams::Bore(b) => Ok(plan_bore(self.spec, b)),
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

/// Spawn the tunnel client described by `plan` and block until its public URL
/// appears (or `timeout` elapses). On timeout/EOF the child is killed and an
/// error returned. A reader thread tails stdout+stderr so the URL is found
/// wherever the backend logs it.
pub fn start(plan: &SharePlan, timeout: Duration) -> Result<RunningShare> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
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

enum Streamable {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}
