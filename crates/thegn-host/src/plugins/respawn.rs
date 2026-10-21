use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Pending restart deadlines, keyed by plugin id so a newer restart replaces
/// an older one for the same plugin.
pub(crate) struct RespawnQueue<P> {
    due: BTreeMap<String, (Instant, P)>,
}

impl<P> Default for RespawnQueue<P> {
    fn default() -> Self {
        Self {
            due: BTreeMap::new(),
        }
    }
}

impl<P> RespawnQueue<P> {
    pub(crate) fn schedule(&mut self, id: String, at: Instant, payload: P) -> bool {
        self.due.insert(id, (at, payload)).is_some()
    }

    pub(crate) fn cancel(&mut self, id: &str) -> bool {
        self.due.remove(id).is_some()
    }

    pub(crate) fn clear(&mut self) -> usize {
        let len = self.due.len();
        self.due.clear();
        len
    }

    pub(crate) fn pop_due(&mut self, now: Instant) -> Option<(String, P)> {
        let (earliest, at) =
            self.due
                .iter()
                .min_by(|(left_id, (left_at, _)), (right_id, (right_at, _))| {
                    left_at.cmp(right_at).then_with(|| left_id.cmp(right_id))
                })?;
        if *at > now {
            return None;
        }
        let earliest = earliest.clone();
        self.due
            .remove(&earliest)
            .map(|(_, payload)| (earliest, payload))
    }

    pub(crate) fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.due
            .values()
            .map(|(at, _)| at.saturating_duration_since(now))
            .min()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.due.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.due.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respawn_queue_pops_earliest_due_first() {
        let t0 = Instant::now();
        let mut queue = RespawnQueue::default();
        queue.schedule("b".into(), t0 + Duration::from_secs(2), 2_u8);
        queue.schedule("a".into(), t0 + Duration::from_secs(1), 1_u8);

        assert_eq!(queue.pop_due(t0), None);
        assert_eq!(
            queue.pop_due(t0 + Duration::from_secs(5)),
            Some(("a".into(), 1))
        );
        assert_eq!(
            queue.pop_due(t0 + Duration::from_secs(5)),
            Some(("b".into(), 2))
        );
        assert_eq!(queue.pop_due(t0 + Duration::from_secs(5)), None);
    }

    #[test]
    fn respawn_queue_breaks_equal_deadline_ties_by_plugin_id() {
        let t0 = Instant::now();
        let mut queue = RespawnQueue::default();
        let deadline = t0 + Duration::from_secs(1);
        queue.schedule("b".into(), deadline, 2_u8);
        queue.schedule("a".into(), deadline, 1_u8);

        assert_eq!(queue.pop_due(deadline), Some(("a".into(), 1)));
        assert_eq!(queue.pop_due(deadline), Some(("b".into(), 2)));
    }

    #[test]
    fn respawn_queue_reschedule_replaces_the_pending_entry() {
        let t0 = Instant::now();
        let mut queue = RespawnQueue::default();
        assert!(!queue.schedule("a".into(), t0 + Duration::from_secs(1), 1_u8));
        assert!(queue.schedule("a".into(), t0 + Duration::from_secs(9), 2_u8));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop_due(t0 + Duration::from_secs(5)), None);
        assert_eq!(
            queue.pop_due(t0 + Duration::from_secs(9)),
            Some(("a".into(), 2))
        );
    }

    #[test]
    fn respawn_queue_cancel_and_clear_drop_pending_entries() {
        let t0 = Instant::now();
        let mut queue = RespawnQueue::default();
        queue.schedule("a".into(), t0 + Duration::from_secs(1), 1_u8);
        queue.schedule("b".into(), t0 + Duration::from_secs(2), 2_u8);
        assert!(queue.cancel("a"));
        assert!(!queue.cancel("a"));
        assert_eq!(queue.clear(), 1);
        assert!(queue.is_empty());
    }

    #[test]
    fn respawn_queue_next_wait_is_time_to_the_earliest_deadline() {
        let t0 = Instant::now();
        let mut queue = RespawnQueue::<u8>::default();
        assert_eq!(queue.next_wait(t0), None);
        queue.schedule("b".into(), t0 + Duration::from_secs(7), 2);
        queue.schedule("a".into(), t0 + Duration::from_secs(3), 1);
        assert_eq!(queue.next_wait(t0), Some(Duration::from_secs(3)));
        assert_eq!(
            queue.next_wait(t0 + Duration::from_secs(4)),
            Some(Duration::ZERO)
        );
    }
}
