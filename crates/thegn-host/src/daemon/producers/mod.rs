//! The daemon's headless producer owner (THE-2208).
//!
//! One tokio task per armed producer kind. Each tick it acquires or renews
//! that kind's lease (`producer_leases`, holder `daemon`, TTL three cadences,
//! at least 60 s), runs the kind's blocking `tick` in `spawn_blocking`, and
//! sleeps the cadence. A kind is armed by `thegn_core::producer_arm::armed`:
//! `[daemon] enabled` and `headless_producers` on AND something needs it.
//!
//! Inert by default: with `headless_producers = false` `install` returns at
//! once and stores nothing, so `rearm` stays a no-op (no task, no timer, no
//! lease write, no blocking job). With it on and nothing that needs a producer,
//! `install` does one off-loop rule read and starts nothing.
//!
//! Lifecycle invariants:
//! - A task owns its kind's lease. `rearm` disarms a kind by cancelling the
//!   task's token; the task finishes any in-flight blocking step, then
//!   releases the lease itself, so a disarm never races an in-flight acquire.
//! - Every lease write runs under [`LEASE_OPS`] and checks the task's
//!   generation, so a task superseded by a re-arm of the same kind neither
//!   acquires nor releases the successor's lease.
//! - Ticks of one kind never overlap: [`TickSlot`] is a per-kind single-flight
//!   held for the whole blocking tick.
//! - Only tokio tasks and `spawn_blocking`: no OS thread of our own.
//! - No `Db` crosses an await: every DB touch opens its own handle inside a
//!   blocking closure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::config::Config;
use thegn_core::producer_arm::{ArmInputs, armed_kinds, cadence_secs};
use thegn_core::producer_lease::{DAEMON_HOLDER, ProducerKind, ttl_ms};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::service::DaemonService;

pub(crate) mod pr;
pub(crate) mod queues;
pub(crate) mod tracker;

/// What one producer tick sees.
#[allow(dead_code, reason = "the tick bodies (b161 lanes 6-8) read these")]
pub(crate) struct TickCtx {
    pub cfg: Arc<Config>,
    pub now_ms: i64,
}

/// What one producer tick did.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code, reason = "the tick bodies (b161 lanes 6-8) produce `Done`")]
pub(crate) enum TickOutcome {
    /// The tick ran; `work` items were produced.
    Done { work: usize },
    /// The tick did nothing, for this reason.
    Skipped(&'static str),
}

/// The handles `install` stores; absent unless `headless_producers` is on.
struct Installed {
    svc: Arc<DaemonService>,
    cfg: Arc<Config>,
    handle: Handle,
}

/// One armed kind's task.
struct Armed {
    generation: u64,
    task: JoinHandle<()>,
    stop: CancellationToken,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static TASKS: Mutex<BTreeMap<ProducerKind, Armed>> = Mutex::new(BTreeMap::new());
static ARMED_ANY: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Installs that passed the `headless_producers` gate (tests pin it at 0).
static INSTALLS: AtomicU64 = AtomicU64::new(0);
/// Serialises rearms so two rule edits cannot interleave start/stop plans.
static REARM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Serialises every lease write with the generation check that guards it.
static LEASE_OPS: Mutex<()> = Mutex::new(());
/// The kinds whose blocking tick is running right now.
static TICKING: Mutex<BTreeSet<ProducerKind>> = Mutex::new(BTreeSet::new());

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether `install` does anything for this config.
fn install_gate(cfg: &Config) -> bool {
    cfg.daemon.headless_producers
}

/// Store the handles and arm every needed kind. Returns at once (storing
/// nothing) unless `[daemon] headless_producers` is on.
pub(crate) fn install(svc: Arc<DaemonService>, cfg: Arc<Config>) {
    if !install_gate(&cfg) {
        return;
    }
    let Ok(handle) = Handle::try_current() else {
        tracing::warn!(target: "thegn::producers", "producers not installed: no tokio runtime");
        return;
    };
    if INSTALLED.set(Installed { svc, cfg, handle }).is_err() {
        return;
    }
    INSTALLS.fetch_add(1, Ordering::SeqCst);
    tracing::info!(target: "thegn::producers", "headless producer owner installed");
    rearm();
}

/// Re-evaluate arming after a rule edit (and at install). A no-op unless
/// `install` stored its handles (not the daemon, or `headless_producers` off).
pub(crate) fn rearm() {
    let Some(inst) = INSTALLED.get() else {
        return;
    };
    inst.handle.spawn(rearm_now(inst));
}

async fn rearm_now(inst: &'static Installed) {
    let _serial = REARM.lock().await;
    let cfg = inst.cfg.clone();
    let wanted: BTreeSet<ProducerKind> = tokio::task::spawn_blocking(move || wanted_kinds(&cfg))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(target: "thegn::producers", %error, "arming read failed; keeping the current producers");
            current_kinds()
        });
    let mut tasks = lock(&TASKS);
    let current: BTreeSet<ProducerKind> = tasks.keys().copied().collect();
    let (start, stop) = plan_rearm(&current, &wanted);
    for kind in stop {
        if let Some(armed) = tasks.remove(&kind) {
            // The task releases its own lease on the way out (see the
            // lifecycle notes); dropping the JoinHandle detaches it.
            armed.stop.cancel();
            drop(armed.task);
            tracing::info!(target: "thegn::producers", kind = kind.as_str(), "producer disarmed");
        }
    }
    for kind in start {
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        let stop = inst.svc.shutdown.child_token();
        let task = inst
            .handle
            .spawn(run_kind(kind, generation, inst.cfg.clone(), stop.clone()));
        tasks.insert(
            kind,
            Armed {
                generation,
                task,
                stop,
            },
        );
        tracing::info!(target: "thegn::producers", kind = kind.as_str(), "producer armed");
    }
    ARMED_ANY.store(!tasks.is_empty(), Ordering::SeqCst);
}

