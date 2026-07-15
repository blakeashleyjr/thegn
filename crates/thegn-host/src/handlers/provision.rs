//! Loop-side drains for the off-thread provisioning channels: live
//! env-provisioning progress (`ProvisionProgress`) and resolved launch specs
//! (`SpecBatch`). Extracted verbatim from the `run.rs` event loop; the drains
//! mutate loop state through [`SpecDrainCtx`] and never do I/O.

use crate::chrome::LoadStep;
use crate::compositor::Rect;
use crate::loading::{SpecOrigin, apply_spec_batch};
use crate::menu::{self, MenuOverlay};
use thegn_core::store::{PoolStore, WorkspaceStore};

/// Resolved launch specs routed back to the requesting group by its unique
/// NAME (a path can be shared by two groups); the batch also carries the path
/// so the spawn (cwd) still lands in the worktree dir.
pub(crate) type SpecBatch = (
    String,     // group name (routing key)
    String,     // worktree path (spawn cwd)
    usize,      // tab index
    SpecOrigin, // which inflight set to clear; prewarm batches may be dropped
    std::result::Result<Vec<(u32, crate::agent::LaunchSpec)>, SpecError>,
);

/// Live env-provisioning progress for a tab's splash, keyed (group name, tab).
pub(crate) type ProvisionProgress = (String, usize, Vec<LoadStep>);

/// Error carried over the off-thread spec-resolution channel. Preserves a
/// [`SandboxHalt`](crate::agent::SandboxHalt) (so the receiver can raise the
/// warning modal) and stringifies everything else.
pub(crate) enum SpecError {
    Halt(crate::agent::SandboxHalt),
    Other(String),
    /// Benign pre-warm skip (provider env not provisioned yet). Clears
    /// `prewarm_inflight` on arrival; paints NO failed splash and sets NO
    /// failed mark — the tab is simply left for materialize to bring up.
    PrewarmSkipped,
}

/// Lower an `anyhow::Error` from `launch_spec` into a channel-safe [`SpecError`],
/// keeping the typed halt when present.
pub(crate) fn spec_err(e: anyhow::Error) -> SpecError {
    match sandbox_halt_in(&e) {
        Some(h) => SpecError::Halt(h.clone()),
        None => SpecError::Other(e.to_string()),
    }
}

/// Find a [`SandboxHalt`](crate::agent::SandboxHalt) anywhere in an error's
/// source chain (robust to `.context()` wrapping). `Some` ⇒ surface the modal
/// instead of a plain status line.
pub(crate) fn sandbox_halt_in(e: &anyhow::Error) -> Option<&crate::agent::SandboxHalt> {
    e.chain()
        .find_map(|src| src.downcast_ref::<crate::agent::SandboxHalt>())
}

pub(crate) fn sandbox_halt_overlay(halt: &crate::agent::SandboxHalt) -> MenuOverlay {
    let title = format!("⚠ {} unavailable", halt.placement);
    let body = format!(
        "{} — failover is off, so thegn won't drop to the host. Fix the env \
         (e.g. set its token) then retry, or set `failover = true` under \
         [env.{}] / [sandbox] to allow a host fallback.",
        halt.reason, halt.env_name
    );
    menu::sandbox_halt_menu(title, body)
}

/// The event-loop locals the spec drain mutates. All are distinct `run()`
/// locals, borrowed for the duration of one drain call.
pub(crate) struct SpecDrainCtx<'a> {
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut crate::panes::Panes,
    pub model: &'a mut crate::chrome::FrameModel,
    pub active_menu: &'a mut Option<MenuOverlay>,
    pub current_config: &'a thegn_core::config::Config,
    pub center: Rect,
    pub loading_state: &'a mut crate::loading::track::LoadingTracker,
    pub loading_remote: &'a mut std::collections::HashMap<(String, usize), bool>,
    pub materialize_inflight: &'a mut std::collections::HashSet<(String, usize)>,
    pub prewarm_inflight: &'a mut std::collections::HashSet<(String, usize)>,
    pub materialize_failed: &'a mut std::collections::HashSet<(String, usize)>,
    pub prewarm_failed: &'a mut std::collections::HashSet<(String, usize)>,
    /// Keys whose sandbox-halt modal was already dismissed: the modal is raised
    /// at most once per key, so a re-materialize of a still-broken env doesn't
    /// re-block the user (the row's error dot carries the state instead).
    pub halt_dismissed: &'a mut std::collections::HashSet<(String, usize)>,
    pub last_pool_reconcile: &'a mut Option<std::time::Instant>,
    pub center_dormant: &'a mut bool,
    pub need_relayout: &'a mut bool,
    pub dirty: &'a mut bool,
    pub loop_perf: &'a mut crate::perf::LoopPerf,
}

