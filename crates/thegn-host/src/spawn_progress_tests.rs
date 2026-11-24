//! Unit tests for the spawn-progress card (`THE-213`). The pane-side
//! integrations (kick seeding, drain retire, fork re-key) are covered by the
//! panes/provision/pty_drain suites; this file pins the card's own rules:
//! grace, monotonic phases, the culprit table, redaction before drawing, and
//! the sampler/ticker's idle contract.

use super::*;
use std::time::Duration;

fn sample(argv: &[&str], started: Instant) -> ProcSample {
    ProcSample {
        pid: 42,
        depth: 1,
        argv: argv.iter().map(|a| a.to_string()).collect(),
        started,
    }
}

#[test]
fn grace_hides_instant_spawns_and_bound_prunes() {
    let t0 = Instant::now();
    let mut tracker = SpawnProgress::kicked(t0);
    // Younger than the grace: no card, so a fast shell never flashes one.
    assert_eq!(tracker.block(None, t0 + GRACE / 2), None);
    // Past the grace: the resolving phase shows with its own elapsed time.
    let card = tracker
        .block(None, t0 + Duration::from_millis(300))
        .expect("past the grace");
    assert_eq!(card.lines[0], "resolving environment · 0.3 s");
    assert!(card.hint.is_none());
    // The grace resets on every phase change (a slow resolve must not make a
    // fast prompt flash the waiting card).
    tracker.advance(Phase::WaitingForShellPrompt, t0 + Duration::from_secs(1));
    assert_eq!(
        tracker.block(None, t0 + Duration::from_secs(1) + GRACE / 2),
        None
    );
    let card = tracker
        .block(None, t0 + Duration::from_secs(3))
        .expect("past the grace again");
    assert_eq!(card.lines[0], "waiting for shell prompt · 2.0 s");
    // Hard bound: 30 s after the kick the tracker is spent.
    assert!(!tracker.is_past_bound(t0 + BOUND));
    assert!(tracker.is_past_bound(t0 + BOUND + Duration::from_millis(1)));
}

#[test]
fn advance_is_monotonic_and_failed_is_terminal() {
    let t0 = Instant::now();
    let mut tracker = SpawnProgress::kicked(t0);
    tracker.advance(Phase::WaitingForShellPrompt, t0 + Duration::from_secs(2));
    // A stale spec-landing update must never regress a moved-on tracker.
    tracker.advance(Phase::StartingSandbox, t0 + Duration::from_secs(3));
    tracker.advance(Phase::StartingShell, t0 + Duration::from_secs(3));
    let card = tracker
        .block(None, t0 + Duration::from_secs(4))
        .expect("card while waiting");
    assert_eq!(card.lines[0], "waiting for shell prompt · 2.0 s");
    // Failed is terminal and carries the reason.
    tracker.fail("no shell in spec", t0 + Duration::from_secs(5));
    tracker.advance(Phase::WaitingForShellPrompt, t0 + Duration::from_secs(6));
    let card = tracker
        .block(None, t0 + Duration::from_secs(6))
        .expect("failure card");
    assert_eq!(card.lines[0], "failed: no shell in spec");
}

#[test]
fn descendant_lines_carry_a_redacted_argv_basename_and_age() {
    let t0 = Instant::now();
    let mut tracker = SpawnProgress::kicked(t0);
    tracker.set_procs(vec![sample(
        &["/nix/store/x-direnv-2.4/bin/direnv", "export", "bash"],
        t0 + Duration::from_millis(400),
    )]);
    let card = tracker
        .block(None, t0 + Duration::from_secs(1))
        .expect("card");
    assert_eq!(card.lines[0], "resolving environment · 1.0 s");
    assert_eq!(card.lines[1], "direnv export bash 0.6 s");
    // The hint comes from the sampled basename, not just the pane program.
    assert_eq!(card.hint.as_deref(), Some("direnv is loading .envrc"));
    // More than three descendants: only the first three lines are kept.
    tracker.set_procs((0..5).map(|_| sample(&["sleep", "1"], t0)).collect());
    assert_eq!(
        tracker
            .block(None, t0 + Duration::from_secs(2))
            .unwrap()
            .lines
            .len(),
        4
    );
}

