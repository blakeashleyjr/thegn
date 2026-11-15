//! Pure scheduling policy for sharing one host-loop ingress iteration.
//!
//! This is the contract that host-loop drain call sites share while their
//! individual wiring is migrated under THE-657.  Wired today: refresh requests
//! ([`REFRESH_SOURCE`]), workspace-clone events ([`WORKSPACE_CLONE_SOURCE`]),
//! merge-queue drive messages ([`MERGE_DRIVE_SOURCE`]), PR-queue messages
//! ([`PR_QUEUE_SOURCE`]), worktree-creation progress
//! ([`WIZARD_CREATE_SOURCE`]), share supervisor events ([`SHARE_SOURCE`]) and
//! forward detector events ([`FORWARD_SOURCE`]), all through [`take_batch`].
//! A caller snapshots each
//! source's readiness and work already consumed in this iteration, asks for a
//! source turn, drains one bounded portion, updates the snapshot, and asks
//! again.  The caller owns the clock: [`IterationBudget::advance`] is the only
//! way the deadline moves, so this policy never reads a wall clock itself.
//!
//! [`IngressClass::LosslessOrdered`] and [`IngressClass::MustDeliver`] are
//! delivery contracts.  They must never be converted to drop-oldest or
//! latest-only semantics to make a budget fit.  Backpressure, a later wake, or
//! an explicit error is required instead.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;

/// Shared per-iteration deadline for the budgeted host-loop drains.
pub const LOOP_DEADLINE: Duration = Duration::from_millis(4);

/// Refresh requests: every request is delivered, at most 64 per iteration.
pub const REFRESH_SOURCE: SourceReady =
    SourceReady::new(IngressClass::MustDeliver, SourceLimits::new(64, usize::MAX));

/// Workspace-clone progress and completion: every event in order, at most 32
/// per iteration.
pub const WORKSPACE_CLONE_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(32, usize::MAX),
);

/// Merge-queue drive messages: every message in order, at most 64 per drain.
pub const MERGE_DRIVE_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(64, usize::MAX),
);

/// PR-queue driver messages: every message in order, at most 64 per drain.
pub const PR_QUEUE_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(64, usize::MAX),
);

/// Worktree-creation progress: every event in order, at most 32 per drain.
pub const WIZARD_CREATE_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(32, usize::MAX),
);

/// Share supervisor events: every event in order, at most 64 per drain.
pub const SHARE_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(64, usize::MAX),
);

/// Forward detector events: every event in order, at most 64 per drain.
pub const FORWARD_SOURCE: SourceReady = SourceReady::new(
    IngressClass::LosslessOrdered,
    SourceLimits::new(64, usize::MAX),
);

/// Delivery semantics for an ingress source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressClass {
    /// Preserve every item and its order.
    LosslessOrdered,
    /// Deliver every item, although ordering may be defined by the source.
    MustDeliver,
    /// A newer value supersedes an older value.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "named by docs/host-event-ingress.md; no wired source yet"
        )
    )]
    ReplaceableSnapshot,
    /// Adjacent values may be combined without losing the source's meaning.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "named by docs/host-event-ingress.md; no wired source yet"
        )
    )]
    Coalescible,
}

/// Per-source work limits for one iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLimits {
    pub max_msgs: usize,
    pub max_bytes: usize,
}

impl SourceLimits {
    pub const fn new(max_msgs: usize, max_bytes: usize) -> Self {
        Self {
            max_msgs,
            max_bytes,
        }
    }
}

/// A caller-owned readiness and accounting snapshot for one source.
///
/// `used_msgs` and `used_bytes` are the amounts drained from this source in
/// the current iteration.  They are deliberately supplied by the caller:
/// only the drain knows the actual byte size of the next message.  Therefore a
/// turn selected while under the byte limit may overshoot it by that one
/// message; the next selection observes the updated usage and stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceReady {
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the delivery contract is asserted by tests and the doc"
        )
    )]
    pub class: IngressClass,
    pub pending: bool,
    pub limits: SourceLimits,
    pub used_msgs: usize,
    pub used_bytes: usize,
}

