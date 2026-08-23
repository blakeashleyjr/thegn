//! The probe registry: every provider the loaded config selects, described.
//!
//! `thegn doctor` prints this as its "Providers" section. Probes are cheap by
//! contract (`which`, a config check) — never a network round-trip — so the
//! whole registry runs in milliseconds and is safe from a subcommand.
//!
//! Seams are added here as they adopt `thegn_core::seam`; today the registry
//! covers ci, forges, issues, calendar, git, sandbox and media. A reserved
//! selection reports [`ProbeReport::reserved`] so doctor explains *why* a seam
//! is unavailable rather than silently omitting it.

use thegn_core::config::Config;
use thegn_core::seam::{Availability, Kind, Probe, ProbeReport};

/// `Ready` when `bin` is on `PATH`, else `Unavailable` naming it.
pub fn binary_availability(bin: &str) -> Availability {
    match thegn_core::util::which_path(bin) {
        Some(path) => {
            let _ = path;
            Availability::Ready
        }
        None => Availability::Unavailable(format!("`{bin}` not found on PATH")),
    }
}

/// Collect every configured provider's probe.
pub fn probes(cfg: &Config) -> Vec<ProbeReport> {
    let mut out = Vec::new();
    out.extend(ci_probes(cfg));
    out.extend(forge_probes(cfg));
    out.extend(issue_probes(cfg));
    out.extend(calendar_probes(cfg));
    out.extend(git_probes(cfg));
    out.extend(sandbox_probes(cfg));
    out.extend(media_probes(cfg));
    out
}

fn ci_probes(cfg: &Config) -> Vec<ProbeReport> {
    use crate::ci::{client_for_system, system_for_kind};
    use thegn_core::config::CiProviderKind;
    let kind = cfg.ci.provider;
    if kind.is_reserved() {
        return vec![ProbeReport::reserved("ci", kind.as_str())];
    }
    match kind {
        CiProviderKind::None => {
            vec![ProbeReport::new("ci", "none", Availability::Ready).note("CI inspection disabled")]
        }
        CiProviderKind::Auto => vec![
            ProbeReport::new("ci", "auto", Availability::Ready)
                .note("resolved per worktree from the origin host / CI files"),
        ],
        explicit => match system_for_kind(explicit).and_then(client_for_system) {
            Some(c) => vec![c.probe()],
            None => vec![ProbeReport::new(
                "ci",
                explicit.as_str(),
                Availability::Unavailable("no client for this kind".into()),
            )],
        },
    }
}

fn forge_probes(cfg: &Config) -> Vec<ProbeReport> {
    crate::forge::probes(cfg)
}

fn issue_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::config_issues::IssueProviderKind;
    cfg.issues
        .active_accounts()
        .into_iter()
        .map(|a| {
            let id = format!("{}:{}", a.provider.as_str(), a.name);
            if a.provider.is_reserved() {
                return ProbeReport::reserved("issues", &id);
            }
            match a.provider {
                IssueProviderKind::None => ProbeReport::new("issues", id, Availability::Ready),
                IssueProviderKind::Github => {
                    ProbeReport::new("issues", id, binary_availability("gh"))
                }
                IssueProviderKind::Linear | IssueProviderKind::Jira | IssueProviderKind::Kaneo => {
                    let avail =
                        if a.token.trim().is_empty() && a.provider != IssueProviderKind::Kaneo {
                            Availability::Unavailable("no token configured".into())
                        } else {
                            Availability::Ready
                        };
                    ProbeReport::new("issues", id, avail)
                        .note("network provider; not probed offline")
                }
            }
        })
        .collect()
}

