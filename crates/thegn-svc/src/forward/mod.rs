//! Per-worktree port-forward detection + the local-container forward mechanism.
//!
//! The *detection* sibling of [`crate::share`]: where share spawns an outbound
//! tunnel to a public URL, this discovers ports bound **inside** a worktree's
//! sandbox (via `ss`/`netstat`) so the host can forward them to localhost for
//! browser preview, and provides the `exec`-bridge argv the host's userspace
//! proxy uses to reach them.
//!
//! **Why an `exec` bridge and not a direct IP dial?** A rootless-podman container
//! (the default backend) lives in its own network namespace reached via
//! slirp4netns/pasta — the host cannot dial its IP directly; only ports published
//! with `-p` at create time are reachable. To forward a port that appears *after*
//! the container starts (a dev server the user launches), we instead enter the
//! container's netns through the one runtime that holds it and bridge stdio to
//! its loopback. This one mechanism works for rootless + rootful podman and
//! docker alike.
//!
//! Pure argv builders are unit-tested here; the subprocess execution is the I/O
//! seam exercised by `test/smoke.sh`.

use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use thegn_core::forward::parse_ss_listening;

use crate::vpn::{OciRuntime, exec_in};

#[cfg(test)]
mod tests;

/// OCI runtimes to probe when reaching a worktree's sandbox container. Every
/// likely runtime is asked because same-named containers make a guess unsafe.
fn likely_runtimes() -> Vec<OciRuntime> {
    vec![
        OciRuntime::podman(),
        OciRuntime::docker(),
        OciRuntime::new(vec!["sudo".into(), "-n".into(), "podman".into()]),
    ]
}

/// The command (run via `exec`) that lists listening TCP ports inside a
/// container. `ss -ltnH` is preferred; `netstat -ltn` is the fallback for images
/// without iproute2. `|| true` keeps the exec exit code 0 so a missing tool reads
/// as "no ports" rather than an error.
pub fn ss_probe_cmd() -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "ss -ltnH 2>/dev/null || netstat -ltn 2>/dev/null || true".into(),
    ]
}

/// One runtime's answer to the listening-port probe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    Reached(BTreeSet<u16>),
    Absent,
    Unknown(String),
}

/// `exec_in`'s result as a [`Reach`]: success means reached; a non-zero exit or
/// a missing runtime binary means absent; other errors remain unknown.
fn classify_probe(result: Result<(bool, String)>) -> Reach {
    match result {
        Ok((true, output)) => Reach::Reached(parse_ss_listening(&output)),
        Ok((false, _)) => Reach::Absent,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Reach::Absent
        }
        Err(error) => Reach::Unknown(format!("{error:#}")),
    }
}

/// Two or more runtimes hold the container name, so forwarding into either
/// could reach the wrong container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousRuntime {
    pub container: String,
    pub runtimes: Vec<String>,
}

impl std::fmt::Display for AmbiguousRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} exists in more than one container runtime ({}); port forwarding is paused for this worktree until only one holds it",
            self.container,
            self.runtimes.join(", ")
        )
    }
}

impl std::error::Error for AmbiguousRuntime {}

/// Pure runtime selection: only one reachable runtime with no unknown probes
/// is safe to use.
fn pick_runtime(
    container: &str,
    outcomes: Vec<(OciRuntime, Reach)>,
) -> Result<(Vec<String>, BTreeSet<u16>)> {
    let reached: Vec<_> = outcomes
        .iter()
        .filter_map(|(runtime, result)| match result {
            Reach::Reached(ports) => Some((runtime, ports)),
            Reach::Absent | Reach::Unknown(_) => None,
        })
        .collect();
    let unknown: Vec<_> = outcomes
        .iter()
        .filter_map(|(runtime, result)| match result {
            Reach::Unknown(reason) => Some((runtime, reason)),
            Reach::Reached(_) | Reach::Absent => None,
        })
        .collect();

    if reached.len() > 1 {
        return Err(anyhow::Error::new(AmbiguousRuntime {
            container: container.to_string(),
            runtimes: reached
                .iter()
                .map(|(runtime, _)| runtime.prefix.join(" "))
                .collect(),
        }));
    }
    if let Some((unknown_runtime, reason)) = unknown.first()
        && !reached.is_empty()
    {
        return Err(anyhow!(
            "cannot rule out a same-named container in '{}': {reason}",
            unknown_runtime.prefix.join(" ")
        ));
    }
    if let Some((runtime, ports)) = reached.first() {
        return Ok((runtime.prefix.clone(), (*ports).clone()));
    }

    let mut message = format!("no OCI runtime could reach {container}");
    for (runtime, reason) in unknown {
        message.push_str(&format!(
            "; '{}' probe failed: {reason}",
            runtime.prefix.join(" ")
        ));
    }
    Err(anyhow!(message))
}

/// Probe every likely runtime once and pick the one that holds `container`.
pub fn resolve_container_runtime(container: &str) -> Result<(Vec<String>, BTreeSet<u16>)> {
    let command = ss_probe_cmd();
    let outcomes = likely_runtimes()
        .into_iter()
        .map(|runtime| {
            let reach = classify_probe(exec_in(&runtime, container, &command));
            (runtime, reach)
        })
        .collect();
    pick_runtime(container, outcomes)
}

/// Probe ports through the already-resolved runtime prefix only.
pub fn probe_ports_in(prefix: &[String], container: &str) -> Result<BTreeSet<u16>> {
    let runtime = OciRuntime {
        prefix: prefix.to_vec(),
    };
    match exec_in(&runtime, container, &ss_probe_cmd()) {
        Ok((true, output)) => Ok(parse_ss_listening(&output)),
        Ok((false, _)) => Err(anyhow!(
            "'{}' could not reach {container}",
            prefix.join(" ")
        )),
        Err(error) => Err(error),
    }
}

/// The full argv for one bridged connection: enter `container`'s netns and pipe
/// stdio to its loopback `port`. `socat` is preferred; `nc` is the fallback. The
/// host proxy spawns this per accepted connection and copies bytes between the
/// TCP client and the child's stdin/stdout.
pub fn exec_bridge_argv(rt_prefix: &[String], container: &str, port: u16) -> Vec<String> {
    let mut v = rt_prefix.to_vec();
    v.push("exec".into());
    v.push("-i".into());
    v.push(container.to_string());
    v.push("sh".into());
    v.push("-c".into());
    v.push(format!(
        "socat -d0 STDIO TCP:127.0.0.1:{port} 2>/dev/null || nc 127.0.0.1 {port}"
    ));
    v
}
