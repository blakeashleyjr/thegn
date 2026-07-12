//! The focused lazy-materialize kick: request launch specs off-thread (the
//! sandbox ensure inside `launch_spec` can block on podman for seconds to
//! minutes) and spawn panes when they land. Extracted from the `run.rs` event
//! loop (god-file ratchet); the spec/provision channel handling lives in
//! [`super::provision`].
//!
//! New here relative to the historical run.rs block: the worker thread
//! installs a scoped [`thegn_core::progress`] sink around spec resolution, so
//! the core's sandbox bring-up phases (connect / image probe / pull progress /
//! build / container create) stream through a
//! [`MaterializeObserver`](crate::loading::observe::MaterializeObserver) into
//! the tab's loading splash instead of leaving the generic three-step seed
//! frozen. The sink is deliberately NOT installed around
//! `provision_worktree` — the provider provisioner already streams its own
//! richer step views over the same channel, and two writers on one splash key
//! flicker.

use std::collections::{HashMap, HashSet};

use tokio::task;

use crate::handlers::provision::{ProvisionProgress, SpecBatch, SpecError, spec_err};
use crate::loading::{SpecOrigin, provision_load_steps, seed_materialize_steps};

type Key = (String, usize);

/// The event-loop locals the materialize kick mutates, borrowed for one call.
pub(crate) struct MaterializeCtx<'a> {
    pub materialize_inflight: &'a mut HashSet<Key>,
    pub materialize_failed: &'a HashSet<Key>,
    pub creating_tabs: &'a HashSet<Key>,
    pub loading_retired: &'a mut HashSet<Key>,
    pub loading_remote: &'a mut HashMap<Key, bool>,
    pub loading_state: &'a mut crate::loading::track::LoadingTracker,
    pub dirty: &'a mut bool,
}

/// Channel endpoints the worker reports back through.
#[derive(Clone)]
pub(crate) struct MaterializeTx {
    pub spec_tx: tokio::sync::mpsc::UnboundedSender<SpecBatch>,
    pub provision_tx: tokio::sync::mpsc::UnboundedSender<ProvisionProgress>,
    pub waker: termwiz::terminal::TerminalWaker,
    pub host_ui: crate::host_flow::HostUiTx,
}

