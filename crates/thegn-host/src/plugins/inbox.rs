use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use thegn_core::plugin_api::RpcMessage;
use thegn_svc::plugin::SessionEvent;

pub(crate) const MAX_QUEUED_EVENTS_PER_PLUGIN: usize = 1024;
pub(crate) const MAX_QUEUED_EVENTS_TOTAL: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushOutcome {
    Queued,
    Coalesced,
    Dropped,
    Rejected,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PluginInboxCounts {
    pub coalesced: usize,
    pub dropped: usize,
    pub rejected: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RejectedRequest {
    pub plugin: String,
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CoalesceKey {
    plugin: String,
    method: String,
    target: String,
}

struct QueuedEvent {
    plugin: String,
    event: SessionEvent,
    sequence: u64,
    key: Option<CoalesceKey>,
}

#[derive(Default)]
struct InboxState {
    queue: VecDeque<QueuedEvent>,
    plugin_counts: HashMap<String, usize>,
    coalesced: HashMap<CoalesceKey, u64>,
    rejected: VecDeque<RejectedRequest>,
    rejected_plugin_counts: HashMap<String, usize>,
    diagnostics: BTreeMap<String, PluginInboxCounts>,
    next_sequence: u64,
}

#[derive(Default)]
pub(crate) struct PluginInbox {
    state: Mutex<InboxState>,
}

impl PluginInbox {
    pub(crate) fn push(&self, plugin: String, event: SessionEvent) -> PushOutcome {
        self.push_with_wake(plugin, event).0
    }

    pub(crate) fn push_with_wake(
        &self,
        plugin: String,
        event: SessionEvent,
    ) -> (PushOutcome, bool) {
        let mut state = lock(&self.state);
        let was_empty = state.plugin_counts.get(&plugin).copied().unwrap_or(0) == 0;
        let key = coalesce_key(&plugin, &event);
        let mut replaced = false;
        if let Some(key) = &key
            && let Some(sequence) = state.coalesced.remove(key)
            && let Some(position) = state
                .queue
                .iter()
                .position(|queued| queued.sequence == sequence)
        {
            state.queue.remove(position);
            decrement(&mut state.plugin_counts, &plugin);
            bump(&mut state.diagnostics, &plugin, |counts| {
                counts.coalesced += 1
            });
            replaced = true;
        }

        let is_exit = matches!(event, SessionEvent::Exit { .. });
        let at_cap = state.plugin_counts.get(&plugin).copied().unwrap_or(0)
            >= MAX_QUEUED_EVENTS_PER_PLUGIN
            || state.queue.len() >= MAX_QUEUED_EVENTS_TOTAL;
        if at_cap && !is_exit {
            if let Some(id) = match &event {
                SessionEvent::Message(RpcMessage { id: Some(id), .. }) => Some(*id),
                _ => None,
            } {
                let rejected_for_plugin = state
                    .rejected_plugin_counts
                    .get(&plugin)
                    .copied()
                    .unwrap_or(0);
                if rejected_for_plugin < MAX_QUEUED_EVENTS_PER_PLUGIN
                    && state.rejected.len() < MAX_QUEUED_EVENTS_TOTAL
                {
                    state.rejected.push_back(RejectedRequest {
                        plugin: plugin.clone(),
                        id,
                    });
                    *state
                        .rejected_plugin_counts
                        .entry(plugin.clone())
                        .or_default() += 1;
                    bump(&mut state.diagnostics, &plugin, |counts| {
                        counts.rejected += 1
                    });
                    return (PushOutcome::Rejected, false);
                }
                bump(&mut state.diagnostics, &plugin, |counts| {
                    counts.dropped += 1
                });
                return (PushOutcome::Dropped, false);
            }
            bump(&mut state.diagnostics, &plugin, |counts| {
                counts.dropped += 1
            });
            return (PushOutcome::Dropped, false);
        }

        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        if let Some(key) = key.clone() {
            state.coalesced.insert(key.clone(), sequence);
        }
        state.queue.push_back(QueuedEvent {
            plugin: plugin.clone(),
            event,
            sequence,
            key,
        });
        *state.plugin_counts.entry(plugin).or_default() += 1;
        (
            if replaced {
                PushOutcome::Coalesced
            } else {
                PushOutcome::Queued
            },
            is_exit || was_empty,
        )
    }

    pub(crate) fn pop(&self) -> Option<(String, SessionEvent)> {
        let mut state = lock(&self.state);
        let queued = state.queue.pop_front()?;
        decrement(&mut state.plugin_counts, &queued.plugin);
        if let Some(key) = queued.key
            && state.coalesced.get(&key) == Some(&queued.sequence)
        {
            state.coalesced.remove(&key);
        }
        Some((queued.plugin, queued.event))
    }

    pub(crate) fn pop_rejected(&self) -> Option<RejectedRequest> {
        let mut state = lock(&self.state);
        let rejected = state.rejected.pop_front()?;
        decrement(&mut state.rejected_plugin_counts, &rejected.plugin);
        Some(rejected)
    }

    pub(crate) fn is_empty(&self) -> bool {
        let state = lock(&self.state);
        state.queue.is_empty() && state.rejected.is_empty()
    }

    pub(crate) fn take_diagnostics(&self) -> BTreeMap<String, PluginInboxCounts> {
        std::mem::take(&mut lock(&self.state).diagnostics)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.state).queue.len()
    }
}

fn coalesce_key(plugin: &str, event: &SessionEvent) -> Option<CoalesceKey> {
    let SessionEvent::Message(message) = event else {
        return None;
    };
    if message.id.is_some() {
        return None;
    }
    let target = match message.method.as_str() {
        "update" | "invalidate" => message.params.get("surface")?.as_str()?,
        "state.set" => message.params.get("key")?.as_str()?,
        _ => return None,
    };
    Some(CoalesceKey {
        plugin: plugin.to_string(),
        method: message.method.clone(),
        target: target.to_string(),
    })
}

fn decrement(counts: &mut HashMap<String, usize>, plugin: &str) {
    if let Some(count) = counts.get_mut(plugin) {
        *count -= 1;
        if *count == 0 {
            counts.remove(plugin);
        }
    }
}

fn bump(
    diagnostics: &mut BTreeMap<String, PluginInboxCounts>,
    plugin: &str,
    update: impl FnOnce(&mut PluginInboxCounts),
) {
    update(diagnostics.entry(plugin.to_string()).or_default());
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(surface: Option<&str>, text: &str, id: Option<u64>) -> SessionEvent {
        let mut params = json!({"view":{"spans":[{"text":text,"role":"Default"}]}});
        if let Some(surface) = surface {
            params["surface"] = json!(surface);
        }
        SessionEvent::Message(RpcMessage {
            id,
            method: "update".into(),
            params,
        })
    }

    fn register() -> SessionEvent {
        SessionEvent::Message(RpcMessage {
            id: None,
            method: "register".into(),
            params: json!({}),
        })
    }

    #[test]
    fn update_flood_coalesces_to_latest_per_surface() {
        let inbox = PluginInbox::default();
        for index in 0..10_000 {
            inbox.push(
                "p".into(),
                update(Some("seg"), &format!("seg-{index}"), None),
            );
            inbox.push(
                "p".into(),
                update(Some("other"), &format!("other-{index}"), None),
            );
        }
        assert_eq!(inbox.len(), 2);
        let (_, first) = inbox.pop().unwrap();
        let (_, second) = inbox.pop().unwrap();
        assert!(
            matches!(first, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "seg-9999")
        );
        assert!(
            matches!(second, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "other-9999")
        );
    }

    #[test]
    fn coalesced_update_moves_after_intervening_register() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), update(Some("s1"), "v1", None));
        inbox.push("p".into(), register());
        inbox.push("p".into(), update(Some("s1"), "v2", None));
        assert!(
            matches!(inbox.pop().unwrap().1, SessionEvent::Message(RpcMessage { method, .. }) if method == "register")
        );
        assert!(
            matches!(inbox.pop().unwrap().1, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "v2")
        );
    }

    #[test]
    fn requests_are_never_coalesced() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), update(Some("seg"), "v1", Some(1)));
        inbox.push("p".into(), update(Some("seg"), "v2", Some(2)));
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn cap_drops_notifications_and_rejects_requests_but_keeps_exit() {
        let inbox = PluginInbox::default();
        for _ in 0..MAX_QUEUED_EVENTS_PER_PLUGIN {
            inbox.push(
                "p".into(),
                SessionEvent::Message(RpcMessage {
                    id: None,
                    method: "emit".into(),
                    params: json!({}),
                }),
            );
        }
        assert_eq!(
            inbox.push(
                "p".into(),
                SessionEvent::Message(RpcMessage {
                    id: None,
                    method: "emit".into(),
                    params: json!({})
                })
            ),
            PushOutcome::Dropped
        );
        assert_eq!(
            inbox.push(
                "p".into(),
                SessionEvent::Message(RpcMessage {
                    id: Some(42),
                    method: "io".into(),
                    params: json!({})
                })
            ),
            PushOutcome::Rejected
        );
        assert_eq!(inbox.pop_rejected().unwrap().id, 42);
        assert_eq!(
            inbox.push("p".into(), SessionEvent::Exit { code: None }),
            PushOutcome::Queued
        );
        assert_eq!(inbox.push("q".into(), register()), PushOutcome::Queued);
    }

    #[test]
    fn missing_target_is_not_coalesced() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), update(None, "a", None));
        inbox.push("p".into(), update(None, "b", None));
        assert_eq!(inbox.len(), 2);
    }
}