fn calendar_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::config_calendar::CalendarProviderKind;
    cfg.calendar
        .active_accounts()
        .into_iter()
        .map(|a| {
            let id = format!("{}:{}", a.provider.as_str(), a.name);
            if a.provider.is_reserved() {
                return ProbeReport::reserved("calendar", &id);
            }
            match a.provider {
                CalendarProviderKind::None => ProbeReport::new("calendar", id, Availability::Ready),
                CalendarProviderKind::Ics => {
                    let ok = !a.path.trim().is_empty() && std::path::Path::new(&a.path).exists();
                    ProbeReport::new(
                        "calendar",
                        id,
                        if ok {
                            Availability::Ready
                        } else {
                            Availability::Unavailable(format!("ics file not found: {}", a.path))
                        },
                    )
                }
                CalendarProviderKind::IcsUrl | CalendarProviderKind::CalDav => {
                    ProbeReport::new("calendar", id, Availability::Ready)
                        .note("network provider; not probed offline")
                }
                CalendarProviderKind::Command => {
                    let bin = a.command.first().cloned().unwrap_or_default();
                    let avail = if bin.is_empty() {
                        Availability::Unavailable("no command configured".into())
                    } else if bin.contains('/') {
                        if std::path::Path::new(&bin).exists() {
                            Availability::Ready
                        } else {
                            Availability::Unavailable(format!("command not found: {bin}"))
                        }
                    } else {
                        binary_availability(&bin)
                    };
                    ProbeReport::new("calendar", id, avail)
                }
            }
        })
        .collect()
}

fn git_probes(cfg: &Config) -> Vec<ProbeReport> {
    let selected = crate::git::backend_for(cfg.git.backend);
    let mut out = vec![
        selected
            .probe()
            .note(format!("[git] backend = {}", cfg.git.backend.as_str())),
    ];
    // The write engine is always the CLI; show it when it isn't the selection.
    if selected.probe().id != "cli" {
        out.push(crate::git::CliGit.probe().note("writes (always)"));
    }
    out
}

fn sandbox_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::config::SandboxBackend;
    use thegn_core::sandbox::Backend;
    if !cfg.sandbox.enabled {
        return vec![
            ProbeReport::new("sandbox", "disabled", Availability::Ready)
                .note("[sandbox] enabled = false"),
        ];
    }
    let kind = cfg.sandbox.backend;
    if kind.is_reserved() {
        return vec![ProbeReport::reserved("sandbox", kind.as_str())];
    }
    match kind {
        SandboxBackend::Auto => {
            // Report every chain entry so doctor shows what `auto` can pick.
            cfg.sandbox
                .backend_chain
                .iter()
                .map(|name| {
                    match SandboxBackend::from_str_validated(name)
                        .ok()
                        .and_then(Backend::from_config)
                    {
                        Some(b) => b.probe().note("candidate in [sandbox] backend_chain"),
                        None => ProbeReport::new(
                            "sandbox",
                            name.clone(),
                            Availability::Unavailable(
                                "unknown or reserved backend_chain entry".into(),
                            ),
                        ),
                    }
                })
                .collect()
        }
        explicit => match Backend::from_config(explicit) {
            Some(b) => vec![b.probe()],
            None => vec![ProbeReport::new(
                "sandbox",
                explicit.as_str(),
                Availability::Unavailable("no backend for this kind".into()),
            )],
        },
    }
}

