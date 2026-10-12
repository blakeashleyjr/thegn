//! Pure selection of the physical medium carrying the network path.
//!
//! Providers supply route and link facts; throughput is intentionally absent,
//! so a quiet connected interface can still describe the selected path.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetMedium {
    Ethernet,
    Wifi,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceFact {
    pub name: String,
    pub medium: NetMedium,
    pub up: bool,
    pub default_route: bool,
}

/// Choose a known physical default route, else one active physical link.
/// Ambiguous, tunnel, and unknown routes deliberately resolve to `Unknown`.
pub fn select_medium(facts: &[IfaceFact]) -> NetMedium {
    let defaults: Vec<_> = facts
        .iter()
        .filter(|fact| !is_loopback(&fact.name) && fact.default_route)
        .collect();
    if !defaults.is_empty() {
        if defaults.len() == 1 && is_physical(defaults[0].medium) {
            return defaults[0].medium;
        }
        return NetMedium::Unknown;
    }

    let active_unknown = facts
        .iter()
        .any(|fact| !is_loopback(&fact.name) && fact.up && fact.medium == NetMedium::Unknown);
    if active_unknown {
        return NetMedium::Unknown;
    }

    let active: Vec<_> = facts
        .iter()
        .filter(|fact| !is_loopback(&fact.name) && fact.up && is_physical(fact.medium))
        .collect();
    if active.len() == 1 {
        active[0].medium
    } else {
        NetMedium::Unknown
    }
}

fn is_loopback(name: &str) -> bool {
    name == "lo" || name.starts_with("lo")
}

fn is_physical(medium: NetMedium) -> bool {
    matches!(medium, NetMedium::Ethernet | NetMedium::Wifi)
}

#[cfg(test)]
mod tests {
    use super::{IfaceFact, NetMedium, select_medium};

    fn fact(name: &str, medium: NetMedium, up: bool, default_route: bool) -> IfaceFact {
        IfaceFact {
            name: name.into(),
            medium,
            up,
            default_route,
        }
    }

    #[test]
    fn medium_selection_uses_route_then_unambiguous_active_link() {
        let fixtures = [
            (
                "ethernet only",
                vec![fact("enp0s1", NetMedium::Ethernet, true, false)],
                NetMedium::Ethernet,
            ),
            (
                "wifi only",
                vec![fact("wlan0", NetMedium::Wifi, true, false)],
                NetMedium::Wifi,
            ),
            (
                "ethernet default among both",
                vec![
                    fact("enp0s1", NetMedium::Ethernet, true, true),
                    fact("wlan0", NetMedium::Wifi, true, false),
                ],
                NetMedium::Ethernet,
            ),
            (
                "wifi default among both",
                vec![
                    fact("enp0s1", NetMedium::Ethernet, true, false),
                    fact("wlan0", NetMedium::Wifi, true, true),
                ],
                NetMedium::Wifi,
            ),
            (
                "tied default routes are ambiguous",
                vec![
                    fact("enp0s1", NetMedium::Ethernet, true, true),
                    fact("wlan0", NetMedium::Wifi, true, true),
                ],
                NetMedium::Unknown,
            ),
            (
                "unknown VPN default",
                vec![
                    fact("wg0", NetMedium::Unknown, true, true),
                    fact("enp0s1", NetMedium::Ethernet, true, false),
                ],
                NetMedium::Unknown,
            ),
            (
                "multiple active without route",
                vec![
                    fact("enp0s1", NetMedium::Ethernet, true, false),
                    fact("wlan0", NetMedium::Wifi, true, false),
                ],
                NetMedium::Unknown,
            ),
            (
                "loopback only",
                vec![fact("lo", NetMedium::Unknown, true, true)],
                NetMedium::Unknown,
            ),
            (
                "unknown medium",
                vec![fact("enp2s0", NetMedium::Unknown, true, false)],
                NetMedium::Unknown,
            ),
            (
                "unknown active link makes fallback ambiguous",
                vec![
                    fact("enp0s1", NetMedium::Ethernet, true, false),
                    fact("wg0", NetMedium::Unknown, true, false),
                ],
                NetMedium::Unknown,
            ),
            // Traffic is deliberately not part of an IfaceFact: a quiet
            // default-route link must still win based on route and link facts.
            (
                "quiet default route",
                vec![fact("wlan0", NetMedium::Wifi, true, true)],
                NetMedium::Wifi,
            ),
        ];

        for (name, facts, expected) in fixtures {
            assert_eq!(select_medium(&facts), expected, "{name}");
        }
    }
}
