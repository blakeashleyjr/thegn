//! Autoscale execution: create an engine-managed VPS host from an ordered
//! `[[placement.autoscale.managed]]` template lane, register it (host def +
//! authoritative capacity spec), and scale drained hosts back down. The
//! *decision* to provision/destroy is the pure broker's
//! ([`thegn_core::scheduler`]); this module only executes it. Blocking —
//! spawn_blocking / maintainer-tick threads only.
//!
//! Consent: `autoscale.enabled` IS the create-and-install consent for these
//! hosts — thegn created the box, so `install_runtime = "auto"` is
//! legitimate (the per-host consent ladder still governs every user-owned
//! machine). Engine hosts are named `tg-auto-<size>-<random>` and labelled
//! `tg-placement=managed` at the vendor, so the reaper can tell them from
//! user instances.

use anyhow::{Context, Result, anyhow};

use thegn_core::capacity::HostOwnership;
use thegn_core::config::{Config, EnvProviderConfig, EnvSshConfig};
use thegn_core::config_placement::ManagedTemplate;
use thegn_core::db::Db;
use thegn_core::host::HostId;
use thegn_core::host_config::{HostBinding, HostConfig, HostReach, InstallConsent};
use thegn_core::store::{
    ComputeLedgerStore, ComputeMeterRow, HealthMarker, HostCapacityRow, HostStore, PlacementStore,
};

/// Engine-host name prefix (also the orphan-reap discriminator).
const AUTO_PREFIX: &str = "tg-auto-";
/// An engine-created instance with no capacity row older than this is a
/// crash orphan (create landed, registration didn't) — destroy it.
const ORPHAN_AGE_SECS: i64 = 20 * 60;
/// Cooldown escalation cap (base × 2^consecutive, at most base × this).
const COOLDOWN_CAP_MULT: u64 = 8;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(u128::from(d.as_secs())))
        .unwrap_or(0)
}

/// Synthetic provider config for a template lane (the engine has no env; the
/// lane carries everything the VPS driver needs).
fn lane_provider_config(cfg: &Config, t: &ManagedTemplate) -> EnvProviderConfig {
    EnvProviderConfig {
        provider: t.provider.clone(),
        region: t.region.clone(),
        size: t.size.clone(),
        // The global engine ceiling doubles as the vendor-side create guard.
        max_instances: cfg.placement.autoscale.effective_max_hosts(),
        ..Default::default()
    }
}