#[test]
fn argv_values_are_redacted_before_drawing() {
    let t0 = Instant::now();
    let mut tracker = SpawnProgress::kicked(t0);
    tracker.set_procs(vec![sample(
        &["gpg-agent", "--token=supersecret"],
        t0 + Duration::from_millis(900),
    )]);
    let card = tracker.block(None, t0 + Duration::from_secs(1)).unwrap();
    let line = &card.lines[1];
    assert!(line.starts_with("gpg-agent --token="), "{line}");
    assert!(line.contains(thegn_core::log_redact::REDACTED), "{line}");
    assert!(!line.contains("supersecret"), "the secret must never draw");
}

#[test]
fn culprit_rows_match_the_pane_program_first() {
    // An ssh pane answers `connecting` before any descendant is sampled.
    assert_eq!(culprit_hint(Some("ssh"), &[]), Some("connecting"));
    // The pane's own program outranks sampled descendants.
    let procs = [sample(&["git", "status"], Instant::now())];
    assert_eq!(culprit_hint(Some("ssh"), &procs), Some("connecting"));
    // Row priority: direnv beats a coincidental git in the same sample.
    let procs = [
        sample(&["git", "status"], Instant::now()),
        sample(&["direnv", "export", "bash"], Instant::now()),
    ];
    assert_eq!(culprit_hint(None, &procs), Some("direnv is loading .envrc"));
    // Nothing on the table: no hint.
    assert_eq!(
        culprit_hint(None, &[sample(&["make", "-j4"], Instant::now())]),
        None
    );
    assert_eq!(culprit_hint(Some("zsh"), &[]), None);
}

#[test]
fn args_are_clipped_to_the_budget() {
    let long: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    let clipped = clip_args(&long);
    assert_eq!(clipped.chars().count(), ARGS_BUDGET, "{clipped}");
    assert!(clipped.ends_with('…'), "{clipped}");
    let short = clip_args(&["export".to_string(), "bash".to_string()]);
    assert_eq!(short, "export bash");
}

#[test]
fn elapsed_text_fits_the_card() {
    assert_eq!(elapsed_text(Duration::from_millis(412)), "0.4 s");
    assert_eq!(elapsed_text(Duration::from_millis(2400)), "2.4 s");
    assert_eq!(elapsed_text(Duration::from_secs(12)), "12 s");
}

#[test]
fn derive_scopes_cards_to_the_displayed_tab_and_prunes() {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut panes = Panes::new(tx);
    let t0 = Instant::now();
    panes.insert_test_pane(1);
    panes.spawn_progress.insert(1, SpawnProgress::kicked(t0));
    panes.spawn_pending.insert(7, SpawnProgress::kicked(t0));

    let mut leaves = std::collections::HashSet::new();
    leaves.insert(1);
    leaves.insert(7);
    let now = t0 + BOUND + Duration::from_millis(1);
    let hints = derive_hints(&mut panes, &leaves, now);
    // Past the hard bound, every tracker prunes — nothing waits forever.
    assert!(hints.is_empty());
    assert!(panes.spawn_progress.is_empty());
    assert!(panes.spawn_pending.is_empty());

    // Back inside the bound. A tracker for a pane that only exists in
    // another tab (id 9, not in `leaves`): KEPT — its pane is still silent
    // and it may become the displayed tab's card — but it never renders on
    // the displayed tab. Displayed leaves get cards, others do not.
    panes.insert_test_pane(1);
    panes.insert_test_pane(9);
    panes.spawn_progress.insert(1, SpawnProgress::kicked(t0));
    panes.spawn_progress.insert(9, SpawnProgress::kicked(t0));
    panes.spawn_pending.insert(7, SpawnProgress::kicked(t0));
    let now = t0 + GRACE * 2;
    let hints = derive_hints(&mut panes, &leaves, now);
    assert!(hints.contains_key(&1), "the pane tracker renders");
    assert!(hints.contains_key(&7), "the pending leaf renders");
    assert!(!hints.contains_key(&9), "cards are display-tab scoped");
    assert!(hints.len() == 2);
    assert!(
        panes.spawn_progress.contains_key(&9),
        "background tracker kept"
    );

    // First output retires the pane's tracker (the drain owns the retire;
    // the derive must agree if the retire raced a render).
    panes.table.get_mut(&1).unwrap().feed(b"prompt$ ");
    let hints = derive_hints(&mut panes, &leaves, now + GRACE);
    assert!(!hints.contains_key(&1), "output kills the pane card");
    assert!(panes.spawn_progress.get(&1).is_none(), "tracker pruned");
    assert!(hints.contains_key(&7));

    // A leaf that went live through another path drops its pending tracker.
    panes.insert_test_pane(7);
    let hints = derive_hints(&mut panes, &leaves, now + GRACE * 2);
    assert!(!hints.contains_key(&7));
    assert!(panes.spawn_pending.is_empty());
}