/// The kinds to start and to stop to move from `current` to `wanted`.
fn plan_rearm(
    current: &BTreeSet<ProducerKind>,
    wanted: &BTreeSet<ProducerKind>,
) -> (BTreeSet<ProducerKind>, BTreeSet<ProducerKind>) {
    (
        wanted.difference(current).copied().collect(),
        current.difference(wanted).copied().collect(),
    )
}

fn current_kinds() -> BTreeSet<ProducerKind> {
    lock(&TASKS).keys().copied().collect()
}

/// Blocking: read the rules and repos and decide which kinds are needed.
fn wanted_kinds(cfg: &Config) -> BTreeSet<ProducerKind> {
    let db = match thegn_core::db::Db::open() {
        Ok(db) => Some(db),
        Err(error) => {
            tracing::warn!(target: "thegn::producers", %error, "state db unreadable; arming from config alone");
            None
        }
    };
    let rules = rules(cfg, db.as_ref());
    let inputs = ArmInputs {
        daemon_enabled: cfg.daemon.enabled,
        headless: cfg.daemon.headless_producers,
        rules: &rules,
        autopilot_any: db.as_ref().is_some_and(|db| autopilot_any(cfg, db)),
        pr_queue_enabled: cfg.pr_queue.enabled,
        github_mentions: cfg.notifications.github_mentions,
        push_configured: cfg.notifications.push.is_configured(),
    };
    armed_kinds(&inputs).into_iter().collect()
}

/// Every rule the runtime would evaluate (config and Lead), read only. Empty
/// when the runtime is not armed, mirroring `automation_schedule`.
fn rules(
    cfg: &Config,
    db: Option<&thegn_core::db::Db>,
) -> Vec<thegn_core::automation::AutomationRule> {
    let effective = cfg.effective_automations();
    if !thegn_core::automation_authoring::runtime_armed(
        effective.enabled,
        effective.rules.len(),
        crate::lead::enabled(cfg),
    ) {
        return Vec::new();
    }
    let Ok(rules) = effective.compiled_rules() else {
        return Vec::new();
    };
    match db {
        Some(db) => crate::automation_runtime::with_lead_rules(db, rules),
        None => rules,
    }
}

