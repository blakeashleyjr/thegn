//! The background remote-sandbox health sampler (OpenSpec
//! `remote-sandbox-visibility`, slice 3).
//!
//! [`tick`] is the throttled entry the hydration pass calls beside the reapers.
//! One pass reads the fleet listing (the same read `thegn fleet` uses), then
//! runs the in-sandbox probe ([`thegn_core::remote_health::PROBE_SCRIPT`])
//! through the provider exec API for each worktree-owned sandbox the provider
//! reports **running**. A cold, warm or stopped sandbox is never probed: the
//! exec would wake and bill it.
//!
//! Each reading is compared with the stored previous one
//! ([`record_sample`]): restart and OOM events are classified, refined with the
//! provider facts, journaled in `remote_events`, and raised once as
//! `resource_alert` notifications, as is a lifetime under one hour. A probe
//! that fails or times out stores nothing (never a fabricated sample) and backs
//! that sandbox off for [`BACKOFF`].
//!
//! [`explain_lost_stream`] is the exec-stream-end hook: when a stream ends
//! without an exit status, one immediate probe replaces the bare message with
//! the specific restart warning when the boot id changed.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::notification::NotificationKind;
use thegn_core::placement::fleet_cost;
use thegn_core::remote_health::{self, HealthEvent, HealthSample, ProviderFacts};
use thegn_core::store::NotificationStore;
use thegn_svc::provider::Provider;

/// Probe cadence while a thegn client is attached (every hydrate pass runs in
/// one).
const ATTACHED_INTERVAL: Duration = Duration::from_secs(60);

/// Probe cadence with no client attached (a headless producer).
const DETACHED_INTERVAL: Duration = Duration::from_secs(300);

/// How long a sandbox whose probe failed or timed out is left alone.
const BACKOFF: Duration = Duration::from_secs(300);

/// Overall deadline of one probe exec.
const PROBE_DEADLINE: Duration = Duration::from_secs(20);

/// Deadline of the probe run when an exec stream has just ended.
const STREAM_END_DEADLINE: Duration = Duration::from_secs(10);

/// Probe cadence for the attachment state.
pub(crate) fn cadence(attached: bool) -> Duration {
    if attached {
        ATTACHED_INTERVAL
    } else {
        DETACHED_INTERVAL
    }
}

/// Whether the provider's state word proves the sandbox is running. An unknown
/// state is not proof: the sampler never risks waking a sandbox.
pub(crate) fn probe_allowed(state: Option<&str>) -> bool {
    state.is_some() && !fleet_cost::is_stopped_state(state)
}

/// Seconds of lifetime left (negative = overdue); `None` without a configured
/// lifetime or a creation time.
pub(crate) fn lifetime_left_secs(
    created_ms: Option<i64>,
    max_lifetime_secs: u64,
    now_ms: i64,
) -> Option<i64> {
    if max_lifetime_secs == 0 {
        return None;
    }
    let created = created_ms?;
    let max = i64::try_from(max_lifetime_secs).ok()?;
    Some(max.saturating_sub(now_ms.saturating_sub(created) / 1000))
}

/// The sandbox noun in warnings.
fn noun(provider: &str) -> &'static str {
    if provider == "sprites" {
        "sprite"
    } else {
        "sandbox"
    }
}

/// Per-sandbox schedule: when it is next due, and whether the provider showed
/// it cold since its last sample (an idle-sleep clue).
#[derive(Default)]
pub(crate) struct Schedule {
    next_due_ms: HashMap<String, i64>,
    cold_seen: HashSet<String>,
}

impl Schedule {
    pub(crate) fn due(&self, key: &str, now_ms: i64) -> bool {
        self.next_due_ms.get(key).is_none_or(|at| now_ms >= *at)
    }

    /// Note that the provider reported `key` not running.
    pub(crate) fn saw_cold(&mut self, key: &str) {
        self.cold_seen.insert(key.to_string());
    }

