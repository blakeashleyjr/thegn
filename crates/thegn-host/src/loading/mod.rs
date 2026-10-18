//! Loading-splash decision logic: the pure helpers behind the per-worktree
//! provisioning splash (`loading_state` in the run loop) — watchdog deadlines,
//! step-shape classification, provision→splash step mapping, and the
//! prewarm-vs-materialize arbitration that keeps a pre-warm from attaching a
//! pane to a half-provisioned sandbox.

pub mod catalog;
pub mod observe;
pub mod plan;
pub mod screen;
pub mod ticker;
pub mod track;

use crate::chrome::LoadStep;

/// The startup-shell watchdog deadline: how long a just-attached login shell may
/// emit no PTY output before it's treated as hung and swapped for a clean rc-free
/// shell. A LOCAL shell should prompt in ~1-2s, so 8s is snappy-but-safe. A
/// REMOTE/provider pane (`remote_env == true`) legitimately stays silent for far
/// longer — opening a provider exec, resuming a cold sandbox, and (the big one) a
/// first `direnv`/`nix develop` that builds the repo's devShell can sit silent for
/// minutes — so it gets a generous 300s window; a genuinely hung remote shell still
/// falls back, just later. Pure so the remote-vs-local choice is unit-tested (it
/// regressed once when driven off a per-path DB blob that read empty for sprites).
pub(crate) fn watchdog_deadline(remote_env: bool) -> std::time::Duration {
    std::time::Duration::from_secs(if remote_env { 300 } else { 8 })
}

/// The startup-shell watchdog deadline for a tab, driven by the per-tab
/// remoteness captured at `loading_state` seed time (NOT the derived step Vec,
/// which is byte-identical across a local and a remote shell-wait tab and
/// previously leaked the wrong deadline across a tab switch). A MISSING entry
/// defaults to the SAFE long window: unknown remoteness must never
/// premature-drop a slowly-resuming sprite; a genuinely hung *local* shell is
/// only delayed, not spared.
pub(crate) fn active_watchdog_deadline(
    remote: &std::collections::HashMap<(String, usize), bool>,
    key: &(String, usize),
) -> std::time::Duration {
    watchdog_deadline(remote.get(key).copied().unwrap_or(true))
}

/// Effective watchdog deadline for a tab, applying the remote one-time
/// extension. The machine0 readiness gate now spends bounded silent time
/// bringing a parked/cold VM back before the shell prompts, and that wait counts
/// against `pane_age` — so a remote tab that already spent its one extension must
/// clear a DOUBLED window before the fallback strips the user's shell for a
/// rc-free one. A local tab (or a remote tab pre-extension) uses `base`. Pure so
/// the extend-once policy is unit-tested alongside `active_watchdog_deadline`.
pub(crate) fn effective_watchdog_deadline(
    base: std::time::Duration,
    remote: bool,
    already_extended: bool,
) -> std::time::Duration {
    if remote && already_extended {
        base.saturating_mul(2)
    } else {
        base
    }
}

/// On a watchdog expiry, whether to EXTEND the deadline once more (`true`)
/// instead of falling back to a clean shell. Only a remote tab that has not yet
/// extended earns the extension; a local tab, or a remote tab that already
/// extended, falls back. Pairs with [`effective_watchdog_deadline`].
pub(crate) fn watchdog_should_extend(remote: bool, already_extended: bool) -> bool {
    remote && !already_extended
}

/// Whether a loading-step list is in the terminal "waiting on the shell" shape —
/// sandbox + container done, the final `shell` step pending/active — as opposed to
/// a still-live provisioning sequence (`workspace`/`clone`/`nix`/`setup`/
/// `agents`/`atuin`/…). Both the materialize
/// (`[sandbox, container, shell]`) and eager-provision streams flow through
/// `load_steps`; the difference is the LAST step's label. Used to gate actions
/// that must only fire once provisioning is DONE and we're merely waiting on the
/// shell to speak: the startup-shell watchdog (catch a hung *login shell*, not a
/// slow provision) and the first-output splash-clear (never drop the splash
/// mid-provision → a premature/bare shell).
pub(crate) fn is_shell_wait(steps: &[LoadStep]) -> bool {
    steps.last().is_some_and(|s| s.label == "shell")
}