/// Any registered git repo with `[autopilot]` enabled for it.
fn autopilot_any(cfg: &Config, db: &thegn_core::db::Db) -> bool {
    use thegn_core::store::WorkspaceStore;
    db.workspaces().is_ok_and(|rows| {
        rows.iter()
            .filter(|w| w.kind == "repo")
            .any(|w| cfg.repo_autopilot(Path::new(&w.repo_path)).enabled)
    })
}

/// True while any producer kind is armed (keeps the daemon from idle-exit).
pub(crate) fn armed_any() -> bool {
    ARMED_ANY.load(Ordering::SeqCst)
}

/// Each producer kind and whether it is armed, for `thegn doctor`.
#[allow(
    dead_code,
    reason = "read by thegn doctor once the queued producers.status verb lands"
)]
pub(crate) fn status() -> Vec<(ProducerKind, bool /*armed*/)> {
    let tasks = lock(&TASKS);
    ProducerKind::ALL
        .into_iter()
        .map(|kind| (kind, tasks.contains_key(&kind)))
        .collect()
}

/// Release every lease this daemon holds (graceful shutdown). Blocking; a
/// no-op unless the owner was installed.
pub(crate) fn release_all() {
    if INSTALLED.get().is_none() {
        return;
    }
    let pid = i64::from(std::process::id());
    let _ops = lock(&LEASE_OPS);
    if let Ok(db) = thegn_core::db::Db::open() {
        for kind in ProducerKind::ALL {
            release_logged(&db, kind, pid);
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// True while `generation` is still `kind`'s armed task.
fn is_current(kind: ProducerKind, generation: u64) -> bool {
    lock(&TASKS)
        .get(&kind)
        .is_some_and(|armed| armed.generation == generation)
}

/// The per-kind single-flight: held for the whole blocking tick.
struct TickSlot(ProducerKind);

impl TickSlot {
    fn try_take(kind: ProducerKind) -> Option<Self> {
        // Release the guard before building the slot: an eager
        // `then_some(Self(kind))` drops the refused slot while `TICKING` is
        // still locked, and its `Drop` re-locks it (self-deadlock).
        let inserted = lock(&TICKING).insert(kind);
        inserted.then(|| Self(kind))
    }
}

impl Drop for TickSlot {
    fn drop(&mut self) {
        lock(&TICKING).remove(&self.0);
    }
}

/// What one pass of a kind's loop did (logged).
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    /// Another holder has the live lease; the tick did not run.
    NotHeld,
    /// This task was superseded or disarmed; it writes nothing more.
    Superseded,
    /// Automations are paused: the lease was renewed, the tick skipped.
    Paused,
    /// A previous tick of this kind is still running.
    Busy,
    /// The lease was held and the tick ran.
    Ran(TickOutcome),
    /// The lease could not be read or written.
    Error(String),
}

/// One blocking pass: acquire or renew the lease, then (unless paused or a
/// tick of this kind is in flight) run `tick`. `current` says whether this
/// task still owns the kind; it is checked under [`LEASE_OPS`].
fn pass(
    kind: ProducerKind,
    pid: i64,
    cadence_secs: u64,
    cfg: &Arc<Config>,
    current: impl Fn() -> bool,
    tick: impl FnOnce(&TickCtx) -> TickOutcome,
) -> Pass {
    let now = now_ms();
    let db = match thegn_core::db::Db::open() {
        Ok(db) => db,
        Err(error) => return Pass::Error(format!("{error:#}")),
    };
    {
        let _ops = lock(&LEASE_OPS);
        if !current() {
            return Pass::Superseded;
        }
        match db.try_acquire_producer_lease(kind, DAEMON_HOLDER, pid, now, ttl_ms(cadence_secs)) {
            Ok(true) => {}
            Ok(false) => return Pass::NotHeld,
            Err(error) => return Pass::Error(format!("{error:#}")),
        }
    }
    if thegn_core::automation_control::paused_fail_closed(&db) {
        return Pass::Paused;
    }
    drop(db);
    let Some(_slot) = TickSlot::try_take(kind) else {
        return Pass::Busy;
    };
    let outcome = tick(&TickCtx {
        cfg: cfg.clone(),
        now_ms: now,
    });
    // Renew at the end of the tick too: the gap between renewals is then the
    // longer of the tick and the cadence, never their sum.
    if let Ok(db) = thegn_core::db::Db::open() {
        let _ops = lock(&LEASE_OPS);
        if current() {
            match db.try_acquire_producer_lease(
                kind,
                DAEMON_HOLDER,
                pid,
                now_ms(),
                ttl_ms(cadence_secs),
            ) {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    target: "thegn::producers",
                    kind = kind.as_str(),
                    "producer lease lapsed during a tick (the tick outlived its TTL)"
                ),
                Err(error) => tracing::warn!(
                    target: "thegn::producers",
                    kind = kind.as_str(),
                    %error,
                    "producer lease renewal after the tick failed"
                ),
            }
        }
    }
    Pass::Ran(outcome)
}

/// The kind's real tick.
fn dispatch(kind: ProducerKind, ctx: &TickCtx) -> TickOutcome {
    match kind {
        ProducerKind::Tracker => tracker::tick(ctx),
        ProducerKind::PrPoll => pr::tick_pr(ctx),
        ProducerKind::Mentions => pr::tick_mentions(ctx),
        ProducerKind::PrQueue => queues::tick_pr_queue(ctx),
    }
}

/// Release our lease on `kind`; a failure is logged (the lease lapses after
/// its TTL, so the UI resumes polling then).
fn release_logged(db: &thegn_core::db::Db, kind: ProducerKind, pid: i64) {
    if let Err(error) = db.release_producer_lease(kind, DAEMON_HOLDER, pid) {
        tracing::warn!(target: "thegn::producers", kind = kind.as_str(), %error, "lease release failed; it lapses after its TTL");
    }
}

/// Release `kind`'s lease if `generation` still owns the kind or nobody does
/// (a successor's lease is never touched). Blocking.
fn release_if_owner(kind: ProducerKind, generation: u64, pid: i64) {
    let _ops = lock(&LEASE_OPS);
    let successor = lock(&TASKS)
        .get(&kind)
        .is_some_and(|armed| armed.generation != generation);
    if successor {
        return;
    }
    if let Ok(db) = thegn_core::db::Db::open() {
        release_logged(&db, kind, pid);
    }
}

/// One armed kind's loop. Exits on disarm or daemon shutdown (both cancel
/// `stop`) and then releases its lease.
async fn run_kind(kind: ProducerKind, generation: u64, cfg: Arc<Config>, stop: CancellationToken) {
    let pid = i64::from(std::process::id());
    let cadence_secs = cadence_secs(kind, &cfg);
    let cadence = Duration::from_secs(cadence_secs);
    let mut warned_not_held = false;
    while !stop.is_cancelled() {
        let started = Instant::now();
        let pass_cfg = cfg.clone();
        let pass_stop = stop.clone();
        let result = tokio::task::spawn_blocking(move || {
            pass(
                kind,
                pid,
                cadence_secs,
                &pass_cfg,
                || !pass_stop.is_cancelled() && is_current(kind, generation),
                |ctx| dispatch(kind, ctx),
            )
        })
        .await
        .unwrap_or_else(|error| Pass::Error(format!("tick panicked: {error}")));
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Pass::Superseded => break,
            Pass::NotHeld => {
                if !warned_not_held {
                    tracing::info!(
                        target: "thegn::producers",
                        kind = kind.as_str(),
                        "producer lease held elsewhere; waiting for it to lapse"
                    );
                    warned_not_held = true;
                }
            }
            other => {
                warned_not_held = false;
                tracing::debug!(
                    target: "thegn::producers",
                    kind = kind.as_str(),
                    outcome = ?other,
                    duration_ms,
                    "producer tick"
                );
            }
        }
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(cadence) => {}
        }
    }
    // Disarmed, superseded or shutting down: give the lease back (unless a
    // successor generation now owns the kind).
    if let Err(error) =
        tokio::task::spawn_blocking(move || release_if_owner(kind, generation, pid)).await
    {
        tracing::warn!(target: "thegn::producers", kind = kind.as_str(), %error, "lease release failed; it lapses after its TTL");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(kinds: &[ProducerKind]) -> BTreeSet<ProducerKind> {
        kinds.iter().copied().collect()
    }

    #[test]
    fn the_uninstalled_owner_arms_nothing() {
        rearm();
        assert!(!armed_any());
        assert!(status().iter().all(|(_, armed)| !armed));
        assert_eq!(status().len(), ProducerKind::ALL.len());
        release_all();
        assert!(
            !crate::daemon::daemon_busy(0, armed_any()),
            "idle stays idle"
        );
    }

    #[test]
    fn an_armed_producer_keeps_the_daemon_busy() {
        ARMED_ANY.store(true, Ordering::SeqCst);
        let busy = crate::daemon::daemon_busy(0, armed_any());
        ARMED_ANY.store(false, Ordering::SeqCst);
        assert!(
            busy,
            "idle-exit must not reap a daemon with a producer armed"
        );
    }

    #[test]
    fn a_disabled_headless_producers_installs_nothing() {
        let cfg = Config::default();
        assert!(!cfg.daemon.headless_producers, "opt-in");
        assert!(!install_gate(&cfg));
        rearm();
        assert_eq!(INSTALLS.load(Ordering::SeqCst), 0);
        assert!(INSTALLED.get().is_none());
        assert!(lock(&TASKS).is_empty());
        assert!(!armed_any());
        let mut on = Config::default();
        on.daemon.headless_producers = true;
        assert!(install_gate(&on));
    }

    #[test]
    fn plan_rearm_starts_the_new_and_stops_the_gone() {
        use ProducerKind::*;
        assert_eq!(plan_rearm(&set(&[]), &set(&[])), (set(&[]), set(&[])));
        assert_eq!(
            plan_rearm(&set(&[]), &set(&[Tracker, PrQueue])),
            (set(&[Tracker, PrQueue]), set(&[]))
        );
        assert_eq!(
            plan_rearm(&set(&[Tracker, PrPoll]), &set(&[PrPoll, Mentions])),
            (set(&[Mentions]), set(&[Tracker]))
        );
        assert_eq!(
            plan_rearm(&set(&ProducerKind::ALL), &set(&[])),
            (set(&[]), set(&ProducerKind::ALL))
        );
        let all = set(&ProducerKind::ALL);
        assert_eq!(plan_rearm(&all, &all), (set(&[]), set(&[])), "steady state");
    }

    #[test]
    fn nothing_is_wanted_with_headless_producers_off() {
        let _state = crate::testenv::IsolatedState::new();
        let mut cfg = Config::default();
        cfg.pr_queue.enabled = true;
        cfg.daemon.enabled = true;
        assert!(wanted_kinds(&cfg).is_empty());
        cfg.daemon.headless_producers = true;
        assert_eq!(wanted_kinds(&cfg), set(&[ProducerKind::PrQueue]));
    }

    #[test]
    fn a_second_holder_skips_the_tick() {
        let _state = crate::testenv::IsolatedState::new();
        let cfg = Arc::new(Config::default());
        let other_daemon = i64::from(std::process::id()) + 1;
        let db = thegn_core::db::Db::open().expect("isolated db");
        assert!(
            db.try_acquire_producer_lease(
                ProducerKind::PrPoll,
                DAEMON_HOLDER,
                other_daemon,
                now_ms(),
                60_000
            )
            .unwrap()
        );
        let pid = i64::from(std::process::id());
        let mut ran = 0;
        let result = pass(
            ProducerKind::PrPoll,
            pid,
            30,
            &cfg,
            || true,
            |_| {
                ran += 1;
                TickOutcome::Done { work: 1 }
            },
        );
        assert_eq!(result, Pass::NotHeld);
        assert_eq!(ran, 0, "the tick never ran under another holder's lease");

        // A free kind is acquired and ticks; the renewal keeps it.
        let result = pass(
            ProducerKind::Tracker,
            pid,
            60,
            &cfg,
            || true,
            |_| TickOutcome::Done { work: 2 },
        );
        assert_eq!(result, Pass::Ran(TickOutcome::Done { work: 2 }));
        let row = db.producer_lease(ProducerKind::Tracker).unwrap().unwrap();
        assert_eq!((row.holder.as_str(), row.holder_pid), (DAEMON_HOLDER, pid));
        assert!(row.expires_at_ms >= row.acquired_at_ms + ttl_ms(60));
    }

    #[test]
    fn a_superseded_task_neither_acquires_nor_ticks() {
        let _state = crate::testenv::IsolatedState::new();
        let cfg = Arc::new(Config::default());
        let pid = i64::from(std::process::id());
        let result = pass(
            ProducerKind::Mentions,
            pid,
            30,
            &cfg,
            || false,
            |_| panic!("a superseded task must not tick"),
        );
        assert_eq!(result, Pass::Superseded);
        let db = thegn_core::db::Db::open().unwrap();
        assert_eq!(db.producer_lease(ProducerKind::Mentions).unwrap(), None);
    }

    #[test]
    fn a_paused_runtime_renews_but_does_not_tick() {
        let _state = crate::testenv::IsolatedState::new();
        let cfg = Arc::new(Config::default());
        let pid = i64::from(std::process::id());
        let db = thegn_core::db::Db::open().unwrap();
        thegn_core::automation_control::set_paused(&db, true).unwrap();
        let result = pass(
            ProducerKind::PrQueue,
            pid,
            15,
            &cfg,
            || true,
            |_| panic!("paused: no tick"),
        );
        assert_eq!(result, Pass::Paused);
        assert!(db.producer_lease(ProducerKind::PrQueue).unwrap().is_some());
    }

    #[test]
    fn ticks_of_one_kind_never_overlap() {
        // Run on a thread with a bounded wait so a lock regression fails fast.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let first = TickSlot::try_take(ProducerKind::PrQueue).expect("free slot");
            assert!(TickSlot::try_take(ProducerKind::PrQueue).is_none());
            assert!(
                lock(&TICKING).contains(&ProducerKind::PrQueue),
                "a refused take must not free the holder's slot"
            );
            let other = TickSlot::try_take(ProducerKind::PrPoll).expect("per kind");
            drop(first);
            assert!(TickSlot::try_take(ProducerKind::PrQueue).is_some());
            drop(other);
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("TickSlot deadlocked or panicked (no completion within 5s)");
    }

    #[tokio::test]
    async fn a_disarmed_task_releases_its_lease_but_not_a_successors() {
        let _state = crate::testenv::IsolatedState::new();
        let pid = i64::from(std::process::id());
        let db = thegn_core::db::Db::open().unwrap();
        let acquire = || {
            assert!(
                db.try_acquire_producer_lease(
                    ProducerKind::Mentions,
                    DAEMON_HOLDER,
                    pid,
                    now_ms(),
                    60_000
                )
                .unwrap()
            );
        };
        // No armed task for the kind (disarmed): the leaving task releases.
        acquire();
        release_if_owner(ProducerKind::Mentions, 1, pid);
        assert_eq!(db.producer_lease(ProducerKind::Mentions).unwrap(), None);

        // A re-arm installed generation 2: generation 1 leaves it alone.
        acquire();
        lock(&TASKS).insert(
            ProducerKind::Mentions,
            Armed {
                generation: 2,
                task: tokio::spawn(async {}),
                stop: CancellationToken::new(),
            },
        );
        assert!(!is_current(ProducerKind::Mentions, 1));
        assert!(is_current(ProducerKind::Mentions, 2));
        release_if_owner(ProducerKind::Mentions, 1, pid);
        assert!(db.producer_lease(ProducerKind::Mentions).unwrap().is_some());
        // The owning generation releases its own.
        release_if_owner(ProducerKind::Mentions, 2, pid);
        assert_eq!(db.producer_lease(ProducerKind::Mentions).unwrap(), None);
        lock(&TASKS).remove(&ProducerKind::Mentions);
    }
}