impl SourceReady {
    pub const fn new(class: IngressClass, limits: SourceLimits) -> Self {
        Self {
            class,
            pending: true,
            limits,
            used_msgs: 0,
            used_bytes: 0,
        }
    }

    pub const fn idle(mut self) -> Self {
        self.pending = false;
        self
    }

    pub const fn with_usage(mut self, used_msgs: usize, used_bytes: usize) -> Self {
        self.used_msgs = used_msgs;
        self.used_bytes = used_bytes;
        self
    }

    fn within_limits(self) -> bool {
        self.used_msgs < self.limits.max_msgs && self.used_bytes < self.limits.max_bytes
    }
}

/// The result to hand to the event loop after an iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IterationResult {
    /// At least one source still has queued work and needs one re-armed wake.
    /// This is a boolean intentionally: callers must not arm one wake per
    /// source, and this policy never installs a timer.
    pub pending_left: bool,
}

/// Shared deadline and fair source-turn policy for one host-loop iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IterationBudget {
    started_at: Instant,
    deadline_at: Instant,
    now: Instant,
    input_pending: bool,
    rotation_start: usize,
    served: Vec<bool>,
    started: bool,
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "wired callers use take_batch's more_pending")
    )]
    pub pending_left: bool,
}

impl IterationBudget {
    /// Create a budget using an injected logical clock.
    ///
    /// Input waiting at the loop boundary halves the requested deadline.  A
    /// zero deadline remains zero, and no source turn is selected once the
    /// aggregate deadline is spent.
    pub fn new(now: Instant, deadline: Duration, input_pending: bool) -> Self {
        let effective_deadline = if input_pending {
            deadline / 2
        } else {
            deadline
        };
        Self {
            started_at: now,
            deadline_at: now + effective_deadline,
            now,
            input_pending,
            rotation_start: 0,
            served: Vec::new(),
            started: false,
            pending_left: false,
        }
    }

    /// Advance the injected clock used by subsequent source selections.
    ///
    /// Clocks moving backwards are ignored.  A drain may finish one message
    /// after the deadline that was observed before that message started; the
    /// next call to [`Self::next_source`] then refuses more work.
    pub fn advance(&mut self, now: Instant) {
        if now > self.now {
            self.now = now;
        }
    }

    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "inspection accessor; wired callers do not need it"
        )
    )]
    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "inspection accessor; wired callers do not need it"
        )
    )]
    pub fn deadline_at(&self) -> Instant {
        self.deadline_at
    }

    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "inspection accessor; wired callers do not need it"
        )
    )]
    pub fn input_pending(&self) -> bool {
        self.input_pending
    }

    pub fn deadline_spent(&self) -> bool {
        self.now >= self.deadline_at
    }

    /// Select the next source, giving every ready source one turn before any
    /// source gets a second turn.  `rotation` is advanced once at the start
    /// of this iteration, so sustained readiness cannot pin the first source.
    pub fn next_source(&mut self, ready: &[SourceReady], rotation: &mut usize) -> Option<usize> {
        if !self.started {
            self.started = true;
            if ready.is_empty() {
                *rotation = 0;
                self.rotation_start = 0;
            } else {
                self.rotation_start = *rotation % ready.len();
                *rotation = (self.rotation_start + 1) % ready.len();
            }
            self.served = vec![false; ready.len()];
        } else if self.served.len() != ready.len() {
            self.served.resize(ready.len(), false);
        }

        self.pending_left = ready.iter().any(|source| source.pending);
        if ready.is_empty() || self.deadline_spent() {
            return None;
        }

        for second_turn in [false, true] {
            for offset in 0..ready.len() {
                let index = (self.rotation_start + offset) % ready.len();
                if (self.served[index] == second_turn)
                    && ready[index].pending
                    && ready[index].within_limits()
                {
                    self.served[index] = true;
                    return Some(index);
                }
            }
        }
        None
    }

    /// Return the one-wake handoff result after the caller's final drain.
    pub fn result(&self, ready: &[SourceReady]) -> IterationResult {
        IterationResult {
            pending_left: ready.iter().any(|source| source.pending),
        }
    }

    /// Convenience accessor for callers that only need the re-arm decision.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "inspection accessor; wired callers do not need it"
        )
    )]
    pub fn pending_left_for(&self, ready: &[SourceReady]) -> bool {
        self.result(ready).pending_left
    }
}

