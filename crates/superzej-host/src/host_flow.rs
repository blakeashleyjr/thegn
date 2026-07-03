//! **ensure_ready(host)** — the blocking, single-flight driver that walks a
//! host's provisioning state machine to `Ready` and injects its assets
//! (digest-pinned image, warm volumes, remote OCI URL) into sandbox specs.
//!
//! Layering: `ensure_ready` completes BEFORE any `sandbox_lock` is taken (the
//! host is a coarser, earlier gate; locks never nest). All entry points —
//! materialize, eager, warm pool, wizard, CLI — converge here, so
//! single-flight is total by construction:
//! - the FIRST caller becomes the leader: it takes `provision_gate::host_lock`
//!   and drives the pure [`host_machine`] against a [`HostRunner`];
//! - concurrent callers become followers on the in-process Flight registry,
//!   forwarding every progress snapshot to their OWN tab's splash callback;
//! - cross-process arbitration rides the DB row's heartbeat (a fresh heartbeat
//!   means another process is driving — wait and re-read, don't double-drive).
//!
//! Everything here BLOCKS and must run off-loop (spawn_blocking / CLI / pool
//! threads) — exactly like the provisioning pipeline it fronts.

use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;

use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::host::{
    HostCaps, HostFailure, HostStep, Reach, ReadyHostSpec, RuntimeKind, apply_ready_host,
};
use superzej_core::host_config::{HostBinding, InstallConsent};
use superzej_core::host_machine::{
    HostEffect, HostEvent as MachineEvent, HostState, MachineCtx, Transition, resume, step,
};
use superzej_core::image::managed_tag;
use superzej_core::inventory::ArtifactKind;
use superzej_svc::host::{HostRunner, local_caps, runner_for};

use crate::agent::{ProvisionState, ProvisionStepView};

/// Injectable runner factory (tests swap in mocks; production uses
/// [`runner_for`]).
pub(crate) type RunnerFactory<'a> =
    &'a mut dyn FnMut(&Reach) -> Result<Box<dyn HostRunner>, String>;

/// How a caller wants install-consent questions handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentPolicy {
    /// Focused materialize / wizard: raise the confirm modal and park.
    Interactive,
    /// Eager / warm pool: never prompt, never install — defer to a focused
    /// materialize.
    BackgroundSkip,
    /// CLI: `--yes` pre-grants; without it a needed consent fails with the
    /// flag named (the CLI prompts on a tty BEFORE calling in).
    Headless { assume_yes: bool },
}

/// Outcome of [`ensure_ready`].
#[derive(Debug)]
pub(crate) enum HostOutcome {
    /// Host is Ready; its assets are registered for spec injection.
    Ready(ReadyHostSpec),
    /// The worktree's env has no explicit host binding — the status quo path.
    NotHostBacked,
    /// Background policy hit a consent question: nothing installed, nothing
    /// failed; a focused materialize will raise the ask.
    Deferred,
}

/// Host-level UI events for the loop (panel/sidebar/chip/consent modal) —
/// keyed by host, unlike the per-tab splash which rides the caller's own
/// progress callback.
#[derive(Debug)]
pub(crate) enum HostUiEvent {
    Progress {
        host: String,
        steps: Vec<ProvisionStepView>,
    },
    NeedsConsent {
        host: String,
        runtime: String,
    },
    Done {
        host: String,
        result: Result<(), HostFailure>,
    },
}

/// The loop-facing sender bundle, cloned into every provisioning closure.
#[derive(Clone)]
pub(crate) struct HostUiTx {
    pub tx: tokio::sync::mpsc::UnboundedSender<HostUiEvent>,
    pub waker: termwiz::terminal::TerminalWaker,
}

impl HostUiTx {
    fn send(&self, ev: HostUiEvent) {
        // best-effort: the loop may be gone during shutdown
        let _ = self.tx.send(ev);
        self.waker.wake().ok();
    }
}

// ── Flight registry: in-process single-flight with live snapshots ──────────

#[derive(Default)]
struct Flight {
    steps: Vec<ProvisionStepView>,
    seq: u64,
    done: Option<Result<ReadyHostSpec, HostFailure>>,
    /// A parked consent ask: `None` = not asked; `Some(None)` = awaiting;
    /// `Some(Some(granted))` = answered.
    consent: Option<Option<bool>>,
}

static FLIGHTS: LazyLock<(Mutex<HashMap<String, Flight>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashMap::new()), Condvar::new()));

/// How long a parked interactive consent ask waits before counting as denied.
const CONSENT_WAIT: Duration = Duration::from_secs(600);
/// A DB heartbeat fresher than this means another PROCESS is driving: attach.
const HEARTBEAT_FRESH_SECS: i64 = 60;

/// Resolve a parked consent ask (the loop's confirm-modal handler and the
/// panel's consent action call this).
pub(crate) fn resolve_consent(host: &str, granted: bool) {
    let (map, cv) = &*FLIGHTS;
    let mut m = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = m.get_mut(host)
        && matches!(f.consent, Some(None))
    {
        f.consent = Some(Some(granted));
        cv.notify_all();
    }
}