/// Map provisioning step views (from the off-loop env provisioner) into the
/// splash's [`LoadStep`]s for a live "setting up your environment" loading screen.
pub(crate) fn provision_load_steps(views: &[crate::agent::ProvisionStepView]) -> Vec<LoadStep> {
    use crate::agent::ProvisionState;
    use crate::chrome::StepState;
    let mut plan = plan::LoadPlan::new();
    for v in views {
        let state = match v.state {
            ProvisionState::Pending => StepState::Pending,
            ProvisionState::Active => StepState::Active,
            ProvisionState::Done => StepState::Done,
            ProvisionState::Failed => StepState::Failed,
        };
        plan = match &v.detail {
            Some(d) => plan.step_detail(v.label.clone(), state, d.clone()),
            None => plan.step(v.label.clone(), state),
        };
    }
    // Provisioning steps share one kind: the tracker matches them by label
    // (stable within a stream) and the splash picks the cold-boot slow hint.
    plan.into_steps()
        .into_iter()
        .map(|s| s.with_kind(crate::chrome::StepKind::Provision))
        .collect()
}

/// Whether the materialize path may seed its `[sandbox, container, shell]`
/// splash over the tab's current `loading_state` entry. Seeding over LIVE
/// provisioning steps (an eager stream that already owns the key) would
/// briefly flip the splash into the shell-wait shape — misarming the
/// first-output clear / watchdog and hiding the real progress until the next
/// step lands. Absent, empty (the lingering eager-success park), or an old
/// shell-wait entry ⇒ seed; live steps ⇒ keep them.
pub(crate) fn seed_materialize_steps(existing: Option<&[LoadStep]>) -> bool {
    !existing.is_some_and(|s| !s.is_empty() && !is_shell_wait(s))
}

/// Which flow requested a spec batch. Tags every `SpecBatch` so the `spec_rx`
/// handler clears only the matching inflight set and can DROP a stale prewarm
/// result that would otherwise attach a pane to a half-provisioned sandbox
/// (the premature-shell bug: switch away from a provisioning worktree, its
/// neighbor prewarm resolves a spec against the bare sprite, and the pane
/// opens on a not-ready shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecOrigin {
    /// The focused lazy-materialize path (incl. warm-spare claim + provision).
    Materialize,
    /// The neighbor/sibling pre-warm path.
    Prewarm,
}

/// True when a materialize/provision currently OWNS this tab's bring-up: its
/// key is in `materialize_inflight`, or `loading_state` holds LIVE (non-empty,
/// non-shell-wait) provisioning steps. EMPTY steps do NOT own — the
/// eager-success and warm-spare paths park an empty Vec in `loading_state`
/// (via `provision_rx`) that can linger for the whole session, and it must not
/// block pre-warm of a ready tab.
pub(crate) fn provision_owns_tab(
    materialize_inflight: bool,
    loading_steps: Option<&[LoadStep]>,
) -> bool {
    materialize_inflight || loading_steps.is_some_and(|s| !s.is_empty() && !is_shell_wait(s))
}

/// Whether a landed spec batch should be applied (spawn panes, update the
/// splash) or dropped. A prewarm batch loses to an owning provision: applying
/// it would flip the splash to the shell-wait shape and attach a premature
/// bare shell. Materialize batches always apply — they ARE the owner.
/// Deliberately NOT gated on tab activity: the user may have switched back
/// before the stale prewarm lands.
pub(crate) fn apply_spec_batch(
    origin: SpecOrigin,
    materialize_inflight: bool,
    loading_steps: Option<&[LoadStep]>,
) -> bool {
    origin == SpecOrigin::Materialize || !provision_owns_tab(materialize_inflight, loading_steps)
}