/// Create + register one engine host from `template`. Returns the host NAME
/// (a `[host.<name>]`-shaped def persisted to the DB). On create failure the
/// lane is cooled down (escalating, capped) and the error propagates so the
/// broker walks the next lane on the next decision.
pub(crate) fn provision_managed(
    cfg: &Config,
    db: &Db,
    template: &ManagedTemplate,
) -> Result<String> {
    if !thegn_core::config::vps_provider_kind(&template.provider) {
        return Err(anyhow!(
            "autoscale lane {}: provider is not a sizable VPS kind",
            template.lane_key()
        ));
    }
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| {
        anyhow!(
            "autoscale lane {}: generate engine host name: {error}",
            template.lane_key()
        )
    })?;
    let name = auto_host_name(&template.size, nonce);
    let pc = lane_provider_config(cfg, template);
    let provider = crate::provider_factory::vps_provider_for(&pc, &name)
        .ok_or_else(|| anyhow!("no API token / managed key for {}", template.provider))?;

    let created = crate::agent::block_on_provider(|| async {
        use thegn_svc::provider::RemoteProvider;
        provider.create().await
    });
    if let Err(e) = created {
        cool_lane(db, template, &format!("create failed: {e}"), cfg);
        return Err(e).context(format!("autoscale create ({})", template.lane_key()));
    }
    let ip = crate::agent::block_on_provider(|| async { provider.resolve_ip(&name).await })
        .context("resolve fresh host ip")?;

    // Register: a DB host def (merged into the config catalog on next load;
    // `db_host_binding` synthesizes a binding for THIS process) + the
    // authoritative capacity spec from the template.
    let hc = engine_host_config(&name, &ip)?;
    db.put_host_def(&name, &hc, unix_now())
        .context("persist engine host def")?;
    db.capacity_put(&HostCapacityRow {
        host: HostId::named(&name),
        ownership: HostOwnership::Managed,
        spec: template.spec(),
        overcommit_cpu_pct: 0, // 0 ⇒ resolved config overcommit applies
        overcommit_mem_pct: 0,
        provider: template.provider.trim().to_string(),
        template: template.size.trim().to_string(),
        created_at: Some(unix_now()),
        measured: None,
        updated_at: unix_now(),
    })
    .context("persist engine host capacity")?;
    // A successful create clears the lane's marker (explicit fail-back).
    let _ = db.health_clear(&template.lane_key()); // best-effort: cache write: the successful create already reported; the marker clear is bookkeeping
    // Fixed-cost meter: an engine host bills from create to destroy. Rate
    // from [placement.price."provider:size"] (fallback "provider"); unpriced
    // meters at 0 with a one-time warn (placement list shows "unpriced").
    let rate = cfg
        .placement
        .price
        .get(&format!(
            "{}:{}",
            template.provider.trim(),
            template.size.trim()
        ))
        .or_else(|| cfg.placement.price.get(template.provider.trim()))
        .copied()
        .unwrap_or_else(|| {
            thegn_core::msg::warn(&format!(
                "placement: no [placement.price] for {} — metering at $0",
                template.lane_key()
            ));
            0.0
        });
    let now_ms = unix_now() * 1000;
    // best-effort: the ledger is bookkeeping, never a create gate here
    let _ = db.start_compute_meter(&ComputeMeterRow {
        resource: name.clone(),
        provider: template.provider.trim().to_string(),
        category: "fixed".into(),
        rate_hourly: rate,
        scope: format!("provider:{}", template.provider.trim()),
        zone: String::new(),
        started_at_ms: now_ms,
        last_accrued_ms: now_ms,
        stopped_at_ms: None,
    });
    thegn_core::msg::info(&format!(
        "placement: provisioned {name} ({}) at {ip}",
        template.lane_key()
    ));
    Ok(name)
}

/// The `[host.*]`-shaped def for an engine-created box: ssh as root with the
/// managed keypair, host key accepted on first connect (a fresh VM's key is
/// unknown by definition), runtime install pre-consented (thegn owns it).
fn engine_host_config(name: &str, ip: &str) -> Result<HostConfig> {
    let identity = thegn_core::managed_ssh::list()?
        .into_iter()
        .find(|record| record.instance == name)
        .map(|record| record.key_path)
        .or_else(|| {
            crate::agent::sprite_ssh_keypair()
                .ok()
                .map(|(path, _)| path)
        })
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(HostConfig {
        reach: HostReach::Ssh,
        install_runtime: InstallConsent::Auto,
        ssh: EnvSshConfig {
            host: format!("root@{ip}"),
            port: 22,
            identity,
            forward_agent: false,
            // Managed fresh VM (no per-instance known_hosts registry entry, so
            // accept-new against the global file) — via the one chokepoint.
            extra_args: thegn_core::hostkey::host_key_args(
                thegn_core::hostkey::HostKeyClass::ManagedFresh,
                &thegn_core::hostkey::HostKeyContext::default(),
            ),
            ..EnvSshConfig::default()
        },
        ..HostConfig::default()
    })
}

/// The def + binding for an engine host persisted this session (the loaded
/// Config predates `put_host_def`, so synthesize from the DB row). A failed
/// read is an error, never "no such host".
pub(crate) fn db_host_def(
    db: &Db,
    name: &str,
) -> anyhow::Result<Option<(HostConfig, HostBinding)>> {
    let defs = db
        .host_defs()
        .with_context(|| format!("read the engine host def for {name}"))?;
    let Some((_, hc)) = defs.into_iter().find(|(n, _)| n == name) else {
        return Ok(None);
    };
    let mut c = Config::default();
    c.host.insert(name.to_string(), hc.clone());
    Ok(c.host_binding(name).map(|binding| (hc, binding)))
}

