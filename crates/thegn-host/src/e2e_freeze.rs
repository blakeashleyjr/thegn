//! `THEGN_E2E=1`: pin the chrome inputs that change on their own so a driven
//! instance renders byte-identical frames across runs — the precondition for
//! text/styled/pixel snapshot regression under muse.
//!
//! What it pins (and where the pin is applied):
//! - system stats (`run.rs` stats ingestion) → a fixed [`stats`] snapshot;
//! - the masthead clock/date (`chrome::masthead_widget`) → [`now`];
//! - the version in the brand widget → [`VERSION`];
//! - the activity-dot FSM (`hydrate.rs`) → not polled, so dots never decay
//!   and the needs-you chip (derived from it) never appears;
//! - the media badge → `[media] enabled = false` forced on every config load
//!   ([`apply_to_config`]), since the player watcher reaches the session bus
//!   (or `playerctl`) even when `DBUS_SESSION_BUS_ADDRESS` is cut;
//! - the splash logotype's version line (`logotype.rs`).
//!
//! It is a test hook only: nothing else reads it, and when the variable is
//! unset every check is one relaxed atomic load.

use std::sync::OnceLock;

static ACTIVE: OnceLock<bool> = OnceLock::new();

/// Whether the freeze is on (`THEGN_E2E` set to anything but `0`/empty).
pub fn active() -> bool {
    *ACTIVE.get_or_init(|| {
        std::env::var_os("THEGN_E2E")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// The version string the brand widget shows while frozen.
pub const VERSION: &str = "0.0.0-e2e";

/// The `vX.Y.Z` the chrome prints — real, unless frozen.
pub fn version_label() -> String {
    if active() {
        format!("v{VERSION}")
    } else {
        format!("v{}", env!("CARGO_PKG_VERSION"))
    }
}

/// Force config knobs that would make frames machine-dependent.
pub fn apply_to_config(cfg: &mut thegn_core::config::Config) {
    if active() {
        cfg.media.enabled = false;
        // The two background measurement scans land asynchronously and their
        // values are whatever this machine's checkout happens to weigh — so a
        // spec would race the size badge and the `LOC` chip appearing mid-run,
        // and record a byte count no other machine reproduces. Off entirely
        // while frozen: `show_sizes` hides cached sizes as well as new ones, and
        // `[loc] enabled = false` hides the chip, its detail table and the
        // Files-footer count.
        cfg.disk.show_sizes = false;
        cfg.loc.enabled = false;
    }
}

/// The wall clock while frozen: 2026-01-01 12:00:00 local time.
pub fn now() -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
        .single()
        .unwrap_or_else(chrono::Local::now)
}

/// A plausible, fixed stats reading: CPU/mem/disk present (so those widgets
/// render), GPU/battery/net absent (so those hide).
pub fn stats() -> thegn_metrics::StatsSnapshot {
    thegn_metrics::StatsSnapshot {
        cpu_pct: Some(12),
        cpu_cores: vec![10, 14],
        cpu_freq_mhz: Some(3000),
        cpu_temp_c: Some(45.0),
        mem_gib: Some((4.0, 16.0)),
        disk_free_pct: Some(50),
        disk_bytes: Some((50 << 30, 100 << 30)),
        load_avg: Some((0.5, 0.4, 0.3)),
        uptime_secs: Some(3600),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_inputs_are_stable() {
        assert_eq!(stats(), stats());
        assert_eq!(now(), now());
        assert_eq!(
            now().format("%Y-%m-%d %H:%M").to_string(),
            "2026-01-01 12:00"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(now(), now());
    }

    /// The freeze must be a no-op when it isn't active — `apply_to_config` runs
    /// on every launch, frozen or not.
    #[test]
    fn apply_to_config_leaves_a_live_session_alone() {
        if active() {
            return; // running under THEGN_E2E; the other assertion is the point
        }
        let mut cfg = thegn_core::config::Config::default();
        apply_to_config(&mut cfg);
        assert!(cfg.disk.show_sizes, "sizes stay on outside the freeze");
        assert!(cfg.loc.enabled, "LOC stays on outside the freeze");
    }
}
