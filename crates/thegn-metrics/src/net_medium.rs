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

/// Why the selector chose a medium or fell back to the generic network glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediumReason {
    DefaultRoute,
    SoleActiveLink,
    NoCandidates,
    AmbiguousDefaultRoutes,
    DefaultRouteNotPhysical,
    ActiveUnknownLink,
    MultipleActiveLinks,
    ProbeUnavailable,
}

impl MediumReason {
    /// Short stable wording for the doctor report.
    pub const fn word(self) -> &'static str {
        match self {
            Self::DefaultRoute => "default route",
            Self::SoleActiveLink => "sole active link",
            Self::NoCandidates => "no active link",
            Self::AmbiguousDefaultRoutes => "ambiguous default routes",
            Self::DefaultRouteNotPhysical => "default route is not a known physical link",
            Self::ActiveUnknownLink => "active link has unknown medium",
            Self::MultipleActiveLinks => "multiple active links",
            Self::ProbeUnavailable => "probe unavailable",
        }
    }
}

/// Selected path information, including enough context to explain generic fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediumSelection {
    pub medium: NetMedium,
    pub iface: Option<String>,
    pub reason: MediumReason,
}

impl MediumSelection {
    /// Represent a platform or permission failure without inventing a path.
    pub fn probe_unavailable() -> Self {
        Self {
            medium: NetMedium::Unknown,
            iface: None,
            reason: MediumReason::ProbeUnavailable,
        }
    }
}

/// Choose a known physical default route, else one active physical link.
/// Ambiguous, tunnel, and unknown routes deliberately resolve to `Unknown`.
pub fn select_medium(facts: &[IfaceFact]) -> NetMedium {
    select_medium_explained(facts).medium
}

/// Choose the path medium and retain its interface and selection rationale.
pub fn select_medium_explained(facts: &[IfaceFact]) -> MediumSelection {
    let defaults: Vec<_> = facts
        .iter()
        .filter(|fact| !is_loopback(&fact.name) && fact.default_route)
        .collect();
    if !defaults.is_empty() {
        if defaults.len() > 1 {
            return selection(
                NetMedium::Unknown,
                None,
                MediumReason::AmbiguousDefaultRoutes,
            );
        }
        let route = defaults[0];
        return if is_physical(route.medium) {
            selection(
                route.medium,
                Some(route.name.clone()),
                MediumReason::DefaultRoute,
            )
        } else {
            selection(
                NetMedium::Unknown,
                Some(route.name.clone()),
                MediumReason::DefaultRouteNotPhysical,
            )
        };
    }

    let active_unknown = facts
        .iter()
        .any(|fact| !is_loopback(&fact.name) && fact.up && fact.medium == NetMedium::Unknown);
    if active_unknown {
        return selection(NetMedium::Unknown, None, MediumReason::ActiveUnknownLink);
    }

    let active: Vec<_> = facts
        .iter()
        .filter(|fact| !is_loopback(&fact.name) && fact.up && is_physical(fact.medium))
        .collect();
    if active.len() == 1 {
        selection(
            active[0].medium,
            Some(active[0].name.clone()),
            MediumReason::SoleActiveLink,
        )
    } else if active.len() > 1 {
        selection(NetMedium::Unknown, None, MediumReason::MultipleActiveLinks)
    } else {
        selection(NetMedium::Unknown, None, MediumReason::NoCandidates)
    }
}

fn selection(medium: NetMedium, iface: Option<String>, reason: MediumReason) -> MediumSelection {
    MediumSelection {
        medium,
        iface,
        reason,
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
    use super::{IfaceFact, MediumReason, NetMedium, select_medium, select_medium_explained};

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

    #[test]
    fn explained_selection_reports_interface_and_each_reason() {
        let fixtures = [
            (
                vec![fact("wlan0", NetMedium::Wifi, true, true)],
                NetMedium::Wifi,
                Some("wlan0"),
                MediumReason::DefaultRoute,
            ),
            (
                vec![fact("eth0", NetMedium::Ethernet, true, false)],
                NetMedium::Ethernet,
                Some("eth0"),
                MediumReason::SoleActiveLink,
            ),
            (vec![], NetMedium::Unknown, None, MediumReason::NoCandidates),
            (
                vec![
                    fact("eth0", NetMedium::Ethernet, true, true),
                    fact("wlan0", NetMedium::Wifi, true, true),
                ],
                NetMedium::Unknown,
                None,
                MediumReason::AmbiguousDefaultRoutes,
            ),
            (
                vec![fact("wg0", NetMedium::Unknown, true, true)],
                NetMedium::Unknown,
                Some("wg0"),
                MediumReason::DefaultRouteNotPhysical,
            ),
            (
                vec![fact("wg0", NetMedium::Unknown, true, false)],
                NetMedium::Unknown,
                None,
                MediumReason::ActiveUnknownLink,
            ),
            (
                vec![
                    fact("eth0", NetMedium::Ethernet, true, false),
                    fact("wlan0", NetMedium::Wifi, true, false),
                ],
                NetMedium::Unknown,
                None,
                MediumReason::MultipleActiveLinks,
            ),
        ];

        for (facts, medium, iface, reason) in fixtures {
            let selected = select_medium_explained(&facts);
            assert_eq!(selected.medium, medium);
            assert_eq!(selected.iface.as_deref(), iface);
            assert_eq!(selected.reason, reason);
            assert_eq!(select_medium(&facts), selected.medium);
        }
        assert_eq!(
            MediumSelection::probe_unavailable(),
            super::MediumSelection {
                medium: NetMedium::Unknown,
                iface: None,
                reason: MediumReason::ProbeUnavailable,
            }
        );
    }
}