    /// Whether `key` was cold since its last sample.
    pub(crate) fn was_cold(&self, key: &str) -> bool {
        self.cold_seen.contains(key)
    }

    /// A probe succeeded: next one after `interval`; the cold mark is spent.
    pub(crate) fn sampled(&mut self, key: &str, now_ms: i64, interval: Duration) {
        self.next_due_ms
            .insert(key.to_string(), now_ms + millis(interval));
        self.cold_seen.remove(key);
    }

    /// A probe failed or timed out: back off.
    pub(crate) fn failed(&mut self, key: &str, now_ms: i64) {
        self.next_due_ms
            .insert(key.to_string(), now_ms + millis(BACKOFF));
    }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// One probe exec (the test seam: production wraps a provider).
pub(crate) trait Prober {
    /// `(exit code, stdout)` of the probe script in sandbox `id` of env `env`.
    fn probe(&self, env: &str, id: &str) -> Result<(i32, String)>;
}

/// A sandbox the sampler may probe.
pub(crate) struct Target {
    pub(crate) env: String,
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) state: Option<String>,
    pub(crate) worktree: String,
    pub(crate) created_ms: Option<i64>,
    pub(crate) max_lifetime_secs: u64,
}

impl Target {
    fn key(&self) -> String {
        remote_health::sandbox_key(&self.env, &self.id)
    }
}

/// The probe's output as a sample, or why it is not one.
pub(crate) fn sample_from(ran: Result<(i32, String)>) -> Result<HealthSample, String> {
    match ran {
        Ok((0, output)) => remote_health::parse_probe(&output),
        Ok((code, _)) => Err(format!("the health probe exited {code}")),
        Err(error) => Err(format!("the health probe could not run: {error:#}")),
    }
}

/// Store `cur` as the sandbox's latest reading, classify it against the stored
/// previous one, and journal + notify each event once. Returns the events
/// (provider-refined).
pub(crate) fn record_sample(
    db: &Db,
    target: &Target,
    cur: &HealthSample,
    cold_between_samples: bool,
    now_ms: i64,
) -> Result<Vec<HealthEvent>> {
    let key = target.key();
    let prev = db.remote_health(&key)?.and_then(|row| row.sample);
    let facts = ProviderFacts {
        cold_between_samples,
        age_secs: target
            .created_ms
            .map(|created| u64::try_from(now_ms.saturating_sub(created) / 1000).unwrap_or(0)),
        max_lifetime_secs: target.max_lifetime_secs,
    };
    let events: Vec<HealthEvent> = remote_health::classify(prev.as_ref(), cur)
        .into_iter()
        .map(|event| remote_health::refine(event, &facts))
        .collect();
    db.put_remote_health(&key, now_ms, cur, target.state.as_deref())?;
    let what = noun(&target.provider);
    for event in &events {
        let kind = remote_health::event_kind(event);
        let detail = serde_json::to_string(event)?;
        db.add_remote_event(&key, now_ms, kind, &detail)?;
        db.put_notification_once(
            NotificationKind::ResourceAlert.as_str(),
            &format!("remote:{key}:{kind}:{now_ms}"),
            &remote_health::warning(what, event),
            &target.worktree,
        )?;
    }
    Ok(events)
}

/// Raise the once-per-lifetime "under an hour left" notification. The source
/// ref carries the expiry instant, so a sandbox recreated later re-arms.
pub(crate) fn raise_lifetime(db: &Db, target: &Target, now_ms: i64) -> Result<bool> {
    let Some(left) = lifetime_left_secs(target.created_ms, target.max_lifetime_secs, now_ms) else {
        return Ok(false);
    };
    let Some(message) = remote_health::lifetime_warning(noun(&target.provider), left) else {
        return Ok(false);
    };
    let expiry_ms = now_ms.saturating_add(left.saturating_mul(1000));
    // Millisecond-exact expiry drifts with the tick; the minute is stable.
    let source = format!("remote:{}:lifetime:{}", target.key(), expiry_ms / 60_000);
    let kind = NotificationKind::ResourceAlert.as_str();
    if db.has_notification(kind, &source)? {
        return Ok(false);
    }
    db.put_notification(kind, &source, &message, &target.worktree)?;
    db.add_remote_event(
        &target.key(),
        now_ms,
        "lifetime",
        &format!("{{\"left_secs\":{left}}}"),
    )?;
    Ok(true)
}