/// Test helper for whether any shell-wait splash remains clearable.
#[cfg(test)]
pub(crate) fn any_clearable_splash(
    loading_state: &std::collections::HashMap<(String, usize), Vec<LoadStep>>,
) -> bool {
    loading_state.values().any(|s| is_shell_wait(s))
}

/// Whether any non-empty splash needs pane-output association. Unlike
/// [`any_clearable_splash`], this includes live provisioning entries so output
/// that arrives before the shell-wait update can be remembered.
pub(crate) fn any_unspoken_splash(
    loading_state: &std::collections::HashMap<(String, usize), Vec<LoadStep>>,
    retired: &std::collections::HashSet<(String, usize)>,
) -> bool {
    loading_state
        .iter()
        .any(|(key, steps)| !steps.is_empty() && !retired.contains(key))
}

/// Whether a pane's PTY output should clear its worktree/tab's loading splash:
/// true ONLY when this exact key holds a splash in the terminal shell-wait shape
/// (provisioning DONE, merely waiting on the login shell to speak). A missing
/// key, a lingering empty marker, or live-provision steps all return `false` —
/// so mid-provision output never drops the splash (the premature-shell guard),
/// and the clear (with its diagnostic log) fires once, not per output chunk.
pub(crate) fn should_clear_splash_on_output(
    loading_state: &std::collections::HashMap<(String, usize), Vec<LoadStep>>,
    key: &(String, usize),
) -> bool {
    loading_state.get(key).is_some_and(|s| is_shell_wait(s))
}

/// What a provisioning update does after PTY output was observed for the tab.
/// A live provisioning shape stays visible until it reaches its terminal
/// shell-wait/empty shape; then the splash is dismissed. An absent entry means
/// the splash was already retired, so late updates stay suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvisionSplashUpdate {
    Apply,
    Clear,
    Ignore,
}

/// Whether `key` has not yet recorded shell output. The retirement set is
/// cleared by a fresh materialize so a genuine re-provision can show its splash.
pub(crate) fn splash_update_allowed(
    retired: &std::collections::HashSet<(String, usize)>,
    key: &(String, usize),
) -> bool {
    !retired.contains(key)
}