fn media_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::config::MediaBackendKind;
    if !cfg.media.enabled {
        return vec![
            ProbeReport::new("media", "disabled", Availability::Ready)
                .note("[media] enabled = false"),
        ];
    }
    let kind = cfg.media.backend;
    if kind.is_reserved() {
        return vec![ProbeReport::reserved("media", kind.as_str())];
    }
    let (avail, note) = match kind {
        MediaBackendKind::Auto => (
            Availability::Ready,
            "resolved at runtime (mpris → playerctl → mpv/mpd)",
        ),
        MediaBackendKind::None => (Availability::Ready, "media control disabled"),
        MediaBackendKind::Mpris => (
            if cfg!(target_os = "linux") {
                Availability::Ready
            } else {
                Availability::Unavailable("MPRIS is Linux-only".into())
            },
            "native D-Bus, falls back to `playerctl`",
        ),
        MediaBackendKind::Mpv => (Availability::Ready, "JSON IPC socket; probed on connect"),
        MediaBackendKind::Mpd => (Availability::Ready, "MPD protocol; probed on connect"),
        MediaBackendKind::Smtc => (
            if cfg!(windows) {
                Availability::Ready
            } else {
                Availability::Unavailable("SMTC is Windows-only".into())
            },
            "",
        ),
        MediaBackendKind::AppleScript => (
            if cfg!(target_os = "macos") {
                binary_availability("osascript")
            } else {
                Availability::Unavailable("AppleScript is macOS-only".into())
            },
            "",
        ),
        // Reserved kinds returned above; exhaustive so a new kind is a compile error.
        MediaBackendKind::Jellyfin => return vec![ProbeReport::reserved("media", "jellyfin")],
    };
    let r = ProbeReport::new("media", kind.as_str(), avail);
    vec![if note.is_empty() { r } else { r.note(note) }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_reports_every_seam_once_and_nothing_reserved() {
        let cfg = Config::default();
        let reports = probes(&cfg);
        let seams: std::collections::BTreeSet<&str> =
            reports.iter().map(|r| r.seam.as_str()).collect();
        for s in ["ci", "forge", "git", "sandbox", "media"] {
            assert!(seams.contains(s), "missing seam {s}: {reports:?}");
        }
        assert!(
            reports
                .iter()
                .all(|r| !matches!(&r.availability, Availability::Unavailable(x) if x.contains("reserved"))),
            "{reports:?}"
        );
        // JSON shape doctor prints.
        let v = serde_json::to_value(&reports).unwrap();
        assert!(v[0]["seam"].is_string() && v[0]["availability"]["state"].is_string());
    }

    #[test]
    fn reserved_selections_are_reported_as_reserved() {
        let mut cfg = Config::default();
        cfg.ci.provider = thegn_core::config::CiProviderKind::Drone;
        cfg.media.enabled = true;
        cfg.media.backend = thegn_core::config::MediaBackendKind::Jellyfin;
        cfg.sandbox.enabled = true;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Wsl;
        cfg.forges.push(thegn_core::config_forge::ForgeConfig {
            name: "codeberg".into(),
            kind: thegn_core::config_forge::ForgeKind::Forgejo,
            ..Default::default()
        });
        let reports = probes(&cfg);
        for (seam, id) in [
            ("ci", "drone"),
            ("media", "jellyfin"),
            ("sandbox", "wsl"),
            ("forge", "forgejo:codeberg"),
        ] {
            let r = reports
                .iter()
                .find(|r| r.seam == seam && r.id == id)
                .unwrap_or_else(|| panic!("{seam}/{id} missing: {reports:?}"));
            match &r.availability {
                Availability::Unavailable(why) => assert!(why.contains("reserved"), "{why}"),
                other => panic!("{seam}/{id}: {other:?}"),
            }
        }
    }

    #[test]
    fn explicit_selections_probe_binaries() {
        let mut cfg = Config::default();
        cfg.ci.provider = thegn_core::config::CiProviderKind::Gitlab;
        cfg.sandbox.enabled = true;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Docker;
        cfg.issues
            .issue_accounts
            .push(thegn_core::config_issues::IssueAccount {
                name: "work".into(),
                provider: thegn_core::config_issues::IssueProviderKind::Linear,
                enabled: true,
                token: String::new(),
                ..Default::default()
            });
        let reports = probes(&cfg);
        let ci = reports.iter().find(|r| r.seam == "ci").unwrap();
        assert_eq!(ci.id, "gitlab");
        let sb = reports.iter().find(|r| r.seam == "sandbox").unwrap();
        assert_eq!(sb.id, "docker");
        let iss = reports.iter().find(|r| r.seam == "issues").unwrap();
        assert_eq!(iss.id, "linear:work");
        assert!(matches!(&iss.availability, Availability::Unavailable(w) if w.contains("token")));
    }

    #[test]
    fn binary_availability_names_the_missing_binary() {
        match binary_availability("thegn-definitely-not-a-binary-xyz") {
            Availability::Unavailable(w) => {
                assert!(w.contains("thegn-definitely-not-a-binary-xyz"))
            }
            other => panic!("{other:?}"),
        }
    }
}