/// Stamp a lane cooldown (escalating per consecutive failure, capped).
fn cool_lane(db: &Db, template: &ManagedTemplate, reason: &str, cfg: &Config) {
    let key = template.lane_key();
    let now_ms = unix_now() * 1000;
    let consecutive = db
        .health_get(&key)
        .ok()
        .flatten()
        .map(|m| m.consecutive + 1)
        .unwrap_or(1);
    let base = cfg.placement.autoscale.cooldown_secs.max(1);
    let mult = 2u64
        .saturating_pow(consecutive.saturating_sub(1))
        .min(COOLDOWN_CAP_MULT);
    let _ = db.health_mark(&HealthMarker {
        // best-effort: cooldown marker: a lost marker only retries the create sooner
        key,
        kind: "create_failure".into(),
        reason: reason.chars().take(200).collect(),
        since_ms: now_ms,
        retry_at_ms: now_ms.saturating_add(thegn_core::time_policy::saturating_i64(
            u128::from(base) * u128::from(mult) * 1000,
        )),
        consecutive,
    });
}

/// Maintainer-tick pass: scale down drained engine hosts (pure decision via
/// [`thegn_core::scheduler::decide_scaledown`]) and reap crash orphans
/// (created instances that never got registered). Blocking; call off-loop.
pub(crate) fn scaledown_tick(cfg: &Config) {
    let a = &cfg.placement.autoscale;
    if !cfg.placement.enabled || !a.enabled {
        return;
    }
    let Ok(db) = Db::open() else { return };
    let now = unix_now();
    let Ok(rows) = db.capacity_all() else { return };
    let mut inputs = Vec::new();
    for row in &rows {
        if row.ownership != HostOwnership::Managed || row.template.is_empty() {
            continue;
        }
        let tenants = db
            .reserved_totals(&row.host)
            .map(|t| t.tenants)
            .unwrap_or(0);
        // Idle since the later of create / last host use.
        let last_used = db
            .host_get(&row.host)
            .ok()
            .flatten()
            .and_then(|h| h.last_used);
        let anchor = last_used.unwrap_or(0).max(row.created_at.unwrap_or(0));
        inputs.push(thegn_core::scheduler::ScaleDownHost {
            host: row.host.clone(),
            ownership: row.ownership,
            tenants,
            engine_created: true,
            idle_secs: now.saturating_sub(anchor).max(0) as u64,
        });
    }
    let victims =
        thegn_core::scheduler::decide_scaledown(&inputs, a.scale_down_idle_secs, a.min_hosts);
    for host in victims {
        destroy_engine_host(cfg, &db, &host, &rows);
    }
    reap_unregistered(cfg, &db, &rows);
}

fn destroy_engine_host(cfg: &Config, db: &Db, host: &HostId, rows: &[HostCapacityRow]) {
    let Some(row) = rows.iter().find(|r| &r.host == host) else {
        return;
    };
    let Some(name) = host.config_name() else {
        return;
    };
    let template = ManagedTemplate {
        provider: row.provider.clone(),
        size: row.template.clone(),
        ..Default::default()
    };
    let pc = lane_provider_config(cfg, &template);
    let Some(provider) = crate::provider_factory::vps_provider_for(&pc, name) else {
        return; // token gone: leave it; the reaper's lifetime ceiling backstops
    };
    match crate::agent::block_on_provider(|| async {
        use thegn_svc::provider::RemoteProvider;
        provider.destroy(name).await
    }) {
        Ok(()) => {
            let _ = db.stop_compute_meter(name, unix_now() * 1000); // best-effort: ledger bookkeeping after a successful destroy
            let _ = db.capacity_delete(host); // best-effort: cache bookkeeping after a successful destroy
            let _ = db.host_delete(host); // best-effort: cache bookkeeping after a successful destroy
            thegn_core::msg::info(&format!("placement: scaled down idle host {name}"));
        }
        Err(e) => thegn_core::msg::warn(&format!("placement: scale-down of {name}: {e}")),
    }
}

fn auto_host_name(size: &str, nonce: [u8; 8]) -> String {
    let random = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{AUTO_PREFIX}{}-{random}", thegn_core::util::slugify(size))
}

