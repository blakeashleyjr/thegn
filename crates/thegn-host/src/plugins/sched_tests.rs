//! Deterministic scheduler tables. These tests use one synthetic clock and
//! never wait for a scheduler tick.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thegn_core::plugin_api::{CadenceHint, NegotiatedManifest, PluginId, PluginMode, PluginSpec};
use thegn_svc::plugin::{LoadedPlugin, ResidentSession, ResidentSupervisor, SessionEvent};

use super::*;

fn loaded_state(id: &str, mode: PluginMode, millis: u64) -> LoadedState {
    let mut spec: PluginSpec = toml::from_str(include_str!(
        "../../../../examples/plugins/hello/plugin.toml"
    ))
    .expect("hello fixture is valid");
    spec.manifest.id = PluginId::new(id);
    spec.mode = mode;
    spec.manifest.contributions.truncate(1);
    spec.manifest.contributions[0].cadence = CadenceHint::Interval { millis };
    let accepted_contributions = spec.manifest.contributions.clone();
    LoadedState {
        plugin: LoadedPlugin {
            spec,
            dir: None,
            trusted_native_ignored: false,
        },
        negotiated: NegotiatedManifest {
            accepted_contributions,
            ..Default::default()
        },
        writer: None,
        session: None,
    }
}

fn empty_runtime_state() -> (Disabled, Sessions) {
    (
        Arc::new(Mutex::new(BTreeSet::new())),
        Arc::new(Mutex::new(BTreeMap::new())),
    )
}

#[test]
fn schedule_deadlines_cover_small_and_unrepresentably_large_intervals() {
    let base = Instant::now();
    for millis in [MIN_PLUGIN_INTERVAL_MS, u64::MAX] {
        let interval = Duration::from_millis(millis);
        let mut schedule = build_schedule(
            &[loaded_state("deadline", PluginMode::OneShot, millis)],
            base,
        );
        let (disabled, sessions) = empty_runtime_state();
        let gate = OneShotGate::default();
        let mut started = 0;

        run_due(
            &mut schedule,
            base,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, _| {
                started += 1;
                drop(ticket);
            },
        );

        assert_eq!(started, 1, "interval {millis} must fire when due");
        assert_eq!(
            schedule[0].next,
            next_schedule_deadline(base, interval),
            "interval {millis} must have a checked deadline"
        );
        assert!(schedule[0].next > base, "interval {millis} must not wrap");
    }
}

#[test]
fn plugin_interval_floor_clamps_without_deadline_overflow() {
    for (requested, expected) in [
        (0, MIN_PLUGIN_INTERVAL_MS),
        (1, MIN_PLUGIN_INTERVAL_MS),
        (999, MIN_PLUGIN_INTERVAL_MS),
        (MIN_PLUGIN_INTERVAL_MS, MIN_PLUGIN_INTERVAL_MS),
        (5_000, 5_000),
        (u64::MAX, u64::MAX),
    ] {
        let clamped = clamp_plugin_interval_millis(requested);
        assert_eq!(clamped, expected, "requested interval {requested}");

        let base = Instant::now();
        let interval = Duration::from_millis(clamped);
        let deadline = next_schedule_deadline(base, interval);
        match base.checked_add(interval) {
            Some(expected_deadline) => assert_eq!(deadline, expected_deadline),
            None => assert!(deadline > base),
        }
    }
}

#[test]
fn build_schedule_clamps_interval_below_floor() {
    let now = Instant::now();
    let schedule = build_schedule(
        &[loaded_state("below-floor", PluginMode::OneShot, 999)],
        now,
    );

    assert_eq!(schedule.len(), 1);
    assert_eq!(
        schedule[0].interval,
        Duration::from_millis(MIN_PLUGIN_INTERVAL_MS)
    );
}

