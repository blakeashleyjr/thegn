//! `[remote]` — ssh transport tuning + control-plane retry + host-heal cadence
//! (the flaky-link hardening knobs). Kept out of the ratcheted `config.rs`;
//! the resolved values are installed process-globally on load (`install`) so
//! the many `ssh_base()` call sites that carry no `Config` read them from the
//! same holders the render caps use.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RemoteConfig {
    /// ssh `ServerAliveInterval` for every control-plane connection (seconds;
    /// `0` disables keepalives).
    pub keepalive_interval_secs: u32,
    /// ssh `ServerAliveCountMax` — unanswered probes before the link is dead.
    pub keepalive_count_max: u32,
    /// ssh `ConnectTimeout` (seconds).
    pub connect_timeout_secs: u32,
    /// ssh `ControlPersist` — idle ControlMaster lifetime (seconds).
    pub control_persist_secs: u32,
    /// Kernel-level `TCPKeepAlive` alongside the application probes.
    pub tcp_keepalive: bool,
    /// Transient-failure retries per bring-up step (connect/probe/resolve).
    pub retry_attempts: u32,
    /// First retry backoff (milliseconds); doubles per attempt.
    pub retry_base_delay_ms: u64,
    /// Backoff ceiling (milliseconds).
    pub retry_max_delay_ms: u64,
    /// Background re-probe cadence for a failed host (seconds; the last step
    /// repeats forever). Empty ⇒ the built-in `[15, 30, 60, 300]`.
    pub heal_backoff_secs: Vec<u64>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        let tune = crate::remote_tune::SshTune::default();
        let cp = crate::retry::ReconnectPolicy::control_plane_default();
        RemoteConfig {
            keepalive_interval_secs: tune.keepalive_interval_secs,
            keepalive_count_max: tune.keepalive_count_max,
            connect_timeout_secs: tune.connect_timeout_secs,
            control_persist_secs: tune.control_persist_secs,
            tcp_keepalive: tune.tcp_keepalive,
            retry_attempts: cp.max_attempts,
            retry_base_delay_ms: cp.base_delay_ms,
            retry_max_delay_ms: cp.max_delay_ms,
            heal_backoff_secs: crate::heal::HealSchedule::default().steps_secs,
        }
    }
}

impl RemoteConfig {
    /// The ssh tuning this config resolves to.
    pub fn ssh_tune(&self) -> crate::remote_tune::SshTune {
        crate::remote_tune::SshTune {
            keepalive_interval_secs: self.keepalive_interval_secs,
            keepalive_count_max: self.keepalive_count_max.max(1),
            connect_timeout_secs: self.connect_timeout_secs.max(1),
            control_persist_secs: self.control_persist_secs,
            tcp_keepalive: self.tcp_keepalive,
        }
    }

    /// The control-plane retry policy this config resolves to.
    pub fn control_plane_policy(&self) -> crate::retry::ReconnectPolicy {
        crate::retry::ReconnectPolicy {
            max_attempts: self.retry_attempts.max(1),
            base_delay_ms: self.retry_base_delay_ms,
            max_delay_ms: self.retry_max_delay_ms.max(self.retry_base_delay_ms),
        }
    }

    /// The host-heal schedule this config resolves to.
    pub fn heal_schedule(&self) -> crate::heal::HealSchedule {
        crate::heal::HealSchedule::from_config(&self.heal_backoff_secs)
    }

    /// Install the resolved tuning into the process-global holders (called
    /// from `Config::post_process` on every load; first set wins, so a
    /// mid-session config reload keeps the startup values — like the theme).
    pub fn install(&self) {
        crate::remote_tune::set_ssh_tune(self.ssh_tune());
        crate::retry::set_control_plane(self.control_plane_policy());
        crate::heal::set_schedule(self.heal_schedule());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_mirror_the_builtin_tuning() {
        let rc = RemoteConfig::default();
        assert_eq!(rc.ssh_tune(), crate::remote_tune::SshTune::default());
        assert_eq!(
            rc.control_plane_policy(),
            crate::retry::ReconnectPolicy::control_plane_default()
        );
        assert_eq!(rc.heal_schedule(), crate::heal::HealSchedule::default());
    }

    #[test]
    fn resolution_clamps_pathological_values() {
        let rc = RemoteConfig {
            keepalive_count_max: 0,
            connect_timeout_secs: 0,
            retry_attempts: 0,
            retry_max_delay_ms: 1, // below base
            retry_base_delay_ms: 1000,
            ..RemoteConfig::default()
        };
        assert_eq!(rc.ssh_tune().keepalive_count_max, 1);
        assert_eq!(rc.ssh_tune().connect_timeout_secs, 1);
        let p = rc.control_plane_policy();
        assert_eq!(p.max_attempts, 1);
        assert_eq!(p.max_delay_ms, 1000, "ceiling never below base");
    }

    #[test]
    fn toml_roundtrip() {
        let toml = r#"
            keepalive_interval_secs = 5
            retry_attempts = 7
            heal_backoff_secs = [10, 20]
        "#;
        let rc: RemoteConfig = toml::from_str(toml).unwrap();
        assert_eq!(rc.keepalive_interval_secs, 5);
        assert_eq!(rc.retry_attempts, 7);
        assert_eq!(rc.heal_schedule().steps_secs, vec![10, 20]);
        // Unspecified keys keep defaults.
        assert_eq!(rc.connect_timeout_secs, 10);
    }
}
