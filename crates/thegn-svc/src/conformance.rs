//! Seam conformance: the cross-seam invariants every provider probe must
//! hold, asserted over the whole registry at once so a new seam or provider
//! is checked the day it lands (`openspec/specs/provider-seams`).
//!
//! Per-seam tests own the *specific* behavior (kind coverage, ladder order,
//! caps); this module owns the *shape*: every report names a known seam and a
//! non-empty id, every `Unavailable` carries a reason, every reserved
//! selection says so, factories return `None` exactly for deactivated
//! accounts, and a missing binary is reported by name.

use thegn_core::seam::{Availability, ProbeReport};

/// Every seam the registry may report. A new seam is added here in the same
/// change that adds its `*_probes` — the conformance tests fail either way
/// if the two drift.
pub const KNOWN_SEAMS: &[&str] = &[
    "ci", "forge", "issues", "calendar", "git", "editor", "files", "sandbox", "media",
    "push",
];

/// Shape invariants for a batch of probe reports (typically
/// `seam::registry::probes(cfg)`); panics with the offending report.
pub fn assert_report_invariants(reports: &[ProbeReport]) {
    assert!(
        !reports.is_empty(),
        "a probe registry never reports nothing"
    );
    for r in reports {
        assert!(
            KNOWN_SEAMS.contains(&r.seam.as_str()),
            "unknown seam {:?} — add it to conformance::KNOWN_SEAMS: {r:?}",
            r.seam
        );
        assert!(!r.id.trim().is_empty(), "empty provider id: {r:?}");
        if let Availability::Unavailable(reason) = &r.availability {
            assert!(
                !reason.trim().is_empty(),
                "Unavailable without a reason: {r:?}"
            );
        }
        for n in &r.notes {
            assert!(!n.trim().is_empty(), "empty note line: {r:?}");
        }
    }
}

/// The seam names present in a batch.
pub fn seams_of(reports: &[ProbeReport]) -> std::collections::BTreeSet<String> {
    reports.iter().map(|r| r.seam.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::registry::{binary_availability, probes};
    use thegn_core::config::Config;
    use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};
    use thegn_core::config_issues::{IssueAccount, IssueProviderKind};
    use thegn_core::seam::Kind;

    #[test]
    fn default_config_reports_hold_the_shape() {
        let reports = probes(&Config::default());
        assert_report_invariants(&reports);
        // The always-on seams report even with nothing configured.
        for s in ["ci", "forge", "git", "editor", "files", "sandbox", "media"] {
            assert!(seams_of(&reports).contains(s), "missing {s}: {reports:?}");
        }
    }

    /// A config with every per-account seam populated reports one entry per
    /// account, and the shape still holds.
    #[test]
    fn fully_configured_registry_reports_every_account() {
        let mut cfg = Config::default();
        for (name, provider) in [
            ("lin", IssueProviderKind::Linear),
            ("gh", IssueProviderKind::Github),
            ("jira", IssueProviderKind::Jira),
            ("kaneo", IssueProviderKind::Kaneo),
        ] {
            cfg.issues.issue_accounts.push(IssueAccount {
                name: name.into(),
                provider,
                token: "env:THEGN_CONFORMANCE_UNSET".into(),
                ..Default::default()
            });
        }
        for (name, provider) in [
            ("f", CalendarProviderKind::Ics),
            ("u", CalendarProviderKind::IcsUrl),
            ("d", CalendarProviderKind::CalDav),
            ("c", CalendarProviderKind::Command),
        ] {
            cfg.calendar.accounts.push(CalendarAccount {
                name: name.into(),
                provider,
                ..Default::default()
            });
        }
        let reports = probes(&cfg);
        assert_report_invariants(&reports);
        assert_eq!(
            reports.iter().filter(|r| r.seam == "issues").count(),
            4,
            "one issues report per account: {reports:?}"
        );
        assert!(
            reports.iter().filter(|r| r.seam == "calendar").count() >= 4,
            "one calendar report per account: {reports:?}"
        );
    }

    /// Every reserved selection reports itself as reserved — the doctor
    /// explains *why* a seam is unavailable rather than omitting it.
    #[test]
    fn reserved_selections_report_reserved() {
        let mut cfg = Config::default();
        cfg.ci.provider = thegn_core::config_ci::CiProviderKind::Drone;
        cfg.media.backend = thegn_core::config::MediaBackendKind::Jellyfin;
        cfg.media.enabled = true;
        let reports = probes(&cfg);
        assert_report_invariants(&reports);
        for seam in ["ci", "media"] {
            let r = reports.iter().find(|r| r.seam == seam).unwrap();
            match &r.availability {
                Availability::Unavailable(reason) => {
                    assert!(reason.contains("reserved"), "{seam}: {reason}")
                }
                other => panic!("{seam} should be reserved-unavailable, got {other:?}"),
            }
        }
    }

    /// A CLI-backed provider whose binary is absent reports `Unavailable`
    /// naming the binary (the NotInstalled honesty rule).
    #[test]
    fn missing_binary_is_reported_by_name() {
        match binary_availability("thegn-conformance-no-such-binary") {
            Availability::Unavailable(reason) => {
                assert!(
                    reason.contains("thegn-conformance-no-such-binary"),
                    "{reason}"
                );
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// Factories return `None` exactly for deactivated (`provider = "none"`)
    /// accounts and `Some` for every implemented kind — the per-account
    /// analogue of `seam::kind_coverage`.
    #[test]
    fn account_factories_cover_every_kind() {
        for k in IssueProviderKind::ALL {
            let a = IssueAccount {
                provider: *k,
                ..Default::default()
            };
            let built = crate::issue::backend_from_account(&a, None).is_some();
            assert_eq!(
                built,
                *k != IssueProviderKind::None && !k.is_reserved(),
                "issue kind {k:?}"
            );
        }
        for k in CalendarProviderKind::ALL {
            let a = CalendarAccount {
                provider: *k,
                ..Default::default()
            };
            let built = crate::calendar::backend_from_account(&a).is_some();
            assert_eq!(
                built,
                *k != CalendarProviderKind::None && !k.is_reserved(),
                "calendar kind {k:?}"
            );
        }
    }

    /// The file-manager seam factory builds every implemented kind and returns
    /// `None` for reserved ones (the seam analogue of `kind_coverage`).
    #[test]
    fn file_manager_factory_covers_every_kind() {
        use thegn_core::file_manager::{DrawerKind, file_manager_for_kind};
        let cfg = Config::default();
        for k in DrawerKind::ALL {
            let built = file_manager_for_kind(*k, &cfg).is_some();
            assert_eq!(built, !k.is_reserved(), "drawer kind {k:?}");
        }
    }

    /// Two registry runs over the same config agree — probes are pure
    /// snapshots (cheap by contract, no network), so doctor output is stable.
    #[test]
    fn probes_are_deterministic() {
        let a = probes(&Config::default());
        let b = probes(&Config::default());
        let key = |rs: &[ProbeReport]| {
            rs.iter()
                .map(|r| format!("{}/{}", r.seam, r.id))
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&a), key(&b));
    }
}
