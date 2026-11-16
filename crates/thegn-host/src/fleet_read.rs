//! The fleet read shared by `thegn fleet list|cost` and the masthead `fleet`
//! widget: every provider sandbox across the configured `[env.<name>]`
//! entries, joined to the worktree that owns it, then priced.
//!
//! Each env with an API provider is inventoried once (20 s budget per env,
//! all envs concurrently, through [`crate::agent::block_on_provider`]). An env
//! whose provider cannot be built (its token does not resolve) or whose read
//! fails is reported as unavailable, not as an error. Two envs sharing one
//! account may list the same sandbox; it is shown once, under the first env.
//!
//! Blocking: callers on the event loop must run [`read`] off it (the widget
//! poll rides `tokio::task::spawn_blocking`, see `hydrate_fleet`).

use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use thegn_core::config::Config;
use thegn_core::placement::fleet_cost::{
    self, Completeness, EnvInventory, FleetCost, FleetView, RecordedSandbox, SandboxRow,
};
use thegn_svc::provider::{InventoryReport, Provider, SandboxInfo};

/// Per-env inventory budget.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(20);

/// One priced fleet read, as the widget and its popup hold it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FleetSnapshot {
    /// Epoch ms of the estimate.
    pub(crate) generated_ms: i64,
    pub(crate) view: FleetView,
    pub(crate) cost: FleetCost,
}

/// Inventory every provider env and price the result at "now".
pub(crate) fn read(cfg: &Config) -> Result<FleetSnapshot> {
    let view = gather(cfg, None)?;
    let generated_ms = thegn_core::util::now_ms();
    let cost = fleet_cost::fleet_cost(
        &view,
        fleet_cost::BUNDLED_PRICES,
        &cfg.placement.price,
        generated_ms,
    );
    Ok(FleetSnapshot {
        generated_ms,
        view,
        cost,
    })
}

/// One env to inventory, in config order.
struct Slot {
    env: String,
    provider_name: String,
    provider: Option<Provider>,
}

pub(crate) fn gather(cfg: &Config, only_env: Option<&str>) -> Result<FleetView> {
    use thegn_core::config::EnvProviderKind;
    let slots: Vec<Slot> = cfg
        .env
        .iter()
        .filter(|(name, _)| only_env.is_none_or(|only| only == name.as_str()))
        .filter(|(_, envc)| {
            !matches!(
                EnvProviderKind::of(&envc.provider.provider),
                EnvProviderKind::Custom
            )
        })
        .map(|(name, envc)| Slot {
            env: name.clone(),
            provider_name: envc.provider.provider.clone(),
            provider: crate::provider_factory::provider_for_named(
                &envc.provider,
                &envc.provider.id,
            ),
        })
        .collect();
    let reads = crate::agent::block_on_provider(|| async {
        let reads = slots.iter().map(|slot| async move {
            match &slot.provider {
                Some(provider) => Some(
                    tokio::time::timeout(INVENTORY_TIMEOUT, provider.inventory())
                        .await
                        .map_err(|_| format!("timed out after {}s", INVENTORY_TIMEOUT.as_secs()))
                        .and_then(|read| read.map_err(|error| format!("{error:#}"))),
                ),
                None => None,
            }
        });
        Ok(futures::future::join_all(reads).await)
    })?;
    let inventories = slots
        .iter()
        .zip(reads)
        .map(|(slot, read)| env_inventory(&slot.env, &slot.provider_name, read))
        .collect();
    Ok(fleet_cost::join(inventories, recorded_sandboxes(cfg)))
}

/// Shape one env's read. `None` = the provider could not be built.
fn env_inventory(
    env: &str,
    provider: &str,
    read: Option<std::result::Result<InventoryReport, String>>,
) -> EnvInventory {
    let to_rows = |rows: Vec<SandboxInfo>| -> Vec<SandboxRow> {
        rows.into_iter()
            .map(|info| SandboxRow {
                provider: provider.to_string(),
                env: env.to_string(),
                id: info.id,
                name: info.name,
                state: info.state,
                size: info.size,
                region: info.region,
                created_ms: info.created_at_ms,
            })
            .collect()
    };
    let (completeness, rows) = match read {
        None => (
            Completeness::Unavailable(
                "unavailable: no API provider or its token could not be resolved".into(),
            ),
            Vec::new(),
        ),
        Some(Err(reason)) => (
            Completeness::Unavailable(format!("unavailable: {reason}")),
            Vec::new(),
        ),
        Some(Ok(InventoryReport::Complete(rows))) => (Completeness::Complete, to_rows(rows)),
        Some(Ok(InventoryReport::Partial { rows, reason })) => (
            Completeness::Partial(format!("partial: {reason}")),
            to_rows(rows),
        ),
    };
    EnvInventory {
        env: env.to_string(),
        provider: provider.to_string(),
        completeness,
        rows,
    }
}

/// Every recorded worktree whose resolved env is a provider placement, with
/// its bound sandbox id. Unreadable rows are skipped (logged at debug).
fn recorded_sandboxes(cfg: &Config) -> Vec<RecordedSandbox> {
    use thegn_core::store::WorkspaceStore;
    let rows = match thegn_core::db::Db::open().and_then(|db| db.worktrees()) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(target: "thegn::fleet", error = %format!("{error:#}"), "worktree rows unreadable; no sandbox is joined");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter(|row| Path::new(&row.worktree).is_dir())
        .filter_map(|row| recorded_for(cfg, &row.worktree))
        .collect()
}

fn recorded_for(cfg: &Config, worktree: &str) -> Option<RecordedSandbox> {
    let env = match crate::cmd::env::resolve_for(cfg, Some(worktree.to_string())) {
        Ok(env) => env,
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", worktree, error = %format!("{error:#}"), "env unresolved");
            return None;
        }
    };
    let thegn_core::placement::Placement::Provider(placement) = &env.placement else {
        return None;
    };
    let id = match crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, &env.name)
    {
        Ok(Some(id)) => id,
        Ok(None) => placement.id.clone(),
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", worktree, error = %format!("{error:#}"), "sandbox binding unreadable");
            return None;
        }
    };
    Some(RecordedSandbox {
        worktree: worktree.to_string(),
        provider: placement.provider.clone(),
        env: env.name.clone(),
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_inventory_shapes_each_read() {
        let info = SandboxInfo {
            id: "a".into(),
            size: Some("s".into()),
            ..SandboxInfo::default()
        };
        let complete = env_inventory(
            "e",
            "fly",
            Some(Ok(InventoryReport::Complete(vec![info.clone()]))),
        );
        assert_eq!(complete.completeness, Completeness::Complete);
        assert_eq!(complete.rows[0].provider, "fly");
        assert_eq!(complete.rows[0].env, "e");
        assert_eq!(complete.rows[0].size.as_deref(), Some("s"));
        let partial = env_inventory(
            "e",
            "fly",
            Some(Ok(InventoryReport::Partial {
                rows: vec![info],
                reason: "x".into(),
            })),
        );
        assert_eq!(
            partial.completeness,
            Completeness::Partial("partial: x".into())
        );
        assert_eq!(partial.rows.len(), 1);
        let failed = env_inventory("e", "fly", Some(Err("boom".into())));
        assert_eq!(
            failed.completeness,
            Completeness::Unavailable("unavailable: boom".into())
        );
        let unbuilt = env_inventory("e", "fly", None);
        assert!(
            matches!(unbuilt.completeness, Completeness::Unavailable(r) if r.contains("token"))
        );
    }
}
