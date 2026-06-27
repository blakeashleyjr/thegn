//! Battery state as `(percent 0–100, on AC)`. sysinfo does not cover battery,
//! so this is ours: a native sysfs reader on Linux (which can distinguish a
//! charge-capped-but-plugged-in battery from one that is truly discharging),
//! and `starship-battery` elsewhere.

/// Read battery state: `(capacity %, on AC)`. `None` when there is no battery
/// (desktop / VM). The bool means "plugged in", not "actively charging".
#[cfg(target_os = "linux")]
pub fn read_battery(base: &std::path::Path) -> Option<(u8, bool)> {
    read_battery_sysfs(base)
}

#[cfg(not(target_os = "linux"))]
pub fn read_battery(_base: &std::path::Path) -> Option<(u8, bool)> {
    read_battery_starship()
}

/// Linux `/sys/class/power_supply` reader. AC presence comes from an adapter's
/// `online` flag — the only signal that survives charge-limiting, since a
/// battery capped at e.g. 80% reports `Discharging` even while plugged in. The
/// battery's own `Charging`/`Full`/`Not charging` status is kept as a fallback
/// for trees that expose no adapter `online` file. Pure given a base dir, so
/// it's unit-testable against a fixture tree.
#[cfg(target_os = "linux")]
fn read_battery_sysfs(base: &std::path::Path) -> Option<(u8, bool)> {
    let mut battery: Option<(u8, bool)> = None; // (capacity, status implies AC)
    let mut ac_online = false;
    for e in std::fs::read_dir(base).ok()?.flatten() {
        let p = e.path();
        // Batteries advertise type "Battery"; adapters say "Mains" (or "USB").
        if std::fs::read_to_string(p.join("type"))
            .map(|t| t.trim() == "Battery")
            .unwrap_or(false)
        {
            if battery.is_some() {
                continue; // first battery wins
            }
            let pct = std::fs::read_to_string(p.join("capacity"))
                .ok()?
                .trim()
                .parse::<u8>()
                .ok()?;
            let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
            let status_ac = matches!(status.trim(), "Charging" | "Full" | "Not charging");
            battery = Some((pct.min(100), status_ac));
        } else if std::fs::read_to_string(p.join("online"))
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
        {
            // A Mains/USB adapter reporting online=1 means we're plugged in,
            // regardless of whether the battery is actually taking charge.
            ac_online = true;
        }
    }
    let (pct, status_ac) = battery?;
    Some((pct, ac_online || status_ac))
}

/// Non-Linux battery via `starship-battery` (macOS IOKit, Windows
/// `GetSystemPowerStatus`, BSD). Reports the first battery; "on AC" is true when
/// it is charging/full or a charger is attached.
#[cfg(not(target_os = "linux"))]
fn read_battery_starship() -> Option<(u8, bool)> {
    use starship_battery::{Manager, State};
    let manager = Manager::new().ok()?;
    let mut batteries = manager.batteries().ok()?;
    let bat = batteries.next()?.ok()?;
    // state_of_charge is a ratio 0.0..=1.0 (a `Ratio` quantity).
    let pct = (bat.state_of_charge().value * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;
    let on_ac = matches!(bat.state(), State::Charging | State::Full);
    Some((pct, on_ac))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn read_battery_parses_fixture_tree() {
        let base = std::env::temp_dir().join(format!("sz-batt-{}", std::process::id()));
        let bat = base.join("BAT0");
        let ac = base.join("AC");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&bat).unwrap();
        std::fs::create_dir_all(&ac).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("capacity"), "73\n").unwrap();
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, true)));

        // Unplugged, no adapter `online` file: status alone drives it.
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, false)));

        // Charge-capped: battery reports Discharging while plugged in, but the
        // Mains adapter's online=1 still reads as on-AC (the bug fix).
        std::fs::write(ac.join("online"), "1\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, true)));
        std::fs::write(ac.join("online"), "0\n").unwrap();
        assert_eq!(read_battery(&base), Some((73, false)));

        // No battery dir at all → None (desktop).
        let empty = base.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(read_battery(&empty), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
