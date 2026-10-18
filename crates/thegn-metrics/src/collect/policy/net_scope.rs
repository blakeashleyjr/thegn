//! Pure network-interface classification and aggregation scope policy.

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IfaceFacts {
    pub type_code: Option<u32>,
    pub devtype: String,
    pub has_device: bool,
    pub wireless: bool,
    pub tun_flags: bool,
    pub bridge: bool,
    pub bonding: bool,
    pub bonding_slave: bool,
}

pub fn classify(name: &str, facts: &IfaceFacts) -> IfaceKind {
    if name == "lo" || facts.type_code == Some(772) {
        IfaceKind::Loopback
    } else if facts.wireless || facts.devtype == "wlan" {
        IfaceKind::Wifi
    } else if facts.devtype == "wireguard" {
        IfaceKind::Wireguard
    } else if facts.tun_flags {
        IfaceKind::Tun
    } else if facts.bridge || facts.devtype == "bridge" {
        IfaceKind::Bridge
    } else if facts.bonding || facts.devtype == "bond" {
        IfaceKind::Bond
    } else if facts.devtype == "vlan" {
        IfaceKind::Vlan
    } else if !facts.has_device && name.starts_with("veth") {
        IfaceKind::Veth
    } else if facts.has_device && facts.type_code == Some(1) {
        IfaceKind::Ethernet
    } else if facts.has_device {
        IfaceKind::Other
    } else {
        IfaceKind::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Physical,
    Vpn,
    Container,
    Virtual,
    Loopback,
}

impl Scope {
    pub const fn index(self) -> usize {
        match self {
            Self::Physical => 0,
            Self::Vpn => 1,
            Self::Container => 2,
            Self::Virtual => 3,
            Self::Loopback => 4,
        }
    }
}

pub const fn scope(kind: IfaceKind, bond_slave: bool) -> Scope {
    match kind {
        IfaceKind::Loopback => Scope::Loopback,
        IfaceKind::Wireguard | IfaceKind::Tun => Scope::Vpn,
        IfaceKind::Veth | IfaceKind::Bridge => Scope::Container,
        IfaceKind::Ethernet | IfaceKind::Wifi | IfaceKind::Bond | IfaceKind::Other
            if !bond_slave =>
        {
            Scope::Physical
        }
        _ => Scope::Virtual,
    }
}

#[cfg(test)]
mod tests {
    use super::{IfaceFacts, IfaceKind, Scope, classify, scope};

    #[test]
    fn classification_preserves_sysfs_decision_order() {
        let cases = [
            ("lo", IfaceFacts::default(), IfaceKind::Loopback),
            (
                "wlan0",
                IfaceFacts {
                    wireless: true,
                    devtype: "wireguard".into(),
                    ..Default::default()
                },
                IfaceKind::Wifi,
            ),
            (
                "wg0",
                IfaceFacts {
                    devtype: "wireguard".into(),
                    ..Default::default()
                },
                IfaceKind::Wireguard,
            ),
            (
                "tailscale0",
                IfaceFacts {
                    tun_flags: true,
                    ..Default::default()
                },
                IfaceKind::Tun,
            ),
            (
                "br0",
                IfaceFacts {
                    bridge: true,
                    ..Default::default()
                },
                IfaceKind::Bridge,
            ),
            (
                "bond0",
                IfaceFacts {
                    bonding: true,
                    ..Default::default()
                },
                IfaceKind::Bond,
            ),
            (
                "vlan0",
                IfaceFacts {
                    devtype: "vlan".into(),
                    ..Default::default()
                },
                IfaceKind::Vlan,
            ),
            ("veth0", IfaceFacts::default(), IfaceKind::Veth),
            (
                "eth0",
                IfaceFacts {
                    type_code: Some(1),
                    has_device: true,
                    ..Default::default()
                },
                IfaceKind::Ethernet,
            ),
            (
                "x0",
                IfaceFacts {
                    has_device: true,
                    ..Default::default()
                },
                IfaceKind::Other,
            ),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(classify(name, &facts), expected);
        }
    }

    #[test]
    fn scope_keeps_vlan_virtual_and_bond_slaves_out_of_physical_totals() {
        assert_eq!(scope(IfaceKind::Ethernet, false), Scope::Physical);
        assert_eq!(scope(IfaceKind::Bond, true), Scope::Virtual);
        assert_eq!(scope(IfaceKind::Vlan, false), Scope::Virtual);
        assert_eq!(scope(IfaceKind::Tun, false), Scope::Vpn);
        assert_eq!(scope(IfaceKind::Veth, false), Scope::Container);
        assert_eq!(scope(IfaceKind::Loopback, false), Scope::Loopback);
    }
}