pub(crate) fn provision_splash_update(
    retired: &std::collections::HashSet<(String, usize)>,
    key: &(String, usize),
    current: Option<&[LoadStep]>,
    incoming: &[LoadStep],
) -> ProvisionSplashUpdate {
    if splash_update_allowed(retired, key) {
        return ProvisionSplashUpdate::Apply;
    }
    if current.is_none() {
        return ProvisionSplashUpdate::Ignore;
    }
    if incoming.is_empty() || is_shell_wait(incoming) {
        ProvisionSplashUpdate::Clear
    } else {
        ProvisionSplashUpdate::Apply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_deadline_gives_provider_panes_the_long_window() {
        // Local shell: snappy 8s. A remote/provider (sprite) pane: 300s, so a cold
        // resume + devShell build isn't mistaken for a hung shell (the restart→bash
        // regression). The bool is the config-resolved placement signal.
        assert_eq!(watchdog_deadline(false), std::time::Duration::from_secs(8));
        assert_eq!(watchdog_deadline(true), std::time::Duration::from_secs(300));
        assert!(watchdog_deadline(true) > watchdog_deadline(false));
    }

    #[test]
    fn active_watchdog_deadline_is_per_tab_not_step_shape() {
        // The premature-shell regression: two shell-wait tabs whose LoadStep Vecs
        // are byte-identical (`[sandbox, container, shell]`) must STILL get the
        // right deadline from the per-tab remoteness bool — the deadline must not
        // ride the (identical) step shape / a stale `load_context`.
        let mut remote: std::collections::HashMap<(String, usize), bool> =
            std::collections::HashMap::new();
        let local = ("wt-local".to_string(), 0usize);
        let sprite = ("wt-sprite".to_string(), 0usize);
        remote.insert(local.clone(), false);
        remote.insert(sprite.clone(), true);
        assert_eq!(
            active_watchdog_deadline(&remote, &local),
            std::time::Duration::from_secs(8),
            "a local tab keeps the snappy 8s"
        );
        assert_eq!(
            active_watchdog_deadline(&remote, &sprite),
            std::time::Duration::from_secs(300),
            "a sprite tab keeps the long 300s even next to a local tab with matching steps"
        );
        // Missing entry ⇒ SAFE long window: never premature-drop an unknown pane.
        let unknown = ("wt-unknown".to_string(), 0usize);
        assert_eq!(
            active_watchdog_deadline(&remote, &unknown),
            std::time::Duration::from_secs(300),
            "a missing entry defaults to the safe 300s window"
        );
    }

    #[test]
    fn remote_watchdog_extends_once_before_falling_back() {
        let base = std::time::Duration::from_secs(300);
        // Local: no extension — expiry falls back immediately.
        assert!(!watchdog_should_extend(false, false));
        assert_eq!(effective_watchdog_deadline(base, false, false), base);
        // Remote, pre-extension: earns exactly one extension, still on the base
        // window until it fires.
        assert!(watchdog_should_extend(true, false));
        assert_eq!(effective_watchdog_deadline(base, true, false), base);
        // Remote, post-extension: no further extension, and the window doubles so
        // the tab isn't re-judged 500ms later.
        assert!(!watchdog_should_extend(true, true));
        assert_eq!(
            effective_watchdog_deadline(base, true, true),
            base.saturating_mul(2)
        );
    }

    #[test]
    fn is_shell_wait_only_in_the_shell_attach_shape() {
        // Nothing / live provisioning steps ⇒ NOT shell-wait: the splash must stay
        // up and the first-output clear must be held (premature-shell guard).
        assert!(!is_shell_wait(&[]));
        assert!(!is_shell_wait(&[LoadStep::active("provisioning")]));
        assert!(!is_shell_wait(&[
            LoadStep::done("nix"),
            LoadStep::active("direnv"),
        ]));
        // Terminal materialize shape (sandbox+container done, waiting on the shell)
        // ⇒ shell-wait: ok to clear on first output / arm the watchdog.
        assert!(is_shell_wait(&[
            LoadStep::done("sandbox"),
            LoadStep::active("container"),
            LoadStep::pending("shell"),
        ]));
    }

    #[test]
    fn provision_owns_tab_holds_prewarm_off_a_provisioning_tab() {
        // An in-flight materialize owns the tab even before its first steps land.
        assert!(provision_owns_tab(true, None));
        // The eager provisioner's splash-lock owns it.
        assert!(provision_owns_tab(
            false,
            Some(&[LoadStep::active("provisioning")])
        ));
        // Live provisioning steps (clone/nix/direnv/…) own it.
        assert!(provision_owns_tab(
            false,
            Some(&[LoadStep::done("nix"), LoadStep::active("direnv")])
        ));
    }

    #[test]
    fn provision_owns_tab_releases_when_no_provision_is_live() {
        // No splash at all: nothing owns the tab.
        assert!(!provision_owns_tab(false, None));
        // The lingering EMPTY entry the eager-success / warm-spare paths park in
        // `loading_state` must not block prewarm for the rest of the session.
        assert!(!provision_owns_tab(false, Some(&[])));
        // Shell-wait shape: provisioning is done, only the shell is pending —
        // a landed spec may apply.
        assert!(!provision_owns_tab(
            false,
            Some(&[
                LoadStep::done("sandbox"),
                LoadStep::done("container"),
                LoadStep::active("shell"),
            ])
        ));
    }

    #[test]
    fn materialize_seed_never_overwrites_live_provision_steps() {
        // No entry / the lingering empty park / a stale shell-wait shape ⇒ the
        // materialize splash may seed.
        assert!(seed_materialize_steps(None));
        assert!(seed_materialize_steps(Some(&[])));
        assert!(seed_materialize_steps(Some(&[
            LoadStep::done("sandbox"),
            LoadStep::done("container"),
            LoadStep::active("shell"),
        ])));
        // LIVE provisioning steps (eager owns the key) must be kept — seeding
        // would flip the splash to the shell-wait shape mid-provision.
        assert!(!seed_materialize_steps(Some(&[LoadStep::active(
            "provisioning"
        )])));
        assert!(!seed_materialize_steps(Some(&[
            LoadStep::done("nix"),
            LoadStep::active("direnv"),
        ])));
    }

    #[test]
    fn prewarm_spec_batch_dropped_while_materialize_owns_the_tab() {
        // The premature-shell repro, distilled: the stale prewarm result that
        // lands while the real provision is still running must be dropped.
        assert!(!apply_spec_batch(
            SpecOrigin::Prewarm,
            true,
            Some(&[LoadStep::active("direnv")])
        ));
        // Eager provisioning owns the tab without an inflight materialize.
        assert!(!apply_spec_batch(
            SpecOrigin::Prewarm,
            false,
            Some(&[LoadStep::active("provisioning")])
        ));
        // Ready tabs (no splash, or the lingering empty entry) still prewarm.
        assert!(apply_spec_batch(SpecOrigin::Prewarm, false, None));
        assert!(apply_spec_batch(SpecOrigin::Prewarm, false, Some(&[])));
        // The owner's own batch always applies — it IS the provision completing.
        assert!(apply_spec_batch(
            SpecOrigin::Materialize,
            true,
            Some(&[LoadStep::active("direnv")])
        ));
    }

    fn shell_wait() -> Vec<LoadStep> {
        vec![
            LoadStep::done("sandbox"),
            LoadStep::done("container"),
            LoadStep::active("shell"),
        ]
    }

    #[test]
    fn any_clearable_splash_ignores_empty_and_live_provision_entries() {
        use std::collections::{HashMap, HashSet};
        let mut ls: HashMap<(String, usize), Vec<LoadStep>> = HashMap::new();
        // Empty map ⇒ nothing to clear ⇒ the output drain skips the tab scan.
        assert!(!any_clearable_splash(&ls));
        // The lingering EMPTY marker the eager-success / warm-spare paths park
        // must NOT keep the drain rescanning + re-logging for the whole session
        // (the "first pane output" log-storm root cause).
        ls.insert(("wt-parked".into(), 0), Vec::new());
        assert!(
            !any_clearable_splash(&ls),
            "an empty marker is not clearable"
        );
        // Live provisioning steps are not clearable either (premature-shell guard).
        ls.insert(
            ("wt-provisioning".into(), 0),
            vec![LoadStep::done("nix"), LoadStep::active("direnv")],
        );
        assert!(
            !any_clearable_splash(&ls),
            "live-provision steps are held, not cleared, on output"
        );
        // A single shell-wait entry flips the pre-gate on.
        ls.insert(("wt-ready".into(), 0), shell_wait());
        assert!(
            any_clearable_splash(&ls),
            "a shell-wait splash makes the drain look for the owning pane"
        );

        let mut retired = HashSet::new();
        assert!(any_unspoken_splash(&ls, &retired));
        retired.insert(("wt-provisioning".to_string(), 0));
        retired.insert(("wt-ready".to_string(), 0));
        assert!(
            !any_unspoken_splash(&ls, &retired),
            "once output was recorded, later chunks skip the tab scan"
        );
    }

    #[test]
    fn provisioning_update_decision_remembers_output_until_shell_wait() {
        use std::collections::HashSet;
        let key = ("wt-fallback".to_string(), 0usize);
        let retired = HashSet::from([key.clone()]);
        let live = [LoadStep::active("probing bwrap")];
        let shell_wait = [
            LoadStep::done("sandbox"),
            LoadStep::done("container (bwrap)"),
            LoadStep::active("shell"),
        ];

        assert_eq!(
            provision_splash_update(&retired, &key, Some(&live), &live),
            ProvisionSplashUpdate::Apply,
            "output during fallback must not dismiss live provisioning progress"
        );
        assert_eq!(
            provision_splash_update(&retired, &key, Some(&live), &shell_wait),
            ProvisionSplashUpdate::Clear,
            "the update reaching shell-wait retires the already-spoken shell's splash"
        );
        assert_eq!(
            provision_splash_update(&retired, &key, None, &shell_wait),
            ProvisionSplashUpdate::Ignore,
            "a previously removed splash still drops late updates"
        );

        let mut fresh_retired = retired;
        fresh_retired.remove(&key);
        assert_eq!(
            provision_splash_update(&fresh_retired, &key, None, &live),
            ProvisionSplashUpdate::Apply,
            "fresh materialize un-retires a genuine re-provision"
        );
    }

    #[test]
    fn drain_provision_clears_shell_wait_after_output_arrived_during_fallback() {
        let key = ("app/home".to_string(), 0usize);
        let session = crate::session::Session {
            id: "s1".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "app/home",
                crate::session::GroupKind::Home,
                "",
            )],
            active: 0,
        };
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let live = vec![LoadStep::active("probing bwrap")];
        loading_state.set(key.clone(), live.clone());
        let mut loading_remote = std::collections::HashMap::from([(key.clone(), true)]);
        // handle_output records this key when output beats the shell-wait
        // update while the pane is already associated with its tab.
        let retired = std::collections::HashSet::from([key.clone()]);
        let terminal = vec![
            LoadStep::done("sandbox"),
            LoadStep::done("container (bwrap)"),
            LoadStep::active("shell"),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut perf = crate::perf::LoopPerf::new();

        tx.send((key.0.clone(), key.1, live)).unwrap();
        let live_dirty = crate::handlers::provision::drain_provision(
            &mut rx,
            &session,
            &mut loading_state,
            &mut loading_remote,
            &retired,
            &mut perf,
        );
        assert!(live_dirty, "live fallback progress remains visible");
        assert!(
            loading_state.contains_key(&key),
            "output during provisioning does not clear live steps"
        );

        tx.send((key.0.clone(), key.1, terminal)).unwrap();
        let dirty = crate::handlers::provision::drain_provision(
            &mut rx,
            &session,
            &mut loading_state,
            &mut loading_remote,
            &retired,
            &mut perf,
        );

        assert!(dirty, "the active splash removal dirties the frame");
        assert!(
            loading_state.is_empty(),
            "the stuck splash entry is removed"
        );
        assert!(
            loading_remote.is_empty(),
            "the watchdog remoteness is removed"
        );
        assert!(
            !any_clearable_splash(&loading_state),
            "the splash ticker has no clearable loading entry to display"
        );
    }

    #[test]
    fn should_clear_splash_on_output_is_keyed_and_shell_wait_only() {
        use std::collections::HashMap;
        let mut ls: HashMap<(String, usize), Vec<LoadStep>> = HashMap::new();
        let ready = ("wt-ready".to_string(), 0usize);
        let parked = ("wt-parked".to_string(), 0usize);
        let provisioning = ("wt-provisioning".to_string(), 1usize);
        ls.insert(ready.clone(), shell_wait());
        ls.insert(parked.clone(), Vec::new());
        ls.insert(provisioning.clone(), vec![LoadStep::active("provisioning")]);

        // Only the shell-wait key clears on its shell's first byte.
        assert!(should_clear_splash_on_output(&ls, &ready));
        // A lingering empty marker never clears (it isn't a live splash).
        assert!(!should_clear_splash_on_output(&ls, &parked));
        // Mid-provision output is HELD — clearing here is the premature-shell bug.
        assert!(!should_clear_splash_on_output(&ls, &provisioning));
        // A key with no entry (a pane whose worktree has no splash) never clears —
        // and, crucially, never logs "first pane output" on every chunk.
        assert!(!should_clear_splash_on_output(
            &ls,
            &("wt-absent".to_string(), 0usize)
        ));
        // The clearable key AND tab-index must both match: same name, wrong tab ⇒ no.
        assert!(!should_clear_splash_on_output(
            &ls,
            &("wt-ready".to_string(), 9usize)
        ));
    }
}
