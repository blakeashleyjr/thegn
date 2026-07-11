//! ControlMaster hygiene for the host control plane.
//!
//! A wedged/dead ControlMaster socket makes every multiplexed exec fail
//! identically (exit 255, empty stderr) no matter how healthy the network is —
//! the retry ladder would burn its whole budget against a local corpse. Before
//! a connect (and between transport-classified retries) we ask the master how
//! it's doing (`ssh -O check`, a local unix-socket ping) and clear a dead one
//! so the next attempt builds a fresh connection.

use std::process::{Command, Stdio};

use thegn_core::placement::Placement;
use thegn_core::remote::control_path;

/// Best-effort master health check + stale-socket cleanup for an ssh
/// placement. No-op for every other placement. Never fails — worst case the
/// next exec reports the real error.
pub(crate) fn master_hygiene(placement: &Placement) {
    let Placement::Ssh(p) = placement else {
        return;
    };
    let sock = control_path(&p.host, p.port);
    if !sock.exists() {
        return; // nothing to check; ssh will build a fresh master
    }
    let check = Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .args(["-O", "check", &p.host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if matches!(check, Ok(s) if s.success()) {
        return; // master alive — reuse it
    }
    // Dead or wedged: ask it to exit (unblocks a half-dead master process),
    // then unlink the socket so ControlMaster=auto starts clean.
    // best-effort: a failure here just means the next exec reports the error.
    let _ = Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", sock.display()))
        .args(["-O", "exit", &p.host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&sock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::placement::{SshPlacement, TransportKind};

    #[test]
    fn hygiene_is_a_noop_for_local_and_missing_socket() {
        // Local placement: returns immediately.
        master_hygiene(&Placement::Local);
        // Ssh placement with no socket on disk: returns without spawning ssh.
        let p = Placement::Ssh(SshPlacement {
            host: format!("nobody@sz-hygiene-test-{}", std::process::id()),
            port: 1,
            forward_agent: false,
            kind: TransportKind::Ssh,
            ssh_config: None,
            jump_host: None,
            identity: None,
            extra_args: Vec::new(),
        });
        master_hygiene(&p); // must not panic or block
    }
}
