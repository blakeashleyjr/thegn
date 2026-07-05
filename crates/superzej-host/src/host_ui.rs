//! Pure UI helpers for the **hosts-as-resources** surfaces: the display
//! snapshot the panel/sidebar/wizard/tab-bar read (built off-loop from the
//! config + DB, live-merged on the loop from [`HostRuntime`]), the wizard
//! badge map, the tab-bar chip decoration, and the System ▸ Hosts panel
//! action keys. Everything except [`panel_key`] (which spawns provisioning
//! off-loop) is pure and unit-tested.

use std::collections::HashMap;

use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::host::HostId;
use superzej_core::host_config::{HostReach, InstallConsent};
use superzej_core::host_machine::HostState;
use superzej_core::store::HostStore;

use crate::handlers::host::HostRuntime;

/// One `[host.*]` entry's display state: the durable DB row flattened to
/// strings (cheap to clone, `Eq` for the frame diff) plus the loop-merged
/// live view. Carried on `PanelData` so the panel, sidebar, and wizard all
/// read one snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostSnapshot {
    /// The `[host.<name>]` config name.
    pub name: String,
    /// Canonical id (`host:<name>`) — the key into `HostRuntime.state`.
    pub id: String,
    /// Reach kind ("ssh"/"iroh"/"cloud"/"local").
    pub reach: String,
    /// Durable state tag ("unknown"/"runtime_ready"/"image_ready"/"ready"/
    /// "failed"); `""` when the host has no DB row yet (never provisioned).
    pub state: String,
    /// Probed runtime ("podman 5.0"); `""` before the first probe.
    pub runtime: String,
    /// Probed "arch os"; `""` before the first probe.
    pub arch_os: String,
    /// Resolved base-image reference (name:tag).
    pub image: String,
    /// Install-consent display ("granted"/"declined"/config policy).
    pub consent: String,
    /// Unix seconds of the last successful probe.
    pub last_probe: Option<i64>,
    /// Mid-drive step persisted by the (possibly external) leader.
    pub active_step: Option<String>,
    /// `failure_reason` when the durable state is failed; `""` otherwise.
    pub error: String,
    /// Inventory lines: "kind digest-short ref".
    pub inventory: Vec<String>,
    /// Recent event lines (newest first, capped at 5): "step — detail".
    pub events: Vec<String>,
    /// Live (loop-merged) provisioning flag — see [`merge_live`].
    pub provisioning: bool,
    /// Live one-line status while provisioning; `""` otherwise.
    pub live_status: String,
}

impl HostSnapshot {
    /// The state glyph shared by the panel, sidebar, and wizard badge:
    /// ● ready / ◐ provisioning / ✗ failed / ○ unprovisioned.
    pub fn glyph(&self) -> &'static str {
        if self.provisioning {
            "◐"
        } else {
            match self.state.as_str() {
                "ready" => "●",
                "failed" => "✗",
                _ => "○",
            }
        }
    }

    /// Terse status for row right-sides: the live step while provisioning,
    /// else the durable state.
    pub fn short_status(&self) -> String {
        if self.provisioning {
            return if self.live_status.is_empty() {
                "provisioning".into()
            } else {
                self.live_status.clone()
            };
        }
        match self.state.as_str() {
            "ready" => "ready".into(),
            "failed" => "failed".into(),
            "runtime_ready" => "runtime ready".into(),
            "image_ready" => "image ready".into(),
            "" => "unprovisioned".into(),
            other => other.to_string(),
        }
    }
}

fn reach_kind(r: HostReach) -> &'static str {
    match r {
        HostReach::Ssh => "ssh",
        HostReach::Iroh => "iroh",
        HostReach::Cloud => "cloud",
        HostReach::Local => "local",
    }
}