/// Eager provisioning (`[lifecycle] eager`): front-run the one-time
/// provisioning (nix + devShell, minutes — and now host bring-up) for
/// worktrees AHEAD of focus, in the background, so opening them is instant.
/// Budget-safe: `provision_pending` only fires when the sandbox does NOT
/// exist yet (a list() GET — never wakes an idle/provisioned one), and
/// `host_pending` is one DB read. Scope: active worktree only, or the whole
/// session (workspace/all). Once per session per worktree. Background policy:
/// a host needing install consent is DEFERRED, never prompted.
pub(crate) fn kick_eager(
    cfg: &thegn_core::config::Config,
    session: &crate::session::Session,
    eager_inflight: &mut std::collections::HashSet<String>,
    provision_tx: &tokio::sync::mpsc::UnboundedSender<ProvisionProgress>,
    waker: &termwiz::terminal::TerminalWaker,
    host_ui: &crate::host_flow::HostUiTx,
) {
    use thegn_core::config::EagerScope;
    let scope = cfg.lifecycle.eager;
    if !cfg.lifecycle.enabled || scope == EagerScope::Off {
        return;
    }
    let active_path = session.active_group().map(|g| g.path.clone());
    // (path, group name, active tab) so the background provisioner can report
    // progress into `loading_state` under the SAME key the splash derives from
    // — switching to a still-provisioning worktree then shows its live loading
    // screen, never a premature shell.
    let targets: Vec<(String, String, usize)> = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .filter(|g| match scope {
            EagerScope::ActiveWorktreePlusNew => active_path.as_deref() == Some(g.path.as_str()),
            _ => true, // ActiveWorkspace / All: every open worktree
        })
        .map(|g| (g.path.clone(), g.name.clone(), g.active_tab))
        .collect();
    for (wt, gname, ti) in targets {
        if !eager_inflight.insert(wt.clone()) {
            continue; // already attempted this session
        }
        let cfg = cfg.clone();
        let wk = waker.clone();
        let ptx = provision_tx.clone();
        let hui = host_ui.clone();
        tokio::task::spawn_blocking(move || {
            if crate::agent::provision_pending(&cfg, &wt)
                || crate::host_flow::host_pending(&cfg, &wt)
            {
                // Lock the loading screen up the moment we commit to
                // provisioning (before the first step), so switching here can
                // never catch a half-ready shell — the splash wins while
                // `loading_state` is non-empty.
                let _ = ptx.send((gname.clone(), ti, vec![LoadStep::active("provisioning")]));
                let _ = wk.wake();
                let prov = crate::host_flow::provision_worktree(
                    &cfg,
                    &wt,
                    crate::host_flow::ConsentPolicy::BackgroundSkip,
                    |views| {
                        let _ = ptx.send((
                            gname.clone(),
                            ti,
                            crate::loading::provision_load_steps(views),
                        ));
                        let _ = wk.wake();
                    },
                    Some(&hui),
                );
                // On success, clear the lock so a now-ready worktree shows no
                // stale splash (materialize takes over on open). On failure,
                // leave the failed steps visible.
                if prov.is_ok() {
                    let _ = ptx.send((gname.clone(), ti, Vec::new()));
                }
                let _ = wk.wake();
            }
        });
    }
}