/// What one sampling visit to a sandbox did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Visit {
    /// The provider does not report it running: left alone.
    Skipped,
    /// Not due yet.
    NotDue,
    /// The probe failed; the sandbox is backed off.
    ProbeFailed(String),
    /// A sample was stored, with the events it raised.
    Sampled(Vec<HealthEvent>),
}

/// Visit one sandbox: skip it unless running and due, probe, record.
pub(crate) fn visit(
    db: &Db,
    prober: &dyn Prober,
    schedule: &mut Schedule,
    target: &Target,
    attached: bool,
    now_ms: i64,
) -> Visit {
    let key = target.key();
    if !probe_allowed(target.state.as_deref()) {
        schedule.saw_cold(&key);
        return Visit::Skipped;
    }
    if !schedule.due(&key, now_ms) {
        return Visit::NotDue;
    }
    if let Err(error) = raise_lifetime(db, target, now_ms) {
        tracing::debug!(target: "thegn::fleet", error = %format!("{error:#}"), "lifetime notification failed");
    }
    match sample_from(prober.probe(&target.env, &target.id)) {
        Err(reason) => {
            schedule.failed(&key, now_ms);
            Visit::ProbeFailed(reason)
        }
        Ok(sample) => match record_sample(db, target, &sample, schedule.was_cold(&key), now_ms) {
            Ok(events) => {
                schedule.sampled(&key, now_ms, cadence(attached));
                Visit::Sampled(events)
            }
            Err(error) => {
                schedule.failed(&key, now_ms);
                Visit::ProbeFailed(format!("{error:#}"))
            }
        },
    }
}

fn probe_argv() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        remote_health::PROBE_SCRIPT.to_string(),
    ]
}

/// Production prober: one provider per env, one exec at a time (one probe in
/// flight per provider account).
struct LiveProber {
    providers: HashMap<String, Provider>,
}

impl Prober for LiveProber {
    fn probe(&self, env: &str, id: &str) -> Result<(i32, String)> {
        let provider = self
            .providers
            .get(env)
            .ok_or_else(|| anyhow::anyhow!("no provider for env {env}"))?;
        let argv = probe_argv();
        crate::agent::block_on_provider(|| async {
            provider
                .run_exec_within(id, &argv, None, &[], PROBE_DEADLINE)
                .await
        })
    }
}

/// Throttled entry: schedule one sampling pass when due. Free when no
/// API-provider env is configured.
pub fn tick(cfg: &Config) {
    use thegn_core::config::EnvProviderKind;
    if !cfg.env.values().any(|envc| {
        !matches!(
            EnvProviderKind::of(&envc.provider.provider),
            EnvProviderKind::Custom
        ) && !envc.provider.provider.trim().is_empty()
    }) {
        return;
    }
    static GATE: crate::reaper_policy::ReaperGate = crate::reaper_policy::ReaperGate::new();
    // Hydration only runs inside an attached client.
    let attached = true;
    let Some(token) = GATE.try_begin(cadence(attached)) else {
        return;
    };
    let cfg = cfg.clone();
    if !crate::reaper_policy::spawn_pass(token, "thegn-remote-health", move || {
        run_pass(&cfg, attached);
    }) {
        tracing::warn!(target: "thegn::lifecycle", "remote health sampler thread could not start");
    }
}

static SCHEDULE: Mutex<Option<Schedule>> = Mutex::new(None);

