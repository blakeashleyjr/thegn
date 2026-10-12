use super::{WINDOWS_TZ_ALIASES, resolve_tzid, resolve_zone};

#[test]
fn windows_alias_targets_are_valid_iana_zones() {
    for (alias, target) in WINDOWS_TZ_ALIASES {
        assert!(
            resolve_zone(target).is_some(),
            "invalid alias target: {target}"
        );
        assert_eq!(
            resolve_tzid(alias).map(|zone| zone.name()),
            Some(*target),
            "alias does not resolve to its target: {alias}"
        );
    }
}

#[test]
fn resolves_windows_aliases_and_vendor_paths_but_not_unknown_names() {
    assert_eq!(
        resolve_tzid("Eastern Standard Time").map(|zone| zone.name()),
        Some("America/New_York")
    );
    assert_eq!(
        resolve_tzid("W. Europe Standard Time").map(|zone| zone.name()),
        Some("Europe/Berlin")
    );
    assert_eq!(
        resolve_tzid("/mozilla.org/20050126_1/America/New_York").map(|zone| zone.name()),
        Some("America/New_York")
    );
    assert_eq!(resolve_tzid("Nowhere/Land"), None);
    assert_eq!(resolve_tzid("Mars/Olympus_Mons"), None);
}