/// Per-worktree assets registered by a completed `ensure_ready`, consumed at
/// sandbox-spec build time by [`apply_ready`].
static READY_SPECS: LazyLock<Mutex<HashMap<String, ReadyHostSpec>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record the pane-entry hook the per-worktree pipeline derived (e.g. eval
/// the synthesized devshell) so later spec builds inject it.
pub(crate) fn set_ready_init(worktree: &str, init: Option<String>) {
    let Some(init) = init else { return };
    let mut m = READY_SPECS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(spec) = m.get_mut(worktree) {
        spec.init_script = Some(init);
    }
}

/// Inject a Ready host's assets into `spec` when this worktree's env is
/// host-backed and its host reached Ready. No-op otherwise. Called from the
/// sandbox-spec build path.
pub(crate) fn apply_ready(worktree: &str, spec: &mut superzej_core::sandbox::SandboxSpec) {
    let map = READY_SPECS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(rh) = map.get(worktree) {
        apply_ready_host(spec, rh);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the worktree's env → explicit host binding. Conservative gate:
/// engages ONLY for envs with an explicit `host = "<name>"` reference — the
/// implicit anonymous-host lowering for inline-ssh envs exists in core but is
/// not auto-engaged (it would switch those envs onto the superzej base image);
/// cloud reaches stay with the existing provider pipeline until the phase-6
/// lowering flips on.
fn resolve_binding(cfg: &Config, worktree: &str) -> Option<(String, HostBinding)> {
    let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let repo_root = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let selected = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        std::path::Path::new(worktree),
        selected.as_deref(),
    );
    let envc = cfg.env.get(&environment.name)?;
    if envc.host.trim().is_empty() {
        return None;
    }
    let binding = cfg.resolve_host_binding(&environment.name, envc)?;
    // Cloud reaches engage ONLY via an explicit [host.<name>] reference (the
    // user opted the machine in); an implicit provider-placement derivation
    // stays with the legacy sprites pipeline until its lowering is
    // live-verified end to end.
    if matches!(binding.reach, Reach::Cloud(_)) && binding.id.config_name().is_none() {
        return None;
    }
    Some((environment.name, binding))
}

/// Quick prewarm/eager gate: does this worktree target a host that is NOT yet
/// Ready-and-fresh? (Prewarm skips such tabs; materialize will drive them.)
pub(crate) fn host_pending(cfg: &Config, worktree: &str) -> bool {
    let Some((_, binding)) = resolve_binding(cfg, worktree) else {
        return false;
    };
    let Ok(db) = Db::open() else {
        return true;
    };
    !ready_fresh(&db, &binding)
}

fn ready_fresh(db: &Db, binding: &HostBinding) -> bool {
    db.host_get(&binding.id).ok().flatten().is_some_and(|row| {
        row.state == HostState::Ready
            && row
                .last_probe
                .is_some_and(|t| unix_now().saturating_sub(t) <= binding.probe_ttl_secs)
    })
}

/// Drop-in wrapper for `agent::provision_worktree`: bring the env's host to
/// Ready first (streaming host steps into the SAME per-tab splash callback),
/// then delegate to the existing provider provisioning. Host failures surface
/// as [`SandboxHalt`](crate::agent::SandboxHalt) so the existing halt modal
/// renders them.
pub(crate) fn provision_worktree(
    cfg: &Config,
    worktree: &str,
    policy: ConsentPolicy,
    mut progress: impl FnMut(&[ProvisionStepView]),
    ui: Option<&HostUiTx>,
) -> anyhow::Result<bool> {
    match ensure_ready(cfg, worktree, policy, &mut progress, ui) {
        Ok(HostOutcome::Ready(_)) => {
            // The host is Ready: run the per-worktree pipeline (repo toolchain
            // + personal layer inside the container, best-effort) and record
            // the pane-entry hook, then fall through to the provider pipeline
            // (a no-op for host-backed ssh/local envs).
            if let Some((env_name, binding)) = resolve_binding(cfg, worktree) {
                let init = crate::host_provision::provision_worktree_on_host(
                    cfg,
                    worktree,
                    &env_name,
                    &binding,
                    &mut progress,
                );
                set_ready_init(worktree, init);
            }
            crate::agent::provision_worktree(cfg, worktree, progress)
        }
        Ok(_) => crate::agent::provision_worktree(cfg, worktree, progress),
        Err(f) => {
            let (env_name, binding) = resolve_binding(cfg, worktree)
                .map(|(n, b)| (n, b.id.to_string()))
                .unwrap_or_else(|| ("?".into(), "host".into()));
            Err(anyhow::Error::new(crate::agent::SandboxHalt {
                env_name,
                placement: binding,
                reason: failure_reason(&f),
            }))
        }
    }
}

/// The actionable one-liner for a host failure.
pub(crate) fn failure_reason(f: &HostFailure) -> String {
    let retry = if f.retryable {
        " (retry from System ▸ Hosts or `superzej host provision`)"
    } else {
        ""
    };
    format!("host {}: {}{retry}", f.step.label(), f.error)
}

/// Bring the worktree's host to `Ready`. Instant when the DB row is Ready with
/// a fresh probe; joins an in-flight provision as a follower; otherwise drives
/// the machine as the leader. Blocking — off-loop only.
pub(crate) fn ensure_ready(
    cfg: &Config,
    worktree: &str,
    policy: ConsentPolicy,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
    ui: Option<&HostUiTx>,
) -> Result<HostOutcome, HostFailure> {
    let Some((_env, binding)) = resolve_binding(cfg, worktree) else {
        return Ok(HostOutcome::NotHostBacked);
    };
    let out = ensure_host_ready(&binding, policy, progress, ui, &mut |reach| {
        runner_for(reach)
    })?;
    if let HostOutcome::Ready(spec) = &out {
        READY_SPECS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(worktree.to_string(), spec.clone());
    }
    Ok(out)
}

/// The binding-level core (also the CLI entry): fast path → flight join →
/// leader drive. `make_runner` is injectable for tests.
pub(crate) fn ensure_host_ready(
    binding: &HostBinding,
    policy: ConsentPolicy,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
    ui: Option<&HostUiTx>,
    make_runner: RunnerFactory<'_>,
) -> Result<HostOutcome, HostFailure> {
    let db = Db::open().map_err(|e| HostFailure {
        step: HostStep::Connect,
        error: format!("state db: {e}"),
        retryable: true,
    })?;
    ensure_host_ready_with(&db, binding, policy, progress, ui, make_runner)
}

pub(crate) fn ensure_host_ready_with(
    db: &Db,
    binding: &HostBinding,
    policy: ConsentPolicy,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
    ui: Option<&HostUiTx>,
    make_runner: RunnerFactory<'_>,
) -> Result<HostOutcome, HostFailure> {
    let key = binding.id.to_string();
    // Golden fast path: one SQLite read, zero network.
    if ready_fresh(db, binding) {
        let _ = db.host_touch_used(&binding.id, unix_now());
        if let Some(spec) = spec_from_inventory(db, binding) {
            return Ok(HostOutcome::Ready(spec));
        }
        // Ready but no usable inventory (pruned?): fall through and re-drive.
    }

    // Join or start the in-process flight.
    {
        let (map, _cv) = &*FLIGHTS;
        let mut m = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if m.contains_key(&key) {
            drop(m);
            return follow_flight(&key, progress);
        }
        m.insert(key.clone(), Flight::default());
    }

    // Leader. The gate lock is belt-and-braces vs a racing CLI thread in the
    // same process that bypassed the flight map.
    let _gate = crate::provision_gate::host_lock(&key);
    let result = drive(db, binding, policy, progress, ui, make_runner);

    // Publish the terminal state and remove the flight.
    let (map, cv) = &*FLIGHTS;
    let mut m = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = m.get_mut(&key) {
        f.done = Some(match &result {
            Ok(HostOutcome::Ready(spec)) => Ok(spec.clone()),
            Ok(_) => Err(HostFailure {
                step: HostStep::Consent,
                error: "deferred (background provisioning never installs)".into(),
                retryable: true,
            }),
            Err(e) => Err(e.clone()),
        });
        f.seq += 1;
    }
    cv.notify_all();
    m.remove(&key);
    drop(m);

    if let Some(ui) = ui {
        ui.send(HostUiEvent::Done {
            host: key,
            result: match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(e.clone()),
            },
        });
    }
    result
}

/// Follower: forward each snapshot to OUR caller's splash until the leader
/// finishes.
fn follow_flight(
    key: &str,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> Result<HostOutcome, HostFailure> {
    let (map, cv) = &*FLIGHTS;
    let mut m = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut seen = 0u64;
    loop {
        let Some(f) = m.get(key) else {
            // Leader finished and cleaned up between wakes: re-read the DB via
            // the caller retrying is overkill — the done snapshot was pushed
            // before removal, so reaching here means we joined very late.
            return Err(HostFailure {
                step: HostStep::Connect,
                error: "host provision finished elsewhere; retry".into(),
                retryable: true,
            });
        };
        if f.seq != seen {
            seen = f.seq;
            let steps = f.steps.clone();
            if let Some(done) = &f.done {
                let done = done.clone();
                drop(m);
                progress(&steps);
                return done.map(HostOutcome::Ready);
            }
            drop(m);
            progress(&steps);
            m = map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        }
        if let Some(done) = &f.done {
            return done.clone().map(HostOutcome::Ready);
        }
        m = cv
            .wait_timeout(m, Duration::from_secs(2))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// Rebuild the fast-path spec from persisted inventory (newest image for the
/// host's arch) — no runner, no network.
fn spec_from_inventory(db: &Db, binding: &HostBinding) -> Option<ReadyHostSpec> {
    let row = db.host_get(&binding.id).ok().flatten()?;
    let arch = row.caps.as_ref().map(|c| c.arch).or(row.arch)?;
    let inv = db.host_inventory(&binding.id).ok()?;
    let image = inv
        .iter()
        .filter(|e| e.key.kind == ArtifactKind::Image && e.key.arch == arch)
        .max_by_key(|e| e.present_at)?;
    Some(ReadyHostSpec {
        image: managed_tag(&image.key.digest),
        oci_url: oci_url_for(&binding.reach, row.caps.as_ref()),
        volumes: binding
            .volumes
            .iter()
            .map(|v| (v.name.clone(), v.dest.clone()))
            .collect(),
        // The per-worktree pipeline fills the pane-entry hook after it runs
        // (see set_ready_init); the host-level spec has none of its own.
        init_script: None,
    })
}

/// The remote OCI daemon URL for spec pinning, derived purely from reach +
/// probed caps (`None` ⇒ the placement transport wraps the argv, as today).
fn oci_url_for(reach: &Reach, caps: Option<&HostCaps>) -> Option<String> {
    match reach {
        Reach::Ssh(p) => {
            let socket = caps?.runtime.as_ref()?.socket.as_deref()?;
            Some(format!("ssh://{}:{}{}", p.host, p.port, socket))
        }
        _ => None,
    }
}

// ── The step board: machine progress → splash step views ───────────────────

/// Fixed display rows for a host provision. Labels never equal "shell", so
/// the loading.rs splash arbitration (`is_shell_wait`, `provision_owns_tab`)
/// treats a host-provisioning tab as owned for its whole duration.
struct StepBoard {
    rows: Vec<ProvisionStepView>,
}

impl StepBoard {
    fn new(host: &str) -> StepBoard {
        let mk = |label: &str| ProvisionStepView {
            label: label.to_string(),
            state: ProvisionState::Pending,
            detail: None,
        };
        StepBoard {
            rows: vec![
                mk(&format!("host {host}")),
                mk("connect"),
                mk("probe runtime"),
                mk("image"),
                mk("warm volumes"),
            ],
        }
    }

    fn row_for(&mut self, step: HostStep) -> &mut ProvisionStepView {
        let i = match step {
            HostStep::Connect => 1,
            HostStep::Probe => 2,
            HostStep::Consent | HostStep::Install => {
                // Install surfaces as a detail on the probe row (it only exists
                // when the probe found no runtime).
                2
            }
            HostStep::ResolveImage | HostStep::Deliver | HostStep::Verify => 3,
            HostStep::SeedVolume => 4,
        };
        &mut self.rows[i]
    }

    fn start(&mut self, step: HostStep, detail: Option<String>) {
        // Header row shows overall liveness.
        self.rows[0].state = ProvisionState::Active;
        let row = self.row_for(step);
        row.state = ProvisionState::Active;
        row.detail = detail;
    }

    fn finish(&mut self, step: HostStep, detail: Option<String>) {
        let row = self.row_for(step);
        row.state = ProvisionState::Done;
        row.detail = detail;
    }

    fn fail(&mut self, step: HostStep, error: &str) {
        let row = self.row_for(step);
        row.state = ProvisionState::Failed;
        row.detail = Some(error.to_string());
        self.rows[0].state = ProvisionState::Failed;
    }

    fn all_done(&mut self) {
        for r in &mut self.rows {
            if r.state != ProvisionState::Failed {
                r.state = ProvisionState::Done;
            }
        }
    }

    fn views(&self) -> Vec<ProvisionStepView> {
        self.rows.clone()
    }
}

/// Human-readable byte count for transfer progress.
fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

// ── The leader drive loop ───────────────────────────────────────────────────

/// Marker for the background-defer control flow inside the drive loop.
enum DriveFlow {
    Event(MachineEvent),
    Deferred,
}

#[expect(clippy::too_many_arguments)]
fn run_effect(
    db: &Db,
    binding: &HostBinding,
    policy: ConsentPolicy,
    runner: &mut dyn HostRunner,
    ctx: &MachineCtx,
    board: &mut StepBoard,
    publish: &mut dyn FnMut(&StepBoard),
    ui: Option<&HostUiTx>,
    effect: HostEffect,
) -> Option<DriveFlow> {
    let now = unix_now();
    let key = binding.id.to_string();
    let ev = match effect {
        HostEffect::Connect => {
            let _ = db.host_heartbeat(&binding.id, "connect", now);
            board.start(HostStep::Connect, None);
            publish(board);
            match runner.connect() {
                Ok(()) => {
                    board.finish(HostStep::Connect, None);
                    MachineEvent::Connected
                }
                Err(error) => MachineEvent::ConnectFailed { error },
            }
        }
        HostEffect::Probe => {
            let _ = db.host_heartbeat(&binding.id, "probe", now);
            board.start(HostStep::Probe, None);
            publish(board);
            match runner.probe() {
                Ok(caps) => {
                    let _ = db.host_touch_probe(&binding.id, unix_now());
                    let detail = caps
                        .runtime
                        .as_ref()
                        .map(|r| format!("{} {}", r.kind.as_str(), r.version));
                    board.finish(HostStep::Probe, detail);
                    MachineEvent::Probed(caps)
                }
                Err(error) => MachineEvent::ProbeFailed { error },
            }
        }
        HostEffect::AskConsent { runtime } => {
            board.start(
                HostStep::Consent,
                Some("awaiting install consent".to_string()),
            );
            publish(board);
            match consent_answer(db, binding, policy, runtime, ui) {
                Some(true) => {
                    let _ = db.host_set_consent(&binding.id, true, unix_now());
                    MachineEvent::ConsentGranted
                }
                Some(false) => {
                    let _ = db.host_set_consent(&binding.id, false, unix_now());
                    MachineEvent::ConsentDenied
                }
                None => return Some(DriveFlow::Deferred),
            }
        }
        HostEffect::Install { runtime } => {
            let _ = db.host_heartbeat(&binding.id, "install", now);
            board.start(
                HostStep::Install,
                Some(format!("installing {}", runtime.as_str())),
            );
            publish(board);
            let mut note = |s: String| {
                board.start(HostStep::Install, Some(s));
                publish(board);
            };
            match runner.install_runtime(runtime, &mut note) {
                Ok(rt) => MachineEvent::Installed(rt),
                Err(error) => MachineEvent::InstallFailed { error },
            }
        }
        HostEffect::ResolveImage { reference } => {
            let _ = db.host_heartbeat(&binding.id, "resolve_image", now);
            board.start(HostStep::ResolveImage, Some(format!("resolve {reference}")));
            publish(board);
            match runner.resolve_image(&reference) {
                Ok(r) => MachineEvent::ImageResolved(r),
                Err(error) => MachineEvent::ResolveFailed { error },
            }
        }
        HostEffect::CheckImage { digest } => {
            let _ = db.host_heartbeat(&binding.id, "verify", now);
            board.start(HostStep::Verify, Some(format!("check {}", digest.short())));
            publish(board);
            match runner.image_present(&binding.image, &digest) {
                Ok(true) => {
                    record_image(db, binding, ctx, &digest, true);
                    board.finish(HostStep::Deliver, Some("image present".into()));
                    MachineEvent::ImagePresent
                }
                Ok(false) => MachineEvent::ImageAbsent,
                Err(error) => MachineEvent::ResolveFailed { error },
            }
        }
        HostEffect::Deliver { strategy, digest } => {
            let _ = db.host_heartbeat(&binding.id, "deliver", now);
            board.start(
                HostStep::Deliver,
                Some(format!("via {}", strategy.as_str())),
            );
            publish(board);
            // Byte progress, throttled to ~4 Hz so a fast transfer can't storm
            // the channel/loop.
            let mut last = std::time::Instant::now() - Duration::from_secs(1);
            let mut on_bytes = |done: u64, total: Option<u64>| {
                if last.elapsed() < Duration::from_millis(250) {
                    return;
                }
                last = std::time::Instant::now();
                let detail = match total {
                    Some(t) => format!("{} / {}", fmt_bytes(done), fmt_bytes(t)),
                    None => fmt_bytes(done),
                };
                board.start(HostStep::Deliver, Some(detail));
                publish(board);
                let _ = db.host_heartbeat(&binding.id, "deliver", unix_now());
            };
            match runner.deliver(strategy, &binding.image, &digest, &mut on_bytes) {
                Ok(verified) => {
                    record_image(db, binding, ctx, &verified, false);
                    let _ = db.host_event(
                        &binding.id,
                        HostStep::Deliver.as_str(),
                        &format!("delivered {} via {}", verified.short(), strategy.as_str()),
                        unix_now(),
                    );
                    board.finish(HostStep::Deliver, Some(strategy.as_str().to_string()));
                    MachineEvent::Delivered {
                        verified_digest: verified,
                    }
                }
                Err(error) => MachineEvent::DeliverFailed { error },
            }
        }
        HostEffect::SeedVolume { spec } => {
            let _ = db.host_heartbeat(&binding.id, "seed_volume", now);
            board.start(HostStep::SeedVolume, Some(format!("seed {}", spec.name)));
            publish(board);
            let digest = ctx
                .resolved
                .as_ref()
                .and_then(|r| {
                    ctx.caps
                        .as_ref()
                        .and_then(|c| r.digest_for(c.arch).cloned())
                })
                .unwrap_or_else(|| {
                    // unreachable by machine construction; a junk digest just
                    // fails the seed with a clear error
                    superzej_core::image::Digest::from_hex(&"0".repeat(64)).expect("static")
                });
            match runner.seed_volume(&spec, &binding.image, &digest) {
                Ok(()) => {
                    record_volume(db, binding, ctx, &spec.name, &digest);
                    MachineEvent::VolumeSeeded { volume: spec.name }
                }
                Err(error) => MachineEvent::VolumeSeedFailed {
                    volume: spec.name,
                    error,
                },
            }
        }
        HostEffect::Checkpoint { state } => {
            let name = binding.id.config_name().unwrap_or("").to_string();
            let _ = db.host_checkpoint(
                &binding.id,
                &name,
                binding.reach.kind(),
                &state,
                ctx.caps.as_ref(),
                unix_now(),
            );
            if matches!(state, HostState::Ready | HostState::Failed(_)) {
                let _ = db.host_heartbeat_clear(&binding.id);
            }
            return None;
        }
        HostEffect::Emit { step, detail } => {
            let _ = db.host_event(&binding.id, step.as_str(), &detail, unix_now());
            if let Some(ui) = ui {
                ui.send(HostUiEvent::Progress {
                    host: key,
                    steps: board.views(),
                });
            }
            return None;
        }
    };
    Some(DriveFlow::Event(ev))
}

fn record_image(
    db: &Db,
    binding: &HostBinding,
    ctx: &MachineCtx,
    digest: &superzej_core::image::Digest,
    verified_now: bool,
) {
    let Some(arch) = ctx.caps.as_ref().map(|c| c.arch) else {
        return;
    };
    let now = unix_now();
    let entry = superzej_core::inventory::InventoryEntry {
        key: superzej_core::inventory::InventoryKey {
            host: binding.id.clone(),
            kind: ArtifactKind::Image,
            digest: digest.clone(),
            arch,
        },
        ref_name: binding.image.name_tag(),
        present_at: now,
        verified_at: verified_now.then_some(now),
        size_bytes: None,
    };
    let _ = db.host_inventory_put(&entry);
    if verified_now {
        let _ = db.host_inventory_verify(&entry.key, now);
    }
}

fn record_volume(
    db: &Db,
    binding: &HostBinding,
    ctx: &MachineCtx,
    name: &str,
    digest: &superzej_core::image::Digest,
) {
    let Some(arch) = ctx.caps.as_ref().map(|c| c.arch) else {
        return;
    };
    let _ = db.host_inventory_put(&superzej_core::inventory::InventoryEntry {
        key: superzej_core::inventory::InventoryKey {
            host: binding.id.clone(),
            kind: ArtifactKind::Volume,
            digest: digest.clone(),
            arch,
        },
        ref_name: name.to_string(),
        present_at: unix_now(),
        verified_at: None,
        size_bytes: None,
    });
}

/// Answer an install-consent ask per policy: config pre-grant (`auto` never
/// reaches here — the machine skips the ask), persisted DB grant, interactive
/// park on the flight, or headless flag. `None` ⇒ defer (background).
fn consent_answer(
    db: &Db,
    binding: &HostBinding,
    policy: ConsentPolicy,
    runtime: RuntimeKind,
    ui: Option<&HostUiTx>,
) -> Option<bool> {
    if binding.consent == InstallConsent::Never {
        return Some(false);
    }
    if let Ok(Some(row)) = db.host_get(&binding.id)
        && let Some(granted) = row.install_consent
    {
        // A persisted decline is re-askable only via an explicit user action
        // (the panel consent row), never spontaneously.
        return Some(granted);
    }
    match policy {
        ConsentPolicy::Headless { assume_yes } => Some(assume_yes),
        ConsentPolicy::BackgroundSkip => None,
        ConsentPolicy::Interactive => {
            let key = binding.id.to_string();
            {
                let (map, _) = &*FLIGHTS;
                let mut m = map
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(f) = m.get_mut(&key) {
                    f.consent = Some(None);
                }
            }
            if let Some(ui) = ui {
                ui.send(HostUiEvent::NeedsConsent {
                    host: key.clone(),
                    runtime: runtime.as_str().to_string(),
                });
            } else {
                // No UI to ask: treat as declined rather than hanging forever.
                return Some(false);
            }
            let (map, cv) = &*FLIGHTS;
            let mut m = map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let deadline = std::time::Instant::now() + CONSENT_WAIT;
            loop {
                match m.get(&key).and_then(|f| f.consent) {
                    Some(Some(granted)) => return Some(granted),
                    _ if std::time::Instant::now() >= deadline => return Some(false),
                    _ => {
                        let (guard, _) = cv
                            .wait_timeout(m, Duration::from_secs(5))
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        m = guard;
                    }
                }
            }
        }
    }
}

/// The leader's drive loop: resume from the DB, execute effects, feed events
/// to the pure machine, publish snapshots, persist checkpoints.
fn drive(
    db: &Db,
    binding: &HostBinding,
    policy: ConsentPolicy,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
    ui: Option<&HostUiTx>,
    make_runner: RunnerFactory<'_>,
) -> Result<HostOutcome, HostFailure> {
    // Cross-process arbitration: a fresh heartbeat means another process is
    // mid-drive — wait for it instead of double-driving.
    if let Ok(Some(row)) = db.host_get(&binding.id)
        && let Some(hb) = row.heartbeat
        && unix_now().saturating_sub(hb) < HEARTBEAT_FRESH_SECS
    {
        return attach_external(db, binding, progress);
    }

    let host_label = binding
        .id
        .config_name()
        .unwrap_or(binding.id.as_str())
        .to_string();
    let mut board = StepBoard::new(&host_label);
    let key = binding.id.to_string();
    let mut publish = {
        let ui = ui.cloned_opt();
        move |board: &StepBoard| {
            let views = board.views();
            progress(&views);
            if let Some(ui) = &ui {
                ui.send(HostUiEvent::Progress {
                    host: key.clone(),
                    steps: views.clone(),
                });
            }
            // Update the in-process flight snapshot for followers.
            let (map, cv) = &*FLIGHTS;
            let mut m = map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(f) = m.get_mut(&key) {
                f.steps = views;
                f.seq += 1;
                cv.notify_all();
            }
        }
    };

    let mut runner = make_runner(&binding.reach).map_err(|error| HostFailure {
        step: HostStep::Connect,
        error,
        retryable: false,
    })?;

    let mut ctx = MachineCtx::new(
        binding.consent,
        local_caps(),
        binding.image.clone(),
        binding.volumes.clone(),
        binding.delivery_prefs.clone(),
    );

    let row = db.host_get(&binding.id).ok().flatten();
    if row.is_none() {
        // Seed the row so UPDATE-based helpers (heartbeat, touch_probe,
        // set_consent) have something to land on from the first step.
        let name = binding.id.config_name().unwrap_or("").to_string();
        let _ = db.host_checkpoint(
            &binding.id,
            &name,
            binding.reach.kind(),
            &HostState::Unknown,
            None,
            unix_now(),
        );
    }
    let persisted = row.map(|r| r.state).unwrap_or(HostState::Unknown);
    let (mut state, mut pending) = resume(
        persisted,
        db.host_get(&binding.id)
            .ok()
            .flatten()
            .and_then(|r| r.last_probe),
        unix_now(),
        binding.probe_ttl_secs,
    );
    if state == HostState::Ready && pending.is_empty() {
        return spec_from_inventory(db, binding)
            .map(HostOutcome::Ready)
            .ok_or_else(|| HostFailure {
                step: HostStep::Verify,
                error: "ready host has no recorded image (run rm-cache + provision)".into(),
                retryable: true,
            });
    }
    // A retryable failed row re-enters: also clear a stale heartbeat.
    let _ = db.host_heartbeat(&binding.id, "resume", unix_now());

    loop {
        let mut event: Option<MachineEvent> = None;
        for effect in pending.drain(..) {
            if event.is_some() {
                // The machine emits at most one actionable effect per
                // transition; trailing Checkpoint/Emit still run.
                if matches!(
                    effect,
                    HostEffect::Checkpoint { .. } | HostEffect::Emit { .. }
                ) {
                    let _ = run_effect(
                        db,
                        binding,
                        policy,
                        runner.as_mut(),
                        &ctx,
                        &mut board,
                        &mut publish,
                        ui,
                        effect,
                    );
                }
                continue;
            }
            match run_effect(
                db,
                binding,
                policy,
                runner.as_mut(),
                &ctx,
                &mut board,
                &mut publish,
                ui,
                effect,
            ) {
                None => {}
                Some(DriveFlow::Event(ev)) => event = Some(ev),
                Some(DriveFlow::Deferred) => {
                    let _ = db.host_heartbeat_clear(&binding.id);
                    return Ok(HostOutcome::Deferred);
                }
            }
        }
        let Some(ev) = event else {
            // No actionable effect ran: the machine has settled.
            break;
        };
        let Transition { next, effects } = step(&state, &mut ctx, ev);
        state = next;
        pending = effects;
        match &state {
            HostState::Ready => {
                // Drain trailing checkpoints/emits, then finish.
                for effect in pending.drain(..) {
                    let _ = run_effect(
                        db,
                        binding,
                        policy,
                        runner.as_mut(),
                        &ctx,
                        &mut board,
                        &mut publish,
                        ui,
                        effect,
                    );
                }
                board.all_done();
                publish(&board);
                let _ = db.host_touch_used(&binding.id, unix_now());
                return spec_from_inventory(db, binding)
                    .map(HostOutcome::Ready)
                    .ok_or_else(|| HostFailure {
                        step: HostStep::Verify,
                        error: "provisioned but no image recorded (driver bug)".into(),
                        retryable: true,
                    });
            }
            HostState::Failed(f) => {
                for effect in pending.drain(..) {
                    let _ = run_effect(
                        db,
                        binding,
                        policy,
                        runner.as_mut(),
                        &ctx,
                        &mut board,
                        &mut publish,
                        ui,
                        effect,
                    );
                }
                board.fail(f.step, &f.error);
                publish(&board);
                return Err(f.clone());
            }
            _ => {}
        }
    }
    Err(HostFailure {
        step: HostStep::Connect,
        error: "host machine settled without reaching Ready (driver bug)".into(),
        retryable: true,
    })
}

/// Another process is driving (fresh heartbeat): poll the row, render its
/// persisted step, and return when it reaches a terminal state.
fn attach_external(
    db: &Db,
    binding: &HostBinding,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> Result<HostOutcome, HostFailure> {
    let host_label = binding
        .id
        .config_name()
        .unwrap_or(binding.id.as_str())
        .to_string();
    loop {
        let Ok(Some(row)) = db.host_get(&binding.id) else {
            return Err(HostFailure {
                step: HostStep::Connect,
                error: "host row vanished while attached".into(),
                retryable: true,
            });
        };
        match &row.state {
            HostState::Ready => {
                return spec_from_inventory(db, binding)
                    .map(HostOutcome::Ready)
                    .ok_or_else(|| HostFailure {
                        step: HostStep::Verify,
                        error: "ready host has no recorded image".into(),
                        retryable: true,
                    });
            }
            HostState::Failed(f) => return Err(f.clone()),
            _ => {}
        }
        let stale = row
            .heartbeat
            .is_none_or(|hb| unix_now().saturating_sub(hb) >= 2 * HEARTBEAT_FRESH_SECS);
        if stale {
            // The external driver died: report retryable so our caller can
            // take over on the next attempt.
            return Err(HostFailure {
                step: HostStep::Connect,
                error: "external provisioner died mid-run; retry to take over".into(),
                retryable: true,
            });
        }
        let step_label = row.active_step.unwrap_or_else(|| "working".into());
        progress(&[
            ProvisionStepView {
                label: format!("host {host_label}"),
                state: ProvisionState::Active,
                detail: Some("provisioning in another superzej process".into()),
            },
            ProvisionStepView {
                label: step_label,
                state: ProvisionState::Active,
                detail: None,
            },
        ]);
        std::thread::sleep(Duration::from_secs(2));
    }
}

trait ClonedOpt {
    fn cloned_opt(&self) -> Option<HostUiTx>;
}
impl ClonedOpt for Option<&HostUiTx> {
    fn cloned_opt(&self) -> Option<HostUiTx> {
        self.cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::host::{Arch, HostId, RuntimeInfo, VolumeSpec};
    use superzej_core::image::{DeliveryStrategy, Digest, ImageRef, ResolvedImage};

    const D_AMD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D_LIST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A scripted runner: counts calls; image present on the Nth check.
    struct MockRunner {
        probes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delivers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        present_after_deliver: std::cell::Cell<bool>,
    }

    impl MockRunner {
        fn new(
            probes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            delivers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            MockRunner {
                probes,
                delivers,
                present_after_deliver: std::cell::Cell::new(false),
            }
        }
    }

    impl HostRunner for MockRunner {
        fn connect(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn probe(&mut self) -> Result<HostCaps, String> {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPODMAN=5.0\nPKGMGR=apt\nEGRESS=full\n")
        }
        fn install_runtime(
            &mut self,
            _kind: RuntimeKind,
            _note: &mut dyn FnMut(String),
        ) -> Result<RuntimeInfo, String> {
            unreachable!("runtime present in mock probe")
        }
        fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, String> {
            Ok(ResolvedImage {
                reference: reference.clone(),
                list_digest: Digest::parse(D_LIST).unwrap(),
                per_arch: [(Arch::Amd64, Digest::parse(D_AMD).unwrap())]
                    .into_iter()
                    .collect(),
            })
        }
        fn image_present(&mut self, _image: &ImageRef, _digest: &Digest) -> Result<bool, String> {
            Ok(self.present_after_deliver.get())
        }
        fn deliver(
            &mut self,
            _strategy: DeliveryStrategy,
            _image: &ImageRef,
            digest: &Digest,
            progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<Digest, String> {
            self.delivers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            progress(512, Some(1024));
            progress(1024, Some(1024));
            self.present_after_deliver.set(true);
            Ok(digest.clone())
        }
        fn seed_volume(
            &mut self,
            _spec: &VolumeSpec,
            _image: &ImageRef,
            _digest: &Digest,
        ) -> Result<(), String> {
            Ok(())
        }
        fn oci_url(&self) -> Option<String> {
            None
        }
    }

    fn binding(name: &str) -> HostBinding {
        HostBinding {
            id: HostId::named(name),
            reach: Reach::Local,
            consent: InstallConsent::Ask,
            image: ImageRef::parse("ghcr.io/x/base:v1").unwrap(),
            volumes: vec![VolumeSpec::by_name("nix-store").unwrap()],
            delivery_prefs: Vec::new(),
            probe_ttl_secs: 900,
        }
    }

    #[test]
    fn drive_provisions_then_second_call_is_a_db_only_no_op() {
        let db = Db::open_memory().unwrap();
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delivers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let b = binding("golden");
        let seen_steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_in_cb = std::sync::Arc::clone(&seen_steps);
        let mut progress = move |views: &[ProvisionStepView]| {
            seen_in_cb.store(views.len(), std::sync::atomic::Ordering::SeqCst);
        };
        let mut mk = |_reach: &Reach| -> Result<Box<dyn HostRunner>, String> {
            Ok(Box::new(MockRunner::new(
                std::sync::Arc::clone(&probes),
                std::sync::Arc::clone(&delivers),
            )))
        };

        let out = ensure_host_ready_with(
            &db,
            &b,
            ConsentPolicy::Interactive,
            &mut progress,
            None,
            &mut mk,
        )
        .expect("provisions");
        let HostOutcome::Ready(spec) = out else {
            panic!("expected Ready");
        };
        assert_eq!(spec.image, managed_tag(&Digest::parse(D_AMD).unwrap()));
        assert_eq!(
            spec.volumes,
            vec![("superzej-nix-store".into(), "/nix".into())]
        );
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(delivers.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            seen_steps.load(std::sync::atomic::Ordering::SeqCst) >= 4,
            "splash steps published"
        );

        // Persisted state is Ready with inventory + events.
        let row = db.host_get(&b.id).unwrap().unwrap();
        assert_eq!(row.state, HostState::Ready);
        assert!(!db.host_inventory(&b.id).unwrap().is_empty());
        assert!(!db.host_events_recent(&b.id, 10).unwrap().is_empty());

        // GOLDEN PATH: the second call performs zero probes, zero delivers —
        // one DB read.
        let out2 = ensure_host_ready_with(
            &db,
            &b,
            ConsentPolicy::Interactive,
            &mut progress,
            None,
            &mut |_reach| panic!("no runner may be built on the fast path"),
        )
        .expect("fast path");
        assert!(matches!(out2, HostOutcome::Ready(_)));
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(delivers.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn consent_denied_is_fatal_and_persisted() {
        let db = Db::open_memory().unwrap();
        let mut b = binding("ask-box");
        b.consent = InstallConsent::Ask;
        // A probe with NO runtime + pkgmgr present → the machine parks on
        // consent; Headless{assume_yes:false} answers no.
        struct BareRunner;
        impl HostRunner for BareRunner {
            fn connect(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn probe(&mut self) -> Result<HostCaps, String> {
                HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPKGMGR=apt\n")
            }
            fn install_runtime(
                &mut self,
                _k: RuntimeKind,
                _n: &mut dyn FnMut(String),
            ) -> Result<RuntimeInfo, String> {
                panic!("must not install without consent")
            }
            fn resolve_image(&mut self, _r: &ImageRef) -> Result<ResolvedImage, String> {
                unreachable!()
            }
            fn image_present(&mut self, _i: &ImageRef, _d: &Digest) -> Result<bool, String> {
                unreachable!()
            }
            fn deliver(
                &mut self,
                _s: DeliveryStrategy,
                _i: &ImageRef,
                _d: &Digest,
                _p: &mut dyn FnMut(u64, Option<u64>),
            ) -> Result<Digest, String> {
                unreachable!()
            }
            fn seed_volume(
                &mut self,
                _s: &VolumeSpec,
                _i: &ImageRef,
                _d: &Digest,
            ) -> Result<(), String> {
                unreachable!()
            }
            fn oci_url(&self) -> Option<String> {
                None
            }
        }
        let err = ensure_host_ready_with(
            &db,
            &b,
            ConsentPolicy::Headless { assume_yes: false },
            &mut |_| {},
            None,
            &mut |_| Ok(Box::new(BareRunner)),
        )
        .unwrap_err();
        assert_eq!(err.step, HostStep::Consent);
        assert!(!err.retryable);
        let row = db.host_get(&b.id).unwrap().unwrap();
        assert_eq!(row.install_consent, Some(false), "decline persisted");
        assert!(matches!(row.state, HostState::Failed(_)));
    }

    #[test]
    fn background_policy_defers_instead_of_asking() {
        let db = Db::open_memory().unwrap();
        let b = binding("bg-box");
        struct BareRunner;
        impl HostRunner for BareRunner {
            fn connect(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn probe(&mut self) -> Result<HostCaps, String> {
                HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPKGMGR=apt\n")
            }
            fn install_runtime(
                &mut self,
                _k: RuntimeKind,
                _n: &mut dyn FnMut(String),
            ) -> Result<RuntimeInfo, String> {
                panic!("background must never install")
            }
            fn resolve_image(&mut self, _r: &ImageRef) -> Result<ResolvedImage, String> {
                unreachable!()
            }
            fn image_present(&mut self, _i: &ImageRef, _d: &Digest) -> Result<bool, String> {
                unreachable!()
            }
            fn deliver(
                &mut self,
                _s: DeliveryStrategy,
                _i: &ImageRef,
                _d: &Digest,
                _p: &mut dyn FnMut(u64, Option<u64>),
            ) -> Result<Digest, String> {
                unreachable!()
            }
            fn seed_volume(
                &mut self,
                _s: &VolumeSpec,
                _i: &ImageRef,
                _d: &Digest,
            ) -> Result<(), String> {
                unreachable!()
            }
            fn oci_url(&self) -> Option<String> {
                None
            }
        }
        let out = ensure_host_ready_with(
            &db,
            &b,
            ConsentPolicy::BackgroundSkip,
            &mut |_| {},
            None,
            &mut |_| Ok(Box::new(BareRunner)),
        )
        .expect("deferred, not failed");
        assert!(matches!(out, HostOutcome::Deferred));
        // Nothing persisted as failed; no consent recorded.
        let row = db.host_get(&b.id).unwrap();
        assert!(
            row.map(|r| !matches!(r.state, HostState::Failed(_)))
                .unwrap_or(true)
        );
    }

    #[test]
    fn concurrent_callers_single_flight_through_one_drive() {
        // Two threads target the same host id; the flight registry must ensure
        // only one drive runs. (Each thread gets its own in-memory DB, so the
        // assertion rides the runner-build counter, not the DB.)
        let builds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let b = binding("flight-box");
        let mut handles = Vec::new();
        for _ in 0..2 {
            let builds = std::sync::Arc::clone(&builds);
            let b = b.clone();
            handles.push(std::thread::spawn(move || {
                let db = Db::open_memory().unwrap();
                let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let delivers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut mk = |_reach: &Reach| -> Result<Box<dyn HostRunner>, String> {
                    builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Slow the drive slightly so the second thread overlaps.
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(Box::new(MockRunner::new(
                        std::sync::Arc::clone(&probes),
                        std::sync::Arc::clone(&delivers),
                    )))
                };
                ensure_host_ready_with(
                    &db,
                    &b,
                    ConsentPolicy::Interactive,
                    &mut |_| {},
                    None,
                    &mut mk,
                )
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // One leader drove; the follower either got the leader's Ready result
        // or a benign "finished elsewhere" retryable error.
        assert_eq!(
            builds.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one runner built ⇒ single-flight held"
        );
        assert!(results.iter().any(|r| r.is_ok()));
    }

    #[test]
    fn step_board_shapes_are_splash_safe() {
        let mut b = StepBoard::new("box");
        b.start(HostStep::Connect, None);
        b.finish(HostStep::Connect, None);
        b.start(HostStep::Deliver, Some("412 MiB / 1.9 GiB".into()));
        let views = b.views();
        assert!(views.iter().all(|v| v.label != "shell"), "never shell-wait");
        assert_eq!(views[0].label, "host box");
        b.fail(HostStep::Deliver, "stalled");
        assert_eq!(b.views()[3].state, ProvisionState::Failed);
        b.all_done();
        assert_eq!(b.views()[3].state, ProvisionState::Failed, "failure sticks");
    }

    #[test]
    fn fmt_bytes_is_humane() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KiB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(fmt_bytes(5_368_709_120), "5.0 GiB");
    }

    #[test]
    fn failure_reason_names_the_remedy() {
        let f = HostFailure {
            step: HostStep::Deliver,
            error: "registry unreachable".into(),
            retryable: true,
        };
        let s = failure_reason(&f);
        assert!(s.contains("transfer image"));
        assert!(s.contains("superzej host provision"));
        let fatal = HostFailure {
            step: HostStep::Consent,
            error: "declined".into(),
            retryable: false,
        };
        assert!(!failure_reason(&fatal).contains("retry from"));
    }
}