#[derive(Debug, PartialEq, Eq)]
enum AutoOrphan {
    Reap,
    Quarantine(&'static str),
    Skip,
}

/// Pure ownership/orphan classification for an old, unregistered `tg-auto-*`
/// engine host. The exact `tg-ctl` id is the ownership proof; `tg-host` alone
/// no longer is.
fn classify_auto_orphan(
    inst: &thegn_svc::vps::VpsInstance,
    ours_ctl: &str,
    ours_host: &str,
    rows: &[HostCapacityRow],
    now: i64,
) -> AutoOrphan {
    let known = rows
        .iter()
        .any(|r| r.host.config_name() == Some(inst.name.as_str()));
    let old = inst
        .created
        .is_some_and(|c| now.saturating_sub(c) > ORPHAN_AGE_SECS);
    if !inst.name.starts_with(AUTO_PREFIX) || known || !old {
        return AutoOrphan::Skip;
    }
    match thegn_svc::vps::controller::classify_ownership(&inst.labels, ours_ctl, ours_host) {
        thegn_svc::vps::controller::Ownership::Ours => AutoOrphan::Reap,
        thegn_svc::vps::controller::Ownership::LegacyHost => AutoOrphan::Quarantine(
            "legacy tg-host label without a controller id; ownership is ambiguous",
        ),
        thegn_svc::vps::controller::Ownership::Foreign
        | thegn_svc::vps::controller::Ownership::Unlabelled => AutoOrphan::Skip,
    }
}

/// Destroy `tg-auto-*` instances that exist at the vendor but have no
/// capacity row and are older than the orphan threshold (a crash between
/// create and register). Only an exact `tg-ctl` match authorizes destruction.
fn reap_unregistered(cfg: &Config, _db: &Db, rows: &[HostCapacityRow]) {
    let ours_ctl = match thegn_svc::vps::controller::controller_id() {
        Ok(id) => id,
        Err(error) => {
            thegn_core::msg::warn(&format!(
                "placement: autoscale reaper skipped: controller id unavailable ({error:#}); nothing destroyed"
            ));
            return;
        }
    };
    let now = unix_now();
    let ours = thegn_svc::vps::host_label();
    for t in &cfg.placement.autoscale.managed {
        if !thegn_core::config::vps_provider_kind(&t.provider) {
            continue;
        }
        let pc = lane_provider_config(cfg, t);
        let Some(provider) = crate::provider_factory::vps_provider_for(&pc, "reap-probe") else {
            continue;
        };
        let Ok(instances) =
            crate::agent::block_on_provider(|| async { provider.list_detailed().await })
        else {
            continue;
        };
        for inst in instances {
            match classify_auto_orphan(&inst, &ours_ctl, &ours, rows, now) {
                AutoOrphan::Skip => {}
                AutoOrphan::Quarantine(reason) => thegn_core::msg::warn(&format!(
                    "placement: quarantined unregistered engine host {}: {reason}; not destroyed",
                    inst.name
                )),
                AutoOrphan::Reap => {
                    let name = inst.name.clone();
                    match crate::agent::block_on_provider(|| async {
                        use thegn_svc::provider::RemoteProvider;
                        provider.destroy(&name).await
                    }) {
                        Ok(()) => thegn_core::msg::warn(&format!(
                            "placement: reaped unregistered engine host {name} (crashed create?)"
                        )),
                        Err(error) => thegn_core::msg::warn(&format!(
                            "placement: destroy of unregistered engine host {name} failed: {error:#}; will retry next scan"
                        )),
                    }
                }
            }
        }
    }
}

/// Queue registry: worktrees whose placement queued (no capacity). The
/// maintainer tick re-notifies when capacity frees; the actual re-place runs
/// on the worktree's next materialize.
static QUEUED: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));

pub(crate) fn queue_worktree(worktree: &str) {
    QUEUED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(worktree.to_string());
}

