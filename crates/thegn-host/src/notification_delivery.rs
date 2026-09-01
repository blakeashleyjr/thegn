//! Bounded, in-memory delivery accounting for routed notifications.
//!
//! The worker owns delivery, but the compositor owns presentation.  This small
//! shared snapshot is the only bridge between those two sides: it contains no
//! payloads, endpoints, or secrets and never causes a periodic wake by itself.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The counters shown by Monitor for one configured sink.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SinkDelivery {
    pub name: String,
    pub kind: String,
    pub queued: u64,
    pub sent: u64,
    pub retries: u64,
    pub rate_limit_drops: u64,
    pub queue_drops: u64,
    pub dead_letters: u64,
}

#[derive(Debug, Default)]
struct DeliveryState {
    sinks: BTreeMap<String, SinkDelivery>,
}

/// A clonable handle to the current delivery counters.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeliverySnapshot {
    state: Arc<Mutex<DeliveryState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryEvent {
    Queued,
    Sent,
    Retry,
    RateLimitDrop,
    QueueDrop,
    DeadLetter,
}

impl DeliverySnapshot {
    /// Replace the configured sink list, retaining no stale endpoint-related
    /// metadata. Counters intentionally survive a config reload only for names
    /// that remain configured, which keeps the view useful without persistence.
    pub(crate) fn configure<I>(&self, sinks: I)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut state = self.state.lock().unwrap();
        let old = std::mem::take(&mut state.sinks);
        state.sinks = sinks
            .into_iter()
            .map(|(name, kind)| {
                let mut row = old.get(&name).cloned().unwrap_or_default();
                row.name = name.clone();
                row.kind = kind;
                (name, row)
            })
            .collect();
    }

    pub(crate) fn event(&self, name: &str, event: DeliveryEvent) {
        let mut state = self.state.lock().unwrap();
        let Some(row) = state.sinks.get_mut(name) else {
            return;
        };
        match event {
            DeliveryEvent::Queued => row.queued = row.queued.saturating_add(1),
            DeliveryEvent::Sent => row.sent = row.sent.saturating_add(1),
            DeliveryEvent::Retry => row.retries = row.retries.saturating_add(1),
            DeliveryEvent::RateLimitDrop => {
                row.rate_limit_drops = row.rate_limit_drops.saturating_add(1)
            }
            DeliveryEvent::QueueDrop => row.queue_drops = row.queue_drops.saturating_add(1),
            DeliveryEvent::DeadLetter => row.dead_letters = row.dead_letters.saturating_add(1),
        }
    }

    pub(crate) fn rows(&self) -> Vec<SinkDelivery> {
        self.state.lock().unwrap().sinks.values().cloned().collect()
    }

    pub(crate) fn visible(&self) -> bool {
        self.state.lock().unwrap().sinks.values().any(|row| {
            row.queued > 0
                || row.sent > 0
                || row.retries > 0
                || row.rate_limit_drops > 0
                || row.queue_drops > 0
                || row.dead_letters > 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_per_sink_and_config_reload_keeps_matching_history() {
        let snapshot = DeliverySnapshot::default();
        snapshot.configure([
            ("oncall".into(), "slack".into()),
            ("phone".into(), "ntfy".into()),
        ]);
        snapshot.event("oncall", DeliveryEvent::QueueDrop);
        snapshot.event("oncall", DeliveryEvent::DeadLetter);
        snapshot.event("phone", DeliveryEvent::Sent);

        snapshot.configure([(String::from("oncall"), String::from("slack"))]);
        let rows = snapshot.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "oncall");
        assert_eq!(rows[0].queue_drops, 1);
        assert_eq!(rows[0].dead_letters, 1);
        assert!(snapshot.visible());
    }

    #[test]
    fn unknown_sink_events_are_ignored() {
        let snapshot = DeliverySnapshot::default();
        snapshot.event("missing", DeliveryEvent::Sent);
        assert!(snapshot.rows().is_empty());
        assert!(!snapshot.visible());
    }
}