fn run_pass(cfg: &Config, attached: bool) {
    let view = match crate::fleet_read::gather(cfg, None) {
        Ok(view) => view,
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", error = %format!("{error:#}"), "remote health: fleet read failed");
            return;
        }
    };
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", error = %format!("{error:#}"), "remote health: state db unavailable");
            return;
        }
    };
    let mut providers = HashMap::new();
    let targets: Vec<Target> = view
        .rows
        .iter()
        .filter_map(|row| {
            let worktree = row.worktree.clone()?;
            let envc = cfg.env.get(&row.env)?;
            if !providers.contains_key(&row.env) {
                match crate::provider_factory::provider_for_inventory(&envc.provider) {
                    Ok(provider) => {
                        providers.insert(row.env.clone(), provider);
                    }
                    Err(_) => return None,
                }
            }
            Some(Target {
                env: row.env.clone(),
                id: row.id.clone(),
                provider: row.provider.clone(),
                state: row.state.clone(),
                worktree,
                created_ms: row.created_ms,
                max_lifetime_secs: envc.provider.max_lifetime_secs,
            })
        })
        .collect();
    let prober = LiveProber { providers };
    let mut guard = SCHEDULE.lock().unwrap_or_else(PoisonError::into_inner);
    let schedule = guard.get_or_insert_with(Schedule::default);
    let now_ms = thegn_core::util::now_ms();
    for target in &targets {
        match visit(&db, &prober, schedule, target, attached, now_ms) {
            Visit::ProbeFailed(reason) => {
                tracing::debug!(target: "thegn::fleet", sandbox = %target.key(), reason = %reason, "remote health probe failed; backing off");
            }
            Visit::Sampled(events) if !events.is_empty() => {
                tracing::info!(target: "thegn::fleet", sandbox = %target.key(), events = events.len(), "remote health events recorded");
            }
            Visit::Sampled(_) | Visit::Skipped | Visit::NotDue => {}
        }
    }
}

/// The exec stream of sandbox `id` (env `env`) just ended without an exit
/// status. Run one fresh probe; when the boot id changed against the stored
/// previous reading, return the specific restart warning to print in place of
/// the bare "stream ended" message. `None` when nothing more is known (no
/// previous reading, same boot, probe failed).
pub(crate) async fn explain_lost_stream(
    provider: &Provider,
    provider_name: &str,
    env: &str,
    id: &str,
    now_ms: i64,
) -> Option<String> {
    let ran = provider
        .run_exec_within(id, &probe_argv(), None, &[], STREAM_END_DEADLINE)
        .await;
    let sample = sample_from(ran).ok()?;
    let db = Db::open().ok()?;
    let target = Target {
        env: env.to_string(),
        id: id.to_string(),
        provider: provider_name.to_string(),
        state: Some("running".to_string()),
        worktree: String::new(),
        created_ms: None,
        max_lifetime_secs: 0,
    };
    let events = record_sample(&db, &target, &sample, false, now_ms).ok()?;
    restart_warning(noun(&target.provider), &events)
}