/// Drain queued worktrees whose placement could now succeed (a host freed or
/// a lane cooled back in): surface a retry hint. Returns the notified set.
pub(crate) fn nudge_queued() -> Vec<String> {
    let mut q = QUEUED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let drained: Vec<String> = q.iter().cloned().collect();
    q.clear();
    drained
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_config_carries_size_region_and_global_cap() {
        let mut cfg = Config::default();
        cfg.placement.autoscale.max_hosts = 5;
        let t = ManagedTemplate {
            provider: "hetzner".into(),
            region: "fsn1".into(),
            size: "cx32".into(),
            ..Default::default()
        };
        let pc = lane_provider_config(&cfg, &t);
        assert_eq!(pc.provider, "hetzner");
        assert_eq!(pc.region, "fsn1");
        assert_eq!(pc.size, "cx32");
        assert_eq!(pc.max_instances, 5);
    }

    #[test]
    fn provision_refuses_non_vps_lanes() {
        let cfg = Config::default();
        let db = Db::open_memory().unwrap();
        let t = ManagedTemplate {
            provider: "sprites".into(),
            size: "big".into(),
            ..Default::default()
        };
        let err = provision_managed(&cfg, &db, &t).unwrap_err();
        assert!(err.to_string().contains("not a sizable VPS kind"), "{err}");
    }

    #[test]
    fn cooldown_escalates_and_caps() {
        let cfg = Config::default(); // cooldown_secs = 60
        let db = Db::open_memory().unwrap();
        let t = ManagedTemplate {
            provider: "hetzner".into(),
            size: "cx32".into(),
            ..Default::default()
        };
        cool_lane(&db, &t, "boom", &cfg);
        let m1 = db.health_get("tpl:hetzner/cx32").unwrap().unwrap();
        assert_eq!(m1.consecutive, 1);
        assert_eq!(m1.retry_at_ms - m1.since_ms, 60_000);
        cool_lane(&db, &t, "boom again", &cfg);
        let m2 = db.health_get("tpl:hetzner/cx32").unwrap().unwrap();
        assert_eq!(m2.consecutive, 2);
        assert_eq!(m2.retry_at_ms - m2.since_ms, 120_000);
        for _ in 0..8 {
            cool_lane(&db, &t, "still down", &cfg);
        }
        let m = db.health_get("tpl:hetzner/cx32").unwrap().unwrap();
        assert_eq!(
            m.retry_at_ms - m.since_ms,
            (60 * COOLDOWN_CAP_MULT * 1000) as i64,
            "escalation capped"
        );
    }

    #[test]
    fn engine_host_config_shape() {
        let hc = engine_host_config("missing-test-instance", "203.0.113.7").unwrap();
        assert_eq!(hc.reach, HostReach::Ssh);
        assert_eq!(hc.install_runtime, InstallConsent::Auto);
        assert_eq!(hc.ssh.host, "root@203.0.113.7");
        // The host-key args are the ManagedFresh chokepoint's; assert against
        // its output (keeps host-key literals out of every file but hostkey.rs).
        let expected = thegn_core::hostkey::host_key_args(
            thegn_core::hostkey::HostKeyClass::ManagedFresh,
            &thegn_core::hostkey::HostKeyContext::default(),
        );
        assert_eq!(hc.ssh.extra_args, expected);
        assert!(
            !hc.ssh.forward_agent,
            "managed fresh VM forces no forwarding"
        );
    }

    fn auto_inst(
        name: &str,
        host_label: Option<&str>,
        ctl: Option<&str>,
        created: i64,
    ) -> thegn_svc::vps::VpsInstance {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("tg-managed".to_string(), "1".to_string());
        if let Some(h) = host_label {
            labels.insert("tg-host".to_string(), h.to_string());
        }
        if let Some(controller) = ctl {
            labels.insert("tg-ctl".to_string(), controller.to_string());
        }
        thegn_svc::vps::VpsInstance {
            id: "1".into(),
            name: name.into(),
            ip: None,
            running: true,
            created: Some(created),
            labels,
        }
    }

    #[test]
    fn reaper_skips_another_hosts_engine_vm() {
        let now = 1_000_000 + ORPHAN_AGE_SECS + 60; // old enough
        let rows: Vec<HostCapacityRow> = Vec::new(); // not known locally
        // A `tg-auto-*` engine host old + unknown, but labelled for ANOTHER host:
        // must NEVER be reaped (that's another controller's live compute).
        let theirs = auto_inst(
            "tg-auto-cx32-abc123",
            Some("other-host-xyz"),
            Some("b".repeat(32).as_str()),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&theirs, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Skip,
            "a different controller label decides ownership alone"
        );
        // Same shape but with our exact controller identity ⇒ reapable orphan.
        let ours = auto_inst(
            "tg-auto-cx32-abc123",
            Some("other-host-xyz"),
            Some(&"a".repeat(32)),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&ours, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Reap
        );
    }

    #[test]
    fn reaper_respects_prefix_age_and_registration() {
        let now = 1_000_000 + ORPHAN_AGE_SECS + 60;
        let rows: Vec<HostCapacityRow> = Vec::new();
        // Unlabelled instance (legacy / foreign) ⇒ not ours, skip.
        let unlabelled = auto_inst("tg-auto-cx32-abc123", None, None, 1_000_000);
        assert_eq!(
            classify_auto_orphan(&unlabelled, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Skip
        );
        // Not a tg-auto-* name ⇒ skip even if labelled for us.
        let non_engine = auto_inst(
            "tg-dev-x1",
            Some("our-host-abc"),
            Some(&"a".repeat(32)),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&non_engine, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Skip
        );
        // Too young ⇒ skip (create/register may still be in flight).
        let young = auto_inst(
            "tg-auto-cx32-abc123",
            Some("our-host-abc"),
            Some(&"a".repeat(32)),
            now,
        );
        assert_eq!(
            classify_auto_orphan(&young, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Skip
        );
        // Known locally (has a capacity row) ⇒ skip (it's registered).
        let known_rows = vec![HostCapacityRow {
            host: HostId::named("tg-auto-cx32-abc123"),
            ownership: HostOwnership::Managed,
            spec: None,
            overcommit_cpu_pct: 0,
            overcommit_mem_pct: 0,
            provider: "hetzner".into(),
            template: "cx32".into(),
            created_at: Some(1_000_000),
            measured: None,
            updated_at: 1_000_000,
        }];
        let ours = auto_inst(
            "tg-auto-cx32-abc123",
            Some("our-host-abc"),
            Some(&"a".repeat(32)),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&ours, &"a".repeat(32), "our-host-abc", &known_rows, now),
            AutoOrphan::Skip
        );
    }

    #[test]
    fn auto_orphan_requires_exact_controller_id() {
        let now = 1_000_000 + ORPHAN_AGE_SECS + 60;
        let rows = Vec::new();
        let ours_ctl = "a".repeat(32);
        let ours = auto_inst(
            "tg-auto-cx32-0000000000000001",
            None,
            Some(&ours_ctl),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&ours, &ours_ctl, "our-host-abc", &rows, now),
            AutoOrphan::Reap
        );
        let foreign_ctl = "b".repeat(32);
        let foreign = auto_inst(
            "tg-auto-cx32-0000000000000002",
            Some("our-host-abc"),
            Some(&foreign_ctl),
            1_000_000,
        );
        assert_eq!(
            classify_auto_orphan(&foreign, &ours_ctl, "our-host-abc", &rows, now),
            AutoOrphan::Skip
        );
    }

    #[test]
    fn legacy_host_engine_vm_is_quarantined_not_reaped() {
        let now = 1_000_000 + ORPHAN_AGE_SECS + 60;
        let rows = Vec::new();
        let legacy = auto_inst(
            "tg-auto-cx32-0000000000000003",
            Some("our-host-abc"),
            None,
            1_000_000,
        );
        assert!(matches!(
            classify_auto_orphan(&legacy, &"a".repeat(32), "our-host-abc", &rows, now),
            AutoOrphan::Quarantine(_)
        ));
    }

    #[test]
    fn auto_host_names_are_random_per_create() {
        let zero = auto_host_name("cx32", [0; 8]);
        assert_eq!(zero, "tg-auto-cx32-0000000000000000");
        let first = auto_host_name("cx32", [1; 8]);
        let second = auto_host_name("cx32", [2; 8]);
        assert_ne!(first, second);
        assert!(first.len() <= 63);
        assert!(thegn_svc::vps::registry::checked_name(&first).is_ok());
    }

    #[test]
    fn db_host_def_finds_a_def_persisted_after_config_load() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();
        let name = "tg-auto-cx32-0000000000000001";
        let hc = HostConfig {
            reach: HostReach::Local,
            ..HostConfig::default()
        };
        db.put_host_def(name, &hc, 1).unwrap();

        let found = db_host_def(&db, name).unwrap();
        assert!(matches!(found, Some((_, binding)) if binding.id == HostId::named(name)));
    }

    #[test]
    fn db_host_def_is_none_for_an_unknown_name() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().unwrap();

        assert!(
            db_host_def(&db, "tg-auto-cx32-0000000000000001")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn queue_registry_round_trip() {
        queue_worktree("/wt/q1");
        queue_worktree("/wt/q2");
        queue_worktree("/wt/q1");
        let drained = nudge_queued();
        assert!(drained.contains(&"/wt/q1".to_string()));
        assert_eq!(drained.len(), 2);
        assert!(nudge_queued().is_empty());
    }
}
