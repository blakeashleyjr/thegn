//! Linux sysfs interface classification and platform-neutral aggregation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{IfaceFact, MediumSelection, NetMedium, select_medium_explained};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IfaceKind {
    Loopback,
    Ethernet,
    Wifi,
    Bond,
    Bridge,
    Vlan,
    Veth,
    Wireguard,
    Tun,
    Other,
    #[default]
    Unknown,
}

impl IfaceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Loopback => "lo",
            Self::Ethernet => "eth",
            Self::Wifi => "wifi",
            Self::Bond => "bond",
            Self::Bridge => "bridge",
            Self::Vlan => "vlan",
            Self::Veth => "veth",
            Self::Wireguard => "wg",
            Self::Tun => "tun",
            Self::Other => "other",
            Self::Unknown => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetIface {
    pub name: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub kind: IfaceKind,
    pub up: Option<bool>,
    pub physical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub(crate) struct IfaceClass {
    pub kind: IfaceKind,
    pub bond_slave: bool,
    pub has_device: bool,
    pub medium: NetMedium,
}

#[cfg_attr(not(any(test, target_os = "linux")), expect(dead_code))]
pub(crate) fn classify_at(root: &Path, name: &str) -> IfaceClass {
    let iface = root.join(name);
    let kind_text = std::fs::read_to_string(iface.join("type")).unwrap_or_default();
    let dev_type = std::fs::read_to_string(iface.join("uevent"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("DEVTYPE="))
        .unwrap_or_default()
        .to_owned();
    let has_device = iface.join("device").exists();
    let wireless = iface.join("wireless").exists() || iface.join("phy80211").exists();
    let bond_slave = iface.join("bonding_slave").exists();
    let kind = if name == "lo" || kind_text.trim() == "772" {
        IfaceKind::Loopback
    } else if wireless || dev_type == "wlan" {
        IfaceKind::Wifi
    } else if dev_type == "wireguard" {
        IfaceKind::Wireguard
    } else if iface.join("tun_flags").exists() {
        IfaceKind::Tun
    } else if iface.join("bridge").exists() || dev_type == "bridge" {
        IfaceKind::Bridge
    } else if iface.join("bonding").exists() || dev_type == "bond" {
        IfaceKind::Bond
    } else if dev_type == "vlan" {
        IfaceKind::Vlan
    } else if !has_device && name.starts_with("veth") {
        IfaceKind::Veth
    } else if has_device && kind_text.trim() == "1" {
        IfaceKind::Ethernet
    } else if has_device {
        IfaceKind::Other
    } else {
        IfaceKind::Unknown
    };
    let medium = if wireless {
        NetMedium::Wifi
    } else if kind_text.trim() == "1" && has_device {
        NetMedium::Ethernet
    } else {
        NetMedium::Unknown
    };
    IfaceClass {
        kind,
        bond_slave,
        has_device,
        medium,
    }
}

#[cfg_attr(not(any(test, target_os = "linux")), expect(dead_code))]
pub(crate) fn link_state_at(root: &Path, name: &str) -> (bool, bool) {
    let iface = root.join(name);
    let operstate = std::fs::read_to_string(iface.join("operstate")).unwrap_or_default();
    let operstate_up = operstate.trim() == "up";
    let flags_up = std::fs::read_to_string(iface.join("flags"))
        .ok()
        .and_then(|flags| {
            let flags = flags.trim().trim_start_matches("0x");
            u32::from_str_radix(flags, 16).ok()
        })
        .is_some_and(|flags| flags & 0x1 != 0);
    (
        operstate_up,
        operstate_up || (operstate.trim() == "unknown" && flags_up),
    )
}

#[cfg_attr(not(any(test, target_os = "linux")), expect(dead_code))]
pub(crate) fn probe_links_at(
    root: &Path,
    routes: &[String],
    cache: &mut BTreeMap<String, IfaceClass>,
) -> (MediumSelection, BTreeMap<String, bool>) {
    let mut available = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        available.extend(
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned()),
        );
    }
    cache.retain(|name, _| available.contains(name));
    let mut names = available.clone();
    names.extend(routes.iter().cloned());
    let mut up = BTreeMap::new();
    let facts = names
        .into_iter()
        .map(|name| {
            let class = *cache
                .entry(name.clone())
                .or_insert_with(|| classify_at(root, &name));
            let (operstate_up, display_up) = link_state_at(root, &name);
            up.insert(name.clone(), display_up);
            IfaceFact {
                default_route: routes.iter().any(|route| route == &name),
                name,
                medium: class.medium,
                up: operstate_up,
            }
        })
        .collect::<Vec<_>>();
    (select_medium_explained(&facts), up)
}

