//! Loop-side drains for the off-thread provisioning channels: live
//! env-provisioning progress (`ProvisionProgress`) and resolved launch specs
//! (`SpecBatch`). Extracted verbatim from the `run.rs` event loop; the drains
//! mutate loop state through [`SpecDrainCtx`] and never do I/O.

use crate::chrome::LoadStep;
use crate::compositor::Rect;
use crate::handlers::worktree_attach::{self, AttachTarget};
use crate::handlers::worktree_launch::RelaunchOutcome;
use crate::loading::{SpecOrigin, apply_spec_batch};
use crate::menu::{self, MenuOverlay};

/// Resolved launch specs routed back to the requesting group by its unique
/// NAME (a path can be shared by two groups); the batch also carries the path
/// so the spawn (cwd) still lands in the worktree dir, and — attach-on-open
/// (THE-85) — the worktree's live daemon agent sessions the off-thread worker
/// probed beside the spec resolve. The drain plans those onto the tab's
/// missing leaves; shells are the fallback for whatever doesn't fit.
pub(crate) struct SpecBatch {
    /// Group name — the routing key.
    pub group: String,
    /// Worktree path (spawn cwd).
    pub worktree: String,
    /// Tab index the batch was captured for.
    pub tab: usize,
    /// Stable leaf identities captured with the request. The tab index is only
    /// a routing hint: closes can shift it onto another live tab before the
    /// worker returns, including for `PrewarmSkipped` errors with no specs.
    pub target_leaves: Vec<u32>,
    /// Which inflight set to clear; prewarm batches may be dropped.
    pub origin: SpecOrigin,
    pub specs: std::result::Result<Vec<(u32, crate::agent::LaunchSpec)>, SpecError>,
    /// The remembered-agent fold's status. `Refused` is deliberately separate
    /// from `specs: Ok`: the shell fallback remains a valid batch result, while
    /// the worker's refusal is retained for downstream bookkeeping.
    pub relaunch: RelaunchOutcome,
    /// Live daemon agent sessions for [`SpecBatch::worktree`], newest first
    /// ([`worktree_attach::live_for_worktree`]). Empty when the worker didn't
    /// probe (terminal groups, daemon route off, probe failure).
    pub attach: Vec<AttachTarget>,
    /// Whether [`SpecBatch::worktree`] lives on a non-local placement, read
    /// off-thread beside the specs ([`worktree_is_remote`]). Seeds
    /// `loading_remote` (the startup-shell watchdog's 8s/300s window) so the
    /// drain never opens SQLite on the compositor loop to find out.
    pub remote: bool,
    /// The worktree's `worktrees.agent` row, read off-thread beside the specs
    /// ([`remembered_agent_row`]). The provider native-exec/ssh restore
    /// relaunches it; absent/unreadable ⇒ the plain shell it has always fallen
    /// back to. Carried because the spawn used to open the DB once PER LEAF.
    pub remembered_agent: Option<String>,
}

/// Whether `worktree` is on a non-local placement, for a spec worker to carry
/// in [`SpecBatch::remote`]. Opens the state DB, so it belongs on the worker
/// thread — never the compositor loop.
pub(crate) fn worktree_is_remote(worktree: &str) -> bool {
    thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree)).is_remote()
}

/// The worktree's remembered agent (`worktrees.agent`), for a spec worker to
/// carry in [`SpecBatch::remembered_agent`]. Opens the state DB, so it belongs
/// on the worker thread — never the compositor loop.
///
/// Only a provider-configured host can take the native-exec/ssh restore branch
/// that reads this, so a config without one reads nothing: the same gate the
/// on-loop per-leaf read applied. The row is carried UNFILTERED — the `[tools]`
/// and `shell` exclusions depend on the config live at spawn time, so the spawn
/// keeps applying them.
pub(crate) fn remembered_agent_row(
    cfg: &thegn_core::config::Config,
    worktree: &str,
) -> Option<String> {
    use thegn_core::store::WorkspaceStore as _;

    if !crate::agent::provider_envs_configured(cfg) || worktree.is_empty() {
        return None;
    }
    thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.worktree_agent(worktree).ok().flatten())
}

/// Live env-provisioning progress for a tab's splash, keyed (group name, tab).
pub(crate) type ProvisionProgress = (String, usize, Vec<LoadStep>);

/// THE-661: most distinct `(group, tab)` keys the provisioning slot holds
/// between two loop drains. At the cap a new key evicts the oldest-first-seen
/// non-empty entry (counted as a drop); the slot never grows past it except
/// as documented on [`ProvisionTx::send`] for clear messages.
pub(crate) const MAX_PROVISION_KEYS: usize = 64;

/// THE-661: byte ceiling (measured by [`provision_entry_bytes`]) for all
/// pending provisioning entries. A non-empty entry larger than this on its
/// own is refused; otherwise the oldest other non-empty entries are evicted
/// until the slot fits again.
pub(crate) const MAX_PROVISION_BYTES: usize = 256 * 1024;

/// Approximate retained size of one pending provisioning entry: the key, the
/// step structs, and their owned strings.
fn provision_entry_bytes(msg: &ProvisionProgress) -> usize {
    msg.0.len()
        + msg
            .2
            .iter()
            .map(|step| {
                std::mem::size_of::<LoadStep>()
                    + step.label.len()
                    + step.detail.as_ref().map_or(0, String::len)
            })
            .sum::<usize>()
}

#[derive(Default)]
struct ProvisionState {
    /// Latest steps per `(group, tab)`, in first-seen key order.
    entries: Vec<ProvisionProgress>,
    bytes: usize,
    drops: u64,
}

impl ProvisionState {
    fn position(&self, group: &str, tab: usize) -> Option<usize> {
        self.entries
            .iter()
            .position(|(name, ti, _)| name == group && *ti == tab)
    }

    /// Oldest-first-seen non-empty entry, skipping `keep`.
    fn oldest_non_empty(&self, keep: Option<(&str, usize)>) -> Option<usize> {
        self.entries
            .iter()
            .position(|(name, ti, steps)| !steps.is_empty() && keep != Some((name.as_str(), *ti)))
    }

    fn evict(&mut self, index: usize) {
        let removed = self.entries.remove(index);
        self.bytes = self.bytes.saturating_sub(provision_entry_bytes(&removed));
        self.drops = self.drops.saturating_add(1);
    }
}

