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
//! - Processes rows → six fixed, high odd PIDs, so the live sampler cannot
//!   expose machine processes or let a driven signal reach one.
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

/// Fixed process rows used by deterministic monitor snapshots.
pub fn procs() -> thegn_metrics::ProcSnapshot {
    use thegn_metrics::ProcOwner;
    thegn_metrics::ProcSnapshot {
        procs: vec![
            row(
                4_194_305,
                None,
                "thegn",
                3.5,
                64 << 20,
                3600,
                ProcOwner::ThegnSelf,
            ),
            row(
                4_194_307,
                None,
                "thegn-daemon",
                0.8,
                32 << 20,
                7200,
                ProcOwner::ThegnDaemon,
            ),
            row(
                4_194_309,
                None,
                "pane-shell",
                1.2,
                48 << 20,
                1800,
                ProcOwner::Pane(1),
            ),
            row(
                4_194_311,
                Some(4_194_309),
                "pane-child",
                0.4,
                12 << 20,
                1200,
                ProcOwner::Pane(1),
            ),
            row(
                4_194_313,
                None,
                "editor",
                8.1,
                256 << 20,
                900,
                ProcOwner::Other,
            ),
            row(
                4_194_315,
                None,
                "worker",
                2.0,
                96 << 20,
                300,
                ProcOwner::Other,
            ),
        ],
        total: 6,
        primed: true,
        enabled: true,
    }
}

fn row(
    pid: u32,
    ppid: Option<u32>,
    name: &str,
    cpu: f32,
    rss: u64,
    run_secs: u64,
    owner: thegn_metrics::ProcOwner,
) -> thegn_metrics::ProcSample {
    thegn_metrics::ProcSample {
        pid,
        ppid,
        start_time: 1_767_268_800,
        start_ticks: None,
        name: name.to_string(),
        cpu_pct: cpu,
        rss_bytes: rss,
        run_secs,
        owner,
    }
}

pub fn procs_or(live: thegn_metrics::ProcSnapshot) -> thegn_metrics::ProcSnapshot {
    procs_when(active(), live)
}

fn procs_when(frozen: bool, live: thegn_metrics::ProcSnapshot) -> thegn_metrics::ProcSnapshot {
    if frozen { procs() } else { live }
}

#[cfg(test)]
pub(crate) const FROZEN_MS: u64 = 1_767_268_800_000;

#[cfg(test)]
pub(crate) fn freeze_frame(frame: &mut thegn_metrics::collect::frame::MonitorFrame) {
    use thegn_metrics::collect::{
        CollectorId,
        health::Health,
        model::cpu_stat::{CoreSample, ModeShare},
        parse::{loadavg::Loadavg, psi::PsiFile, uptime::Uptime},
    };
    frame.reset();
    frame.seq = 1;
    frame.at_ms = FROZEN_MS;
    frame.dt_ms = 1000;
    for id in [
        CollectorId::Loadavg,
        CollectorId::Uptime,
        CollectorId::Psi,
        CollectorId::CpuStat,
    ] {
        frame.sampled.insert(id);
    }
    frame.health = [Health::Ok; thegn_metrics::collect::COLLECTORS];
    frame.parts.loadavg.value = Some(Loadavg {
        load1: 50,
        load5: 40,
        load15: 30,
        running: 1,
        total: 6,
        last_pid: 4_194_315,
    });
    frame.parts.uptime.value = Some(Uptime {
        up_centi: 360_000,
        idle_centi: 0,
    });
    frame.parts.psi.cpu.present = true;
    frame.parts.psi.cpu.file = PsiFile::default();
    frame.parts.psi.cpu.some_stall_centi = Some(12);
    frame.parts.psi.memory.present = true;
    frame.parts.psi.memory.some_stall_centi = Some(4);
    frame.parts.cpu_stat.total = Some(ModeShare {
        user: 120,
        system: 80,
        idle: 800,
        busy: 200,
        ..Default::default()
    });
    frame.parts.cpu_stat.cores.clear();
    frame.parts.cpu_stat.cores.extend([
        CoreSample {
            id: 0,
            online: true,
            share: frame.parts.cpu_stat.total,
        },
        CoreSample {
            id: 1,
            online: true,
            share: frame.parts.cpu_stat.total,
        },
    ]);
}