/// Build the per-`[host.*]` display snapshots from the config + DB. Small DB
/// reads only — runs on the hydration thread, never the loop.
pub fn host_snapshots(cfg: &Config, db: &Db) -> Vec<HostSnapshot> {
    if cfg.host.is_empty() {
        return Vec::new();
    }
    let rows = db.hosts_all().unwrap_or_default();
    cfg.host
        .iter()
        .map(|(name, hc)| {
            let id = HostId::named(name);
            let row = rows.iter().find(|r| r.id == id);
            let image = if hc.image.trim().is_empty() {
                superzej_core::image::ImageRef::default_base().name_tag()
            } else {
                hc.image.clone()
            };
            let consent = match row.and_then(|r| r.install_consent) {
                Some(true) => "granted".into(),
                Some(false) => "declined".into(),
                None => match hc.install_runtime {
                    InstallConsent::Auto => "auto (config)".into(),
                    InstallConsent::Never => "never (config)".into(),
                    InstallConsent::Ask => "unset — will ask".to_string(),
                },
            };
            let caps = row.and_then(|r| r.caps.as_ref());
            let mut snap = HostSnapshot {
                name: name.clone(),
                id: id.as_str().to_string(),
                reach: reach_kind(hc.reach).into(),
                state: row
                    .and_then(|r| r.state.durable_tag())
                    .unwrap_or_default()
                    .into(),
                runtime: caps
                    .and_then(|c| c.runtime.as_ref())
                    .map(|r| format!("{} {}", r.kind.as_str(), r.version))
                    .unwrap_or_default(),
                arch_os: caps
                    .map(|c| format!("{} {}", c.arch.oci_name(), c.os))
                    .or_else(|| row.and_then(|r| r.arch).map(|a| a.oci_name().to_string()))
                    .unwrap_or_default(),
                image,
                consent,
                last_probe: row.and_then(|r| r.last_probe),
                active_step: row.and_then(|r| r.active_step.clone()),
                ..HostSnapshot::default()
            };
            if let Some(HostState::Failed(f)) = row.map(|r| &r.state) {
                snap.error = crate::host_flow::failure_reason(f);
            }
            snap.inventory = db
                .host_inventory(&id)
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {}",
                        e.key.kind.as_str(),
                        e.key.digest.short(),
                        e.ref_name
                    )
                })
                .collect();
            snap.events = db
                .host_events_recent(&id, 5)
                .unwrap_or_default()
                .into_iter()
                .map(|(_, step, detail)| {
                    if detail.is_empty() {
                        step
                    } else {
                        format!("{step} — {detail}")
                    }
                })
                .collect();
            snap
        })
        .collect()
}

/// Merge the loop's live per-host view onto the DB-built snapshots. Called on
/// the loop after a [`HostRuntime`] drain (I/O-free).
pub fn merge_live(hosts: &mut [HostSnapshot], rt: &HostRuntime) {
    for h in hosts.iter_mut() {
        let Some(v) = rt.state.get(&h.id) else {
            continue;
        };
        h.provisioning = v.provisioning;
        h.live_status = rt.status_line(&h.id).unwrap_or_default();
        if let Some(f) = &v.failed {
            h.state = "failed".into();
            h.error = crate::host_flow::failure_reason(f);
        } else if v.ready {
            h.state = "ready".into();
            h.error.clear();
        }
    }
}

/// Clip to `max` chars with an ellipsis (badge/status hygiene).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The wizard's per-env host-readiness badges: ENV KEY → a short dim string
/// (`✓ ready` / `◐ <step>` / `✗ failed` / `○ new`) for envs bound to a
/// `[host.*]` entry. Envs without a host binding get no badge.
pub fn wizard_host_badges(cfg: &Config, hosts: &[HostSnapshot]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (key, envc) in &cfg.env {
        let host = envc.host.trim();
        if host.is_empty() {
            continue;
        }
        let badge = match hosts.iter().find(|h| h.name == host) {
            None => "○ unknown host".to_string(),
            Some(h) if h.provisioning => format!("◐ {}", clip(&h.short_status(), 18)),
            Some(h) => match h.state.as_str() {
                "ready" => "✓ ready".into(),
                "failed" => "✗ failed".into(),
                _ => "○ new".into(),
            },
        };
        out.insert(key.clone(), badge);
    }
    out
}

/// Decorate the tab-bar placement chip with the backing host's state:
/// ready (`None`) ⇒ unchanged; failed ⇒ `<kind> !`; otherwise `<kind> ~<s>`.
pub fn decorate_placement_kind(kind: &str, status: Option<&str>) -> String {
    match status {
        None => kind.to_string(),
        Some("failed") => format!("{kind} !"),
        Some(s) => format!("{kind} ~{}", clip(s, 14)),
    }
}