struct ProvisionShared {
    state: std::sync::Mutex<ProvisionState>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl ProvisionShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, ProvisionState> {
        // Same poisoning policy as `snapshot_slot`: the state is plain data
        // that every operation leaves consistent, so recover rather than
        // panic the loop.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// THE-661: sender half of the bounded provisioning-progress slot. Each send
/// stores or replaces the latest steps for its `(group, tab)` key and wakes
/// the loop only when the slot goes from empty to non-empty.
#[derive(Clone)]
pub(crate) struct ProvisionTx {
    shared: std::sync::Arc<ProvisionShared>,
}

/// THE-661: loop-side half of the provisioning slot.
pub(crate) struct ProvisionInbox {
    shared: std::sync::Arc<ProvisionShared>,
}

impl ProvisionTx {
    /// Build the slot, waking `waker` on each empty-to-non-empty change.
    pub(crate) fn channel(waker: termwiz::terminal::TerminalWaker) -> (Self, ProvisionInbox) {
        Self::with_wake(move || {
            if let Err(e) = waker.wake() {
                tracing::debug!(target: "thegn::ingress", "provision slot wake failed: {e}");
            }
        })
    }

    /// Build the slot with an arbitrary wake callback (the test seam).
    fn with_wake(wake: impl Fn() + Send + Sync + 'static) -> (Self, ProvisionInbox) {
        let shared = std::sync::Arc::new(ProvisionShared {
            state: std::sync::Mutex::new(ProvisionState::default()),
            wake: Box::new(wake),
        });
        (
            Self {
                shared: std::sync::Arc::clone(&shared),
            },
            ProvisionInbox { shared },
        )
    }

    /// Store or replace the latest steps for the message's key. Returns
    /// whether the message was accepted.
    ///
    /// An empty step list (the "clear the splash lock" signal) always lands:
    /// it replaces its key's entry, and at the key cap it evicts the oldest
    /// non-empty entry. Only when every pending entry is itself a clear does a
    /// new clear evict the oldest clear, which needs more than
    /// [`MAX_PROVISION_KEYS`] tabs finishing within one loop iteration.
    pub(crate) fn send(&self, msg: ProvisionProgress) -> bool {
        let bytes = provision_entry_bytes(&msg);
        let is_clear = msg.2.is_empty();
        let pulse = {
            let mut state = self.shared.lock();
            if !is_clear && bytes > MAX_PROVISION_BYTES {
                state.drops = state.drops.saturating_add(1);
                return false;
            }
            let was_empty = state.entries.is_empty();
            let (name, ti, steps) = msg;
            if let Some(index) = state.position(&name, ti) {
                let old = provision_entry_bytes(&state.entries[index]);
                state.bytes = state.bytes.saturating_sub(old).saturating_add(bytes);
                state.entries[index].2 = steps;
            } else {
                if state.entries.len() >= MAX_PROVISION_KEYS {
                    match state.oldest_non_empty(None) {
                        Some(victim) => state.evict(victim),
                        None if is_clear => state.evict(0),
                        None => {
                            state.drops = state.drops.saturating_add(1);
                            return false;
                        }
                    }
                }
                state.bytes = state.bytes.saturating_add(bytes);
                state.entries.push((name.clone(), ti, steps));
            }
            while state.bytes > MAX_PROVISION_BYTES {
                match state.oldest_non_empty(Some((name.as_str(), ti))) {
                    Some(victim) => state.evict(victim),
                    None => break,
                }
            }
            was_empty
        };
        if pulse {
            (self.shared.wake)();
        }
        true
    }
}

impl ProvisionInbox {
    /// Take every pending entry (first-seen key order) under the lock. A send
    /// that lands after this take finds the slot empty and wakes again.
    pub(crate) fn take_all(&self) -> Vec<ProvisionProgress> {
        let mut state = self.shared.lock();
        state.bytes = 0;
        std::mem::take(&mut state.entries)
    }

    #[cfg(test)]
    fn drops(&self) -> u64 {
        self.shared.lock().drops
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shared.lock().entries.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.shared.lock().bytes
    }
}

/// Where [`drain_provision`] takes pending progress from: the bounded slot in
/// production; an unbounded channel in older tests that build one directly.
pub(crate) trait ProvisionSource {
    fn take_pending(&mut self) -> Vec<ProvisionProgress>;
}

impl ProvisionSource for ProvisionInbox {
    fn take_pending(&mut self) -> Vec<ProvisionProgress> {
        self.take_all()
    }
}

#[cfg(test)]
impl ProvisionSource for tokio::sync::mpsc::UnboundedReceiver<ProvisionProgress> {
    fn take_pending(&mut self) -> Vec<ProvisionProgress> {
        let mut out = Vec::new();
        while let Ok(msg) = self.try_recv() {
            out.push(msg);
        }
        out
    }
}

/// Error carried over the off-thread spec-resolution channel. Preserves a
/// [`SandboxHalt`](crate::agent::SandboxHalt) (so the receiver can raise the
/// warning modal) and stringifies everything else.
#[derive(Debug)]
pub(crate) enum SpecError {
    /// Boxed: `SandboxHalt` is large and every spec closure returns this error.
    Halt(Box<crate::agent::SandboxHalt>),
    /// An unset Sprites `model_credentials` needs a one-time answer. Boxed for
    /// the same size reason as `Halt`.
    Consent(Box<crate::agent::ModelCredentialConsentRequired>),
    Other(String),
    /// Benign automatic pre-warm skip (provider env not provisioned yet, or
    /// host execution was the only fresh spec). Clears `prewarm_inflight` on
    /// arrival; paints NO failed splash and sets NO failed mark — the tab is
    /// simply left for focused materialize to bring up.
    PrewarmSkipped,
}

/// Lower an `anyhow::Error` from `launch_spec` into a channel-safe [`SpecError`],
/// keeping the typed halt or credential prompt when present.
pub(crate) fn spec_err(e: anyhow::Error) -> SpecError {
    if let Some(h) = sandbox_halt_in(&e) {
        return SpecError::Halt(Box::new(h.clone()));
    }
    if let Some(prompt) = model_credential_consent_in(&e) {
        return SpecError::Consent(Box::new(prompt.clone()));
    }
    SpecError::Other(e.to_string())
}

/// Find a [`ModelCredentialConsentRequired`](crate::agent::ModelCredentialConsentRequired)
/// anywhere in an error's source chain (robust to `.context()` wrapping).
pub(crate) fn model_credential_consent_in(
    e: &anyhow::Error,
) -> Option<&crate::agent::ModelCredentialConsentRequired> {
    e.chain()
        .find_map(|src| src.downcast_ref::<crate::agent::ModelCredentialConsentRequired>())
}

/// Find a [`SandboxHalt`](crate::agent::SandboxHalt) anywhere in an error's
/// source chain (robust to `.context()` wrapping). `Some` ⇒ surface the modal
/// instead of a plain status line.
pub(crate) fn sandbox_halt_in(e: &anyhow::Error) -> Option<&crate::agent::SandboxHalt> {
    e.chain()
        .find_map(|src| src.downcast_ref::<crate::agent::SandboxHalt>())
}

/// The sandbox-halt/ask modal's `[r] retry` and `[h] run on host` choices:
/// re-arm the active tab's lazy materialize (retry re-attempts the real env;
/// run-on-host pins it to the host for the session). Returns `true` when it
/// handled `choice` (the caller then flips `dirty` + `continue`s). `active_wt` /
/// `active_key` are the active worktree's path + `(group, tab)` key.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_sandbox_retry_choice(
    choice: &menu::MenuChoice,
    cfg: &thegn_core::config::Config,
    session: &crate::session::Session,
    active_wt: Option<&std::path::Path>,
    materialize_failed: &mut std::collections::HashSet<(String, usize)>,
    prewarm_failed: &mut std::collections::HashSet<(String, usize)>,
    halt_dismissed: &mut std::collections::HashSet<(String, usize)>,
    center_dormant: &mut bool,
    status: &mut String,
) -> bool {
    let active_key = session
        .worktrees
        .get(session.active)
        .map(|g| (g.name.clone(), g.active_tab));
    match choice {
        // Retry: clear the provider failure cooldown + any run-on-host pin so it
        // re-attempts the REAL env; if still down the blocked-launch handler
        // re-raises the modal. Keep `halt_dismissed` — a manual retry is silent.
        menu::MenuChoice::SandboxRetry => {
            if let Some(wt) = active_wt {
                let wt = wt.to_string_lossy();
                crate::agent::clear_native_exec_cooldown(cfg, &wt);
                crate::agent::clear_force_host(&wt);
            }
            if let Some(key) = active_key {
                materialize_failed.remove(&key);
                prewarm_failed.remove(&key);
            }
            *center_dormant = false;
            *status = "Retrying environment bring-up…".into();
            true
        }
        // Start the dormant runtime, then retry exactly as `SandboxRetry` does.
        // The start itself is slow (a VM boot), so it runs off the loop; the
        // probe cache is dropped first so the retry re-probes instead of
        // replaying the "absent" answer that got us here.
        menu::MenuChoice::SandboxStartRuntime(name) => {
            let name = name.clone();
            if let Some(wt) = active_wt {
                crate::sandbox_fallback_memo::clear_worktree(&wt.to_string_lossy());
            }
            *status = format!("Starting {name}…");
            thegn_core::thread_name::spawn_named("thegn-sbx-start", move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility); // Utility: user-requested runtime start.
                let argv = thegn_core::sandbox_dormant::start_argv(
                    thegn_core::sandbox::Backend::parse(&name)
                        .unwrap_or(thegn_core::sandbox::Backend::None),
                    thegn_core::sandbox_backend::host_os(),
                    &|bin: &str| thegn_core::util::have(bin),
                );
                if let Some(argv) = argv {
                    let ok = crate::sandbox_start::run(&argv);
                    thegn_core::sandbox_backend::clear_probe_cache();
                    if ok {
                        thegn_core::msg::info(&format!(
                            "{name} started — reopen the pane to use it"
                        ));
                    } else {
                        thegn_core::msg::warn(&format!(
                            "could not start {name}; start it yourself and retry"
                        ));
                    }
                }
            });
            if let Some(key) = active_key {
                materialize_failed.remove(&key);
                prewarm_failed.remove(&key);
            }
            *center_dormant = false;
            true
        }
        // Run on host: pin the worktree to the host so the respawn degrades
        // instead of re-blocking; don't re-prompt (host was chosen explicitly).
        menu::MenuChoice::SandboxRunOnHost => {
            if let Some(wt) = active_wt {
                crate::agent::request_force_host(&wt.to_string_lossy());
            }
            if let Some(key) = active_key {
                materialize_failed.remove(&key);
                prewarm_failed.remove(&key);
                halt_dismissed.insert(key);
            }
            *center_dormant = false;
            *status = "Running on the host (env unavailable) — retry from the env menu".into();
            true
        }
        _ => false,
    }
}

/// Record a `provider_degraded` notification (unread badge + routed desktop
/// toast) when a launch degraded a managed provider to the host — the transient
/// status line is easy to miss, and this matches the sidebar `«env ✗»` marker.
/// No-op unless `spec.degraded`. Called from the materialize-done path.
pub(crate) fn note_provider_degraded(
    notify: &crate::notify::NotifyState,
    branch: &str,
    path: &str,
    spec: &crate::agent::LaunchSpec,
) {
    if !spec.degraded {
        return;
    }
    let msg = spec
        .warning_summary()
        .unwrap_or_else(|| format!("{branch} running on host"));
    let dec = crate::notify::route(notify, "provider_degraded", path, &msg, path);
    if dec.record {
        let path = path.to_string();
        let routed = dec.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(db) = thegn_core::db::Db::open() {
                let _ = crate::automation_events::insert_routed(
                    &db,
                    "provider_degraded",
                    &path,
                    &msg,
                    &path,
                    Default::default(),
                    &routed,
                    false,
                ); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
        });
    }
}

fn sealed_halt_body(halt: &crate::agent::SandboxHalt) -> String {
    let fix = if halt.reason.contains("thegn sandbox profile set standard") {
        String::new()
    } else {
        format!(
            " Fix: {}",
            thegn_core::sandbox_floor::standard_switch_hint(None)
        )
    };
    format!(
        "{} — a sealed profile cannot fall back to a host shell.{}",
        halt.reason, fix
    )
}

/// Modal body for a local halt whose reason carries the no-backend fix.
fn no_backend_halt_body(halt: &crate::agent::SandboxHalt) -> String {
    format!(
        "{} — thegn will not silently run this pane on the host. [h] runs this worktree \
         on the host for this session with NO sandbox: the pane can read and write \
         everything your user can. Nothing is remembered; `thegn doctor` shows the \
         backend status. [r] retries after you install or start one.",
        halt.reason
    )
}

pub(crate) fn sandbox_halt_overlay(halt: &crate::agent::SandboxHalt) -> MenuOverlay {
    if halt.sealed_home {
        let title = format!("⚠ {} unavailable", halt.placement);
        return menu::sandbox_halt_menu(title, sealed_halt_body(halt));
    }
    // A runtime that is installed but stopped gets its own modal: the fix is a
    // command we can run, not an env to repair, so "retry" is the wrong verb.
    if let Some(rt) = &halt.dormant {
        let title = format!("⚠ {} is not running", rt.name);
        let body = format!(
            "{} — thegn will not silently run this pane on the host with no kernel              boundary. Start it, or choose [h] to run on the host anyway (the pane              will say so). Set `on_dormant = \"host\"` under [sandbox] to skip this              prompt, or `\"start\"` to start it automatically.",
            halt.reason
        );
        return menu::sandbox_dormant_menu(title, body, &rt.name, rt.start_display().as_deref());
    }
    // THE-2644: no backend is ready on this host (a clean Mac). The reason
    // already names the backends and their install fix; offer the one-key host
    // escape with its caveat, pinned for this session only.
    if halt
        .reason
        .contains(thegn_core::sandbox_floor::NO_BACKEND_MARKER)
    {
        return menu::sandbox_ask_menu("⚠ no sandbox backend", no_backend_halt_body(halt));
    }
    let title = format!("⚠ {} unavailable", halt.placement);
    if halt.ask {
        // `failover = "ask"`: surface the real cause and offer an explicit host
        // escape hatch beside the retry — the pane is NOT silently degraded.
        let body = format!(
            "{} — [r] retry the bring-up, or [h] run this worktree on the host for \
             now (you'll see it's degraded in the sidebar). Set `failover = \"auto\"` \
             under [env.{}] / [sandbox] to skip this prompt.",
            halt.reason, halt.env_name
        );
        return menu::sandbox_ask_menu(title, body);
    }
    let body = format!(
        "{} — failover is off, so thegn won't drop to the host. Fix the env \
         (e.g. set its token) then retry, or set `failover = \"ask\"`/`\"auto\"` under \
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
    pub loading_retired: &'a std::collections::HashSet<(String, usize)>,
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
/// Budget-safe: `provision_check` only fires when the sandbox does NOT
/// exist yet (a list() GET) or is bare; a parked Vercel sandbox is probed with
/// a never-resuming GET and left alone, and
/// `host_pending` is one DB read. Scope: active worktree only, or the whole
/// session (workspace/all). Once per session per worktree. Background policy:
/// a host needing install consent is DEFERRED, never prompted.
pub(crate) fn kick_eager(
    cfg: &thegn_core::config::Config,
    session: &crate::session::Session,
    eager_inflight: &mut std::collections::HashSet<String>,
    provision_tx: &ProvisionTx,
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
        let focused = active_path.as_deref() == Some(wt.as_str());
        let cfg = cfg.clone();
        let wk = waker.clone();
        let ptx = provision_tx.clone();
        let hui = host_ui.clone();
        tokio::task::spawn_blocking(move || {
            let provider = (!focused)
                .then(|| crate::cmd::env::resolve_for(&cfg, Some(wt.clone())).ok())
                .flatten()
                .and_then(|env| cfg.env.get(&env.name).map(|e| e.provider.provider.clone()));
            if !eager_allows(focused, provider.as_deref()) {
                return;
            }
            // `Pending` only: a parked Vercel sandbox is never resumed from
            // the background (only an explicit open buys its compute).
            if crate::agent::provision_check(&cfg, &wt) == crate::agent::ProvisionCheck::Pending
                || crate::host_flow::host_pending(&cfg, &wt)
            {
                // Lock the loading screen up the moment we commit to
                // provisioning (before the first step), so switching here can
                // never catch a half-ready shell — the splash wins while
                // `loading_state` is non-empty.
                let _ = ptx.send((gname.clone(), ti, vec![LoadStep::active("provisioning")])); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                let _ = wk.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                let prov = crate::host_flow::provision_worktree(
                    &cfg,
                    &wt,
                    crate::host_flow::ConsentPolicy::BackgroundSkip,
                    |views| {
                        // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                        let _ = ptx.send((
                            gname.clone(),
                            ti,
                            crate::loading::provision_load_steps(views),
                        ));
                        let _ = wk.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                    },
                    Some(&hui),
                );
                // On success, clear the lock so a now-ready worktree shows no
                // stale splash (materialize takes over on open). On failure,
                // leave the failed steps visible.
                let deferred = match prov {
                    Ok(outcome) => {
                        let _ = ptx.send((gname.clone(), ti, Vec::new())); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                        outcome.deferred
                    }
                    Err(_) => None,
                };
                let _ = wk.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                if let Some(handoff) = deferred {
                    crate::provision_bg::finish_in_background(&cfg, &wt, handoff);
                }
            }
        });
    }
}