/// Pop one bounded portion of a lossless channel into a `Vec`.
///
/// The caller keeps its existing per-message body as a `for` over the
/// returned items, so every `continue` and local borrow stays as it was.  At
/// most `source.limits.max_msgs` items are taken, and none after the shared
/// iteration deadline, except that one item is always taken when the channel
/// is non-empty: a spent deadline must never stall a source outright.  Nothing
/// is ever dropped or coalesced; what is not taken stays queued in order, and
/// the returned `more_pending` tells the caller to re-arm one wake.
///
/// `clock` is the caller's clock, read before each further item, so tests can
/// inject a logical time.  Bytes are not measured here: `source.used_bytes` is
/// carried through unchanged.
pub fn take_batch<T>(
    rx: &mut UnboundedReceiver<T>,
    source: SourceReady,
    budget: &mut IterationBudget,
    rotation: &mut usize,
    mut clock: impl FnMut() -> Instant,
) -> (Vec<T>, bool) {
    let mut items = Vec::new();
    // Mandatory progress: the first queued item is taken unconditionally.
    match rx.try_recv() {
        Ok(item) => items.push(item),
        Err(_) => return (items, false),
    }
    loop {
        budget.advance(clock());
        let turn = [source.with_usage(items.len(), source.used_bytes)];
        if budget.next_source(&turn, rotation).is_none() {
            break;
        }
        match rx.try_recv() {
            Ok(item) => items.push(item),
            Err(_) => return (items, false),
        }
    }
    let left = if rx.is_empty() { source.idle() } else { source };
    let more_pending = budget.result(&[left]).pending_left;
    (items, more_pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEADLINE: Duration = Duration::from_millis(10);

    fn ready_sources(count: usize) -> Vec<SourceReady> {
        (0..count)
            .map(|_| SourceReady::new(IngressClass::Coalescible, SourceLimits::new(8, 128)))
            .collect()
    }

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn many_simultaneously_ready_sources_all_get_a_turn() {
        let start = base();
        let mut budget = IterationBudget::new(start, DEADLINE, false);
        let mut rotation = 0;
        let ready = ready_sources(4);
        let mut selected = Vec::new();
        for _ in 0..4 {
            selected.push(budget.next_source(&ready, &mut rotation).unwrap());
        }
        assert_eq!(selected, [0, 1, 2, 3]);
        assert_eq!(budget.next_source(&ready, &mut rotation), Some(0));
    }

    #[test]
    fn perpetually_ready_source_cannot_starve_a_quiet_source() {
        let mut budget = IterationBudget::new(base(), DEADLINE, false);
        let mut rotation = 0;
        let ready = ready_sources(2);
        assert_eq!(budget.next_source(&ready, &mut rotation), Some(0));
        assert_eq!(budget.next_source(&ready, &mut rotation), Some(1));
    }

    #[test]
    fn input_pending_shrinks_the_budget() {
        let start = base();
        let quiet = IterationBudget::new(start, DEADLINE, false);
        let hot = IterationBudget::new(start, DEADLINE, true);
        assert_eq!(quiet.started_at(), start);
        assert!(hot.deadline_at() < quiet.deadline_at());
        assert_eq!(hot.deadline_at(), start + DEADLINE / 2);
        assert!(hot.input_pending());
    }

    #[test]
    fn deadline_exhaustion_leaves_pending_left() {
        let start = base();
        let mut budget = IterationBudget::new(start, DEADLINE, false);
        let mut rotation = 0;
        let ready = ready_sources(2);
        budget.advance(start + DEADLINE);
        assert_eq!(budget.next_source(&ready, &mut rotation), None);
        assert!(budget.result(&ready).pending_left);
        assert!(budget.pending_left_for(&ready));
        assert!(budget.pending_left);
    }

    #[test]
    fn deadline_overshoot_by_one_selected_message_is_allowed() {
        let start = base();
        let mut budget = IterationBudget::new(start, DEADLINE, false);
        let mut rotation = 0;
        let ready = ready_sources(1);
        // Selection happened before the drain learned that this message took
        // the clock past the deadline.  The in-flight message may finish;
        // there must be no second selection.
        assert_eq!(budget.next_source(&ready, &mut rotation), Some(0));
        budget.advance(start + DEADLINE + Duration::from_nanos(1));
        assert_eq!(budget.next_source(&ready, &mut rotation), None);
        assert!(budget.result(&ready).pending_left);
    }

    #[test]
    fn zero_work_iteration_returns_idle_without_spin() {
        let mut budget = IterationBudget::new(base(), DEADLINE, false);
        let mut rotation = 0;
        let ready = ready_sources(2)
            .into_iter()
            .map(SourceReady::idle)
            .collect::<Vec<_>>();
        assert_eq!(budget.next_source(&ready, &mut rotation), None);
        assert_eq!(
            budget.result(&ready),
            IterationResult {
                pending_left: false
            }
        );
        assert_eq!(rotation, 1);
    }

    #[test]
    fn rotation_fairness_over_n_iterations() {
        let start = base();
        let mut rotation = 0;
        let mut first = Vec::new();
        for iteration in 0..12 {
            let mut budget =
                IterationBudget::new(start + DEADLINE * (iteration as u32), DEADLINE, false);
            let ready = ready_sources(3);
            first.push(budget.next_source(&ready, &mut rotation).unwrap());
        }
        assert_eq!(first, [0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn overshoot_by_one_message_is_documented_and_stops_next_turn() {
        let mut budget = IterationBudget::new(base(), DEADLINE, false);
        let mut rotation = 0;
        let mut ready = vec![SourceReady::new(
            IngressClass::LosslessOrdered,
            SourceLimits::new(4, 4),
        )];
        assert_eq!(budget.next_source(&ready, &mut rotation), Some(0));
        // The selected message was already in flight when its five bytes
        // crossed the four-byte limit.  One-message overshoot is allowed;
        // another turn is not.
        ready[0].used_msgs = 1;
        ready[0].used_bytes = 5;
        assert_eq!(budget.next_source(&ready, &mut rotation), None);
        assert!(budget.result(&ready).pending_left);
    }

    #[test]
    fn seeded_property_style_inputs_preserve_limits_and_fairness() {
        let start = base();
        let mut seed = 0x9e37_79b9_u32;
        for case in 0..128 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let count = (seed as usize % 6) + 1;
            let mut ready = Vec::with_capacity(count);
            for index in 0..count {
                seed = seed.rotate_left(5);
                let pending = (seed & 1) != 0;
                let max_msgs = (seed as usize % 4) + 1;
                let max_bytes = ((seed >> 8) as usize % 4) + 1;
                ready.push(
                    SourceReady::new(
                        if index % 2 == 0 {
                            IngressClass::ReplaceableSnapshot
                        } else {
                            IngressClass::MustDeliver
                        },
                        SourceLimits::new(max_msgs, max_bytes),
                    )
                    .with_usage(usize::from(!pending), usize::from(!pending)),
                );
            }
            let mut budget =
                IterationBudget::new(start + DEADLINE * (case as u32), DEADLINE, false);
            let mut rotation = case % count;
            let mut seen = vec![0_usize; count];
            while let Some(index) = budget.next_source(&ready, &mut rotation) {
                assert!(ready[index].pending);
                assert!(matches!(
                    ready[index].class,
                    IngressClass::ReplaceableSnapshot | IngressClass::MustDeliver
                ));
                assert!(ready[index].used_msgs < ready[index].limits.max_msgs);
                assert!(ready[index].used_bytes < ready[index].limits.max_bytes);
                seen[index] += 1;
                ready[index].used_msgs += 1;
                ready[index].used_bytes += 1;
            }
            for (index, turns) in seen.into_iter().enumerate() {
                assert!(turns <= ready[index].limits.max_msgs);
                assert!(turns <= ready[index].limits.max_bytes);
            }
        }
    }

    fn flood(count: u32) -> UnboundedReceiver<u32> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for value in 0..count {
            tx.send(value).unwrap();
        }
        rx
    }

    #[test]
    fn take_batch_flood_yields_exactly_max_msgs_and_reports_more() {
        let start = base();
        let mut rx = flood(10_000);
        let mut budget = IterationBudget::new(start, LOOP_DEADLINE, false);
        let mut rotation = 0;
        let (items, more) = take_batch(&mut rx, REFRESH_SOURCE, &mut budget, &mut rotation, || {
            start
        });
        assert_eq!(items.len(), REFRESH_SOURCE.limits.max_msgs);
        assert_eq!(items, (0..64).collect::<Vec<u32>>());
        assert!(more);
    }

    #[test]
    fn take_batch_repeated_calls_drain_everything_in_order() {
        let start = base();
        let mut rx = flood(10_000);
        let mut rotation = 0;
        let mut drained = Vec::new();
        let mut calls = 0_u32;
        loop {
            let now = start + LOOP_DEADLINE * calls;
            let mut budget = IterationBudget::new(now, LOOP_DEADLINE, false);
            let (items, more) = take_batch(
                &mut rx,
                WORKSPACE_CLONE_SOURCE,
                &mut budget,
                &mut rotation,
                || now,
            );
            assert!(items.len() <= WORKSPACE_CLONE_SOURCE.limits.max_msgs);
            drained.extend(items);
            calls += 1;
            if !more {
                break;
            }
        }
        assert_eq!(drained, (0..10_000).collect::<Vec<u32>>());
        assert_eq!(calls, 10_000_u32.div_ceil(32));
        assert!(rx.is_empty());
    }

    #[test]
    fn take_batch_yields_one_message_when_the_deadline_is_spent() {
        let start = base();
        let mut rx = flood(5);
        let mut budget = IterationBudget::new(start, LOOP_DEADLINE, false);
        budget.advance(start + LOOP_DEADLINE);
        let mut rotation = 0;
        let (items, more) = take_batch(&mut rx, REFRESH_SOURCE, &mut budget, &mut rotation, || {
            start + LOOP_DEADLINE
        });
        assert_eq!(items, [0]);
        assert!(more);
    }

    #[test]
    fn take_batch_input_pending_halves_the_deadline() {
        let start = base();
        // Three quarters of the full deadline: inside it when input is quiet,
        // past the halved deadline when input is pending.
        let later = start + LOOP_DEADLINE * 3 / 4;
        let mut quiet_rx = flood(10);
        let mut quiet = IterationBudget::new(start, LOOP_DEADLINE, false);
        let mut rotation = 0;
        let (quiet_items, quiet_more) = take_batch(
            &mut quiet_rx,
            REFRESH_SOURCE,
            &mut quiet,
            &mut rotation,
            || later,
        );
        assert_eq!(quiet_items.len(), 10);
        assert!(!quiet_more);

        let mut hot_rx = flood(10);
        let mut hot = IterationBudget::new(start, LOOP_DEADLINE, true);
        let (hot_items, hot_more) =
            take_batch(&mut hot_rx, REFRESH_SOURCE, &mut hot, &mut rotation, || {
                later
            });
        assert_eq!(hot_items, [0]);
        assert!(hot_more);
    }

    #[test]
    fn take_batch_empty_channel_returns_nothing_and_no_pending() {
        let start = base();
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let mut budget = IterationBudget::new(start, LOOP_DEADLINE, false);
        let mut rotation = 0;
        let (items, more) = take_batch(
            &mut rx,
            WORKSPACE_CLONE_SOURCE,
            &mut budget,
            &mut rotation,
            || start,
        );
        assert_eq!(items, Vec::<u32>::new());
        assert!(!more);
    }

    #[test]
    fn wired_sources_keep_lossless_delivery_classes() {
        assert_eq!(REFRESH_SOURCE.class, IngressClass::MustDeliver);
        assert_eq!(REFRESH_SOURCE.limits, SourceLimits::new(64, usize::MAX));
        assert_eq!(WORKSPACE_CLONE_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(
            WORKSPACE_CLONE_SOURCE.limits,
            SourceLimits::new(32, usize::MAX)
        );
        assert_eq!(MERGE_DRIVE_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(MERGE_DRIVE_SOURCE.limits, SourceLimits::new(64, usize::MAX));
        assert_eq!(PR_QUEUE_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(PR_QUEUE_SOURCE.limits, SourceLimits::new(64, usize::MAX));
        assert_eq!(LOOP_DEADLINE, Duration::from_millis(4));
    }

    #[test]
    fn ui_event_sources_keep_lossless_ordered_delivery() {
        assert_eq!(WIZARD_CREATE_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(WIZARD_CREATE_SOURCE.limits.max_msgs, 32);
        assert_eq!(
            WIZARD_CREATE_SOURCE.limits,
            SourceLimits::new(32, usize::MAX)
        );
        assert_eq!(SHARE_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(SHARE_SOURCE.limits.max_msgs, 64);
        assert_eq!(SHARE_SOURCE.limits, SourceLimits::new(64, usize::MAX));
        assert_eq!(FORWARD_SOURCE.class, IngressClass::LosslessOrdered);
        assert_eq!(FORWARD_SOURCE.limits.max_msgs, 64);
        assert_eq!(FORWARD_SOURCE.limits, SourceLimits::new(64, usize::MAX));
    }

    /// One item over the cap: the first call takes exactly `max_msgs` in
    /// order and reports more; the second takes the last item and reports
    /// none.  The input exceeds the limit, so the cap is really exercised.
    fn assert_cap_plus_one_drains_in_two_batches(source: SourceReady) {
        let cap = source.limits.max_msgs;
        let total = u32::try_from(cap + 1).unwrap();
        let start = base();
        let mut rx = flood(total);
        let mut rotation = 0;

        let mut first = IterationBudget::new(start, LOOP_DEADLINE, false);
        let (items, more) = take_batch(&mut rx, source, &mut first, &mut rotation, || start);
        assert_eq!(items.len(), cap);
        assert_eq!(items, (0..total - 1).collect::<Vec<u32>>());
        assert!(more);

        let later = start + LOOP_DEADLINE;
        let mut second = IterationBudget::new(later, LOOP_DEADLINE, false);
        let (rest, more) = take_batch(&mut rx, source, &mut second, &mut rotation, || later);
        assert_eq!(rest, vec![total - 1]);
        assert!(!more);
        assert!(rx.is_empty());
    }

    #[test]
    fn take_batch_wizard_create_caps_at_32_then_drains_the_rest() {
        assert_cap_plus_one_drains_in_two_batches(WIZARD_CREATE_SOURCE);
    }

    #[test]
    fn take_batch_share_caps_at_64_then_drains_the_rest() {
        assert_cap_plus_one_drains_in_two_batches(SHARE_SOURCE);
    }

    #[test]
    fn take_batch_forward_caps_at_64_then_drains_the_rest() {
        assert_cap_plus_one_drains_in_two_batches(FORWARD_SOURCE);
    }
}