/// Drain live env-provisioning progress for the active tab → the splash. The
/// off-loop spec task streams setup steps while a fresh provider sandbox is
/// built; only the active tab's stream drives the loading screen. Returns
/// whether the frame went dirty.
pub(crate) fn drain_provision(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ProvisionProgress>,
    session: &crate::session::Session,
    loading_state: &mut crate::loading::track::LoadingTracker,
    loading_remote: &mut std::collections::HashMap<(String, usize), bool>,
    loading_retired: &std::collections::HashSet<(String, usize)>,
    loop_perf: &mut crate::perf::LoopPerf,
) -> bool {
    let mut dirty = false;
    while let Ok((name, ti, steps)) = rx.try_recv() {
        loop_perf.tick(crate::perf::WakeSource::Spec);
        let key = (name, ti);
        // A tab whose shell has already spoken has RETIRED its splash: a late
        // provisioning update (queued before the first-output clear) must not
        // re-raise it over the live shell — the flash-back-to-splash flicker.
        if !crate::loading::splash_update_allowed(loading_retired, &key) {
            continue;
        }
        // Always record this worktree's progress; the splash for whichever
        // worktree is active is derived from `loading_state` below.
        let active = session
            .worktrees
            .iter()
            .position(|g| g.name == key.0)
            .is_some_and(|gi| gi == session.active && session.worktrees[gi].active_tab == key.1);
        // This channel used to carry ONLY provider (remote) streams, so
        // remoteness was hardcoded `true`. The materialize observer now
        // streams local bring-ups here too — its seed site already recorded
        // the correct per-tab remoteness, which must NOT be clobbered (a
        // local tab marked remote gets the 300s watchdog, letting a genuinely
        // hung local shell linger for minutes). Default only a MISSING entry
        // to the safe long window.
        loading_remote.entry(key.clone()).or_insert(true);
        loading_state.set(key, steps);
        if active {
            dirty = true;
        }
    }
    dirty
}

/// Drain resolved launch specs: finish the deferred materialize (lazy focus
/// path and pre-warm alike). Results for a group/tab that vanished mid-flight
/// are dropped, and `materialize_with_specs` itself skips leaves that came
/// alive some other way in the meantime.
pub(crate) fn drain_specs(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SpecBatch>,
    ctx: &mut SpecDrainCtx<'_>,
) {
    while let Ok((name, wt, ti, origin, specs)) = rx.try_recv() {
        ctx.loop_perf.tick(crate::perf::WakeSource::Spec);
        let tab_key = (name.clone(), ti);
        match origin {
            SpecOrigin::Materialize => {
                ctx.materialize_inflight.remove(&tab_key);
            }
            SpecOrigin::Prewarm => {
                ctx.prewarm_inflight.remove(&tab_key);
            }
        }
        // A stale PREWARM batch that lands while a materialize/provision owns
        // this tab (its provision is still running) must be dropped whole:
        // applying it would flip the splash to the shell-wait shape and attach
        // a pane to a half-provisioned sandbox — the premature-shell bug. Not
        // gated on tab activity: the user may have already switched back.
        if !apply_spec_batch(
            origin,
            ctx.materialize_inflight.contains(&tab_key),
            ctx.loading_state.get(&tab_key).map(Vec::as_slice),
        ) {
            continue;
        }
        // Remoteness of this worktree, resolved once from its path — seeds
        // `loading_remote` alongside every `loading_state` write below so the
        // startup-shell watchdog reads the correct 8s/300s deadline for THIS
        // tab regardless of its (possibly identical) step Vec.
        let tab_remote =
            thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt)).is_remote();
        // A remote materialize may have just CLAIMED a warm spare (ready →
        // claimed), so the chip's count is now stale. Force the pool
        // maintainer to re-read on the next loop (it's otherwise throttled to
        // ~8s) so `warm N/M` drops promptly instead of showing a phantom
        // ready spare that was already handed off.
        if tab_remote {
            *ctx.last_pool_reconcile = None;
        }
        let Some(gi) = ctx.session.worktrees.iter().position(|g| g.name == name) else {
            continue;
        };
        let is_active = gi == ctx.session.active && ctx.session.worktrees[gi].active_tab == ti;
        if is_active && *ctx.center_dormant {
            continue; // splash still up: stay lazy
        }
        let specs = match specs {
            Ok(specs) => {
                // Provisioning finished — only the shell attach remains.
                // Advance the tab's live plan (a rich backend-aware list keeps
                // its rows + timings); a missing/stale entry falls back to the
                // classic three-step shape inside `advance_to_shell`.
                let backend = specs
                    .first()
                    .map(|(_, s)| s.backend.clone())
                    .unwrap_or_else(|| "host".into());
                ctx.loading_remote.insert(tab_key.clone(), tab_remote);
                ctx.loading_state
                    .advance_to_shell(tab_key.clone(), &backend);
                if is_active {
                    *ctx.dirty = true;
                }
                specs
            }
            Err(e) => {
                // Failover off + non-local env couldn't come up: raise the
                // warning modal instead of just a status line. Otherwise a
                // plain blocked-launch status.
                let err_detail = match e {
                    // Benign prewarm skip (provider env not provisioned yet):
                    // no failed splash, no failed mark — the tab is left for
                    // the focused materialize to provision + open.
                    SpecError::PrewarmSkipped => continue,
                    SpecError::Halt(halt) => {
                        ctx.model.status =
                            format!("{} unavailable: {}", halt.placement, halt.reason);
                        // Raise the blocking modal only the FIRST time for this
                        // key; once dismissed the row's red error dot carries
                        // the state and re-visiting no longer re-blocks.
                        if is_active && !ctx.halt_dismissed.contains(&tab_key) {
                            *ctx.active_menu = Some(sandbox_halt_overlay(&halt));
                        }
                        halt.reason.clone()
                    }
                    SpecError::Other(s) => {
                        ctx.model.status = format!("Pane launch blocked: {s}");
                        s
                    }
                };
                ctx.loading_remote.insert(tab_key.clone(), tab_remote);
                // Mark the step that was actually running as failed (the live
                // plan's rows stay intact) with the error as its sub-line.
                ctx.loading_state.fail_active(tab_key.clone(), &err_detail);
                match origin {
                    SpecOrigin::Materialize => {
                        if is_active {
                            *ctx.center_dormant = true;
                        }
                        ctx.materialize_failed.insert(tab_key);
                    }
                    SpecOrigin::Prewarm => {
                        ctx.prewarm_failed.insert(tab_key);
                    }
                }
                *ctx.dirty = true;
                continue;
            }
        };
        let warnings = specs
            .iter()
            .filter_map(|(_, spec)| spec.warning_summary())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let Some(tab) = ctx.session.worktrees[gi].tabs.get_mut(ti) else {
            continue;
        };
        {
            // Everything but the shell attach is done → shell active. (The
            // Ok-arm already advanced the plan; this re-advance is a no-op
            // for it and covers the early-continue paths.)
            let backend = specs
                .first()
                .map(|(_, s)| s.backend.clone())
                .unwrap_or_else(|| "host".into());
            ctx.loading_remote.insert(tab_key.clone(), tab_remote);
            ctx.loading_state
                .advance_to_shell(tab_key.clone(), &backend);
        }
        if let Err(e) =
            ctx.panes
                .materialize_with_specs(ctx.current_config, tab, &wt, &specs, ctx.center)
        {
            ctx.model.status = format!("Pane spawn failed: {e}");
            ctx.loading_remote.insert(tab_key.clone(), tab_remote);
            // The shell step is the active one after `advance_to_shell`.
            ctx.loading_state
                .fail_active(tab_key.clone(), &format!("{e}"));
        } else {
            // Keep the loading entry until first PTY output arrives (cleared in
            // the PaneEvent::Output handler above) so the loading screen
            // stays visible until the shell actually produces content —
            // no blank flash between fork and first render.
            if is_active && !warnings.is_empty() {
                ctx.model.status = format!("⚠ Sandbox fallback: {}", warnings.join("; "));
            }
        }
        if is_active {
            *ctx.need_relayout = true;
        }
        *ctx.dirty = true;
    }
}

