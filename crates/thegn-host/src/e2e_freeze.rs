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
//! - the weather widget and the popup's weather block → `[weather] enabled =
//!   false`, also in [`apply_to_config`]. Same class again: a live reading whose
//!   text changes on its own, fetched over the network. Because the feature is
//!   off by default, this changes no baseline;
//! - the splash logotype's version line (`logotype.rs`);
//! - the startup status line's build stamp (`hydrate::startup_status_line`) →
//!   [`BUILD_TIME`], since `THEGN_BUILD_TIME` changes on every rebuild.
//! - the UI locale → `en-US` before `i18n::init`, so host locale/config and the
//!   developer pseudolocale cannot change snapshots.
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
        // The two background measurement scans land asynchronously and their
        // values are whatever this machine's checkout happens to weigh — so a
        // spec would race the size badge and the `LOC` chip appearing mid-run,
        // and record a byte count no other machine reproduces. Off entirely
        // while frozen: `show_sizes` hides cached sizes as well as new ones, and
        // `[loc] enabled = false` hides the chip, its detail table and the
        // Files-footer count.
        cfg.disk.show_sizes = false;
        cfg.loc.enabled = false;
        // The usage gauge polls provider APIs and renders live percentages —
        // the same volatility class; pinned off while frozen.
        cfg.usage.enabled = false;
        // The model proxy spawns a background daemon and reaches the network on
        // agent traffic; disable it under the freeze so no process launches and
        // the usage panel's proxy-spend block never renders.
        cfg.model_proxy.enabled = false;
        // Weather reaches the network and renders a live reading whose text
        // changes on its own — the two things a byte-identical frame cannot
        // survive. Off entirely while frozen, like `[usage]` and `[media]`.
        cfg.weather.enabled = false;
        // Voice is an explicit microphone/process surface; deterministic runs
        // must remain idle and must never launch a user command.
        cfg.voice.enabled = false;
    }
}

/// Pin the startup-only UI locale before `i18n::init` resolves it.
pub fn pin_locale(language: &mut String) {
    pin_locale_when(active(), language);
}

fn pin_locale_when(frozen: bool, language: &mut String) {
    if frozen {
        "en-US".clone_into(language);
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

/// The clipboard image-paste drop filename while frozen — the generated name
/// (`img-<utc-ms>-<rand>.png`) is volatile by construction, so a muse spec
/// driving paste-image would record a name no other run reproduces. Returns
/// `None` (⇒ the real generator) unless the freeze is on.
pub fn paste_image_name() -> Option<String> {
    active().then(|| "img-e2e.png".to_string())
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
    fn paste_image_name_pins_only_when_frozen() {
        if active() {
            assert_eq!(paste_image_name().as_deref(), Some("img-e2e.png"));
        } else {
            assert_eq!(
                paste_image_name(),
                None,
                "real generator outside the freeze"
            );
        }
    }

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
        assert!(!cfg.voice.enabled, "voice defaults off outside the freeze");
    }

    #[test]
    fn locale_pin_is_explicit_and_freeze_only() {
        let mut frozen = "ja-JP".to_string();
        pin_locale_when(true, &mut frozen);
        assert_eq!(frozen, "en-US");

        let mut live = "ja-JP".to_string();
        pin_locale_when(false, &mut live);
        assert_eq!(live, "ja-JP");
    }
}
