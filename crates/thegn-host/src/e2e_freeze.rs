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
//! - the AI-usage gauge and its panel section → `[usage] enabled = false`, also
//!   in [`apply_to_config`]. Its numbers are a live reading of whichever
//!   accounts happen to be logged in on the machine, and its chip carries a
//!   countdown that changes every minute — the two things a byte-identical
//!   frame cannot survive. It also reaches the network, which a driven instance
//!   must not do;
//! - the splash logotype's version line (`logotype.rs`);
//! - the startup status line's build stamp (`hydrate::startup_status_line`) →
//!   [`BUILD_TIME`], since `THEGN_BUILD_TIME` changes on every rebuild.
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

/// The build stamp the startup status line shows while frozen.
///
/// `THEGN_BUILD_TIME` is baked in at compile time, so the real value changes on
/// every rebuild — which makes the startup status line, and therefore every
/// snapshot that includes the statusbar, unreproducible by construction. It was
/// added without a pin here; this is that pin.
pub const BUILD_TIME: &str = "e2e";

/// The build stamp the startup status line prints — real, unless frozen.
pub fn build_stamp() -> &'static str {
    if active() {
        BUILD_TIME
    } else {
        env!("THEGN_BUILD_TIME")
    }
}

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
        cfg.usage.enabled = false;
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
}
