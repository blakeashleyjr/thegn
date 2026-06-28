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
//! container's netns with `podman exec` and bridge stdio to its loopback. This
//! one mechanism works for rootless + rootful podman and docker alike.
//!
//! Pure argv builders are unit-tested here; the subprocess execution is the I/O
//! seam exercised by `test/smoke.sh`.

use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use superzej_core::forward::parse_ss_listening;

use crate::vpn::{OciRuntime, exec_in};

#[cfg(test)]
mod tests;

/// OCI runtimes to try when reaching a worktree's sandbox container. We don't
/// record which one created it, so try the likely ones (mirrors
/// `share::likely_runtimes`); a wrong runtime fails to find the container and is
/// skipped.
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

/// Probe the listening TCP ports inside `container`, trying each likely runtime.
/// Returns `(runtime_prefix, ports)` for the first runtime that reaches the
/// container — the prefix so the caller can build the `exec`-bridge argv (see
/// [`exec_bridge_argv`]) without a second blocking probe. `Err` when no runtime
/// can reach it (so the caller keeps its last-known set rather than tearing
/// forwards down on a transient failure / stopped sandbox).
pub fn probe_container_ports(container: &str) -> Result<(Vec<String>, BTreeSet<u16>)> {
    let cmd = ss_probe_cmd();
    let mut last_err = None;
    for rt in likely_runtimes() {
        match exec_in(&rt, container, &cmd) {
            Ok((true, out)) => return Ok((rt.prefix, parse_ss_listening(&out))),
            Ok((false, _)) => {
                last_err = Some(anyhow!(
                    "'{}' could not reach {container}",
                    rt.prefix.join(" ")
                ));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no OCI runtime could reach {container}")))
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
