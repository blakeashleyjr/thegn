//! The probe registry: every provider the loaded config selects, described.
//!
//! `thegn doctor` prints this as its "Providers" section. Probes are cheap by
//! contract (`which`, a config check) — never a network round-trip — so the
//! whole registry runs in milliseconds and is safe from a subcommand.
//!
//! Seams are added here as they adopt `thegn_core::seam`; today the registry
//! covers ci, forges, issues, calendar, weather, git, editor, files (the drawer
//! file manager), sandbox and media. A reserved selection reports
//! [`ProbeReport::reserved`] so doctor explains *why* a seam is unavailable
//! rather than silently omitting it.

use thegn_core::config::Config;
use thegn_core::seam::{Availability, Kind, Probe, ProbeReport};

/// `Ready` when `bin` is on `PATH`, else `Unavailable` naming it.
pub fn binary_availability(bin: &str) -> Availability {
    match thegn_core::util::which_path(bin) {
        Some(_) => Availability::Ready,
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
    out.extend(weather_probes(cfg));
    out.extend(git_probes(cfg));
    out.extend(editor_probes(cfg));
    out.extend(file_manager_probes(cfg));
    out.extend(sandbox_probes(cfg));
    out.extend(media_probes(cfg));
    out.extend(push_probes(cfg));
    out.extend(structural_probes(cfg));
    out.extend(host_discovery_probes(cfg));
    out
}

/// The push-to-phone channel: the outbound provider (`ntfy` / reserved kinds)
/// and the inbound command inbox's status. Both are cheap config checks (no
/// network round-trip), matching the probe contract.
fn push_probes(cfg: &Config) -> Vec<ProbeReport> {
    let p = &cfg.notifications.push;
    let mut out = Vec::new();
    // Outbound delivery channel.
    if p.kind.is_reserved() {
        out.push(ProbeReport::reserved("push", p.kind.as_str()));
    } else if let Some(provider) = crate::push::provider_for(p) {
        out.push(provider.probe());
    } else {
        out.push(ProbeReport::new(
            "push",
            p.kind.as_str(),
            Availability::Unavailable(
                "no [notifications.push] topic configured — outbound push off".into(),
            ),
        ));
    }
    // Inbound command inbox (a daemon feature).
    let inbox = &p.inbox;
    let inbox_report = if !inbox.enabled {
        ProbeReport::new("push", "inbox", Availability::Ready)
            .note("command inbox off ([notifications.push.inbox] enabled = false)")
    } else if let Some(reason) = inbox.startup_block_reason() {
        ProbeReport::new("push", "inbox", Availability::Unavailable(reason))
    } else {
        let n = inbox.allow_set().len();
        ProbeReport::new("push", "inbox", Availability::Ready)
            .note(format!(
                "command inbox on: {n} allowed capabilit{}",
                if n == 1 { "y" } else { "ies" }
            ))
            .note(format!("scope ceiling: {}", inbox.scopes.join(",")))
            .note("requires a running daemon ([daemon] enabled = true)")
    };
    out.push(inbox_report);
    out
}

/// The drawer's file-manager provider (`thegn_core::file_manager`). A directly
/// selected reserved kind is reported reserved (a config-file load remaps it to
/// the default with a warning; a programmatic selection can still hold it);
/// otherwise the selected provider's own probe (binary availability,
/// config-home mode, caps), with a note when the config is the ambiguous
/// `kind = "yazi"` beside a `command`.
fn file_manager_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::file_manager;
    if let Some(kind) = cfg.drawer.kind
        && kind.is_reserved()
    {
        return vec![ProbeReport::reserved("files", kind.as_str())];
    }
    let mut report = file_manager::file_manager_for(cfg).probe();
    if file_manager::ambiguous_yazi_command(cfg) {
        report = report.note(
            "[drawer] kind = \"yazi\" set beside a non-empty command; the command wins (custom) — pick one",
        );
    }
    vec![report]
}