#[cfg(test)]
pub(crate) fn seed_series(
    store: &mut thegn_core::series_store::SeriesStore,
    keys: &[(
        thegn_core::series_store::SeriesKey,
        thegn_core::series_store::Class,
    )],
    now_ms: u64,
) -> usize {
    use thegn_core::series_store::horizon_secs;
    let mut seeded = 0;
    for (key, class) in keys {
        let Some(id) = store.intern(*key, *class) else {
            continue;
        };
        seeded += 1;
        let start = now_ms.saturating_sub(u64::from(horizon_secs(*class)) * 1000);
        let hour_start = now_ms.saturating_sub(3_600_000).max(start);
        let ten_min_start = now_ms.saturating_sub(600_000).max(start);
        let mut at = start;
        let mut step = 0_u64;
        while at < hour_start {
            let _pushed = store.push(id, at, seed_value(*key, step));
            step += 1;
            at = at.saturating_add(60_000);
        }
        at = hour_start.max(at);
        while at < ten_min_start {
            let _pushed = store.push(id, at, seed_value(*key, step));
            step += 1;
            at = at.saturating_add(5_000);
        }
        at = ten_min_start.max(at);
        while at <= now_ms {
            let _pushed = store.push(id, at, seed_value(*key, step));
            step += 1;
            let next = at.saturating_add(1_000);
            if next == at {
                break;
            }
            at = next;
        }
    }
    seeded
}

#[cfg(test)]
fn seed_value(key: thegn_core::series_store::SeriesKey, step: u64) -> f32 {
    ((u64::from(key.family) * 31 + u64::from(key.metric) * 17 + u64::from(key.instance) * 7 + step)
        % 1000) as f32
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

    #[test]
    fn procs_when_frozen_replaces_the_live_list() {
        let live = thegn_metrics::ProcSnapshot {
            procs: vec![row(
                1,
                None,
                "init",
                0.0,
                0,
                0,
                thegn_metrics::ProcOwner::Other,
            )],
            total: 1,
            primed: false,
            enabled: false,
        };
        let frozen = procs_when(true, live.clone());
        assert_eq!(frozen, procs());
        assert_eq!(frozen.total, 6);
        assert!(frozen.primed && frozen.enabled);
        assert_eq!(procs_when(false, live.clone()), live);
    }

    #[test]
    fn frozen_pids_cannot_name_a_real_process() {
        let fixture = procs();
        let pids: std::collections::HashSet<_> = fixture.procs.iter().map(|p| p.pid).collect();
        assert_eq!(fixture.procs.len(), 6);
        for process in &fixture.procs {
            assert!(process.pid >= 4_194_305 && process.pid % 2 == 1);
            if let Some(parent) = process.ppid {
                assert!(pids.contains(&parent));
            }
        }
    }

    #[test]
    fn freeze_frame_is_stable_and_reuses_buffers() {
        use thegn_metrics::collect::frame::MonitorFrame;
        let mut frame = MonitorFrame::default();
        frame.parts.cpu_stat.cores.reserve(8);
        frame.parts.cpu_stat.cores.push(Default::default());
        freeze_frame(&mut frame);
        let first = frame.clone();
        let pointer = frame.parts.cpu_stat.cores.as_ptr();
        let capacity = frame.parts.cpu_stat.cores.capacity();
        frame.parts.cpu_stat.cores.clear();
        frame.parts.cpu_stat.cores.push(Default::default());
        freeze_frame(&mut frame);
        assert_eq!(frame, first);
        assert_eq!(frame.parts.cpu_stat.cores.as_ptr(), pointer);
        assert_eq!(frame.parts.cpu_stat.cores.capacity(), capacity);
        assert_eq!(frame.at_ms, FROZEN_MS);
    }

    #[test]
    fn seed_series_is_deterministic_and_covers_the_horizon() {
        use thegn_core::series_store::{Class, SeriesBuf, SeriesKey, SeriesStore, Tier};
        let keys = [(
            SeriesKey {
                family: 1,
                metric: 2,
                instance: 3,
            },
            Class::A,
        )];
        let now = FROZEN_MS;
        let mut first = SeriesStore::new();
        let mut second = SeriesStore::new();
        assert_eq!(seed_series(&mut first, &keys, now), 1);
        assert_eq!(seed_series(&mut second, &keys, now), 1);
        let id1 = first.id_of(keys[0].0).unwrap();
        let id2 = second.id_of(keys[0].0).unwrap();
        assert_eq!(first.covered_secs(id1, now), Some(86_400));
        for tier in [Tier::T0, Tier::T1, Tier::T2] {
            let mut a = SeriesBuf::default();
            let mut b = SeriesBuf::default();
            assert!(first.read(id1, tier, now - 86_400_000, now, &mut a));
            assert!(second.read(id2, tier, now - 86_400_000, now, &mut b));
            assert_eq!(a.rows, b.rows);
        }
        let over_cap: Vec<_> = (0..97_u16)
            .map(|instance| {
                (
                    SeriesKey {
                        family: 4,
                        metric: 1,
                        instance,
                    },
                    Class::A,
                )
            })
            .collect();
        let mut capped = SeriesStore::new();
        assert_eq!(seed_series(&mut capped, &over_cap, now), 96);
    }
}