/// The warning for the first restart among `events`.
pub(crate) fn restart_warning(what: &str, events: &[HealthEvent]) -> Option<String> {
    events
        .iter()
        .find(|event| matches!(event, HealthEvent::Restarted { .. }))
        .map(|event| remote_health::warning(what, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use thegn_core::remote_health::RestartCause;

    const NOW: i64 = 1_800_000_000_000;

    struct Fake {
        outputs: RefCell<Vec<Result<(i32, String)>>>,
        calls: RefCell<usize>,
    }

    impl Fake {
        fn new(outputs: Vec<Result<(i32, String)>>) -> Self {
            Self {
                outputs: RefCell::new(outputs),
                calls: RefCell::new(0),
            }
        }
    }

    impl Prober for Fake {
        fn probe(&self, _env: &str, _id: &str) -> Result<(i32, String)> {
            *self.calls.borrow_mut() += 1;
            self.outputs.borrow_mut().remove(0)
        }
    }

    fn probe(boot: &str, mem_cur: u64) -> Result<(i32, String)> {
        Ok((
            0,
            format!(
                "thegn-health v1\nboot_id {boot}\nuptime 100\ncg_mem_max 4000000000\ncg_mem_current {mem_cur}\n"
            ),
        ))
    }

    fn target(state: &str) -> Target {
        Target {
            env: "sx".into(),
            id: "s1".into(),
            provider: "sprites".into(),
            state: Some(state.into()),
            worktree: "/wt".into(),
            created_ms: None,
            max_lifetime_secs: 0,
        }
    }

    #[test]
    fn probe_allowed_never_wakes_a_cold_sandbox() {
        for (state, allowed) in [
            (Some("running"), true),
            (Some("Running"), true),
            (Some("warm"), false),
            (Some("cold"), false),
            (Some("stopped"), false),
            (Some("suspended"), false),
            (None, false),
        ] {
            assert_eq!(probe_allowed(state), allowed, "{state:?}");
        }
    }

    #[test]
    fn cadence_and_lifetime_tables() {
        assert_eq!(cadence(true), Duration::from_secs(60));
        assert_eq!(cadence(false), Duration::from_secs(300));
        assert_eq!(
            lifetime_left_secs(Some(NOW - 3_000_000), 3600, NOW),
            Some(600)
        );
        assert_eq!(
            lifetime_left_secs(Some(NOW - 4_000_000), 3600, NOW),
            Some(-400)
        );
        assert_eq!(lifetime_left_secs(None, 3600, NOW), None);
        assert_eq!(lifetime_left_secs(Some(NOW), 0, NOW), None);
    }

    #[test]
    fn schedule_throttles_and_backs_off() {
        let mut s = Schedule::default();
        assert!(s.due("k", NOW));
        s.sampled("k", NOW, ATTACHED_INTERVAL);
        assert!(!s.due("k", NOW + 59_000));
        assert!(s.due("k", NOW + 60_000));
        s.failed("k", NOW);
        assert!(!s.due("k", NOW + 299_000));
        assert!(s.due("k", NOW + 300_000));
        s.saw_cold("k");
        assert!(s.was_cold("k"));
        s.sampled("k", NOW, ATTACHED_INTERVAL);
        assert!(!s.was_cold("k"));
    }

    #[test]
    fn a_cold_sandbox_is_skipped_without_a_probe() {
        let db = Db::open_memory().unwrap();
        let fake = Fake::new(vec![]);
        let mut schedule = Schedule::default();
        let out = visit(&db, &fake, &mut schedule, &target("cold"), true, NOW);
        assert_eq!(out, Visit::Skipped);
        assert_eq!(*fake.calls.borrow(), 0);
        assert!(schedule.was_cold("sx/s1"));
        assert_eq!(db.remote_health("sx/s1").unwrap(), None);
    }

    #[test]
    fn a_reboot_after_pressure_is_journaled_and_notified_once() {
        let db = Db::open_memory().unwrap();
        let fake = Fake::new(vec![
            probe("boot-a", 3_800_000_000),
            probe("boot-b", 100_000_000),
            probe("boot-b", 100_000_000),
        ]);
        let mut schedule = Schedule::default();
        let t = target("running");
        assert_eq!(
            visit(&db, &fake, &mut schedule, &t, true, NOW),
            Visit::Sampled(vec![])
        );
        assert_eq!(
            visit(&db, &fake, &mut schedule, &t, true, NOW + 1_000),
            Visit::NotDue
        );
        let Visit::Sampled(events) = visit(&db, &fake, &mut schedule, &t, true, NOW + 61_000)
        else {
            panic!("expected a sample");
        };
        assert!(matches!(
            events.as_slice(),
            [HealthEvent::Restarted {
                cause: RestartCause::LikelyOutOfMemory { .. },
                ..
            }]
        ));
        assert_eq!(
            restart_warning("sprite", &events).as_deref(),
            Some(
                "sprite restarted: likely out of memory (3.8 GB of 4.0 GB used before the restart)"
            )
        );
        let journal = db.remote_events("sx/s1", 10).unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].kind, "restart");
        let unread = db.get_unread_notifications().unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].worktree_path, "/wt");
        assert!(unread[0].message.contains("likely out of memory"));
        // The same boot again raises nothing new.
        let Visit::Sampled(again) = visit(&db, &fake, &mut schedule, &t, true, NOW + 130_000)
        else {
            panic!("expected a sample");
        };
        assert!(again.is_empty());
        assert_eq!(db.get_unread_notifications().unwrap().len(), 1);
        assert_eq!(db.remote_events("sx/s1", 10).unwrap().len(), 1);
    }

    #[test]
    fn an_idle_sleep_between_samples_names_the_cause() {
        let db = Db::open_memory().unwrap();
        let fake = Fake::new(vec![probe("a", 1_000), probe("b", 1_000)]);
        let mut schedule = Schedule::default();
        visit(&db, &fake, &mut schedule, &target("running"), true, NOW);
        assert_eq!(
            visit(
                &db,
                &fake,
                &mut schedule,
                &target("cold"),
                true,
                NOW + 30_000
            ),
            Visit::Skipped
        );
        let Visit::Sampled(events) = visit(
            &db,
            &fake,
            &mut schedule,
            &target("running"),
            true,
            NOW + 90_000,
        ) else {
            panic!("expected a sample");
        };
        assert_eq!(
            restart_warning("sprite", &events).as_deref(),
            Some("sprite restarted: it slept while idle")
        );
    }

    #[test]
    fn a_failed_probe_stores_nothing_and_backs_off() {
        let db = Db::open_memory().unwrap();
        let fake = Fake::new(vec![
            Err(anyhow::anyhow!("timed out")),
            Ok((127, String::new())),
        ]);
        let mut schedule = Schedule::default();
        let t = target("running");
        let Visit::ProbeFailed(reason) = visit(&db, &fake, &mut schedule, &t, true, NOW) else {
            panic!("expected a failure");
        };
        assert!(reason.contains("timed out"));
        assert_eq!(db.remote_health("sx/s1").unwrap(), None);
        assert_eq!(
            visit(&db, &fake, &mut schedule, &t, true, NOW + 60_000),
            Visit::NotDue
        );
        let Visit::ProbeFailed(reason) = visit(&db, &fake, &mut schedule, &t, true, NOW + 300_000)
        else {
            panic!("expected a failure");
        };
        assert_eq!(reason, "the health probe exited 127");
    }

    #[test]
    fn an_oom_kill_is_raised_and_a_nearly_expired_lifetime_once() {
        let db = Db::open_memory().unwrap();
        let oom = (
            0,
            "thegn-health v1\nboot_id a\noom Killed process 9 (rustc) anon-rss:3612672kB\n"
                .to_string(),
        );
        let fake = Fake::new(vec![Ok(oom.clone()), Ok(oom)]);
        let mut schedule = Schedule::default();
        let mut t = target("running");
        t.created_ms = Some(NOW - 3_000_000);
        t.max_lifetime_secs = 3600;
        let Visit::Sampled(events) = visit(&db, &fake, &mut schedule, &t, true, NOW) else {
            panic!("expected a sample");
        };
        assert!(matches!(events.as_slice(), [HealthEvent::OomKilled { .. }]));
        let messages: Vec<String> = db
            .get_unread_notifications()
            .unwrap()
            .into_iter()
            .map(|n| n.message)
            .collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("kernel killed rustc")));
        assert!(
            messages
                .iter()
                .any(|m| m == "sprite reaches its maximum lifetime in 10m")
        );
        // The next visit re-reads the same log tail and the same lifetime.
        visit(&db, &fake, &mut schedule, &t, true, NOW + 60_000);
        assert_eq!(db.get_unread_notifications().unwrap().len(), 2);
    }

    #[test]
    fn restart_warning_ignores_other_events() {
        assert_eq!(
            restart_warning("sprite", &[HealthEvent::OomKilled { kills: Vec::new() }]),
            None
        );
    }
}