#[test]
fn shipped_hello_example_uses_at_least_the_plugin_interval_floor() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/hello/plugin.toml");
    let text = std::fs::read_to_string(path).expect("shipped hello manifest is readable");
    let spec: PluginSpec = toml::from_str(&text).expect("shipped hello manifest is valid");
    let intervals: Vec<_> = spec
        .manifest
        .contributions
        .iter()
        .filter_map(|contribution| match &contribution.cadence {
            CadenceHint::Interval { millis } => Some(*millis),
            _ => None,
        })
        .collect();

    assert!(
        !intervals.is_empty(),
        "hello example must declare an interval"
    );
    assert!(
        intervals
            .iter()
            .all(|&millis| millis >= MIN_PLUGIN_INTERVAL_MS),
        "hello example cadence must respect the {MIN_PLUGIN_INTERVAL_MS} ms floor: {intervals:?}"
    );
}

#[test]
fn long_scheduler_gap_coalesces_to_one_fire_and_one_future_tick() {
    let base = Instant::now();
    let interval = Duration::from_secs(3);
    let mut schedule = build_schedule(
        &[loaded_state("coalesced", PluginMode::OneShot, 3_000)],
        base,
    );
    let (disabled, sessions) = empty_runtime_state();
    let gate = OneShotGate::default();
    let late = base + interval * 101;
    let mut started = 0;

    run_due(
        &mut schedule,
        late,
        &disabled,
        &sessions,
        &gate,
        &mut |ticket, _| {
            started += 1;
            drop(ticket);
        },
    );
    assert_eq!(started, 1);
    assert_eq!(schedule[0].next, late + interval);

    run_due(
        &mut schedule,
        late + interval,
        &disabled,
        &sessions,
        &gate,
        &mut |ticket, _| {
            started += 1;
            drop(ticket);
        },
    );
    assert_eq!(started, 2, "a long gap must not create a catch-up burst");
}

#[test]
fn simultaneously_due_entries_keep_stable_last_started_ties() {
    let base = Instant::now();
    let interval = Duration::from_secs(1);
    let states = [
        loaded_state("first", PluginMode::OneShot, 1_000),
        loaded_state("second", PluginMode::OneShot, 1_000),
    ];
    let mut schedule = build_schedule(&states, base);
    let (disabled, sessions) = empty_runtime_state();
    let gate = OneShotGate::default();
    let mut passes = Vec::new();

    for pass in 0..2 {
        let mut started = Vec::new();
        run_due(
            &mut schedule,
            base + interval * pass,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, entry| {
                started.push(entry.plugin.clone());
                drop(ticket);
            },
        );
        passes.push(started);
    }

    assert_eq!(
        passes,
        vec![
            vec!["first".to_string(), "second".to_string()],
            vec!["first".to_string(), "second".to_string()]
        ]
    );
}

#[test]
fn disabled_due_entries_do_not_advance_or_consume_admission() {
    let base = Instant::now();
    let interval = Duration::from_secs(1);
    let mut schedule = build_schedule(
        &[loaded_state("disabled", PluginMode::OneShot, 1_000)],
        base,
    );
    let (disabled, sessions) = empty_runtime_state();
    lock(&disabled).insert("disabled".into());
    let gate = OneShotGate::default();
    let mut started = 0;

    run_due(
        &mut schedule,
        base,
        &disabled,
        &sessions,
        &gate,
        &mut |ticket, _| {
            started += 1;
            drop(ticket);
        },
    );
    assert_eq!(started, 0);
    assert_eq!(schedule[0].next, base);
    assert_eq!(schedule[0].skipped, 0);
    assert_eq!(schedule[0].overloaded, 0);

    lock(&disabled).clear();
    run_due(
        &mut schedule,
        base,
        &disabled,
        &sessions,
        &gate,
        &mut |ticket, _| {
            started += 1;
            drop(ticket);
        },
    );
    assert_eq!(started, 1);
    assert_eq!(schedule[0].next, base + interval);
}

