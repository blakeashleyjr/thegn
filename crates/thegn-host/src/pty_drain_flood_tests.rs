use std::cell::Cell;
use std::time::{Duration, Instant};

use super::{PtyBacklog, drain_backlog_work};
use crate::loop_policy::{BACKLOG_HIGH_WATER, MAX_SLICE, drain_budget};

const HUGE_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

#[test]
fn production_flood_keeps_a_quiet_pane_ordered_and_bounded_in_passes() {
    let mut backlog = PtyBacklog::default();
    let quiet = b"quiet pane\n".to_vec();
    let normal_budget = drain_budget(false).max_bytes;
    backlog.push(1, vec![b'f'; normal_budget * 2]);
    backlog.push(2, quiet.clone());

    let mut fed = Vec::new();
    let max_passes = 4;
    for pass_number in 0..max_passes {
        // Keep producing flood output while the quiet pane is waiting.
        backlog.push(1, vec![b'f'; MAX_SLICE]);
        drain_backlog_work(
            &mut backlog,
            drain_budget(false).max_bytes,
            HUGE_DEADLINE,
            Instant::now(),
            |id, slice, _| fed.push((pass_number, id, slice.to_vec())),
            || false,
        );
    }

    let mut quiet_fed = Vec::new();
    let mut quiet_pass = None;
    for (pass_number, id, slice) in fed {
        if id == 2 {
            if quiet_pass.is_none() {
                quiet_pass = Some(pass_number);
            }
            quiet_fed.extend_from_slice(&slice);
        }
    }
    assert!(quiet_pass.is_some_and(|pass| pass < max_passes));
    assert_eq!(quiet_fed, quiet);
}

#[test]
fn production_flood_preemption_preserves_work_for_a_later_pass() {
    let mut backlog = PtyBacklog::default();
    let input_budget = drain_budget(true).max_bytes;
    backlog.push(7, vec![b'x'; input_budget + MAX_SLICE]);

    let fed_bytes = Cell::new(0usize);
    let input_polls = Cell::new(0usize);
    let first = drain_backlog_work(
        &mut backlog,
        input_budget,
        HUGE_DEADLINE,
        Instant::now(),
        |_, slice, _| fed_bytes.set(fed_bytes.get() + slice.len()),
        || {
            input_polls.set(input_polls.get() + 1);
            fed_bytes.get() > 0
        },
    );

    assert!(first.preempted);
    assert_eq!(fed_bytes.get(), input_budget);
    assert_eq!(input_polls.get(), 1);
    assert!(
        backlog.has_work(),
        "preemption must leave the wake-worthy tail"
    );

    let later = drain_backlog_work(
        &mut backlog,
        drain_budget(false).max_bytes,
        HUGE_DEADLINE,
        Instant::now(),
        |_, _, _| {},
        || false,
    );
    assert!(!later.preempted);
    assert!(
        !backlog.has_work(),
        "a later input-free pass drains the tail"
    );
}

#[test]
fn production_flood_makes_budget_derived_progress_across_passes() {
    let mut backlog = PtyBacklog::default();
    let passes = 4;
    let normal_budget = drain_budget(false).max_bytes;
    backlog.push(3, vec![b'f'; normal_budget * passes]);

    let mut fed = 0usize;
    for _ in 0..passes {
        drain_backlog_work(
            &mut backlog,
            drain_budget(false).max_bytes,
            HUGE_DEADLINE,
            Instant::now(),
            |_, slice, _| fed += slice.len(),
            || false,
        );
    }

    let lower_bound = (0..passes)
        .map(|_| drain_budget(false).max_bytes)
        .sum::<usize>();
    assert!(fed >= lower_bound);
}

#[test]
fn production_flood_writer_stall_model_keeps_backlog_within_one_chunk() {
    let mut backlog = PtyBacklog::default();
    let chunk = vec![b'f'; MAX_SLICE];

    while backlog.total < BACKLOG_HIGH_WATER {
        backlog.push(5, chunk.clone());
        assert!(backlog.total <= BACKLOG_HIGH_WATER + chunk.len());
    }

    assert!(backlog.total >= BACKLOG_HIGH_WATER);
    assert!(backlog.total <= BACKLOG_HIGH_WATER + chunk.len());
}

#[test]
fn production_flood_preserves_exit_tail_and_retires_once_after_preemption() {
    let mut backlog = PtyBacklog::default();
    let normal_budget = drain_budget(false).max_bytes;
    let flood = vec![b'f'; normal_budget * 2];
    let tail = vec![b't'; MAX_SLICE + 7];
    backlog.push(1, flood.clone());
    backlog.push(2, tail.clone());
    backlog.pending_exit(2, Some(42));
    backlog.pending_exit(2, Some(99));

    let input_budget = drain_budget(true).max_bytes;
    let fed_after_first = Cell::new(0usize);
    let mut fed = Vec::new();
    let first = drain_backlog_work(
        &mut backlog,
        input_budget,
        HUGE_DEADLINE,
        Instant::now(),
        |id, slice, _| {
            fed_after_first.set(fed_after_first.get() + slice.len());
            fed.push((id, slice.to_vec()));
        },
        || fed_after_first.get() > 0,
    );
    assert!(first.preempted);
    assert!(first.completed_exits.is_empty());
    assert!(backlog.has_work());
    assert_eq!(fed[0].0, 1, "the flood is ahead of the queued exit tail");

    let mut completed = first.completed_exits;
    while backlog.has_work() {
        let pass = drain_backlog_work(
            &mut backlog,
            drain_budget(false).max_bytes,
            HUGE_DEADLINE,
            Instant::now(),
            |id, slice, _| fed.push((id, slice.to_vec())),
            || false,
        );
        completed.extend(pass.completed_exits);
    }

    let mut flood_fed = Vec::new();
    let mut tail_fed = Vec::new();
    for (id, slice) in fed {
        if id == 1 {
            flood_fed.extend_from_slice(&slice);
        } else if id == 2 {
            tail_fed.extend_from_slice(&slice);
        }
    }
    assert_eq!(flood_fed, flood);
    assert_eq!(tail_fed, tail);
    assert_eq!(completed, vec![(2, Some(42))]);
    assert!(!backlog.has_work());
}

#[test]
fn production_flood_with_spent_deadline_makes_no_progress_but_keeps_work() {
    let mut backlog = PtyBacklog::default();
    backlog.push(8, vec![b'x'; MAX_SLICE]);
    let before = backlog.total;
    let started = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("test instant supports subtraction");
    let fed = Cell::new(0usize);

    let pass = drain_backlog_work(
        &mut backlog,
        drain_budget(false).max_bytes,
        Duration::ZERO,
        started,
        |_, slice, _| fed.set(fed.get() + slice.len()),
        || false,
    );

    assert_eq!(fed.get(), 0);
    assert!(pass.fed_panes.is_empty());
    assert_eq!(backlog.total, before);
    assert!(backlog.has_work());
}