/// The chip status for an env's backing host: `None` when the env has no
/// host binding or the host is ready; `Some("failed")`; else a short
/// in-progress marker (the persisted mid-drive step, or "pending").
pub fn env_host_status(cfg: &Config, env_name: &str, hosts: &[HostSnapshot]) -> Option<String> {
    let host = cfg.env.get(env_name)?.host.trim().to_string();
    if host.is_empty() {
        return None;
    }
    let h = hosts.iter().find(|h| h.name == host)?;
    match h.state.as_str() {
        "ready" => None,
        "failed" => Some("failed".into()),
        _ => Some(
            h.active_step
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "pending".into()),
        ),
    }
}

/// A `Section::Hosts` action key: `p` provision, `r` re-probe (reset the
/// probe TTL, then provision), `c` grant install consent (confirmed), `x`
/// forget the cached host state (confirmed). Returns whether the key was
/// claimed. Provisions run off-loop; their result comes back through the
/// same [`HostUiEvent`](crate::host_flow::HostUiEvent) channel as every
/// other driver, so failures surface in the panel — never swallowed.
pub(crate) fn panel_key(
    key: termwiz::input::KeyCode,
    cursor: usize,
    model: &mut crate::chrome::FrameModel,
    cfg: &Config,
    host_ui: &crate::host_flow::HostUiTx,
    active_menu: &mut Option<crate::menu::MenuOverlay>,
) -> bool {
    use termwiz::input::KeyCode::Char;
    let Some(h) = model.panel.hosts.get(cursor) else {
        return false;
    };
    let (name, id) = (h.name.clone(), h.id.clone());
    match key {
        Char(c @ ('p' | 'r')) => {
            let Some(binding) = cfg.host_binding(&name) else {
                model.status = format!("host {name}: invalid [host.{name}] config");
                return true;
            };
            let reprobe = c == 'r';
            model.status = if reprobe {
                format!("re-probing {name}…")
            } else {
                format!("provisioning {name}…")
            };
            let ui = host_ui.clone();
            tokio::task::spawn_blocking(move || {
                if reprobe && let Ok(db) = Db::open() {
                    // best-effort: a failed reset just means the fast path may
                    // still short-circuit; the drive below re-probes anyway
                    let _ = db.host_touch_probe(&binding.id, 0);
                }
                // The outcome flows to the loop via HostUiEvent::Done (sent by
                // ensure_host_ready itself), so this result needs no plumbing.
                let _ = crate::host_flow::ensure_host_ready(
                    &binding,
                    crate::host_flow::ConsentPolicy::Interactive,
                    &mut |_| {},
                    Some(&ui),
                    &mut |reach| superzej_svc::host::runner_for(reach),
                );
            });
            true
        }
        Char('c') => {
            *active_menu = Some(crate::menu::confirm_menu(
                format!("⚙ grant install on {name}?"),
                format!(
                    "Record install consent for {name}: superzej may bootstrap a \
                     container runtime there on the next provision. Persists until \
                     you remove the host's cached state."
                ),
                "host-grant",
                id,
                false,
            ));
            true
        }
        Char('x') => {
            *active_menu = Some(crate::menu::confirm_menu(
                format!("✕ forget cached state for {name}?"),
                format!(
                    "Removes {name}'s cached row, inventory, and event trail from the \
                     local DB. Nothing on the machine itself is touched; the next use \
                     re-probes and re-provisions from scratch."
                ),
                "host-rm",
                id,
                true,
            ));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ProvisionState, ProvisionStepView};
    use crate::host_flow::HostUiEvent;
    use superzej_core::host::{HostCaps, HostFailure, HostStep};

    fn cfg_with_host(name: &str) -> Config {
        let mut cfg = Config::default();
        cfg.host.insert(
            name.into(),
            superzej_core::host_config::HostConfig {
                reach: HostReach::Local,
                ..Default::default()
            },
        );
        cfg
    }

    fn seeded_db(name: &str, state: &HostState, caps: Option<&HostCaps>) -> Db {
        let db = Db::open_memory().unwrap();
        let id = HostId::named(name);
        db.host_checkpoint(&id, name, "local", state, caps, 100)
            .unwrap();
        db
    }

    #[test]
    fn snapshots_cover_every_config_host_even_without_db_rows() {
        let cfg = cfg_with_host("box");
        let db = Db::open_memory().unwrap();
        let snaps = host_snapshots(&cfg, &db);
        assert_eq!(snaps.len(), 1);
        let s = &snaps[0];
        assert_eq!(s.name, "box");
        assert_eq!(s.id, "host:box");
        assert_eq!(s.reach, "local");
        assert_eq!(s.state, "");
        assert_eq!(s.glyph(), "○");
        assert_eq!(s.short_status(), "unprovisioned");
        assert!(s.image.contains("superzej-sandbox"), "{}", s.image);
        assert!(s.consent.contains("will ask"), "{}", s.consent);
        // No [host.*] entries ⇒ no snapshots (and no DB reads worth making).
        assert!(host_snapshots(&Config::default(), &db).is_empty());
    }

    #[test]
    fn snapshots_join_db_state_caps_inventory_and_events() {
        let cfg = cfg_with_host("box");
        let caps =
            HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPODMAN=5.0\nPKGMGR=apt\n").unwrap();
        let db = seeded_db("box", &HostState::Ready, Some(&caps));
        let id = HostId::named("box");
        db.host_touch_probe(&id, 90).unwrap();
        db.host_set_consent(&id, true, 91).unwrap();
        db.host_event(&id, "deliver", "delivered 11111111 via oci", 92)
            .unwrap();
        let digest = superzej_core::image::Digest::from_hex(&"1".repeat(64)).unwrap();
        db.host_inventory_put(&superzej_core::inventory::InventoryEntry {
            key: superzej_core::inventory::InventoryKey {
                host: id.clone(),
                kind: superzej_core::inventory::ArtifactKind::Image,
                digest,
                arch: superzej_core::host::Arch::Amd64,
            },
            ref_name: "ghcr.io/x/base:v1".into(),
            present_at: 93,
            verified_at: None,
            size_bytes: None,
        })
        .unwrap();

        let s = &host_snapshots(&cfg, &db)[0];
        assert_eq!(s.state, "ready");
        assert_eq!(s.glyph(), "●");
        assert_eq!(s.runtime, "podman 5.0");
        assert!(s.arch_os.contains("linux"), "{}", s.arch_os);
        assert_eq!(s.consent, "granted");
        assert_eq!(s.last_probe, Some(90));
        assert_eq!(s.inventory.len(), 1);
        assert!(s.inventory[0].starts_with("image "), "{}", s.inventory[0]);
        assert!(s.inventory[0].contains("ghcr.io/x/base:v1"));
        assert_eq!(s.events.len(), 1);
        assert!(s.events[0].contains("deliver — delivered"));
        assert!(s.error.is_empty());
    }

    #[test]
    fn failed_snapshot_carries_the_failure_reason() {
        let cfg = cfg_with_host("box");
        let db = seeded_db(
            "box",
            &HostState::Failed(HostFailure {
                step: HostStep::Deliver,
                error: "registry unreachable".into(),
                retryable: true,
            }),
            None,
        );
        let s = &host_snapshots(&cfg, &db)[0];
        assert_eq!(s.state, "failed");
        assert_eq!(s.glyph(), "✗");
        assert!(s.error.contains("registry unreachable"), "{}", s.error);
    }

    fn runtime_with(events: Vec<HostUiEvent>) -> HostRuntime {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for e in events {
            tx.send(e).unwrap();
        }
        let mut rt = HostRuntime::new(rx);
        rt.drain(false);
        rt
    }

    #[test]
    fn merge_live_overlays_progress_ready_and_failure() {
        let rt = runtime_with(vec![
            HostUiEvent::Progress {
                host: "host:a".into(),
                steps: vec![ProvisionStepView {
                    label: "transfer image".into(),
                    state: ProvisionState::Active,
                    detail: Some("42 MiB".into()),
                }],
            },
            HostUiEvent::Done {
                host: "host:b".into(),
                result: Ok(()),
            },
            HostUiEvent::Done {
                host: "host:c".into(),
                result: Err(HostFailure {
                    step: HostStep::Connect,
                    error: "timeout".into(),
                    retryable: true,
                }),
            },
        ]);
        let mk = |name: &str| HostSnapshot {
            name: name.into(),
            id: format!("host:{name}"),
            ..Default::default()
        };
        let mut hosts = vec![mk("a"), mk("b"), mk("c"), mk("d")];
        merge_live(&mut hosts, &rt);
        assert!(hosts[0].provisioning);
        assert_eq!(hosts[0].glyph(), "◐");
        assert!(hosts[0].live_status.contains("transfer image"));
        assert_eq!(hosts[1].state, "ready");
        assert!(!hosts[1].provisioning);
        assert_eq!(hosts[2].state, "failed");
        assert!(hosts[2].error.contains("timeout"));
        // Untouched host stays at its DB-built defaults.
        assert_eq!(hosts[3], mk("d"));
    }

    fn env_on_host(cfg: &mut Config, env: &str, host: &str) {
        cfg.env.insert(
            env.into(),
            superzej_core::config::EnvConfig {
                host: host.into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn wizard_badges_map_env_keys_to_host_states() {
        let mut cfg = cfg_with_host("box");
        env_on_host(&mut cfg, "prod", "box");
        env_on_host(&mut cfg, "ghost", "nowhere");
        env_on_host(&mut cfg, "plain", "");
        let hosts = vec![HostSnapshot {
            name: "box".into(),
            id: "host:box".into(),
            state: "ready".into(),
            ..Default::default()
        }];
        let badges = wizard_host_badges(&cfg, &hosts);
        assert_eq!(badges.get("prod").map(String::as_str), Some("✓ ready"));
        assert_eq!(
            badges.get("ghost").map(String::as_str),
            Some("○ unknown host")
        );
        assert!(!badges.contains_key("plain"), "no host binding ⇒ no badge");

        // Failed / provisioning / new variants.
        let variants = [
            ("failed", false, "✗ failed"),
            ("", false, "○ new"),
            ("image_ready", false, "○ new"),
        ];
        for (state, prov, want) in variants {
            let hosts = vec![HostSnapshot {
                name: "box".into(),
                state: state.into(),
                provisioning: prov,
                ..Default::default()
            }];
            assert_eq!(
                wizard_host_badges(&cfg, &hosts)
                    .get("prod")
                    .map(String::as_str),
                Some(want),
                "state={state}"
            );
        }
        let hosts = vec![HostSnapshot {
            name: "box".into(),
            provisioning: true,
            live_status: "transfer image — 42 MiB / 1 GiB of things".into(),
            ..Default::default()
        }];
        let b = wizard_host_badges(&cfg, &hosts);
        let badge = b.get("prod").unwrap();
        assert!(badge.starts_with("◐ "), "{badge}");
        assert!(badge.chars().count() <= 20, "clipped: {badge}");
    }

    #[test]
    fn decorate_placement_kind_marks_failed_and_in_progress() {
        assert_eq!(decorate_placement_kind("ssh", None), "ssh");
        assert_eq!(decorate_placement_kind("ssh", Some("failed")), "ssh !");
        assert_eq!(
            decorate_placement_kind("ssh", Some("deliver")),
            "ssh ~deliver"
        );
        // Long statuses are clipped so the chip can't eat the tab bar:
        // "ssh" + " ~" + at most 14 status chars.
        let d = decorate_placement_kind("ssh", Some("a very very long step name"));
        assert!(d.chars().count() <= 3 + 2 + 14, "{d}");
    }

    #[test]
    fn env_host_status_resolves_the_bound_hosts_state() {
        let mut cfg = cfg_with_host("box");
        env_on_host(&mut cfg, "prod", "box");
        env_on_host(&mut cfg, "plain", "");
        let host = |state: &str, step: Option<&str>| HostSnapshot {
            name: "box".into(),
            state: state.into(),
            active_step: step.map(String::from),
            ..Default::default()
        };
        assert_eq!(env_host_status(&cfg, "prod", &[host("ready", None)]), None);
        assert_eq!(
            env_host_status(&cfg, "prod", &[host("failed", None)]).as_deref(),
            Some("failed")
        );
        assert_eq!(
            env_host_status(&cfg, "prod", &[host("", Some("deliver"))]).as_deref(),
            Some("deliver")
        );
        assert_eq!(
            env_host_status(&cfg, "prod", &[host("image_ready", None)]).as_deref(),
            Some("pending")
        );
        // No binding / unknown env / no snapshot ⇒ no decoration.
        assert_eq!(
            env_host_status(&cfg, "plain", &[host("failed", None)]),
            None
        );
        assert_eq!(env_host_status(&cfg, "nope", &[host("failed", None)]), None);
        assert_eq!(env_host_status(&cfg, "prod", &[]), None);
    }

    #[test]
    fn short_status_prefers_the_live_step_while_provisioning() {
        let mut h = HostSnapshot {
            state: "image_ready".into(),
            ..Default::default()
        };
        assert_eq!(h.short_status(), "image ready");
        h.provisioning = true;
        assert_eq!(h.short_status(), "provisioning");
        h.live_status = "seed cargo".into();
        assert_eq!(h.short_status(), "seed cargo");
    }
}
