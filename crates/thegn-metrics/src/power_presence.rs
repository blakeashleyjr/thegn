//! One-time static power-source discovery for the Power monitor tab.
//!
//! `probe` runs from `StatsSampler::new`, called by
//! `hydrate_refresh_ticker::prime_stats` on the Background-QoS ticker thread,
//! as well as from `gtui-query` and `thegn doctor`. Keep this probe off the UI
//! loop; the result is copied into each `StatsSnapshot`.

use std::path::Path;

/// Probe static sysfs facts that make the Power tab useful without a battery.
/// Called once when the stats sampler is built, away from the UI loop.
pub(crate) fn probe(root: &Path) -> bool {
    has_powercap(root) || root.join("firmware/acpi/platform_profile").exists() || has_mains(root)
}

fn has_powercap(root: &Path) -> bool {
    let path = root.join("class/powercap");
    let Ok(zones) = std::fs::read_dir(path) else {
        return false;
    };

    zones.take(64).flatten().any(|zone| {
        let path = zone.path();
        path.join("energy_uj").exists() || path.join("power_uw").exists()
    })
}

fn has_mains(root: &Path) -> bool {
    let path = root.join("class/power_supply");
    let Ok(supplies) = std::fs::read_dir(path) else {
        return false;
    };

    supplies.take(64).flatten().any(|supply| {
        std::fs::read_to_string(supply.path().join("type"))
            .is_ok_and(|kind| matches!(kind.trim(), "Mains" | "USB"))
    })
}

#[cfg(test)]
mod tests {
    use super::probe;
    use std::path::Path;

    fn mkdir(root: &Path, path: &str) {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }

    fn write(root: &Path, path: &str, contents: &str) {
        std::fs::write(root.join(path), contents).unwrap();
    }

    fn battery_only(root: &Path) {
        mkdir(root, "class/power_supply/BAT0");
        write(root, "class/power_supply/BAT0/type", "Battery\n");
    }

    fn rapl_only(root: &Path) {
        mkdir(root, "class/powercap/intel-rapl-0");
        write(root, "class/powercap/intel-rapl-0/energy_uj", "");
    }

    fn powercap_power_only(root: &Path) {
        mkdir(root, "class/powercap/package-0");
        write(root, "class/powercap/package-0/power_uw", "");
    }

    fn profile_only(root: &Path) {
        mkdir(root, "firmware/acpi");
        write(root, "firmware/acpi/platform_profile", "balanced\n");
    }

    fn mains_only(root: &Path) {
        mkdir(root, "class/power_supply/AC");
        write(root, "class/power_supply/AC/type", "Mains\n");
    }

    fn usb_only(root: &Path) {
        mkdir(root, "class/power_supply/USB");
        write(root, "class/power_supply/USB/type", "USB\n");
    }

    fn empty(root: &Path) {
        mkdir(root, "class");
    }

    type ProbeSetup = fn(&Path);
    type ProbeCase = (&'static str, ProbeSetup, bool);

    #[test]
    fn power_presence_probe_recognizes_static_sources() {
        let temp = tempfile::tempdir().unwrap();
        let cases: [ProbeCase; 7] = [
            ("battery only", battery_only, false),
            ("RAPL only", rapl_only, true),
            ("powercap power only", powercap_power_only, true),
            ("platform profile only", profile_only, true),
            ("mains only", mains_only, true),
            ("USB only", usb_only, true),
            ("nothing", empty, false),
        ];

        for (name, setup, expected) in cases {
            let root = temp.path().join(name.replace(' ', "-"));
            setup(&root);
            assert_eq!(probe(&root), expected, "unexpected probe result for {name}");
        }
    }

    #[test]
    fn power_presence_probe_stops_after_64_power_supply_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let supplies = root.join("class/power_supply");
        std::fs::create_dir_all(&supplies).unwrap();

        for index in 0..65 {
            std::fs::create_dir(supplies.join(format!("supply-{index:02}"))).unwrap();
        }

        let entries = std::fs::read_dir(&supplies)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let sixty_fifth = entries.get(64).expect("fixture has 65 supplies");
        std::fs::write(sixty_fifth.join("type"), "Mains\n").unwrap();

        assert!(!probe(root));
    }
}
