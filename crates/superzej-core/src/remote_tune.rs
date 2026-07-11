//! Process-global ssh transport tuning for the control plane.
//!
//! `remote::ssh_base` is called from dozens of sites that have no `Config` in
//! hand (GitLoc shims, placement wraps, host runners), so the resolved
//! `[remote]` tuning is installed once at startup into a `OnceLock` — the same
//! pattern as the render-caps holder — and `ssh_base` reads it on every build.
//! Before `set_ssh_tune` runs (unit tests, early CLI paths) the defaults apply,
//! which match the historical hardcoded values plus keepalives.

use std::sync::OnceLock;

/// Resolved ssh transport tuning (from `[remote]` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SshTune {
    /// `ServerAliveInterval` — seconds between client keepalive probes over the
    /// encrypted channel. `0` disables keepalives entirely.
    pub keepalive_interval_secs: u32,
    /// `ServerAliveCountMax` — unanswered probes before ssh declares the
    /// connection dead (interval × count ≈ time-to-declare-dead).
    pub keepalive_count_max: u32,
    /// `ConnectTimeout` — seconds to wait for the TCP/proxy connection.
    pub connect_timeout_secs: u32,
    /// `ControlPersist` — seconds an idle ControlMaster stays alive.
    pub control_persist_secs: u32,
    /// `TCPKeepAlive` — kernel-level TCP keepalive (catches dead NAT paths the
    /// application-level probe can miss, and vice versa).
    pub tcp_keepalive: bool,
}

impl Default for SshTune {
    fn default() -> Self {
        SshTune {
            // 15s × 4 ≈ 60s to declare a dead link: tolerant enough for a
            // 200ms+ lossy cellular path, fast enough that a retry ladder
            // (Phase 2) still converges within a bring-up step budget.
            keepalive_interval_secs: 15,
            keepalive_count_max: 4,
            connect_timeout_secs: 10,
            control_persist_secs: 300,
            tcp_keepalive: true,
        }
    }
}

static TUNE: OnceLock<SshTune> = OnceLock::new();

/// Install the resolved `[remote]` tuning. First call wins; later calls are
/// ignored (config is resolved once at startup — a mid-session change requires
/// a restart, like the theme palette).
pub fn set_ssh_tune(t: SshTune) {
    let _ = TUNE.set(t); // best-effort: first-set-wins by design
}

/// The active tuning (defaults until [`set_ssh_tune`] runs).
pub fn ssh_tune() -> SshTune {
    TUNE.get().copied().unwrap_or_default()
}

/// The `-o` argument pairs for a tune's keepalive settings. Pure — unit-tested
/// separately from the `ssh_base` assembly.
pub fn keepalive_args(t: SshTune) -> Vec<String> {
    let mut v = Vec::new();
    if t.keepalive_interval_secs > 0 {
        v.push("-o".into());
        v.push(format!("ServerAliveInterval={}", t.keepalive_interval_secs));
        v.push("-o".into());
        v.push(format!("ServerAliveCountMax={}", t.keepalive_count_max));
    }
    if t.tcp_keepalive {
        v.push("-o".into());
        v.push("TCPKeepAlive=yes".into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_keepalive_on() {
        let t = SshTune::default();
        assert_eq!(t.keepalive_interval_secs, 15);
        assert_eq!(t.keepalive_count_max, 4);
        assert_eq!(t.connect_timeout_secs, 10);
        assert_eq!(t.control_persist_secs, 300);
        assert!(t.tcp_keepalive);
    }

    #[test]
    fn keepalive_args_default() {
        let args = keepalive_args(SshTune::default());
        let joined = args.join(" ");
        assert!(joined.contains("-o ServerAliveInterval=15"), "{joined}");
        assert!(joined.contains("-o ServerAliveCountMax=4"), "{joined}");
        assert!(joined.contains("-o TCPKeepAlive=yes"), "{joined}");
    }

    #[test]
    fn keepalive_args_disabled() {
        // interval 0 ⇒ no ServerAlive pair at all (CountMax alone is meaningless).
        let t = SshTune {
            keepalive_interval_secs: 0,
            tcp_keepalive: false,
            ..SshTune::default()
        };
        assert!(keepalive_args(t).is_empty());
    }

    #[test]
    fn keepalive_args_tcp_only() {
        let t = SshTune {
            keepalive_interval_secs: 0,
            ..SshTune::default()
        };
        assert_eq!(keepalive_args(t), vec!["-o", "TCPKeepAlive=yes"]);
    }

    #[test]
    fn tune_getter_defaults_without_set() {
        // The OnceLock may or may not be set by another test in this process;
        // either way the getter returns a valid tune.
        let t = ssh_tune();
        assert!(t.connect_timeout_secs > 0);
    }
}