/// Adjust the warm-spare-pool target for the ACTIVE workspace's `(repo, env)` by
/// `delta`, persisting the override in the DB (the `+`/`-` hotkeys). Returns a
/// status message (the new target, or why it didn't apply). The caller resets the
/// maintainer throttle so the change takes effect immediately.
pub(crate) fn pool_target_adjust(
    session: &crate::session::Session,
    cfg: &thegn_core::config::Config,
    delta: i64,
) -> Option<String> {
    let g = session.active_group().filter(|g| !g.path.is_empty())?;
    let wt = g.path.clone();
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&wt));
    if !loc.is_remote() {
        return Some("warm pool: only for provider (remote) workspaces".into());
    }
    let db = thegn_core::db::Db::open().ok()?;
    let repo_root = db
        .repo_root_for(&wt)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            thegn_core::repo::main_worktree(std::path::Path::new(&wt))
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| wt.clone());
    let env_name = cfg
        .resolve_env(
            std::path::Path::new(&repo_root),
            &loc,
            std::path::Path::new(&wt),
            None,
        )
        .name;
    let cur = db
        .pool_target(&repo_root, &env_name)
        .ok()
        .flatten()
        .unwrap_or(cfg.lifecycle.pool.size as i64);
    let new = (cur + delta).max(0);
    db.set_pool_target(&repo_root, &env_name, new).ok()?;
    Some(format!("warm pool target: {new} spare(s)"))
}