/// The structural (AST) search & rewrite tier (`[search] structural`). Offline:
/// a `which` for the vendor binary. Reserved kinds report why they are
/// unavailable; `none` disables the tier.
fn structural_probes(cfg: &Config) -> Vec<ProbeReport> {
    use thegn_core::config::StructuralKind;
    let kind = cfg.search.structural;
    if kind.is_reserved() {
        return vec![ProbeReport::reserved("structural", kind.as_str())];
    }
    match kind {
        StructuralKind::None => vec![
            ProbeReport::new("structural", "none", Availability::Ready)
                .note("structural tier disabled ([search] structural = \"none\")"),
        ],
        StructuralKind::AstGrep => {
            let avail = if thegn_core::util::which_path("ast-grep").is_some()
                || thegn_core::util::which_path("sg").is_some()
            {
                Availability::Ready
            } else {
                Availability::Unavailable("`ast-grep`/`sg` not found on PATH".into())
            };
            vec![
                ProbeReport::new("structural", "ast-grep", avail)
                    .note("AST search/rewrite; rewrites apply via thegn's guarded write path"),
            ]
        }
        // Reserved kinds returned above.
        _ => Vec::new(),
    }
}

/// The inbound host-discovery seam: a reserved kind reports as such; the
/// implemented `tailnet` kind runs its (cheap, local) probe unless disabled.
fn host_discovery_probes(cfg: &Config) -> Vec<ProbeReport> {
    use crate::host_discovery::HostDiscovery;
    use thegn_core::config::HostDiscoveryKind;
    let kind = cfg.host_discovery.kind;
    if kind.is_reserved() {
        return vec![ProbeReport::reserved("host_discovery", kind.as_str())];
    }
    match kind {
        HostDiscoveryKind::Tailnet => {
            let tc = &cfg.host_discovery.tailnet;
            if !tc.enabled {
                return vec![
                    ProbeReport::new("host_discovery", "tailnet", Availability::Ready)
                        .note("[host_discovery.tailnet] enabled = false"),
                ];
            }
            vec![crate::host_discovery::TailnetDiscovery::new(tc.clone()).probe()]
        }
        // Reserved kinds returned above; exhaustive so a new kind is a compile error.
        HostDiscoveryKind::Mdns | HostDiscoveryKind::Consul => {
            vec![ProbeReport::reserved("host_discovery", kind.as_str())]
        }
    }
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
    use crate::issue::IssueCaps;
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
                IssueProviderKind::None => ProbeReport::new("issues", id, Availability::Ready)
                    .with_caps(&IssueCaps::default()),
                IssueProviderKind::Github => {
                    ProbeReport::new("issues", id, binary_availability("gh"))
                        .with_caps(&IssueCaps::default())
                }
                IssueProviderKind::Linear | IssueProviderKind::Jira | IssueProviderKind::Kaneo => {
                    let avail =
                        if a.token.trim().is_empty() && a.provider != IssueProviderKind::Kaneo {
                            Availability::Unavailable("no token configured".into())
                        } else {
                            Availability::Ready
                        };
                    let caps = if a.provider == IssueProviderKind::Kaneo {
                        IssueCaps {
                            comments: true,
                            labels: true,
                        }
                    } else {
                        IssueCaps::default()
                    };
                    ProbeReport::new("issues", id, avail)
                        .with_caps(&caps)
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

/// The weather seam. Nothing is reported while `[weather] enabled = false` —
/// an unconfigured optional feature is not a doctor finding, and reporting one
/// would put a row in every default `doctor` run for a feature nobody asked
/// for.
///
/// Offline by contract: the implemented backend is keyless and probing it
/// would be a network round trip, so this is a pure config read.
fn weather_probes(cfg: &Config) -> Vec<ProbeReport> {
    use crate::weather::wttr_in::PROVIDER_ID;
    use thegn_core::config_weather::WeatherProviderKind;
    let w = &cfg.weather;
    if !w.enabled {
        return Vec::new();
    }
    // A config *file* cannot currently reach this arm: a reserved `provider`
    // value warns and deserializes to `none` (design §6.3). It exists for shape
    // parity with the other seams and for a programmatically-built `Config`.
    if w.provider.is_reserved() {
        return vec![ProbeReport::reserved("weather", w.provider.as_str())];
    }
    match w.provider {
        WeatherProviderKind::None => vec![ProbeReport::new(
            "weather",
            "none",
            Availability::Unavailable("[weather] provider = \"none\" — nothing to fetch".into()),
        )],
        WeatherProviderKind::WttrIn => vec![
            // The id comes from the backend, so the vendor token is spelled in
            // exactly one file.
            ProbeReport::new("weather", PROVIDER_ID, Availability::Ready)
                .note("keyless; not probed offline")
                // The location itself is never printed — it is the one piece of
                // user data this feature handles.
                .note(if w.location.trim().is_empty() {
                    "location: inferred from request IP"
                } else {
                    "location: as configured"
                }),
        ],
        // Reserved kinds returned above; exhaustive so a new kind is a compile
        // error rather than a silently missing report.
        WeatherProviderKind::OpenMeteo | WeatherProviderKind::OpenWeatherMap => {
            vec![ProbeReport::reserved("weather", w.provider.as_str())]
        }
    }
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

fn editor_probes(cfg: &Config) -> Vec<ProbeReport> {
    let editor = thegn_core::editor::editor_for(cfg);
    let caps = editor.caps();
    let note = format!(
        "[editor] open_in = {}; line jump {}",
        cfg.editor.open_in.as_str(),
        if caps.line { "yes" } else { "no" }
    );
    vec![editor.probe().note(note)]
}

/// A sandbox backend's probe, enriched with the runtime-state truth
/// (`sandbox_support::classify`): "installed but not running" reports
/// `Degraded` with the remedy instead of masquerading as ready — the same
/// honesty rule the pane labels follow (`sandbox_truth`).
///
/// The row also carries the sandbox seam's container-events cap (THE-79):
/// a static property of the backend's profile-table row, attached before the
/// state classification so it rides every path out of here — an uninstalled
/// podman still *implements* events. Caps shape: `caps.events` is `true`
/// (implemented), the reservation reason (reserved), or `false` (no stream).
fn sandbox_backend_probe(b: thegn_core::sandbox::Backend) -> ProbeReport {
    use thegn_core::sandbox_events::EventsCap;
    use thegn_core::sandbox_support::{BackendState, classify, remedy};
    let base = b.probe();
    let base = match b.profile().events {
        EventsCap::Yes => {
            let id = b.events().map(|t| t.id()).unwrap_or(b.label());
            base.note(format!("events: exec+network audit ({id})"))
                .with_caps(&serde_json::json!({ "events": true }))
        }
        EventsCap::Reserved(reason) => base
            .note(format!("events: reserved — {reason}"))
            .with_caps(&serde_json::json!({ "events": reason })),
        EventsCap::No => base.with_caps(&serde_json::json!({ "events": false })),
    };
    let state = classify(
        b,
        &thegn_core::placement::Placement::Local,
        thegn_core::sandbox_backend::host_os(),
    );
    let availability = match state {
        BackendState::Ready => Availability::Ready,
        BackendState::NotRunning => Availability::Degraded("installed but not running".into()),
        BackendState::NotInstalled => return base, // which-based reason already right
        BackendState::Unsupported => Availability::Unavailable("not supported on this OS".into()),
        BackendState::Unreachable => Availability::Unavailable("runtime unreachable".into()),
    };
    let mut report = ProbeReport {
        availability,
        ..base
    };
    if let Some(r) = remedy(b, state) {
        report = report.note(r);
    }
    report
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
                        Some(b) => {
                            sandbox_backend_probe(b).note("candidate in [sandbox] backend_chain")
                        }
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
            Some(b) => vec![sandbox_backend_probe(b)],
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
        let mut cfg = Config::default();
        // Keep the probe hermetic: don't shell out to a real `tailscale` client
        // from a unit test (the disabled path still reports the seam).
        cfg.host_discovery.tailnet.enabled = false;
        let reports = probes(&cfg);
        let seams: std::collections::BTreeSet<&str> =
            reports.iter().map(|r| r.seam.as_str()).collect();
        for s in [
            "ci",
            "forge",
            "git",
            "editor",
            "files",
            "sandbox",
            "media",
            "host_discovery",
        ] {
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
    fn sandbox_report_carries_the_events_cap() {
        use thegn_core::sandbox_events::EventsCap;
        // The sandbox row reports the seam's container-events cap (THE-79):
        // podman implements the op (note + `caps.events == true`), docker is
        // reserved (the reason is both the note and the caps value), the
        // process wrappers have no event stream (`caps.events == false`, no
        // note). The bit is a static profile-table property, so every
        // assertion holds whether or not the runtime is installed here.
        let row = |backend| {
            let mut cfg = Config::default();
            cfg.sandbox.enabled = true;
            cfg.sandbox.backend = backend;
            let mut r = sandbox_probes(&cfg);
            crate::conformance::assert_report_invariants(&r);
            assert_eq!(r.len(), 1, "{r:?}");
            let r = r.pop().unwrap();
            assert_eq!(r.seam, "sandbox");
            r
        };

        // podman: implemented — one note naming the transport, caps true.
        let podman = row(thegn_core::config::SandboxBackend::Podman);
        assert_eq!(podman.id, "podman-rootless");
        let events_notes = || {
            podman
                .notes
                .iter()
                .filter(|n| n.starts_with("events:"))
                .count()
        };
        assert_eq!(events_notes(), 1, "{podman:?}");
        let note = podman
            .notes
            .iter()
            .find(|n| n.starts_with("events:"))
            .unwrap();
        assert!(note.contains("exec+network audit"), "{note}");
        assert_eq!(podman.caps["events"], serde_json::json!(true));

        // docker: reserved — the reason rides the note and is the caps value.
        let docker = row(thegn_core::config::SandboxBackend::Docker);
        let note = docker
            .notes
            .iter()
            .find(|n| n.starts_with("events:"))
            .expect("docker row carries an events-reserved note");
        let reason = note
            .strip_prefix("events: reserved — ")
            .expect("reserved note shape: {note}");
        assert!(!reason.trim().is_empty(), "non-empty reason: {note}");
        assert_eq!(docker.caps["events"], serde_json::json!(reason));

        // bwrap: no container event stream — no events note, caps false.
        let bwrap = row(thegn_core::config::SandboxBackend::Bwrap);
        assert!(
            !bwrap.notes.iter().any(|n| n.starts_with("events:")),
            "{bwrap:?}"
        );
        assert_eq!(bwrap.caps["events"], serde_json::json!(false));

        // Every backend row describes its events cap — the seam rule that an
        // implementation can describe itself — whatever the runtime state,
        // including the not-installed early return (the bit is a static
        // profile-table property, not live state).
        for b in thegn_core::sandbox::Backend::ALL {
            let r = sandbox_backend_probe(b);
            match b.profile().events {
                EventsCap::Yes => {
                    assert!(
                        r.notes.iter().any(|n| n.contains("exec+network audit")),
                        "{b:?}: {r:?}"
                    );
                    assert_eq!(r.caps["events"], serde_json::json!(true), "{b:?}");
                }
                EventsCap::Reserved(reason) => {
                    let want = format!("events: reserved — {reason}");
                    assert!(r.notes.iter().any(|n| n == &want), "{b:?}: {r:?}");
                    assert_eq!(r.caps["events"], serde_json::json!(reason), "{b:?}");
                }
                EventsCap::No => {
                    assert!(
                        !r.notes.iter().any(|n| n.starts_with("events:")),
                        "{b:?}: {r:?}"
                    );
                    assert_eq!(r.caps["events"], serde_json::json!(false), "{b:?}");
                }
            }
        }
    }

    #[test]
    fn host_discovery_reserved_and_disabled_paths() {
        // A reserved discovery kind reports as reserved (never silently omitted).
        let mut cfg = Config::default();
        cfg.host_discovery.kind = thegn_core::config::HostDiscoveryKind::Consul;
        let r = host_discovery_probes(&cfg);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].seam, "host_discovery");
        match &r[0].availability {
            Availability::Unavailable(why) => assert!(why.contains("reserved"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Disabled tailnet: reported (id `tailnet`), Ready, no subprocess.
        let mut off = Config::default();
        off.host_discovery.tailnet.enabled = false;
        let r = host_discovery_probes(&off);
        assert_eq!(r[0].id, "tailnet");
        assert!(r[0].availability.is_ready());
        assert!(r[0].notes.iter().any(|n| n.contains("enabled = false")));
    }

    #[test]
    fn reserved_selections_are_reported_as_reserved() {
        let mut cfg = Config::default();
        cfg.ci.provider = thegn_core::config::CiProviderKind::Drone;
        cfg.media.enabled = true;
        cfg.media.backend = thegn_core::config::MediaBackendKind::Jellyfin;
        cfg.sandbox.enabled = true;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Wsl;
        cfg.drawer.kind = Some(thegn_core::config::DrawerKind::Lf);
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
            ("files", "lf"),
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

    /// `[weather]` is off by default, so a default config reports no weather
    /// row at all; enabling it adds exactly one `Ready` report, and a reserved
    /// or deactivated selection still explains itself. Every batch holds the
    /// cross-seam shape invariants.
    #[test]
    fn weather_reports_only_when_enabled() {
        use thegn_core::config_weather::WeatherProviderKind;
        let mut cfg = Config::default();
        cfg.host_discovery.tailnet.enabled = false; // hermetic: no real tailscale exec
        let weather_rows = |rs: &[ProbeReport]| rs.iter().filter(|r| r.seam == "weather").count();

        // Off by default ⇒ no row.
        let reports = probes(&cfg);
        crate::conformance::assert_report_invariants(&reports);
        assert_eq!(weather_rows(&reports), 0, "{reports:?}");

        // Enabled ⇒ exactly one Ready row, and no location in the notes.
        cfg.weather.enabled = true;
        cfg.weather.location = "Reykjavík".into();
        let reports = probes(&cfg);
        crate::conformance::assert_report_invariants(&reports);
        assert_eq!(weather_rows(&reports), 1, "{reports:?}");
        let r = reports.iter().find(|r| r.seam == "weather").unwrap();
        assert_eq!(r.id, crate::weather::wttr_in::PROVIDER_ID);
        assert!(r.availability.is_ready(), "{r:?}");
        assert!(r.notes.iter().any(|n| n.contains("as configured")), "{r:?}");
        assert!(
            !r.notes.iter().any(|n| n.contains("Reykjavík")),
            "the probe leaked the location: {r:?}"
        );

        // No location ⇒ the note says the service infers one.
        cfg.weather.location = String::new();
        let reports = probes(&cfg);
        let r = reports.iter().find(|r| r.seam == "weather").unwrap();
        assert!(r.notes.iter().any(|n| n.contains("request IP")), "{r:?}");

        // `none` ⇒ one row explaining why there is nothing to fetch.
        cfg.weather.provider = WeatherProviderKind::None;
        let reports = probes(&cfg);
        crate::conformance::assert_report_invariants(&reports);
        let r = reports.iter().find(|r| r.seam == "weather").unwrap();
        assert!(matches!(&r.availability, Availability::Unavailable(w) if w.contains("none")));

        // A reserved kind reports as reserved, never silently omitted.
        cfg.weather.provider = WeatherProviderKind::OpenMeteo;
        let reports = probes(&cfg);
        crate::conformance::assert_report_invariants(&reports);
        let r = reports.iter().find(|r| r.seam == "weather").unwrap();
        assert_eq!(r.id, "open_meteo");
        assert!(matches!(&r.availability, Availability::Unavailable(w) if w.contains("reserved")));
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