/// Whether the eager provisioner may front-run `provider`'s sandbox. The
/// focused worktree always may (an open would create it anyway); a background
/// (`workspace`/`all`) target may not when its kind is not
/// [`eager_create_enabled`](thegn_core::config::EnvProviderKind::eager_create_enabled).
fn eager_allows(focused: bool, provider: Option<&str>) -> bool {
    focused
        || provider
            .is_none_or(|p| thegn_core::config::EnvProviderKind::of(p).eager_create_enabled())
}

/// Drain live env-provisioning progress for the active tab → the splash. The
/// off-loop spec task streams setup steps while a fresh provider sandbox is
/// built; only the active tab's stream drives the loading screen. Returns
/// whether the frame went dirty.
pub(crate) fn drain_provision(
    rx: &mut impl ProvisionSource,
    session: &crate::session::Session,
    loading_state: &mut crate::loading::track::LoadingTracker,
    loading_remote: &mut std::collections::HashMap<(String, usize), bool>,
    loading_retired: &std::collections::HashSet<(String, usize)>,
    loop_perf: &mut crate::perf::LoopPerf,
) -> bool {
    let mut dirty = false;
    let mut batch = Vec::new();
    for msg in rx.take_pending() {
        // Wake accounting stays honest: one tick per taken entry.
        loop_perf.tick(crate::perf::WakeSource::Spec);
        batch.push(msg);
    }
    // Each message replaces the whole step list for its key, so only the last
    // per key matters. Known, accepted difference: a step that went Pending to
    // Active to Done entirely within one drain pass records no `took` (it
    // would have been about 0 ms).
    for (name, ti, steps) in coalesce_latest(batch) {
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

/// Collapse progress messages to the latest steps per `(group, tab)` key,
/// keeping first-seen key order. Bounded by the number of distinct keys.
fn coalesce_latest(msgs: impl IntoIterator<Item = ProvisionProgress>) -> Vec<ProvisionProgress> {
    let mut out: Vec<ProvisionProgress> = Vec::new();
    for (name, ti, steps) in msgs {
        if let Some(slot) = out.iter_mut().find(|(n, t, _)| *n == name && *t == ti) {
            slot.2 = steps;
        } else {
            out.push((name, ti, steps));
        }
    }
    out
}

/// Adopt live daemon sessions when a prewarm has no safe fresh spec to spawn.
/// The empty leaf list deliberately makes every target a surplus split, so no
/// shell is synthesized just to make an attachment possible.
fn attach_prewarm_sessions(
    ctx: &mut SpecDrainCtx<'_>,
    gi: usize,
    ti: usize,
    targets: Vec<worktree_attach::AttachTarget>,
) {
    if targets.is_empty() || !ctx.panes.daemon_route_enabled() {
        return;
    }
    let shown: Vec<String> = ctx
        .panes
        .table
        .values()
        .filter_map(|pane| pane.provider_session())
        .filter(|session| session.provider == "daemon")
        .map(|session| session.session.clone())
        .collect();
    // THE-2683: sessions a persisted daemon record names reattach onto their
    // recorded leaf when their tab materializes, so grafting one here would
    // show it twice once that tab opens.
    let recorded = worktree_attach::recorded_daemon_sessions(ctx.session);
    let targets: Vec<_> = targets
        .into_iter()
        .filter(|target| !shown.contains(&target.session))
        .filter(|target| !recorded.contains(&target.session))
        .collect();
    let budget = worktree_attach::MAX_PANES_PER_TAB
        .saturating_sub(ctx.session.worktrees[gi].tabs[ti].center.pane_ids().len());
    let plan = worktree_attach::plan(&[], targets, budget);
    let landed = worktree_attach::graft_surplus(
        &plan.surplus,
        gi,
        ti,
        ctx.session,
        ctx.panes,
        ctx.current_config,
        ctx.center,
    );
    if landed > 0 {
        *ctx.need_relayout = true;
        *ctx.dirty = true;
    }
}

/// Drain resolved launch specs: finish the deferred materialize (lazy focus
/// path and pre-warm alike). Results for a group/tab that vanished mid-flight
/// are dropped, and `materialize_with_specs` itself skips leaves that came
/// alive some other way in the meantime.
pub(crate) fn drain_specs(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SpecBatch>,
    ctx: &mut SpecDrainCtx<'_>,
) {
    while let Ok(batch) = rx.try_recv() {
        let SpecBatch {
            group: name,
            worktree: wt,
            tab: ti,
            target_leaves,
            origin,
            specs,
            relaunch,
            attach: batch_attach,
            remote: tab_remote,
            remembered_agent,
        } = batch;
        if relaunch == RelaunchOutcome::Refused {
            tracing::debug!(
                target: "thegn::agent",
                group = %name,
                tab = ti,
                "spec batch retained shell fallback after remembered-agent refusal"
            );
        }
        ctx.loop_perf.tick(crate::perf::WakeSource::Spec);
        let tab_key = (name.clone(), ti);
        let Some(gi) = ctx.session.worktrees.iter().position(|g| g.name == name) else {
            // No group with this name exists, so this completed request cannot
            // belong to a replacement tab. Release only its own origin's key.
            // A recreated group takes the stable-leaf guard below instead.
            match origin {
                SpecOrigin::Materialize => {
                    ctx.materialize_inflight.remove(&tab_key);
                }
                SpecOrigin::Prewarm => {
                    ctx.prewarm_inflight.remove(&tab_key);
                }
            }
            continue;
        };
        // The numeric tab index is only a routing hint. Validate the captured
        // stable leaf identity before settling a reservation or touching any
        // loading/pool/attach state. In particular, an Err(PrewarmSkipped)
        // batch has no resolved leaves to inspect, so `target_leaves` is the
        // only stale-result guard it has.
        let target_matches = ctx
            .session
            .worktrees
            .get(gi)
            .and_then(|g| g.tabs.get(ti))
            .is_some_and(|tab| {
                let leaves = tab.center.pane_ids();
                target_leaves.iter().any(|id| leaves.contains(id))
            });
        if !target_matches {
            // A close shifts inflight keys along with the surviving tab. Settle
            // this request at its current index only when its captured leaves
            // identify exactly one surviving tab; never clear the replacement
            // at the old numeric index or guess between ambiguous targets.
            let mut matching =
                ctx.session.worktrees[gi]
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| {
                        let leaves = tab.center.pane_ids();
                        target_leaves.iter().any(|id| leaves.contains(id))
                    });
            if let Some((current_index, _)) = matching.next()
                && matching.next().is_none()
            {
                let captured_key = (name.clone(), current_index);
                match origin {
                    SpecOrigin::Materialize => {
                        ctx.materialize_inflight.remove(&captured_key);
                    }
                    SpecOrigin::Prewarm => {
                        ctx.prewarm_inflight.remove(&captured_key);
                    }
                }
            }
            tracing::debug!(
                target: "thegn::startup",
                group = %name, tab = ti,
                "dropping a spec batch whose captured target leaves no longer belong to the tab"
            );
            continue;
        }
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
        // Remoteness of this worktree, carried by the worker that resolved the
        // specs (it is a state-DB read, so the drain must not do it here) —
        // seeds `loading_remote` alongside every `loading_state` write below so
        // the startup-shell watchdog reads the correct 8s/300s deadline for
        // THIS tab regardless of its (possibly identical) step Vec.
        // A remote materialize may have just CLAIMED a warm spare (ready →
        // claimed), so the chip's count is now stale. Force the pool
        // maintainer to re-read on the next loop (it's otherwise throttled to
        // ~8s) so `warm N/M` drops promptly instead of showing a phantom
        // ready spare that was already handed off.
        if tab_remote {
            *ctx.last_pool_reconcile = None;
        }
        // The stable-leaf guard above handles the tab-index shift before any
        // loading mutation. `materialize_with_specs` still independently skips
        // individual leaves that came alive while this batch was in flight.
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
                if !ctx.loading_retired.contains(&tab_key) {
                    ctx.loading_remote.insert(tab_key.clone(), tab_remote);
                    ctx.loading_state
                        .advance_to_shell(tab_key.clone(), &backend);
                }
                // Spec resolution landed (THE-213): the pre-fork phase card
                // moves past "resolving environment" — "starting sandbox"
                // only for a real local sandbox wrapper (bwrap/systemd-run
                // style); a host shell and every remote placement
                // (ssh/provider/k8s) read "starting shell". The fork below
                // then advances the tracker to the shell-wait phase,
                // pane-keyed.
                let sandboxed = specs.first().is_some_and(|(_, spec)| {
                    !spec.remote && !matches!(spec.backend.as_str(), "host" | "none" | "")
                });
                let phase = if sandboxed {
                    crate::spawn_progress::Phase::StartingSandbox
                } else {
                    crate::spawn_progress::Phase::StartingShell
                };
                for (leaf, _) in &specs {
                    if let Some(tracker) =
                        ctx.panes.spawn_pending.get_mut(&(name.clone(), ti, *leaf))
                    {
                        tracker.advance(phase, std::time::Instant::now());
                    }
                }
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
                    // Benign prewarm skip (provider env not provisioned yet, or
                    // host execution was the only fresh spec): no failed splash,
                    // no failed mark — focused materialize owns any new launch.
                    SpecError::PrewarmSkipped => {
                        // A host prewarm is also skipped, but a live daemon
                        // session discovered beside it is still safe to adopt.
                        // Keep attachment independent from fresh host-spec
                        // construction so a prewarm cannot create a host shell
                        // merely to preserve resurrection behavior.
                        if origin == SpecOrigin::Prewarm {
                            attach_prewarm_sessions(ctx, gi, ti, batch_attach);
                        }
                        continue;
                    }
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
                    SpecError::Consent(prompt) => {
                        ctx.model.status =
                            format!("{}: approve credential forwarding", prompt.env_name);
                        if is_active && origin != SpecOrigin::Prewarm {
                            *ctx.active_menu = Some(crate::model_credential_prompt::menu_for(
                                prompt.as_ref(),
                                &name,
                                ti,
                            ));
                        }
                        format!("model credentials for {} need approval", prompt.sandbox)
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
                // THE-213: the pre-fork phase cards carry the reason. The
                // leaves stay missing until a retry re-kicks and overwrites
                // the tracker, so the card is the only visible trace.
                for leaf in &target_leaves {
                    if let Some(tracker) =
                        ctx.panes.spawn_pending.get_mut(&(name.clone(), ti, *leaf))
                    {
                        tracker.fail(err_detail.as_str(), std::time::Instant::now());
                    }
                }
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
        // THE-2683: a persisted daemon record is a promise that materialize
        // reattaches that session onto its own leaf (below), so a recorded
        // session is not headless and must not also enter the attach pool —
        // the probe lists it beside the truly headless sessions (it is live
        // and carries this worktree), and the surplus graft used to show it a
        // second time beside the leaf that reattached it. Every group/tab of
        // the session counts, not just this tab's: a neighboring tab's
        // recorded session reattaches when ITS tab materializes. Computed
        // before the mutable tab borrow below; recomputed per batch.
        let recorded = worktree_attach::recorded_daemon_sessions(ctx.session);
        let Some(tab) = ctx.session.worktrees[gi].tabs.get_mut(ti) else {
            continue;
        };
        // Daemon route disabled (`[daemon] enabled = false`, THEGN_NO_DAEMON,
        // or a harness env) with persisted daemon-backed records on this
        // batch's dead leaves: claim (consume) the records BEFORE materialize
        // — the warm-reattach branch is off, so an unclaimed record would (a)
        // respawn the same program on the fallback path while the daemon copy
        // keeps running under its untimed default lease (a duplicated
        // process, then a permanent invisible orphan once the post-remap
        // persist prunes the record), and (b) let the native provider-exec
        // fallback attach a daemon session id without a provider filter.
        // Only NOT-already-live leaves are claimed (mirroring materialize's
        // own skip): a daemon pane spawned before a live config reload
        // disabled the route keeps its session. The kill itself is dispatched
        // only after materialize succeeds, below — on a spawn failure the
        // daemon copies stay alive exactly as before this fix.
        let orphaned = if !ctx.panes.daemon_route_enabled() {
            let stale: Vec<u32> = specs
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| !ctx.panes.table.contains_key(id))
                .collect();
            super::daemon_lifecycle::claim_disabled_daemon_sessions(tab, &stale)
        } else {
            Vec::new()
        };
        {
            // Everything but the shell attach is done → shell active. (The
            // Ok-arm already advanced the plan; this re-advance is a no-op
            // for it and covers the early-continue paths.)
            let backend = specs
                .first()
                .map(|(_, s)| s.backend.clone())
                .unwrap_or_else(|| "host".into());
            if !ctx.loading_retired.contains(&tab_key) {
                ctx.loading_remote.insert(tab_key.clone(), tab_remote);
                ctx.loading_state
                    .advance_to_shell(tab_key.clone(), &backend);
            }
        }
        // Attach-on-open (THE-85): plan the worktree's live agent sessions
        // onto the tab's missing leaves BEFORE materialize consumes them (the
        // tree remaps inside). The worker probed with an empty `shown` (it
        // can't see this process's panes), so the plan re-dedups against
        // `panes.table` here — cheap and correct. Leaves with a persisted
        // daemon record are excluded from the pool: they reattach their OWN
        // prior session (the existing warm-reattach contract) and must not
        // have an agent session aimed at them — and the recorded sessions are
        // excluded from the targets for the same reason (THE-2683): the
        // materialize below reattaches each one onto its own leaf, so one
        // that stayed in the pool landed a second time as a surplus split,
        // restoring a single-pane tab with an extra pane and a different
        // split.
        let (attach, surplus, overflow) =
            if batch_attach.is_empty() || !ctx.panes.daemon_route_enabled() {
                (Vec::new(), Vec::new(), 0usize)
            } else {
                let shown: Vec<String> = ctx
                    .panes
                    .table
                    .values()
                    .filter_map(|p| p.provider_session())
                    .filter(|ps| ps.provider == "daemon")
                    .map(|ps| ps.session.clone())
                    .collect();
                let targets: Vec<AttachTarget> = batch_attach
                    .into_iter()
                    .filter(|t| !shown.contains(&t.session))
                    .filter(|t| !recorded.contains(&t.session))
                    .collect();
                let fit = targets.len();
                let spawn_leaves: Vec<u32> = specs
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| !ctx.panes.table.contains_key(id))
                    .filter(|id| {
                        tab.pane_sessions
                            .get(id)
                            .is_none_or(|ps| ps.provider != "daemon")
                    })
                    .collect();
                let budget =
                    worktree_attach::MAX_PANES_PER_TAB.saturating_sub(tab.center.pane_ids().len());
                let plan = worktree_attach::plan(&spawn_leaves, targets, budget);
                let overflow = fit.saturating_sub(plan.assignments.len() + plan.surplus.len());
                (plan.assignments, plan.surplus, overflow)
            };
        if let Err(e) = ctx.panes.materialize_with_specs_keyed(
            ctx.current_config,
            tab,
            Some(&tab_key),
            &wt,
            &specs,
            ctx.center,
            &attach,
            remembered_agent.as_deref(),
        ) {
            if !orphaned.is_empty() {
                // The records were already consumed, but the batch aborted
                // before any replacement pane came up: killing the daemon
                // copies now would lose BOTH processes (the spawn failed, so
                // there is no replacement). Leave them running — recoverable via
                // `thegn session list` — the same orphan the pre-fix code left.
                // The `materialize_failed` mark below stops the retry loop, so
                // this stays a single recoverable orphan rather than a
                // duplicate-forever storm.
                tracing::warn!(
                    target: "thegn::daemon",
                    sessions = orphaned.len(),
                    "materialize failed after claiming daemon-disabled records; \
                     leaving the daemon sessions running (inspect: `thegn session list`)"
                );
            }
            ctx.model.status = format!("Pane spawn failed: {e}");
            ctx.loading_remote.insert(tab_key.clone(), tab_remote);
            // The shell step is the active one after `advance_to_shell`.
            let spawn_detail = format!("{e}");
            ctx.loading_state
                .fail_active(tab_key.clone(), &spawn_detail);
            // THE-213: the fork failed, so the leaves stay missing and their
            // pending cards turn terminal with the spawn error.
            for (leaf, _) in &specs {
                if let Some(tracker) = ctx.panes.spawn_pending.get_mut(&(name.clone(), ti, *leaf)) {
                    tracker.fail(spawn_detail.as_str(), std::time::Instant::now());
                }
            }
            // A spawn (openpty/fork) failure — EMFILE/ENOMEM, or a vanished
            // sandbox-wrapper binary — leaves the leaf missing and un-remapped.
            // Mark the tab failed (and dormant when active) exactly like the
            // spec-resolution Err arm above: without this, `maybe_materialize`
            // (which gates only on `materialize_inflight`/`materialize_failed`,
            // and inflight was cleared at batch receipt) re-kicks the leaf every
            // loop turn — an unbounded self-waking spec-resolution hot loop. The
            // mark routes recovery through the [r]-retry UX instead.
            match origin {
                SpecOrigin::Materialize => {
                    if is_active {
                        *ctx.center_dormant = true;
                    }
                    ctx.materialize_failed.insert(tab_key.clone());
                }
                SpecOrigin::Prewarm => {
                    ctx.prewarm_failed.insert(tab_key.clone());
                }
            }
        } else {
            // Attach-on-open (THE-85): live sessions beyond the missing
            // leaves split in via the `--adopt` graft, newest first, capped
            // by the pane budget; the overflow is named in the status line,
            // never silently dropped.
            if !surplus.is_empty() {
                worktree_attach::graft_surplus(
                    &surplus,
                    gi,
                    ti,
                    ctx.session,
                    ctx.panes,
                    ctx.current_config,
                    ctx.center,
                );
            }
            if overflow > 0 && is_active {
                ctx.model.status = format!(
                    "{overflow} more live agent session{} — `thegn attach <id>`",
                    if overflow == 1 { "" } else { "s" }
                );
            }
            // Keep the loading entry until first PTY output arrives (cleared in
            // the PaneEvent::Output handler above) so the loading screen
            // stays visible until the shell actually produces content —
            // no blank flash between fork and first render.
            if is_active && !warnings.is_empty() {
                ctx.model.status = format!("⚠ Sandbox fallback: {}", warnings.join("; "));
            }
            if !orphaned.is_empty() {
                // Set AFTER the sandbox-fallback warning so the rarer, more
                // consequential daemon-disabled note wins this frame's status.
                // "respawned without the daemon", not "fresh shells": the
                // fallback may have taken the ssh/native/sandbox branch, which
                // carries no scrollback repaint or relaunch offer.
                ctx.model.status = format!(
                    "[daemon] disabled — stopping {} persisted daemon session(s); \
                     panes respawned without the daemon",
                    orphaned.len()
                );
                super::daemon_lifecycle::kill_orphaned_daemon_sessions(
                    ctx.current_config.daemon.clone(),
                    orphaned,
                );
            }
        }
        if is_active {
            *ctx.need_relayout = true;
        }
        *ctx.dirty = true;
    }
}