pub(crate) fn aggregate<'a>(
    readings: impl Iterator<Item = (&'a str, u64, u64)>,
    classes: &BTreeMap<String, IfaceClass>,
    up: &BTreeMap<String, bool>,
    classified: bool,
) -> (Vec<NetIface>, (u64, u64), (u64, u64)) {
    let mut rows = Vec::new();
    let mut physical = (0u64, 0u64);
    let mut all = (0u64, 0u64);
    for (name, rx_bps, tx_bps) in readings {
        if name == "lo" || name.starts_with("lo") {
            continue;
        }
        all.0 += rx_bps;
        all.1 += tx_bps;
        let class = classes.get(name).copied();
        let kind = if classified {
            class.map_or(IfaceKind::Unknown, |class| class.kind)
        } else {
            IfaceKind::Unknown
        };
        let is_physical = if !classified {
            true
        } else {
            class.is_some_and(|class| {
                matches!(
                    class.kind,
                    IfaceKind::Ethernet | IfaceKind::Wifi | IfaceKind::Other | IfaceKind::Bond
                ) && !class.bond_slave
                    && up.get(name) != Some(&false)
            })
        };
        if is_physical {
            physical.0 += rx_bps;
            physical.1 += tx_bps;
        }
        rows.push(NetIface {
            name: name.to_owned(),
            rx_bps,
            tx_bps,
            kind,
            up: if classified {
                up.get(name).copied()
            } else {
                None
            },
            physical: is_physical,
        });
    }
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    // Down links carry no traffic; if every physical link is down, the fallback still reports all links.
    if classified && !rows.iter().any(|row| row.physical) {
        physical = all;
    }
    (rows, physical, all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn iface(root: &Path, name: &str, kind: &str, devtype: &str, state: &str, flags: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("type"), kind).unwrap();
        std::fs::write(path.join("uevent"), format!("DEVTYPE={devtype}\n")).unwrap();
        std::fs::write(path.join("operstate"), state).unwrap();
        std::fs::write(path.join("flags"), flags).unwrap();
    }

    fn route(iface: &str) -> String {
        format!(
            "Iface Destination Gateway Flags RefCnt Use Metric Mask\n{iface} 00000000 00000000 0001 0 0 10 00000000\n"
        )
    }

    #[test]
    fn classifies_physical_vpn_idle_down_and_loopback_links() {
        let temp = root();
        let root = temp.path();
        iface(root, "wlan0", "1", "wlan", "up", "0x1003");
        std::fs::create_dir(root.join("wlan0/device")).unwrap();
        std::fs::create_dir(root.join("wlan0/wireless")).unwrap();
        iface(
            root,
            "wg0-mullvad",
            "65534",
            "wireguard",
            "unknown",
            "0x1091",
        );
        iface(root, "enp95s0", "1", "", "down", "0x1003");
        std::fs::create_dir(root.join("enp95s0/device")).unwrap();
        iface(root, "lo", "772", "", "unknown", "0x9");
        let routes = crate::sample::parse_default_route(&route("wlan0"));
        let (selection, up) = probe_links_at(root, &routes, &mut BTreeMap::new());
        assert_eq!(selection.medium, NetMedium::Wifi);
        let mut classes = BTreeMap::new();
        for name in ["wlan0", "wg0-mullvad", "enp95s0", "lo"] {
            classes.insert(name.to_owned(), classify_at(root, name));
        }
        let (rows, physical, all) = aggregate(
            [
                ("wlan0", 10, 20),
                ("wg0-mullvad", 30, 40),
                ("enp95s0", 0, 0),
                ("lo", 999, 999),
            ]
            .into_iter(),
            &classes,
            &up,
            true,
        );
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["enp95s0", "wg0-mullvad", "wlan0"]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.physical)
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["wlan0"]
        );
        assert_eq!((physical, all), ((10, 20), (40, 60)));
        let down = rows.iter().find(|row| row.name == "enp95s0").unwrap();
        assert_eq!(down.up, Some(false));
        let vpn = rows.iter().find(|row| row.name == "wg0-mullvad").unwrap();
        assert_eq!(vpn.kind, IfaceKind::Wireguard);
        assert_eq!(vpn.up, Some(true));
    }

    #[test]
    fn bond_slave_and_virtual_links_do_not_double_count_and_empty_physical_falls_back() {
        let temp = root();
        let root = temp.path();
        iface(root, "bond0", "1", "bond", "up", "0x1003");
        std::fs::create_dir(root.join("bond0/bonding")).unwrap();
        for name in ["eth0", "eth1"] {
            iface(root, name, "1", "", "up", "0x1003");
            std::fs::create_dir(root.join(name).join("device")).unwrap();
            std::fs::create_dir(root.join(name).join("bonding_slave")).unwrap();
        }
        iface(root, "docker0", "1", "bridge", "up", "0x1003");
        std::fs::create_dir(root.join("docker0/bridge")).unwrap();
        iface(root, "vethX", "1", "", "up", "0x1003");
        iface(root, "tailscale0", "65534", "", "unknown", "0x1003");
        std::fs::write(root.join("tailscale0/tun_flags"), "1").unwrap();
        let names = ["bond0", "eth0", "eth1", "docker0", "vethX", "tailscale0"];
        let classes = names
            .into_iter()
            .map(|name| (name.to_owned(), classify_at(root, name)))
            .collect();
        let readings = [
            ("bond0", 1, 1),
            ("eth0", 2, 2),
            ("eth1", 3, 3),
            ("docker0", 4, 4),
            ("vethX", 5, 5),
            ("tailscale0", 6, 6),
        ];
        let (rows, physical, all) =
            aggregate(readings.into_iter(), &classes, &BTreeMap::new(), true);
        assert_eq!(
            rows.iter()
                .filter(|row| row.physical)
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["bond0"]
        );
        assert!(
            rows.iter()
                .filter(|row| ["docker0", "vethX", "tailscale0"].contains(&row.name.as_str()))
                .all(|row| !row.physical)
        );
        assert_eq!((physical, all), ((1, 1), (21, 21)));
        let (fallback_rows, fallback, fallback_all) = aggregate(
            [("vethX", 7, 8)].into_iter(),
            &classes,
            &BTreeMap::new(),
            true,
        );
        assert!(!fallback_rows[0].physical);
        assert_eq!(fallback, fallback_all);
        assert_eq!(fallback, (7, 8));

        let netns_root = tempfile::tempdir().unwrap();
        let root = netns_root.path();
        iface(root, "eth0", "1", "", "up", "0x1003");
        let classes = BTreeMap::from([("eth0".to_owned(), classify_at(root, "eth0"))]);
        assert_eq!(classes["eth0"].kind, IfaceKind::Unknown);
        assert!(!classes["eth0"].has_device);
        let (netns_rows, netns_physical, netns_all) = aggregate(
            [("eth0", 9, 12)].into_iter(),
            &classes,
            &BTreeMap::from([("eth0".to_owned(), true)]),
            true,
        );
        assert!(!netns_rows[0].physical);
        assert_eq!(netns_physical, netns_all);
        assert_eq!(netns_physical, (9, 12));
    }

    #[test]
    fn cached_classification_keeps_medium_semantics_and_drops_removed_interfaces() {
        let temp = root();
        let root = temp.path();
        iface(root, "wlan0", "1", "", "up", "0x1003");
        std::fs::create_dir(root.join("wlan0/device")).unwrap();
        std::fs::create_dir(root.join("wlan0/wireless")).unwrap();
        iface(root, "wg0", "65534", "wireguard", "unknown", "0x1091");
        let mut cache = BTreeMap::new();
        let routes = crate::sample::parse_default_route(&route("wlan0"));
        let (wifi, up) = probe_links_at(root, &routes, &mut cache);
        assert_eq!(wifi.reason, crate::MediumReason::DefaultRoute);
        assert_eq!(wifi.medium, NetMedium::Wifi);
        assert!(up["wg0"]);
        let routes = crate::sample::parse_default_route(&route("wg0"));
        let wg = probe_links_at(root, &routes, &mut cache).0;
        assert_eq!(wg.reason, crate::MediumReason::DefaultRouteNotPhysical);
        std::fs::remove_dir_all(root.join("wlan0/wireless")).unwrap();
        let routes = crate::sample::parse_default_route(&route("wlan0"));
        assert_eq!(
            probe_links_at(root, &routes, &mut cache).0.medium,
            NetMedium::Wifi
        );
        std::fs::remove_dir_all(root.join("wlan0")).unwrap();
        let routes = crate::sample::parse_default_route(
            "Iface Destination Gateway Flags RefCnt Use Metric Mask\n",
        );
        probe_links_at(root, &routes, &mut cache);
        assert!(!cache.contains_key("wlan0"));
    }

    #[test]
    fn unclassified_platform_keeps_all_interfaces_physical_without_link_state() {
        let (rows, physical, all) = aggregate(
            [("eth0", 4, 8), ("wg0", 2, 3), ("lo", 100, 100)].into_iter(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            false,
        );
        assert!(rows.iter().all(|row| row.physical && row.up.is_none()));
        assert_eq!(physical, all);
        assert_eq!(all, (6, 11));
    }
}