#[test]
fn sampler_targets_track_live_panes_only() {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut panes = Panes::new(tx);
    panes
        .spawn_progress
        .insert(3, SpawnProgress::kicked(Instant::now()));
    // Tracker without a pane (departed): not a target.
    assert!(sampler_targets(&panes).is_empty());
    panes.insert_test_pane(3);
    // Test panes have no host pid: kept as a None target so the payload map
    // stays pane-keyed.
    let targets = sampler_targets(&panes);
    assert_eq!(targets, HashMap::from([(3, None)]));
}

#[test]
fn hub_set_targets_and_take_latest_round_trip() {
    let hub = SpawnSamplerHub::default();
    // Nothing published yet.
    assert!(hub.take_latest().is_empty());
    // A target set with no host pid samples nothing (deterministic off
    // Linux too): no payload is published, but the slot survives.
    hub.set_targets(&HashMap::from([(11, None)]));
    hub.sample_all();
    assert!(hub.take_latest().is_empty());
    // Dropping the pane from the target set drops the slot.
    hub.set_targets(&HashMap::new());
    hub.sample_all();
    // Publishing and picking up payloads: one-shot (snapshots, not deltas).
    hub.set_targets(&HashMap::from([(12, None)]));
    assert!(hub.take_latest().is_empty());
    // Redundant set_targets is idempotent (the loop calls it per frame).
    hub.set_targets(&HashMap::from([(12, None)]));
}

/// Poll until `cond` holds (fixed sleeps flake on loaded machines); panics
/// with `what` after a generous deadline.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn ticker_ticks_while_visible_and_parks_itself_out_of_existence() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let t = SpawnTicker::with_period(
        move || {
            c.fetch_add(1, Ordering::Relaxed);
        },
        Duration::from_millis(5),
    );
    t.set_visible(true);
    wait_for("ticks while visible", || count.load(Ordering::Relaxed) >= 3);
    t.set_visible(false);
    wait_for("thread exits after hidden", || {
        !t.running.load(Ordering::Relaxed)
    });
    let settled = count.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(
        count.load(Ordering::Relaxed),
        settled,
        "no ticks after the last pane printed — the idle contract"
    );
}

#[test]
fn ticker_respawns_and_repeat_calls_do_not_stack() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let t = SpawnTicker::with_period(
        move || {
            c.fetch_add(1, Ordering::Relaxed);
        },
        Duration::from_millis(5),
    );
    // Idempotent while up: many calls, one thread (the CAS admits one).
    t.set_visible(true);
    t.set_visible(true);
    t.set_visible(true);
    wait_for("ticking", || count.load(Ordering::Relaxed) >= 2);
    t.set_visible(false);
    wait_for("thread exits", || !t.running.load(Ordering::Relaxed));
    let settled = count.load(Ordering::Relaxed);
    // A later waiting pane brings it back.
    t.set_visible(true);
    wait_for("respawned", || count.load(Ordering::Relaxed) > settled);
    t.set_visible(false);
    wait_for("thread exits again", || !t.running.load(Ordering::Relaxed));
}

#[test]
fn ticker_never_spawns_when_invisible() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let t = SpawnTicker::with_period(
        move || {
            c.fetch_add(1, Ordering::Relaxed);
        },
        Duration::from_millis(5),
    );
    t.set_visible(false);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(count.load(Ordering::Relaxed), 0);
    assert!(!t.running.load(Ordering::Relaxed));
}