/// Kick a two-phase materialize for `(name, ti)` when its tab has missing
/// leaves and nothing else owns the bring-up. One request per (group, tab) at
/// a time, keyed by the unique group name. Mirrors the historical run.rs
/// block verbatim, plus the observer sink.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_materialize(
    ctx: &mut MaterializeCtx<'_>,
    cfg: &thegn_core::config::Config,
    tx: &MaterializeTx,
    missing: Vec<u32>,
    name: &str,
    path: &str,
    ti: usize,
    is_terminal: bool,
) {
    let key = (name.to_string(), ti);
    if missing.is_empty()
        || ctx.materialize_inflight.contains(&key)
        || ctx.materialize_failed.contains(&key)
        // A worktree mid-creation owns its splash; don't race it.
        || ctx.creating_tabs.contains(&key)
    {
        return;
    }
    ctx.materialize_inflight.insert(key.clone());
    // A fresh materialize is a genuine (re)bring-up: un-retire.
    ctx.loading_retired.remove(&key);
    let remote = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(path)).is_remote();
    ctx.loading_remote.insert(key.clone(), remote);
    // The seed plan: backend-aware when the config names a concrete backend
    // (a podman user sees image/container rows from frame one), the generic
    // kinded three-step shape otherwise. Either way it ends in "shell" and the
    // worker's observer refines the SAME rows as core phases stream in.
    let seed = match crate::loading::catalog::seed_target(cfg, remote) {
        Some(t) => crate::loading::catalog::plan_for(&t).into_steps(),
        None => crate::loading::catalog::generic_seed(),
    };
    // Seed the splash — but never overwrite LIVE provisioning steps (an eager
    // stream may already own this key; the seed's shell-wait shape would
    // briefly misclassify the tab).
    if seed_materialize_steps(ctx.loading_state.get(&key).map(Vec::as_slice)) {
        ctx.loading_state.set(key, seed.clone());
    }
    *ctx.dirty = true;
    let cfg = cfg.clone();
    let tx = tx.clone();
    let wt = path.to_string();
    let gname = name.to_string();
    task::spawn_blocking(move || {
        let MaterializeTx {
            spec_tx,
            provision_tx: ptx,
            waker: wk,
            host_ui,
        } = tx;
        let hui = Some(host_ui);
        // Wrap a spec-resolution stage with the scoped core-progress sink:
        // sandbox phases → observer → the tab's splash. Installed per-stage
        // (not thread-wide) so `provision_worktree`'s own step stream never
        // has a second writer.
        let observed = |f: &dyn Fn() -> anyhow::Result<crate::agent::LaunchSpec>| {
            let mut observer = crate::loading::observe::MaterializeObserver::from_steps(&seed);
            let ptx = ptx.clone();
            let wk = wk.clone();
            let gname = gname.clone();
            let _sink = thegn_core::progress::scoped(Box::new(move |ev| {
                let _ = ptx.send((gname.clone(), ti, observer.on_event(ev)));
                let _ = wk.wake();
            }));
            f()
        };
        let specs = if is_terminal {
            let (conn, sandbox) = crate::run::terminal_launch_for(&gname);
            let spec = crate::panes::terminal_launch_spec(&cfg, &conn, &sandbox);
            Ok(missing.into_iter().map(|id| (id, spec.clone())).collect())
        } else if let Some(halt) = crate::agent::env_halt_reason(&cfg, &wt) {
            // Non-local env, failover off, known-down (token unset / exec
            // cooldown): halt rather than degrade to host.
            Err(SpecError::Halt(halt))
        } else {
            // FAST PATH: claim a pre-provisioned warm spare for this
            // (repo, env) — an instant hand-over (bind + branch checkout)
            // instead of a from-scratch provision. Skipped while a provision
            // for this worktree is already in flight (eager) — the claim
            // would clear the live splash and flip the binding under it.
            // Falls through to a full provision when no spare is ready (which
            // serializes on the per-sandbox lock and marker-short-circuits).
            if crate::provision_gate::try_claim_spare(&cfg, &wt) {
                // Bound to a ready spare — clear any loading lock and open the
                // pane straight against it (no provisioning).
                tracing::debug!(
                    target: "thegn::loading",
                    worktree = %gname,
                    "splash cleared: warm spare claimed (no provisioning)"
                );
                let _ = ptx.send((gname.clone(), ti, Vec::new()));
                let _ = wk.wake();
                observed(&|| {
                    crate::direnv_warm::launch_spec_synced(&cfg, &wt, None, "shell")
                })
                .map(|spec| missing.iter().map(|id| (*id, spec.clone())).collect())
                .map_err(spec_err)
            } else {
                // Provision the env first (provider only; no-op otherwise):
                // clone the repo + reproduce the declared toolchain + personal
                // layer, streaming live steps to the splash. Then resolve the
                // pane's launch spec so the pane only attaches once the env is
                // ready.
                let gname_p = gname.clone();
                let ptx_p = ptx.clone();
                let wk_p = wk.clone();
                let prov = crate::host_flow::provision_worktree(
                    &cfg,
                    &wt,
                    crate::host_flow::ConsentPolicy::Interactive,
                    |views| {
                        let _ = ptx_p.send((gname_p.clone(), ti, provision_load_steps(views)));
                        let _ = wk_p.wake();
                    },
                    hui.as_ref(),
                );
                match prov {
                    Ok(_) => observed(&|| {
                        crate::direnv_warm::launch_spec_synced(&cfg, &wt, None, "shell")
                    })
                    .map(|spec| missing.iter().map(|id| (*id, spec.clone())).collect())
                    .map_err(spec_err),
                    Err(e) => Err(match crate::handlers::provision::sandbox_halt_in(&e) {
                        Some(h) => SpecError::Halt(h.clone()),
                        None => SpecError::Other(format!("environment setup failed: {e}")),
                    }),
                }
            }
        };
        if spec_tx
            .send((gname, wt, ti, SpecOrigin::Materialize, specs))
            .is_ok()
        {
            let _ = wk.wake();
        }
    });
}
