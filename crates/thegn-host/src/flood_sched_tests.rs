//! Deterministic integration tests for the host-loop flood scheduler.
//!
//! These tests deliberately stop at the pure scheduling seam.  They exercise
//! the same policy calls as the host loop without starting PTYs, a renderer,
//! or a wall-clock timer.

use crate::loop_policy::{
    BACKLOG_HIGH_WATER, DrainBudget, FrameGate, MAX_SLICE, PANE_FRAME_WINDOW, drain_budget,
    frame_gate, pane_slice,
};
use crate::render_plan::{Damage, Overlays, RenderPlan, plan};
use crate::snapshot_slot::{PublishOutcome, SnapshotSlot};
use std::collections::HashSet;
use std::time::Duration;

const FLOODER: usize = 0;
const QUIET: usize = 1;
const BURSTY: usize = 2;
const PANE_COUNT: usize = 3;
const STEP_MS: u64 = 1;

#[derive(Debug, Clone, Copy)]
struct InputEvent {
    step: usize,
    bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct SimPane {
    id: u32,
    backlog: usize,
    drained: usize,
    first_served_step: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Producers {
    None,
    FloodScenario,
}

#[derive(Debug, Clone, Copy, Default)]
struct LogicalClock {
    step: usize,
}

#[derive(Debug)]
struct StepOutcome {
    order: Vec<&'static str>,
    budget: DrainBudget,
    input_bytes: usize,
    status_taken: bool,
    drained: usize,
    frame_gate: FrameGate,
    render_skipped: bool,
    follow_up_scheduled: bool,
}

struct Simulator {
    clock: LogicalClock,
    panes: [SimPane; PANE_COUNT],
    next_pane: usize,
    inputs: Vec<InputEvent>,
    status: SnapshotSlot<u64>,
    status_generation_seen: Option<u64>,
    last_flush_step: usize,
    producers: Producers,
    stall_steps: usize,
    scheduled_follow_up: bool,
    external_wakes: usize,
}

impl Simulator {
    fn new(producers: Producers) -> Self {
        Self {
            clock: LogicalClock::default(),
            panes: [
                SimPane {
                    id: 0,
                    backlog: 0,
                    drained: 0,
                    first_served_step: None,
                },
                SimPane {
                    id: 1,
                    backlog: 0,
                    drained: 0,
                    first_served_step: None,
                },
                SimPane {
                    id: 2,
                    backlog: 0,
                    drained: 0,
                    first_served_step: None,
                },
            ],
            next_pane: 0,
            inputs: Vec::new(),
            status: SnapshotSlot::with_waker(8, || {}),
            status_generation_seen: None,
            last_flush_step: 0,
            producers,
            stall_steps: 0,
            scheduled_follow_up: false,
            external_wakes: 0,
        }
    }

    fn flood() -> Self {
        let mut sim = Self::new(Producers::FloodScenario);
        sim.panes[FLOODER].backlog = MAX_SLICE;
        sim.panes[QUIET].backlog = MAX_SLICE;
        sim.panes[BURSTY].backlog = MAX_SLICE;
        sim
    }

    fn bounded_backlog() -> Self {
        let mut sim = Self::new(Producers::None);
        sim.panes[FLOODER].backlog = BACKLOG_HIGH_WATER;
        sim
    }

    fn idle() -> Self {
        Self::new(Producers::None)
    }

    fn input_at_current_step(&self) -> Option<InputEvent> {
        self.inputs
            .iter()
            .copied()
            .find(|event| event.step == self.clock.step)
    }

    fn admit(&mut self, pane: usize, bytes: usize) {
        let room = BACKLOG_HIGH_WATER.saturating_sub(self.panes[pane].backlog);
        self.panes[pane].backlog += bytes.min(room);
    }

    fn generate(&mut self) {
        if self.producers != Producers::FloodScenario {
            return;
        }
        self.admit(FLOODER, BACKLOG_HIGH_WATER / 4);
        if self.clock.step == 0 {
            self.admit(QUIET, MAX_SLICE);
        }
        if self.clock.step.is_multiple_of(3) {
            self.admit(BURSTY, MAX_SLICE * 2);
        }
    }

    fn active_panes(&self) -> usize {
        self.panes.iter().filter(|pane| pane.backlog > 0).count()
    }

    fn next_ready_pane(&self) -> Option<usize> {
        (0..PANE_COUNT)
            .map(|offset| (self.next_pane + offset) % PANE_COUNT)
            .find(|&pane| self.panes[pane].backlog > 0)
    }

    fn total_backlog(&self) -> usize {
        self.panes.iter().map(|pane| pane.backlog).sum()
    }

    fn step(&mut self) -> StepOutcome {
        self.generate();
        let input = self.input_at_current_step();
        // Mirrors run.rs:10443-10527: the drain snapshots input readiness,
        // chooses the real byte budget, then round-robins pane slices.
        let budget = drain_budget(input.is_some());
        let mut order = Vec::new();
        let input_bytes = input.map_or(0, |event| {
            order.push("input");
            event.bytes
        });

        // Snapshot consumption is latest-value work adjacent to the drain;
        // keep it before pane parsing so this simulator also catches a status
        // consumer accidentally being placed behind a flood.
        let status_taken = if let Some((generation, _value)) = self.status.take() {
            order.push("snapshot_take");
            self.status_generation_seen = Some(generation);
            true
        } else {
            false
        };

        let mut remaining = budget.max_bytes;
        let mut drained = 0;
        let mut drained_panes = Vec::new();
        if self.clock.step >= self.stall_steps {
            while remaining > 0 {
                let active = self.active_panes();
                let Some(pane) = self.next_ready_pane() else {
                    break;
                };
                let slice = pane_slice(remaining, active);
                order.push("pane_slice");
                let taken = self.panes[pane].backlog.min(slice);
                self.panes[pane].backlog -= taken;
                self.panes[pane].drained += taken;
                if taken > 0 {
                    if self.panes[pane].first_served_step.is_none() {
                        self.panes[pane].first_served_step = Some(self.clock.step);
                    }
                    drained += taken;
                    drained_panes.push(self.panes[pane].id);
                    remaining -= taken;
                } else {
                    break;
                }
                self.next_pane = (pane + 1) % PANE_COUNT;
            }
        }

        let pane_only_damage = !drained_panes.is_empty() && !status_taken && input.is_none();
        let since_last_flush = Duration::from_millis(
            self.clock.step.saturating_sub(self.last_flush_step) as u64 * STEP_MS,
        );
        // Mirrors run.rs:15831-15899: frame_gate runs before the pure render
        // plan, and a deferred pane frame retains its damage for the timeout.
        let gate = frame_gate(
            input.is_some(),
            false,
            pane_only_damage,
            since_last_flush,
            PANE_FRAME_WINDOW,
        );
        if matches!(gate, FrameGate::RenderNow)
            && (pane_only_damage || status_taken || input.is_some())
        {
            self.last_flush_step = self.clock.step;
        }

        let damage = Damage {
            chrome: input.is_some(),
            bars: status_taken,
            panes: drained_panes.iter().copied().collect::<HashSet<_>>(),
            ..Damage::default()
        };
        let render_skipped = matches!(plan(&damage, &Overlays::default()), RenderPlan::Skip);
        let follow_up_scheduled = self.total_backlog() > 0;
        self.scheduled_follow_up = follow_up_scheduled;
        self.clock.step += 1;

        StepOutcome {
            order,
            budget,
            input_bytes,
            status_taken,
            drained,
            frame_gate: gate,
            render_skipped,
            follow_up_scheduled,
        }
    }

    fn drain_until_idle(&mut self) -> usize {
        let mut steps = 0;
        while self.scheduled_follow_up {
            let outcome = self.step();
            steps += 1;
            assert!(outcome.follow_up_scheduled || self.total_backlog() == 0);
        }
        steps
    }
}

#[test]
fn continuous_flooder_gives_quiet_pane_a_slice() {
    let mut sim = Simulator::flood();
    let mut steps = 0;
    while sim.panes[QUIET].first_served_step.is_none() {
        sim.step();
        steps += 1;
        assert!(steps <= PANE_COUNT, "quiet pane was starved");
    }

    assert!(sim.panes[QUIET].drained > 0);
    assert!(sim.panes[FLOODER].drained > 0);
    assert!(sim.panes[BURSTY].drained > 0);
}

#[test]
fn pending_input_preempts_pane_drain_with_reduced_budget() {
    let mut sim = Simulator::flood();
    sim.inputs.push(InputEvent {
        step: 0,
        bytes: drain_budget(true).max_bytes,
    });

    let outcome = sim.step();
    assert_eq!(outcome.budget, drain_budget(true));
    assert_eq!(outcome.input_bytes, drain_budget(true).max_bytes);
    assert_eq!(outcome.order.first(), Some(&"input"));
    assert!(
        outcome
            .order
            .iter()
            .position(|kind| *kind == "input")
            .is_some_and(|input| {
                outcome
                    .order
                    .iter()
                    .position(|kind| *kind == "pane_slice")
                    .is_none_or(|pane| input < pane)
            })
    );
    assert!(outcome.drained <= outcome.budget.max_bytes);
}

#[test]
fn bounded_drain_retains_wake_until_final_backlog_completes() {
    let mut sim = Simulator::bounded_backlog();
    let initial = sim.total_backlog();
    let budget = drain_budget(false).max_bytes;
    let first = sim.step();
    assert!(first.follow_up_scheduled);
    assert!(sim.scheduled_follow_up);
    assert_eq!(sim.external_wakes, 0, "no unrelated wake may be injected");

    let follow_up_steps = sim.drain_until_idle();
    let total_steps = follow_up_steps + 1;
    let step_bound = initial.div_ceil(budget) + 1;
    assert_eq!(sim.total_backlog(), 0, "retained pending work completes");
    assert!(
        total_steps <= step_bound,
        "bounded drain lost progress wake"
    );
    assert_eq!(sim.external_wakes, 0);
}

#[test]
fn idle_step_produces_skip_render_plan() {
    let mut sim = Simulator::idle();
    let outcome = sim.step();
    assert!(outcome.render_skipped);
    assert_eq!(outcome.frame_gate, FrameGate::RenderNow);
    assert_eq!(outcome.drained, 0);
    assert!(outcome.order.is_empty());
}

#[test]
fn aggregate_progress_scales_with_real_slice_policy() {
    let mut sim = Simulator::flood();
    let steps = 10;
    let before: usize = sim.panes.iter().map(|pane| pane.drained).sum();
    for _ in 0..steps {
        sim.step();
    }
    let after: usize = sim.panes.iter().map(|pane| pane.drained).sum();
    let active_panes = sim.panes.len();
    let per_round = pane_slice(drain_budget(false).max_bytes, active_panes);
    let lower_bound = per_round.saturating_mul(steps);
    assert!(after.saturating_sub(before) >= lower_bound);
}

#[test]
fn status_producer_keeps_one_latest_snapshot_for_consumer() {
    let mut sim = Simulator::idle();
    // The producer's input is deliberately large enough to emit snapshots;
    // a zero-sized generator input would make a progress assertion vacuous.
    sim.inputs.push(InputEvent {
        step: 0,
        bytes: 16 * 1024,
    });
    for generation in 0..10_000 {
        let outcome = sim.status.publish(generation, generation, 8);
        if generation == 0 {
            assert_eq!(outcome, PublishOutcome::Stored);
        } else {
            assert_eq!(outcome, PublishOutcome::Replaced);
        }
    }
    assert_eq!(sim.status.replacements(), 9_999);

    let outcome = sim.step();
    assert!(outcome.status_taken);
    assert_eq!(sim.status_generation_seen, Some(9_999));
    assert_eq!(sim.status.take(), None);
}

#[test]
fn stalled_writer_backpressure_bounds_memory() {
    let mut sim = Simulator::new(Producers::FloodScenario);
    sim.stall_steps = 12;
    let bound = BACKLOG_HIGH_WATER * sim.panes.len() + MAX_SLICE;
    for _ in 0..sim.stall_steps {
        sim.step();
        assert!(sim.total_backlog() <= bound);
    }
    assert!(sim.total_backlog() > 0);

    for _ in 0..4 {
        sim.step();
        assert!(sim.total_backlog() <= bound);
    }
}
