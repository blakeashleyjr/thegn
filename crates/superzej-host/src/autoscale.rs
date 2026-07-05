//! Autoscale execution: create an engine-managed VPS host from an ordered
//! `[[placement.autoscale.managed]]` template lane, register it (host def +
//! authoritative capacity spec), and scale drained hosts back down. The
//! *decision* to provision/destroy is the pure broker's
//! ([`superzej_core::scheduler`]); this module only executes it. Blocking —
//! spawn_blocking / maintainer-tick threads only.
//!
//! Consent: `autoscale.enabled` IS the create-and-install consent for these
//! hosts — superzej created the box, so `install_runtime = "auto"` is
//! legitimate (the per-host consent ladder still governs every user-owned
//! machine). Engine hosts are named `sz-auto-<size>-<hash>` and labelled
//! `sz-placement=managed` at the vendor, so the reaper can tell them from
//! user instances.

use anyhow::{Context, Result, anyhow};

use superzej_core::capacity::HostOwnership;
use superzej_core::config::{Config, EnvProviderConfig, EnvSshConfig};
use superzej_core::config_placement::ManagedTemplate;
use superzej_core::db::Db;
use superzej_core::host::HostId;
use superzej_core::host_config::{HostBinding, HostConfig, HostReach, InstallConsent};
use superzej_core::store::{HealthMarker, HostCapacityRow, HostStore, PlacementStore};

/// Engine-host name prefix (also the orphan-reap discriminator).
const AUTO_PREFIX: &str = "sz-auto-";
/// An engine-created instance with no capacity row older than this is a
/// crash orphan (create landed, registration didn't) — destroy it.
const ORPHAN_AGE_SECS: i64 = 20 * 60;
/// Cooldown escalation cap (base × 2^consecutive, at most base × this).
const COOLDOWN_CAP_MULT: u64 = 8;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
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
    if !superzej_core::config::vps_provider_kind(&template.provider) {
        return Err(anyhow!(
            "autoscale lane {}: provider is not a sizable VPS kind",
            template.lane_key()
        ));
    }
    let name = format!(
        "{AUTO_PREFIX}{}-{}",
        superzej_core::util::slugify(&template.size),
        superzej_core::util::short_hash(
            &format!(
                "{}-{}-{}",
                template.lane_key(),
                std::process::id(),
                unix_now()
            ),
            6
        )
    );
    let pc = lane_provider_config(cfg, template);
    let provider = crate::provider_factory::vps_provider_for(&pc, &name)
        .ok_or_else(|| anyhow!("no API token / managed key for {}", template.provider))?;

    let created = crate::agent::block_on_provider(|| async {
        use superzej_svc::provider::RemoteProvider;
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
    let hc = engine_host_config(&ip);
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
    let _ = db.health_clear(&template.lane_key());
    superzej_core::msg::info(&format!(
        "placement: provisioned {name} ({}) at {ip}",
        template.lane_key()
    ));
    Ok(name)
}

/// The `[host.*]`-shaped def for an engine-created box: ssh as root with the
/// managed keypair, host key accepted on first connect (a fresh VM's key is
/// unknown by definition), runtime install pre-consented (superzej owns it).
fn engine_host_config(ip: &str) -> HostConfig {
    let identity = crate::agent::sprite_ssh_keypair()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    HostConfig {
        reach: HostReach::Ssh,
        install_runtime: InstallConsent::Auto,
        ssh: EnvSshConfig {
            host: format!("root@{ip}"),
            port: 22,
            identity,
            forward_agent: false,
            extra_args: vec!["-o".into(), "StrictHostKeyChecking=accept-new".into()],
            ..EnvSshConfig::default()
        },
        ..HostConfig::default()
    }
}

/// Binding for an engine host def persisted this session (the loaded Config
/// predates `put_host_def`, so synthesize from the DB row).
pub(crate) fn db_host_binding(name: &str) -> Option<HostBinding> {
    let db = Db::open().ok()?;
    let defs = db.host_defs().ok()?;
    let (_, hc) = defs.into_iter().find(|(n, _)| n == name)?;
    let mut c = Config::default();
    c.host.insert(name.to_string(), hc);
    c.host_binding(name)
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
        key,
        kind: "create_failure".into(),
        reason: reason.chars().take(200).collect(),
        since_ms: now_ms,
        retry_at_ms: now_ms + (base * mult * 1000) as i64,
        consecutive,
    });
}

/// Maintainer-tick pass: scale down drained engine hosts (pure decision via
/// [`superzej_core::scheduler::decide_scaledown`]) and reap crash orphans
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
        inputs.push(superzej_core::scheduler::ScaleDownHost {
            host: row.host.clone(),
            ownership: row.ownership,
            tenants,
            engine_created: true,
            idle_secs: now.saturating_sub(anchor).max(0) as u64,
        });
    }
    let victims =
        superzej_core::scheduler::decide_scaledown(&inputs, a.scale_down_idle_secs, a.min_hosts);
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
        use superzej_svc::provider::RemoteProvider;
        provider.destroy(name).await
    }) {
        Ok(()) => {
            let _ = db.capacity_delete(host);
            let _ = db.host_delete(host);
            superzej_core::msg::info(&format!("placement: scaled down idle host {name}"));
        }
        Err(e) => superzej_core::msg::warn(&format!("placement: scale-down of {name}: {e}")),
    }
}

/// Destroy `sz-auto-*` instances that exist at the vendor but have no
/// capacity row and are older than the orphan threshold (a crash between
/// create and register).
fn reap_unregistered(cfg: &Config, _db: &Db, rows: &[HostCapacityRow]) {
    let now = unix_now();
    for t in &cfg.placement.autoscale.managed {
        if !superzej_core::config::vps_provider_kind(&t.provider) {
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
            let known = rows
                .iter()
                .any(|r| r.host.config_name() == Some(inst.name.as_str()));
            let old = inst
                .created
                .is_some_and(|c| now.saturating_sub(c) > ORPHAN_AGE_SECS);
            if inst.name.starts_with(AUTO_PREFIX) && !known && old {
                let name = inst.name.clone();
                let _ = crate::agent::block_on_provider(|| async {
                    use superzej_svc::provider::RemoteProvider;
                    provider.destroy(&name).await
                });
                superzej_core::msg::warn(&format!(
                    "placement: reaped unregistered engine host {name} (crashed create?)"
                ));
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
        let hc = engine_host_config("203.0.113.7");
        assert_eq!(hc.reach, HostReach::Ssh);
        assert_eq!(hc.install_runtime, InstallConsent::Auto);
        assert_eq!(hc.ssh.host, "root@203.0.113.7");
        assert!(
            hc.ssh
                .extra_args
                .join(" ")
                .contains("StrictHostKeyChecking=accept-new")
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