/// Queue an adjustment to the active workspace's warm-spare-pool target. The DB
/// read and write happen together on the critical writer in FIFO order.
pub(crate) fn pool_target_adjust(
    session: &crate::session::Session,
    cfg: &thegn_core::config::Config,
    delta: i64,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::run::run_pending_db::PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) -> Option<String> {
    let g = session.active_group().filter(|g| !g.path.is_empty())?;
    crate::run::run_pending_db::queue_pool_target_adjust(
        tx,
        waker,
        cfg.clone(),
        g.path.clone(),
        delta,
    );
    Some("warm pool: updating target…".into())
}

pub(crate) fn adjust_pool_target(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
    wt: &str,
    delta: i64,
) -> String {
    use thegn_core::store::{PoolStore as _, WorkspaceStore as _};

    let location = match db.location_for(wt) {
        Ok(location) => location,
        Err(error) => return format!("warm pool: persist failed: {error}"),
    };
    let loc = thegn_core::remote::GitLoc::from_db(wt, location.as_deref());
    if !loc.is_remote() {
        return "warm pool: only for provider (remote) workspaces".into();
    }
    let (_, repo, env) = match crate::lifecycle::pool_context(db, cfg, wt, &loc) {
        Ok(context) => context,
        Err(error) => return format!("{error:#}"),
    };
    let cur = match crate::lifecycle::effective_pool_target(db, cfg, &repo, &env) {
        Ok(cur) => cur,
        Err(error) => return format!("warm pool: cannot read the current target: {error:#}"),
    };
    let new = (cur as i64 + delta).clamp(0, crate::lifecycle::POOL_TARGET_CEILING as i64);
    if let Err(error) = db.set_pool_target(&repo, &env, new) {
        return format!("warm pool: persist failed: {error}");
    }
    format!("warm pool target: {new} spare(s)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneEvent;
    use crate::session::{GroupKind, ProviderSession, Session, WorktreeGroup};
    use tokio::sync::mpsc as tokio_mpsc;

    #[test]
    fn eager_background_scope_never_creates_a_vercel_sandbox() {
        assert!(!eager_allows(false, Some("vercel")));
        assert!(eager_allows(true, Some("vercel")));
        assert!(eager_allows(false, Some("daytona")));
        assert!(!eager_allows(false, Some("sprites")));
        assert!(eager_allows(true, Some("sprites")));
        assert!(eager_allows(false, None));
    }

    fn ls(label: &str, state: crate::chrome::StepState) -> LoadStep {
        LoadStep {
            label: label.into(),
            state,
            detail: None,
            kind: crate::chrome::StepKind::default(),
            started_at: None,
            took: None,
            progress: None,
        }
    }

    fn msg(g: &str, t: usize, label: &str) -> ProvisionProgress {
        (
            g.into(),
            t,
            vec![ls(label, crate::chrome::StepState::Active)],
        )
    }

    fn labels(m: &[ProvisionProgress]) -> Vec<(String, usize, String)> {
        m.iter()
            .map(|(g, t, s)| (g.clone(), *t, s[0].label.clone()))
            .collect::<Vec<_>>()
    }

    #[test]
    fn coalesce_latest_table() {
        assert!(coalesce_latest(Vec::new()).is_empty());
        assert_eq!(
            labels(&coalesce_latest(vec![msg("a", 0, "x")])),
            vec![("a".to_string(), 0, "x".to_string())]
        );
        assert_eq!(
            labels(&coalesce_latest(vec![
                msg("a", 0, "1"),
                msg("a", 0, "2"),
                msg("a", 0, "3")
            ])),
            vec![("a".to_string(), 0, "3".to_string())]
        );
        assert_eq!(
            labels(&coalesce_latest(vec![msg("a", 0, "1"), msg("b", 1, "2")])),
            vec![
                ("a".to_string(), 0, "1".to_string()),
                ("b".to_string(), 1, "2".to_string())
            ]
        );
        assert_eq!(
            labels(&coalesce_latest(vec![
                msg("a", 0, "a1"),
                msg("b", 0, "b1"),
                msg("a", 0, "a2"),
                msg("b", 0, "b2"),
                msg("a", 1, "c1"),
            ])),
            vec![
                ("a".to_string(), 0, "a2".to_string()),
                ("b".to_string(), 0, "b2".to_string()),
                ("a".to_string(), 1, "c1".to_string())
            ]
        );
    }

    #[test]
    fn provision_slot_flood_keeps_latest_per_key_in_first_seen_order() {
        let (tx, inbox) = ProvisionTx::with_wake(|| {});
        let keys = [("b", 0usize), ("a", 1usize), ("c", 0usize)];
        for i in 0..10_000usize {
            let (g, t) = keys[i % keys.len()];
            assert!(tx.send(msg(g, t, &i.to_string())));
        }
        assert_eq!(inbox.len(), 3);
        assert_eq!(inbox.drops(), 0);
        assert_eq!(
            labels(&inbox.take_all()),
            vec![
                ("b".to_string(), 0, "9999".to_string()),
                ("a".to_string(), 1, "9997".to_string()),
                ("c".to_string(), 0, "9998".to_string()),
            ]
        );
        assert_eq!(inbox.len(), 0);
        assert_eq!(inbox.bytes(), 0);
    }

    #[test]
    fn provision_slot_distinct_key_flood_stays_at_cap_and_counts_drops() {
        let (tx, inbox) = ProvisionTx::with_wake(|| {});
        let total = MAX_PROVISION_KEYS * 4;
        for i in 0..total {
            assert!(tx.send(msg(&format!("g{i}"), 0, "x")));
            assert!(inbox.len() <= MAX_PROVISION_KEYS);
        }
        assert_eq!(inbox.len(), MAX_PROVISION_KEYS);
        assert_eq!(inbox.drops(), (total - MAX_PROVISION_KEYS) as u64);
        assert!(inbox.bytes() <= MAX_PROVISION_BYTES);
        // The oldest keys were evicted; the newest MAX_PROVISION_KEYS remain.
        let taken = inbox.take_all();
        assert_eq!(
            taken.first().map(|m| m.0.as_str()),
            Some(format!("g{}", total - MAX_PROVISION_KEYS).as_str())
        );
        assert_eq!(
            taken.last().map(|m| m.0.as_str()),
            Some(format!("g{}", total - 1).as_str())
        );
    }

    #[test]
    fn provision_slot_byte_cap_evicts_oldest_and_refuses_oversized() {
        let (tx, inbox) = ProvisionTx::with_wake(|| {});
        let big = "x".repeat(MAX_PROVISION_BYTES / 4);
        for i in 0..8usize {
            assert!(tx.send(msg(&format!("g{i}"), 0, &big)));
            assert!(inbox.bytes() <= MAX_PROVISION_BYTES);
        }
        assert!(inbox.len() < 8);
        assert!(inbox.drops() > 0);
        let huge = "y".repeat(MAX_PROVISION_BYTES + 1);
        let before = inbox.drops();
        assert!(!tx.send(msg("huge", 0, &huge)));
        assert_eq!(inbox.drops(), before + 1);
        assert!(inbox.bytes() <= MAX_PROVISION_BYTES);
    }

    #[test]
    fn provision_slot_clear_replaces_and_survives_the_cap() {
        let (tx, inbox) = ProvisionTx::with_wake(|| {});
        assert!(tx.send(msg("a", 0, "busy")));
        assert!(tx.send(("a".to_string(), 0, Vec::new())));
        let taken = inbox.take_all();
        assert_eq!(taken.len(), 1);
        assert!(
            taken[0].2.is_empty(),
            "the clear replaced the non-empty list"
        );

        // Fill to the cap with a clear first-seen, then flood new keys.
        assert!(tx.send(("cleared".to_string(), 0, Vec::new())));
        for i in 0..MAX_PROVISION_KEYS * 3 {
            assert!(tx.send(msg(&format!("g{i}"), 0, "x")));
        }
        // A clear for a NEW key at the cap boundary lands too.
        assert!(tx.send(("late".to_string(), 0, Vec::new())));
        assert_eq!(inbox.len(), MAX_PROVISION_KEYS);
        let taken = inbox.take_all();
        assert!(
            taken.iter().any(|(g, _, s)| g == "cleared" && s.is_empty()),
            "a pending clear is never evicted by non-empty traffic"
        );
        assert!(taken.iter().any(|(g, _, s)| g == "late" && s.is_empty()));
    }

    #[test]
    fn provision_slot_wakes_only_on_empty_to_non_empty_and_after_take() {
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, inbox) = ProvisionTx::with_wake({
            let wakes = std::sync::Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        for i in 0..1_000usize {
            assert!(tx.send(msg(&format!("g{}", i % 5), 0, "x")));
        }
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(inbox.take_all().len(), 5);
        assert!(inbox.take_all().is_empty());

        // A replacement arriving after the take is returned next time and re-wakes.
        assert!(tx.send(msg("g0", 0, "late")));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(tx.send(msg("g0", 0, "later")));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            labels(&inbox.take_all()),
            vec![("g0".to_string(), 0, "later".to_string())]
        );
    }

    #[test]
    fn drain_provision_keeps_latest_per_key_and_honors_retired() {
        let session = Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, ""),
                WorktreeGroup::new("app/other", GroupKind::Home, ""),
            ],
            active: 0,
        };
        let (tx, mut rx) = ProvisionTx::with_wake(|| {});
        let mut tracker = crate::loading::track::LoadingTracker::default();
        let mut remote = std::collections::HashMap::new();
        let mut perf = crate::perf::LoopPerf::new();
        let retired: std::collections::HashSet<(String, usize)> =
            std::iter::once(("app/other".to_string(), 0)).collect();

        assert!(tx.send(msg("app/other", 1, "o1")));
        assert!(tx.send(msg("app/home", 0, "h1")));
        assert!(tx.send(msg("app/other", 0, "retired")));
        assert!(tx.send(msg("app/home", 0, "h2")));
        assert!(tx.send(msg("app/home", 0, "h3")));
        let dirty = drain_provision(
            &mut rx,
            &session,
            &mut tracker,
            &mut remote,
            &retired,
            &mut perf,
        );
        assert!(dirty, "active tab got an update");
        let home = tracker.get(&("app/home".to_string(), 0)).unwrap();
        assert_eq!(home.len(), 1);
        assert_eq!(home[0].label, "h3");
        assert_eq!(
            tracker.get(&("app/other".to_string(), 1)).unwrap()[0].label,
            "o1"
        );
        assert!(tracker.get(&("app/other".to_string(), 0)).is_none());

        assert!(tx.send(msg("app/other", 1, "o2")));
        let dirty = drain_provision(
            &mut rx,
            &session,
            &mut tracker,
            &mut remote,
            &retired,
            &mut perf,
        );
        assert!(!dirty, "only a non-active tab updated");
        assert_eq!(
            tracker.get(&("app/other".to_string(), 1)).unwrap()[0].label,
            "o2"
        );
    }

    /// THE regression gate for the daemon-disabled claim, at the level where
    /// it can break: a real `SpecBatch` flows through the real `drain_specs`
    /// with the daemon route disabled (`daemon_cfg` unset) and a persisted
    /// `provider = "daemon"` record on the tab. The record must be consumed
    /// (exactly-once claim — no later persist can resurrect it, no daemon sid
    /// reaches the native-exec attach), the leaf must come up as exactly one
    /// live NON-daemon pane, and the status must say what happened. Dropping
    /// the claim call in `drain_specs` — or inverting its
    /// `!daemon_route_enabled()` gate — fails this test; the pure-helper unit
    /// tests alone would not notice.
    ///
    /// The kill dispatch is exercised up to its no-runtime no-op contract
    /// (`kill_orphaned_daemon_sessions` returns before latching when no tokio
    /// runtime exists — this test runs without one); the kill's best-effort
    /// semantics are pinned in `handlers::daemon_lifecycle::tests`.
    #[test]
    fn drain_specs_with_daemon_disabled_claims_persisted_daemon_records() {
        // Serialize env access; the spawned argv below is explicit (/bin/sh).
        let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
        let mut session = Session {
            id: "s1".into(),
            // Empty worktree path: the ssh-over-wss and native-exec fallbacks
            // are gated on a non-empty worktree, so the leaf takes the plain
            // spawn path (and no DB is touched).
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        {
            let tab = &mut session.worktrees[0].tabs[0];
            tab.center = crate::center::CenterTree::Leaf(leaf);
            tab.focused_pane = leaf;
            tab.pane_sessions.insert(
                leaf,
                ProviderSession {
                    provider: "daemon".into(),
                    id: "local".into(),
                    session: "sess-1".into(),
                },
            );
        }
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        // `Panes::new` leaves `daemon_cfg` unset — the daemon route is OFF,
        // exactly the resurrect-after-disable shape.
        let mut panes = crate::panes::Panes::new(tx);
        assert!(!panes.daemon_route_enabled());

        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = false;
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();

        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        let spec = crate::agent::LaunchSpec {
            argv: vec!["/bin/sh".into()],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        };
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Materialize,
                specs: Ok(vec![(leaf, spec)]),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        let tab = &session.worktrees[0].tabs[0];
        assert!(
            tab.pane_sessions.is_empty(),
            "the persisted daemon record must be consumed by the claim"
        );
        assert_eq!(panes.table.len(), 1, "exactly one live pane — no duplicate");
        let id = tab.center.pane_ids()[0];
        assert!(
            !panes
                .table
                .get(&id)
                .expect("remapped pane")
                .is_daemon_backed(),
            "the replacement pane must not be daemon-backed"
        );
        assert!(
            model.status.contains("[daemon] disabled"),
            "status must surface the claim: {:?}",
            model.status
        );
        assert!(dirty, "the drain leaves the frame dirty");
    }

    /// The spawn no longer reads the state DB on the loop: the drain's
    /// remoteness seed (which picks the startup-shell watchdog's 8s/300s
    /// window) is computed by the off-thread spec worker and carried in the
    /// batch. Both halves are pinned here — the carried value lands in
    /// `loading_remote`, and the drain opens nothing on the way. Reverting to
    /// the old on-loop `GitLoc::for_worktree` read fails the open count.
    #[test]
    fn drain_specs_carries_remoteness_and_opens_no_state_db() {
        let _state = crate::testenv::IsolatedState::new();
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(leaf);
        session.worktrees[0].tabs[0].focused_pane = leaf;
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(tx);
        let cfg = thegn_core::config::Config::default();
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();

        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Materialize,
                // A failed resolve keeps the drain off the spawn path, so the
                // measurement covers the loop-side bookkeeping alone.
                specs: Err(SpecError::Other("provider unavailable".into())),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: true,
                remembered_agent: None,
            })
            .unwrap();

        let opens_before = thegn_core::db_open_stats::total_opens();
        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert_eq!(
            thegn_core::db_open_stats::total_opens() - opens_before,
            0,
            "the drain must not open the state DB: the worker carried what the spawn needs"
        );
        assert_eq!(
            loading_remote.get(&("app/home".to_string(), 0)),
            Some(&true),
            "the carried placement seeds the watchdog window"
        );
        assert!(
            materialize_failed.contains(&("app/home".to_string(), 0)),
            "the failed resolve is still marked"
        );
        assert!(panes.table.is_empty(), "a failed resolve spawns nothing");
    }

    /// THE-2683: the demo quit-and-relaunch. The tab's daemon session
    /// survives the quit (the pane daemon owns the PTY), so the relaunch's
    /// worktree probe returns it — live, this worktree — and the session
    /// model also persisted its record; materialize reattaches that session
    /// onto its own leaf (the warm-reattach contract). Before the fix the
    /// drain filtered the probe targets only against live panes (`shown` —
    /// empty here, the pane does not exist yet), so the recorded session
    /// sailed into the surplus pool and `graft_surplus` split it in beside
    /// the leaf that had just reattached it: the restored single-pane tab
    /// came back with a second pane and a different split. The drain now
    /// drops targets a persisted daemon record names; removing that filter
    /// fails this test with two panes.
    #[test]
    fn drain_specs_restores_a_persisted_session_without_grafting_a_second_pane() {
        let mut session = Session {
            id: "s1".into(),
            // Empty worktree path: the remote fallbacks are gated on a
            // non-empty worktree, so the leaf takes the plain attach path.
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        {
            let tab = &mut session.worktrees[0].tabs[0];
            tab.center = crate::center::CenterTree::Leaf(leaf);
            tab.focused_pane = leaf;
            tab.pane_sessions.insert(
                leaf,
                ProviderSession {
                    provider: "daemon".into(),
                    id: "local".into(),
                    session: "warm-1".into(),
                },
            );
        }
        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let cfg = thegn_core::config::Config::default();
        let mut daemon_cfg = cfg.daemon.clone();
        daemon_cfg.enabled = true;
        panes.set_daemon_config(daemon_cfg);
        let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attached_by_spawn = std::sync::Arc::clone(&attached);
        panes.set_test_daemon_spawn(move |panes, session| {
            let session = session.ok_or_else(|| anyhow::anyhow!("attach target missing"))?;
            let id = panes.insert_test_daemon_pane(session.to_string());
            attached_by_spawn
                .lock()
                .unwrap()
                .push((session.to_string(), id));
            Ok(id)
        });

        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();

        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        let spec = crate::agent::LaunchSpec {
            argv: vec!["/bin/sh".into()],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        };
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Materialize,
                specs: Ok(vec![(leaf, spec)]),
                relaunch: RelaunchOutcome::NotAttempted,
                // The probe found the still-live daemon session the
                // persisted record names — the relaunch batch from the
                // repro.
                attach: vec![AttachTarget {
                    session: "warm-1".into(),
                    program: "sh".into(),
                }],
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        let mut attached_now = attached.lock().unwrap().clone();
        assert_eq!(
            attached_now.len(),
            1,
            "the recorded session reattaches exactly once"
        );
        let (_, restored) = attached_now.pop().expect("the single attach spawn");
        assert_eq!(
            panes.table.keys().copied().collect::<Vec<_>>(),
            vec![restored],
            "no surplus pane is grafted beside the reattached one"
        );
        let tab = &session.worktrees[0].tabs[0];
        assert_eq!(
            tab.center,
            crate::center::CenterTree::Leaf(restored),
            "the single-pane tab stays a single pane — no adopt::graft split"
        );
        assert_eq!(tab.focused_pane, restored);
        assert!(
            panes.table[&restored].is_daemon_backed(),
            "the restored pane rides the daemon"
        );
        assert_eq!(
            panes.table[&restored]
                .provider_session()
                .expect("daemon pane")
                .session,
            "warm-1",
            "the pane carries the reattached session, not a fresh shell"
        );
    }

    /// THE-85 still lands: a live session the session model holds no record
    /// for takes the tab's missing leaf — the recorded filter must not empty
    /// the pool for everyone. (Such a target exists when the pane daemon has
    /// a session this session never knew, e.g. a tab closed while its
    /// session lived on in another group.)
    #[test]
    fn drain_specs_attaches_a_recordless_live_target_onto_the_missing_leaf() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(leaf);
        session.worktrees[0].tabs[0].focused_pane = leaf;

        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let cfg = thegn_core::config::Config::default();
        let mut daemon_cfg = cfg.daemon.clone();
        daemon_cfg.enabled = true;
        panes.set_daemon_config(daemon_cfg);
        let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attached_by_spawn = std::sync::Arc::clone(&attached);
        panes.set_test_daemon_spawn(move |panes, session| {
            let session = session.ok_or_else(|| anyhow::anyhow!("attach target missing"))?;
            let id = panes.insert_test_daemon_pane(session.to_string());
            attached_by_spawn
                .lock()
                .unwrap()
                .push((session.to_string(), id));
            Ok(id)
        });

        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();

        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        let spec = crate::agent::LaunchSpec {
            argv: vec!["/bin/sh".into()],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        };
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Materialize,
                specs: Ok(vec![(leaf, spec)]),
                relaunch: RelaunchOutcome::NotAttempted,
                // Headless: no persisted record names this session.
                attach: vec![AttachTarget {
                    session: "headless-1".into(),
                    program: "sh".into(),
                }],
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        let mut attached_now = attached.lock().unwrap().clone();
        assert_eq!(
            attached_now.len(),
            1,
            "the headless session attaches exactly once"
        );
        let (_, restored) = attached_now.pop().expect("the single attach spawn");
        assert_eq!(
            panes.table.keys().copied().collect::<Vec<_>>(),
            vec![restored],
            "the headless session takes the missing leaf — exactly one pane"
        );
        let tab = &session.worktrees[0].tabs[0];
        assert_eq!(
            tab.center,
            crate::center::CenterTree::Leaf(restored),
            "the attach rides the existing leaf, no split"
        );
        assert_eq!(
            panes.table[&restored]
                .provider_session()
                .expect("daemon pane")
                .session,
            "headless-1"
        );
        assert!(need_relayout, "the new pane must be laid out");
        assert!(dirty, "the new pane changes the frame");
    }

    #[test]
    fn drain_specs_prewarm_skip_attaches_live_target_without_fresh_spawn() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(leaf);
        session.worktrees[0].tabs[0].focused_pane = leaf;

        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let cfg = thegn_core::config::Config::default();
        let mut daemon_cfg = cfg.daemon.clone();
        daemon_cfg.enabled = true;
        panes.set_daemon_config(daemon_cfg);
        let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attached_by_spawn = std::sync::Arc::clone(&attached);
        panes.set_test_daemon_spawn(move |panes, session| {
            let session = session.ok_or_else(|| anyhow::anyhow!("attach target missing"))?;
            attached_by_spawn.lock().unwrap().push(session.to_string());
            Ok(panes.insert_test_daemon_pane(session.to_string()))
        });

        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::from([("app/home".into(), 0)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Prewarm,
                specs: Err(SpecError::PrewarmSkipped),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: vec![AttachTarget {
                    session: "live-session".into(),
                    program: "claude".into(),
                }],
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert_eq!(*attached.lock().unwrap(), vec!["live-session"]);
        assert_eq!(panes.table.len(), 1, "the attach is the only new pane");
        let pane_id = session.worktrees[0].tabs[0]
            .center
            .pane_ids()
            .into_iter()
            .find(|id| *id != leaf)
            .expect("attached session was grafted into the tab");
        assert!(panes.table[&pane_id].is_daemon_backed());
        assert_eq!(
            panes.table[&pane_id].provider_session().unwrap().session,
            "live-session"
        );
        assert!(
            prewarm_inflight.is_empty(),
            "the skipped request is settled"
        );
        assert!(prewarm_failed.is_empty(), "a benign skip is not a failure");
        assert!(need_relayout, "the adopted pane changes tab geometry");
        assert!(dirty, "the adopted pane changes the frame");
    }

    /// THE-2683 on the prewarm-skip path: a skipped prewarm still attaches
    /// the worktree live sessions, but the tab's own persisted record — the
    /// still-alive session the quit left behind — reattaches when a later
    /// focused materialize consumes its leaf. The recorded session must
    /// stay out of the surplus graft, or the tab would come up with an
    /// extra pane split beside the pane that reattached it.
    #[test]
    fn drain_specs_prewarm_skip_leaves_a_persisted_sessions_record_to_its_leaf() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 7u32;
        {
            let tab = &mut session.worktrees[0].tabs[0];
            tab.center = crate::center::CenterTree::Leaf(leaf);
            tab.focused_pane = leaf;
            tab.pane_sessions.insert(
                leaf,
                ProviderSession {
                    provider: "daemon".into(),
                    id: "local".into(),
                    session: "live-session".into(),
                },
            );
        }
        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let cfg = thegn_core::config::Config::default();
        let mut daemon_cfg = cfg.daemon.clone();
        daemon_cfg.enabled = true;
        panes.set_daemon_config(daemon_cfg);
        let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attached_by_spawn = std::sync::Arc::clone(&attached);
        panes.set_test_daemon_spawn(move |panes, session| {
            let session = session.ok_or_else(|| anyhow::anyhow!("attach target missing"))?;
            attached_by_spawn.lock().unwrap().push(session.to_string());
            Ok(panes.insert_test_daemon_pane(session.to_string()))
        });

        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::from([("app/home".into(), 0)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Prewarm,
                specs: Err(SpecError::PrewarmSkipped),
                relaunch: RelaunchOutcome::NotAttempted,
                // The probe found the very session the persisted record
                // names — it is live and carries this worktree.
                attach: vec![AttachTarget {
                    session: "live-session".into(),
                    program: "claude".into(),
                }],
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(
            attached.lock().unwrap().is_empty(),
            "the recorded session must not be grafted"
        );
        assert!(panes.table.is_empty(), "a benign skip still spawns nothing");
        assert!(
            prewarm_inflight.is_empty(),
            "the skipped request is settled"
        );
        assert!(prewarm_failed.is_empty(), "a benign skip is not a failure");
        assert!(!need_relayout, "nothing was grafted, so no relayout");
        assert!(!dirty, "nothing changed the frame");
    }

    /// A skipped prewarm still carries attach targets, but its result has no
    /// resolved leaf ids for the stale-batch guard to inspect.  Close the tab
    /// to the left of the captured target so the same index now names a
    /// different live tab; the delayed attach must be dropped rather than
    /// grafting an agent into that unrelated tab.
    #[test]
    fn drain_specs_drops_stale_prewarm_skip_before_attaching_to_shifted_tab() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        {
            let g = &mut session.worktrees[0];
            g.tabs[0].center = crate::center::CenterTree::Leaf(5);
            g.tabs[0].focused_pane = 5;
            g.add_tab();
            g.tabs[1].center = crate::center::CenterTree::Leaf(6);
            g.tabs[1].focused_pane = 6;
            g.add_tab();
            g.tabs[2].center = crate::center::CenterTree::Leaf(7);
            g.tabs[2].focused_pane = 7;
            g.active_tab = 1;
            // The worker captured tab index 1 while it held leaf 6.  Closing
            // the tab to its left shifts leaf 7 into that index.
            g.tabs.remove(0);
            g.active_tab = 0;
        }

        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        panes.insert_test_pane(5);
        panes.insert_test_pane(6);
        panes.insert_test_pane(7);
        let cfg = thegn_core::config::Config::default();
        let mut daemon_cfg = cfg.daemon.clone();
        daemon_cfg.enabled = true;
        panes.set_daemon_config(daemon_cfg);
        let attached = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let attached_by_spawn = std::sync::Arc::clone(&attached);
        panes.set_test_daemon_spawn(move |panes, session| {
            let session = session.ok_or_else(|| anyhow::anyhow!("attach target missing"))?;
            attached_by_spawn.lock().unwrap().push(session.to_string());
            Ok(panes.insert_test_daemon_pane(session.to_string()))
        });

        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        // Model a replacement request that owns the shifted index. A late
        // result for the closed tab must not settle this newer reservation.
        let mut prewarm_inflight =
            std::collections::HashSet::from([("app/home".into(), 0), ("app/home".into(), 1)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 1,
                target_leaves: vec![6],
                origin: SpecOrigin::Prewarm,
                specs: Err(SpecError::PrewarmSkipped),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: vec![AttachTarget {
                    session: "stale-session".into(),
                    program: "claude".into(),
                }],
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(
            attached.lock().unwrap().is_empty(),
            "a stale skipped prewarm must not attach into the shifted tab"
        );
        assert_eq!(
            panes.table.len(),
            3,
            "no pane may be grafted by a stale batch"
        );
        assert_eq!(session.worktrees[0].tabs.len(), 2);
        assert_eq!(
            session.worktrees[0].tabs[1].center,
            crate::center::CenterTree::Leaf(7),
            "the shifted live tab remains unchanged"
        );
        assert!(
            prewarm_inflight.contains(&("app/home".into(), 1)),
            "a stale result must not settle the replacement tab's reservation"
        );
        assert!(
            !prewarm_inflight.contains(&("app/home".into(), 0)),
            "the captured tab's shifted reservation is settled"
        );
        assert!(!need_relayout, "a dropped batch does not change geometry");
        assert!(!dirty, "a dropped batch does not dirty the frame");
    }

    /// A spec batch is addressed by tab INDEX, captured at request time. Close a
    /// tab to its left and that index names a different, already-live tab — so
    /// the batch must be dropped WHOLE, before any loading write. Without the
    /// guard, `advance_to_shell` dressed the live tab in the shell-wait splash,
    /// which is what arms `startup_watchdog::tick` against its healthy pane (the
    /// watchdog then swaps that pane for a clean rc-free shell: the reported
    /// "the tab to the right drops to a bare bash prompt").
    ///
    /// Pane ids are stable and never reused, so the batch's own leaf ids are the
    /// identity check.
    #[test]
    fn drain_specs_does_not_reseed_a_splash_after_shell_output() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        let leaf = 5;
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(leaf);
        session.worktrees[0].tabs[0].focused_pane = leaf;
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(tx);
        panes.insert_test_pane(leaf);

        let cfg = thegn_core::config::Config::default();
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let key = ("app/home".to_string(), 0);
        // This is the reachable post-output state: handle_output has removed
        // the shell-wait entry and recorded its key for late spec batches.
        let loading_retired = std::collections::HashSet::from([key.clone()]);
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        spec_tx
            .send(SpecBatch {
                group: key.0.clone(),
                worktree: String::new(),
                tab: key.1,
                target_leaves: vec![leaf],
                origin: SpecOrigin::Materialize,
                specs: Ok(Vec::new()),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(
            loading_state.get(&key).is_none(),
            "a spec result arriving after shell output must not restore shell-wait"
        );
        assert!(
            loading_remote.is_empty(),
            "a retired splash has no watchdog entry"
        );
    }

    #[test]
    fn drain_specs_drops_a_batch_whose_target_tab_no_longer_holds_its_leaves() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        {
            let g = &mut session.worktrees[0];
            g.tabs[0].center = crate::center::CenterTree::Leaf(5);
            g.tabs[0].focused_pane = 5;
            g.add_tab();
            g.tabs[1].center = crate::center::CenterTree::Leaf(6);
            g.tabs[1].focused_pane = 6;
            g.active_tab = 0;
        }
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(tx);
        panes.insert_test_pane(5);
        panes.insert_test_pane(6);

        let cfg = thegn_core::config::Config::default();
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::new();
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();

        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel::<SpecBatch>();
        let spec = crate::agent::LaunchSpec {
            argv: vec!["/bin/sh".into()],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        };
        // Leaf 99 belonged to the tab that was closed; index 1 now names a
        // different, live tab (leaf 6).
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 1,
                target_leaves: vec![99],
                origin: SpecOrigin::Prewarm,
                specs: Ok(vec![(99u32, spec)]),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(
            loading_state.get(&("app/home".to_string(), 1)).is_none(),
            "a stale batch must not dress a live tab in a shell-wait splash"
        );
        assert!(
            loading_remote.is_empty(),
            "nor seed its watchdog-deadline entry"
        );
        let tab = &session.worktrees[0].tabs[1];
        assert_eq!(tab.center, crate::center::CenterTree::Leaf(6));
        assert_eq!(tab.focused_pane, 6, "the live tab's focus is untouched");
        assert!(
            panes.table.contains_key(&5) && panes.table.contains_key(&6),
            "no live pane is disturbed"
        );
    }

    /// The stale guard must also run when there is no daemon attachment to
    /// graft. This keeps the replacement request's reservation intact for the
    /// same shifted-index race without relying on the attach effect to expose
    /// the bug.
    #[test]
    fn drain_specs_drops_stale_prewarm_skip_without_attach() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "")],
            active: 0,
        };
        {
            let g = &mut session.worktrees[0];
            g.tabs[0].center = crate::center::CenterTree::Leaf(5);
            g.tabs[0].focused_pane = 5;
            g.add_tab();
            g.tabs[1].center = crate::center::CenterTree::Leaf(6);
            g.tabs[1].focused_pane = 6;
            g.add_tab();
            g.tabs[2].center = crate::center::CenterTree::Leaf(7);
            g.tabs[2].focused_pane = 7;
            g.tabs.remove(0);
        }

        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(16);
        let mut panes = crate::panes::Panes::new(pane_tx);
        for id in [6, 7] {
            panes.insert_test_pane(id);
        }
        let cfg = thegn_core::config::Config::default();
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::from([("app/home".into(), 1)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(SpecBatch {
                group: "app/home".into(),
                worktree: String::new(),
                tab: 1,
                target_leaves: vec![6],
                origin: SpecOrigin::Prewarm,
                specs: Err(SpecError::PrewarmSkipped),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(prewarm_inflight.contains(&("app/home".into(), 1)));
        assert!(loading_state.get(&("app/home".into(), 1)).is_none());
        assert!(!need_relayout);
        assert!(!dirty);
    }

    /// A worktree can be removed while its off-thread prewarm is resolving.
    /// The result has no tab left to target, but its reservation still belongs
    /// to the removed group and must not remain in the loop-local set forever.
    #[test]
    fn drain_specs_releases_prewarm_reservation_when_group_disappears() {
        vanished_group_reservation(SpecOrigin::Prewarm);
    }

    #[test]
    fn drain_specs_releases_materialize_reservation_when_group_disappears() {
        vanished_group_reservation(SpecOrigin::Materialize);
    }

    fn vanished_group_reservation(origin: SpecOrigin) {
        let mut session = Session {
            id: "s1".into(),
            worktrees: Vec::new(),
            active: 0,
        };
        let (pane_tx, _pane_rx) = tokio_mpsc::channel::<PaneEvent>(16);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let cfg = thegn_core::config::Config::default();
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let loading_retired = std::collections::HashSet::new();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight =
            std::collections::HashSet::from([("removed/group".into(), 0usize)]);
        let mut prewarm_inflight =
            std::collections::HashSet::from([("removed/group".into(), 0usize)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(SpecBatch {
                group: "removed/group".into(),
                worktree: String::new(),
                tab: 0,
                target_leaves: vec![42],
                origin,
                specs: Err(SpecError::PrewarmSkipped),
                relaunch: RelaunchOutcome::NotAttempted,
                attach: Vec::new(),
                remote: false,
                remembered_agent: None,
            })
            .unwrap();

        drain_specs(
            &mut spec_rx,
            &mut SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_retired: &loading_retired,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert_eq!(prewarm_inflight.is_empty(), origin == SpecOrigin::Prewarm);
        assert_eq!(
            materialize_inflight.is_empty(),
            origin == SpecOrigin::Materialize
        );
        assert!(!need_relayout);
        assert!(!dirty);
    }

    fn provider_db() -> thegn_core::db::Db {
        use thegn_core::store::WorkspaceStore as _;
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree(
            "repo/main",
            "/repo",
            "/remote/wt",
            "main",
            Some(r#"{"control_prefix":["provider"],"path":"/remote/wt"}"#),
            None,
        )
        .unwrap();
        db
    }

    #[test]
    fn adjust_pool_target_writes_the_effective_env_key() {
        use thegn_core::store::{PoolStore as _, WorkspaceStore as _};
        let db = provider_db();
        db.set_worktree_env("/remote/wt", "selected-env").unwrap();
        let cfg = thegn_core::config::Config::default();
        assert_eq!(
            adjust_pool_target(&db, &cfg, "/remote/wt", 1),
            "warm pool target: 1 spare(s)"
        );
        assert_eq!(db.pool_target("/repo", "selected-env").unwrap(), Some(1));
        assert_eq!(db.pool_target("/repo", "default").unwrap(), None);
    }

    #[test]
    fn adjust_pool_target_refuses_a_target_read_error_without_persisting() {
        use thegn_core::store::{PoolStore as _, WorkspaceStore as _};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool-target.sqlite");
        let db = thegn_core::db::Db::open_at(&path).unwrap();
        db.put_worktree(
            "repo/main",
            "/repo",
            "/remote/wt",
            "main",
            Some(r#"{"control_prefix":["provider"],"path":"/remote/wt"}"#),
            None,
        )
        .unwrap();
        db.set_pool_target("/repo", "default", 2).unwrap();

        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE pool_targets")
            .unwrap();

        let refusal =
            adjust_pool_target(&db, &thegn_core::config::Config::default(), "/remote/wt", 1);
        assert!(
            refusal.starts_with("warm pool: cannot read the current target:"),
            "failed target reads must be reported as a refusal, got: {refusal}"
        );

        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE pool_targets (
                   repo_path TEXT NOT NULL,
                   env_name TEXT NOT NULL,
                   target INTEGER NOT NULL,
                   PRIMARY KEY (repo_path, env_name)
                 )",
            )
            .unwrap();
        assert_eq!(db.pool_target("/repo", "default").unwrap(), None);
    }

    #[test]
    fn adjust_pool_target_clamps_at_the_ceiling_and_floors_at_zero() {
        use thegn_core::store::PoolStore as _;
        let db = provider_db();
        let cfg = thegn_core::config::Config::default();
        db.set_pool_target(
            "/repo",
            "default",
            crate::lifecycle::POOL_TARGET_CEILING as i64,
        )
        .unwrap();
        assert_eq!(
            adjust_pool_target(&db, &cfg, "/remote/wt", 1),
            "warm pool target: 8 spare(s)"
        );
        assert_eq!(db.pool_target("/repo", "default").unwrap(), Some(8));
        assert_eq!(
            adjust_pool_target(&db, &cfg, "/remote/wt", -20),
            "warm pool target: 0 spare(s)"
        );
        assert_eq!(db.pool_target("/repo", "default").unwrap(), Some(0));
    }

    #[test]
    fn adjust_pool_target_refuses_local_worktrees() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let cfg = thegn_core::config::Config::default();
        assert_eq!(
            adjust_pool_target(&db, &cfg, "/local/wt", 1),
            "warm pool: only for provider (remote) workspaces"
        );
    }

    #[test]
    fn pool_target_adjust_does_not_block_on_a_held_write_lock() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        use thegn_core::store::WorkspaceStore as _;

        let state = crate::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree(
            "repo/main",
            "/repo",
            "/remote/wt",
            "main",
            Some(r#"{"control_prefix":["provider"],"path":"/remote/wt"}"#),
            None,
        )
        .unwrap();
        drop(db);
        let lock = rusqlite::Connection::open(state.state_home().join("thegn/thegn.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let session = crate::session::Session {
            id: "test".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "repo/main",
                crate::session::GroupKind::Home,
                "/remote/wt",
            )],
            active: 0,
        };
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let (waker, _fixture) = crate::platform::sound_process::test_support::test_waker();
        let started = std::time::Instant::now();
        assert_eq!(
            pool_target_adjust(
                &session,
                &thegn_core::config::Config::default(),
                1,
                &tx,
                &waker
            )
            .as_deref(),
            Some("warm pool: updating target…")
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio_mpsc::error::TryRecvError::Empty)
        ));
        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
        crate::db_task::persist_critical(move |_| {
            if barrier_tx.send(()).is_err() {
                tracing::debug!(target: "thegn::provision", "pool target barrier receiver closed");
            }
        });
        barrier_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::run::run_pending_db::PendingDbResult::PoolTarget { status })
                if status == "warm pool target: 1 spare(s)"
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio_mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn sealed_hint_doctor_home_lines_name_the_switch() {
        use thegn_core::sandbox_mounts::HomeView;

        let not_enforced = crate::cmd::doctor::home_report_lines(
            thegn_core::config::SandboxProfile::Sealed,
            HomeView::Hidden,
            false,
        );
        assert_eq!(not_enforced.len(), 1);
        assert!(not_enforced[0].starts_with("NOT ENFORCED:"));
        assert!(not_enforced[0].contains("thegn sandbox profile set standard"));

        let missed = crate::cmd::doctor::home_report_lines(
            thegn_core::config::SandboxProfile::Sealed,
            HomeView::ReadOnly,
            true,
        );
        assert_eq!(missed.len(), 1);
        assert!(missed[0].starts_with("MISSED:"));
        assert!(missed[0].contains("thegn sandbox profile set standard"));

        assert!(
            crate::cmd::doctor::home_report_lines(
                thegn_core::config::SandboxProfile::Sealed,
                HomeView::Hidden,
                true,
            )
            .is_empty()
        );
        assert!(
            crate::cmd::doctor::home_report_lines(
                thegn_core::config::SandboxProfile::Hardened,
                HomeView::ReadOnly,
                true,
            )
            .is_empty()
        );
    }

    #[test]
    fn sealed_hint_overlay_for_sealed_halt_has_no_run_on_host_item() {
        let halt = crate::agent::SandboxHalt {
            env_name: "dev".into(),
            placement: "provider:sprites".into(),
            reason: format!(
                "profile `sealed` requires a sandbox that hides $HOME. {}",
                thegn_core::sandbox_floor::standard_switch_hint(None)
            ),
            ask: true,
            dormant: None,
            sealed_home: true,
        };
        let overlay = sandbox_halt_overlay(&halt);
        let labels = overlay
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        assert!(
            labels.iter().any(|label| label.contains("retry")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("dismiss")),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("run on host")),
            "{labels:?}"
        );
        let body = sealed_halt_body(&halt);
        assert!(body.contains("profile `sealed` requires a sandbox that hides $HOME"));
        assert!(
            body.contains("thegn sandbox profile set standard"),
            "{body}"
        );
        assert_eq!(
            body.matches("thegn sandbox profile set standard").count(),
            1,
            "the sealed hint appears once"
        );
    }

    #[test]
    fn no_backend_halt_offers_one_key_run_on_host_with_install_fix() {
        // A clean-Mac stub: the launch path appends the per-OS fix to the reason.
        let halt = crate::agent::SandboxHalt {
            env_name: "local".into(),
            placement: "local".into(),
            reason: format!(
                "no usable backend produced a runnable sandbox; {}",
                thegn_core::sandbox_floor::no_backend_fix(true)
            ),
            ask: true,
            dormant: None,
            sealed_home: false,
        };
        let overlay = sandbox_halt_overlay(&halt);
        let keyed = |key: char, choice: &menu::MenuChoice| {
            overlay
                .items()
                .iter()
                .any(|item| item.key == Some(key) && &item.choice == choice)
        };
        assert!(keyed('h', &menu::MenuChoice::SandboxRunOnHost));
        assert!(keyed('r', &menu::MenuChoice::SandboxRetry));
        assert!(keyed('n', &menu::MenuChoice::Dismiss));
        assert_eq!(overlay.title, "⚠ no sandbox backend");
        let body = no_backend_halt_body(&halt);
        assert!(body.contains("Apple `container`"), "{body}");
        assert!(body.contains("brew install podman"), "{body}");
        assert!(body.contains("NO sandbox"), "{body}");
        assert!(body.contains("Nothing is remembered"), "{body}");
        assert!(!body.contains("failover"), "{body}");
    }

    #[test]
    fn sealed_hint_sealed_halt_offers_no_host_remedy() {
        let sealed = crate::agent::SandboxHalt {
            env_name: "dev".into(),
            placement: "provider:sprites".into(),
            reason: format!(
                "profile `sealed` requires a sandbox that hides $HOME. {}",
                thegn_core::sandbox_floor::standard_switch_hint(None)
            ),
            ask: true,
            dormant: None,
            sealed_home: true,
        };
        let message = sealed.to_string();
        assert!(
            message.contains("thegn sandbox profile set standard"),
            "{message}"
        );
        assert_eq!(
            message
                .matches("thegn sandbox profile set standard")
                .count(),
            1,
            "the sealed hint appears once"
        );
        for forbidden in ["backend = \"none\"", "isolation_floor", "run on host"] {
            assert!(!message.contains(forbidden), "{message}");
        }

        let ordinary = crate::agent::SandboxHalt {
            env_name: "dev".into(),
            placement: "provider:sprites".into(),
            reason: "token missing".into(),
            ask: true,
            dormant: None,
            sealed_home: false,
        };
        assert_eq!(
            ordinary.to_string(),
            "environment 'dev' (provider:sprites) could not be brought up: token missing — thegn will not silently fall back to the host. Fix the env, retry, choose \"run on host\", set `backend = \"none\"`, disable sandboxing, or set `isolation_floor = \"off\"`; otherwise check the provider token (`thegn env test dev`; `api_key_env` in [env.dev.provider]) ([sandbox] or [env.dev]) to allow it."
        );
    }
}