#[test]
fn resident_render_is_admitted_while_one_shot_is_busy_or_overloaded() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("resident test runtime");
    let supervisor = ResidentSupervisor::new(runtime.handle().clone());
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let resident_command = vec![
        "sh".into(),
        "-c".into(),
        r#"while IFS= read -r _line; do printf '%s\n' '{"method":"render"}'; done"#.into(),
    ];
    let resident = ResidentSession::spawn(
        &supervisor,
        "sched-resident-test",
        &resident_command,
        &BTreeMap::new(),
        None,
        move |event| {
            event_tx
                .send(event)
                .expect("resident event receiver remains live");
        },
    )
    .expect("resident fixture starts");

    let base = Instant::now();
    let interval = Duration::from_secs(1);
    let states = [
        loaded_state("slow", PluginMode::OneShot, 1_000),
        loaded_state("resident", PluginMode::Resident, 1_000),
    ];
    let mut schedule = build_schedule(&states, base);
    let disabled: Disabled = Arc::new(Mutex::new(BTreeSet::new()));
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::from([("resident".into(), resident)])));
    let gate = OneShotGate::default();

    let busy = gate.try_begin("slow").expect("busy holder");
    run_due(
        &mut schedule,
        base,
        &disabled,
        &sessions,
        &gate,
        &mut |_, _| panic!("busy one-shot must not start"),
    );
    drop(busy);
    assert_eq!(schedule[0].skipped, 1);
    assert_eq!(schedule[1].next, base + interval);
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_secs(10)),
        Ok(SessionEvent::Message(message)) if message.method == "render"
    ));

    let holders: Vec<_> = (0..MAX_INFLIGHT_ONE_SHOTS)
        .map(|index| gate.try_begin(&format!("holder-{index}")))
        .collect::<Result<_, _>>()
        .expect("global cap holders");
    let next = base + interval;
    run_due(
        &mut schedule,
        next,
        &disabled,
        &sessions,
        &gate,
        &mut |_, _| panic!("overloaded one-shot must not start"),
    );
    drop(holders);
    assert_eq!(schedule[0].overloaded, 1);
    assert_eq!(schedule[1].next, next + interval);
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_secs(10)),
        Ok(SessionEvent::Message(message)) if message.method == "render"
    ));

    drop(sessions);
    let report = runtime
        .block_on(supervisor.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(10)));
    assert!(
        report.is_settled(),
        "resident fixture must shut down cleanly"
    );
}

#[test]
fn uneven_one_shot_durations_never_starve_over_two_hundred_passes() {
    const PASSES: usize = 200;
    const SLOW_DURATION: usize = 7;
    let base = Instant::now();
    let interval = Duration::from_secs(1);
    let states = [
        loaded_state("slow", PluginMode::OneShot, 1_000),
        loaded_state("fast", PluginMode::OneShot, 1_000),
    ];
    let mut schedule = build_schedule(&states, base);
    let (disabled, sessions) = empty_runtime_state();
    let gate = OneShotGate::default();
    let mut starts: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut slow_ticket = None;

    for pass in 0..PASSES {
        if slow_ticket
            .as_ref()
            .is_some_and(|(_, release_at)| *release_at == pass)
        {
            drop(slow_ticket.take());
        }
        run_due(
            &mut schedule,
            base + interval * pass as u32,
            &disabled,
            &sessions,
            &gate,
            &mut |ticket, entry| {
                starts.entry(entry.plugin.clone()).or_default().push(pass);
                if entry.plugin == "slow" {
                    slow_ticket = Some((ticket, pass + SLOW_DURATION));
                } else {
                    drop(ticket);
                }
            },
        );
    }

    let fast = &starts["fast"];
    let slow = &starts["slow"];
    assert_eq!(fast.len(), PASSES, "fast plugin must run on every pass");
    assert!(!slow.is_empty(), "slow plugin must receive a turn");
    assert!(
        slow.windows(2)
            .all(|pair| pair[1] - pair[0] <= SLOW_DURATION),
        "slow plugin's maximum admission gap exceeded: {slow:?}"
    );
    assert!(
        slow.last()
            .is_some_and(|pass| PASSES - 1 - pass < SLOW_DURATION),
        "slow plugin must continue receiving turns near the end: {slow:?}"
    );
}
