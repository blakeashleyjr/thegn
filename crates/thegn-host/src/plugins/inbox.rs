use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use thegn_core::plugin_api::RpcMessage;
use thegn_svc::plugin::SessionEvent;

use super::SessionGen;

pub(crate) const MAX_QUEUED_EVENTS_PER_PLUGIN: usize = 1024;
pub(crate) const MAX_QUEUED_EVENTS_TOTAL: usize = 8192;
pub(crate) const MAX_QUEUED_BYTES_PER_PLUGIN: usize = 8 << 20;
pub(crate) const MAX_QUEUED_BYTES_TOTAL: usize = 32 << 20;

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
    pub session: SessionGen,
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CoalesceKey {
    plugin: String,
    session: SessionGen,
    method: String,
    target: String,
}

struct QueuedEvent {
    plugin: String,
    session: SessionGen,
    event: SessionEvent,
    sequence: u64,
    key: Option<CoalesceKey>,
    bytes: usize,
}

#[derive(Default)]
struct InboxState {
    queue: VecDeque<QueuedEvent>,
    plugin_counts: HashMap<String, usize>,
    plugin_bytes: HashMap<String, usize>,
    total_bytes: usize,
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
    pub(crate) fn push(
        &self,
        plugin: String,
        session: SessionGen,
        event: SessionEvent,
    ) -> PushOutcome {
        self.push_with_wake(plugin, session, event).0
    }

    pub(crate) fn push_with_wake(
        &self,
        plugin: String,
        session: SessionGen,
        event: SessionEvent,
    ) -> (PushOutcome, bool) {
        let mut state = lock(&self.state);
        let was_empty = state.plugin_counts.get(&plugin).copied().unwrap_or(0) == 0;
        let key = coalesce_key(&plugin, session, &event);
        let bytes = event_bytes(&event);
        let mut replaced = false;
        let mut replacement = None;
        if let Some(key) = &key
            && let Some(sequence) = state.coalesced.get(key).copied()
            && let Some(position) = state
                .queue
                .iter()
                .position(|queued| queued.sequence == sequence)
        {
            let barrier = state.queue.iter().skip(position + 1).any(|queued| {
                queued.plugin == plugin && queued.session == session && is_barrier(&queued.event)
            });
            if !barrier {
                replacement = Some(position);
                replaced = true;
            }
        }

        let is_exit = matches!(event, SessionEvent::Exit { .. });
        let replaced_bytes = replacement.map_or(0, |position| state.queue[position].bytes);
        let plugin_count = state.plugin_counts.get(&plugin).copied().unwrap_or(0)
            - usize::from(replacement.is_some());
        let total_count = state.queue.len() - usize::from(replacement.is_some());
        let plugin_bytes = state.plugin_bytes.get(&plugin).copied().unwrap_or(0);
        let projected_plugin_bytes = plugin_bytes - replaced_bytes;
        let projected_total_bytes = state.total_bytes - replaced_bytes;
        let at_cap = plugin_count >= MAX_QUEUED_EVENTS_PER_PLUGIN
            || total_count >= MAX_QUEUED_EVENTS_TOTAL
            || bytes > MAX_QUEUED_BYTES_PER_PLUGIN.saturating_sub(projected_plugin_bytes)
            || bytes > MAX_QUEUED_BYTES_TOTAL.saturating_sub(projected_total_bytes);
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
                        session,
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

        if let Some(position) = replacement {
            let queued = state.queue.remove(position).expect("replacement position");
            remove_accounting(&mut state, &queued);
            bump(&mut state.diagnostics, &plugin, |counts| {
                counts.coalesced += 1
            });
        }

        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        if let Some(key) = key.clone() {
            state.coalesced.insert(key.clone(), sequence);
        }
        state.queue.push_back(QueuedEvent {
            plugin: plugin.clone(),
            session,
            event,
            sequence,
            key,
            bytes,
        });
        *state.plugin_counts.entry(plugin.clone()).or_default() += 1;
        *state.plugin_bytes.entry(plugin.clone()).or_default() += bytes;
        state.total_bytes += bytes;
        (
            if replaced {
                PushOutcome::Coalesced
            } else {
                PushOutcome::Queued
            },
            is_exit || was_empty,
        )
    }

    pub(crate) fn pop(&self) -> Option<(String, SessionGen, SessionEvent)> {
        let mut state = lock(&self.state);
        let queued = state.queue.pop_front()?;
        remove_accounting(&mut state, &queued);
        Some((queued.plugin, queued.session, queued.event))
    }

    /// Exits participate in session lifecycle ordering even though ordinary
    /// session events use the bounded inbox. Drain them before lifecycle
    /// control messages such as `Respawned` from the separate channel.
    pub(crate) fn pop_exit(&self) -> Option<(String, SessionGen, SessionEvent)> {
        let mut state = lock(&self.state);
        let position = state
            .queue
            .iter()
            .position(|queued| matches!(&queued.event, SessionEvent::Exit { .. }))?;
        let queued = state.queue.remove(position)?;
        remove_accounting(&mut state, &queued);
        Some((queued.plugin, queued.session, queued.event))
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

fn coalesce_key(plugin: &str, session: SessionGen, event: &SessionEvent) -> Option<CoalesceKey> {
    let SessionEvent::Message(message) = event else {
        return None;
    };
    if message.id.is_some() {
        return None;
    }
    if message.method == "update" && has_focus(&message.params) {
        return None;
    }
    let target = match message.method.as_str() {
        "update" | "invalidate" => message.params.get("surface")?.as_str()?,
        "state.set" => message.params.get("key")?.as_str()?,
        _ => return None,
    };
    Some(CoalesceKey {
        plugin: plugin.to_string(),
        session,
        method: message.method.clone(),
        target: target.to_string(),
    })
}

fn has_focus(params: &serde_json::Value) -> bool {
    params.get("focus").and_then(serde_json::Value::as_bool) == Some(true)
        || params
            .get("focus_ticket")
            .is_some_and(|ticket| !ticket.is_null())
}

fn is_barrier(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::Message(message)
        if message.id.is_some() || (message.method == "update" && has_focus(&message.params)))
}

fn event_bytes(event: &SessionEvent) -> usize {
    match event {
        SessionEvent::Message(message) => {
            serde_json::to_vec(message).map_or(usize::MAX, |v| v.len())
        }
        SessionEvent::Response(response) => {
            serde_json::to_vec(response).map_or(usize::MAX, |v| v.len())
        }
        SessionEvent::Junk(line) => line.len(),
        SessionEvent::Exit { .. } => 0,
    }
}

fn remove_accounting(state: &mut InboxState, queued: &QueuedEvent) {
    decrement(&mut state.plugin_counts, &queued.plugin);
    let remove_plugin_bytes = if let Some(bytes) = state.plugin_bytes.get_mut(&queued.plugin) {
        *bytes -= queued.bytes;
        *bytes == 0
    } else {
        false
    };
    if remove_plugin_bytes {
        state.plugin_bytes.remove(&queued.plugin);
    }
    state.total_bytes -= queued.bytes;
    if let Some(key) = &queued.key
        && state.coalesced.get(key) == Some(&queued.sequence)
    {
        state.coalesced.remove(key);
    }
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

    fn focused_update(ticket: Option<serde_json::Value>) -> SessionEvent {
        let mut params = json!({"surface":"s", "view":{}});
        if let Some(ticket) = ticket {
            params["focus_ticket"] = ticket;
        } else {
            params["focus"] = json!(true);
        }
        SessionEvent::Message(RpcMessage {
            id: None,
            method: "update".into(),
            params,
        })
    }

    #[test]
    fn update_flood_coalesces_to_latest_per_surface() {
        let inbox = PluginInbox::default();
        for index in 0..10_000 {
            inbox.push(
                "p".into(),
                1,
                update(Some("seg"), &format!("seg-{index}"), None),
            );
            inbox.push(
                "p".into(),
                1,
                update(Some("other"), &format!("other-{index}"), None),
            );
        }
        assert_eq!(inbox.len(), 2);
        let (_, _, first) = inbox.pop().unwrap();
        let (_, _, second) = inbox.pop().unwrap();
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
        inbox.push("p".into(), 1, update(Some("s1"), "v1", None));
        inbox.push("p".into(), 1, register());
        inbox.push("p".into(), 1, update(Some("s1"), "v2", None));
        assert!(
            matches!(inbox.pop().unwrap().2, SessionEvent::Message(RpcMessage { method, .. }) if method == "register")
        );
        assert!(
            matches!(inbox.pop().unwrap().2, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "v2")
        );
    }

    #[test]
    fn requests_are_never_coalesced() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, update(Some("seg"), "v1", Some(1)));
        inbox.push("p".into(), 1, update(Some("seg"), "v2", Some(2)));
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn cap_drops_notifications_and_rejects_requests_but_keeps_exit() {
        let inbox = PluginInbox::default();
        for _ in 0..MAX_QUEUED_EVENTS_PER_PLUGIN {
            inbox.push(
                "p".into(),
                1,
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
                1,
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
                1,
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
            inbox.push("p".into(), 1, SessionEvent::Exit { code: None }),
            PushOutcome::Queued
        );
        assert_eq!(inbox.push("q".into(), 1, register()), PushOutcome::Queued);
    }

    #[test]
    fn missing_target_is_not_coalesced() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, update(None, "a", None));
        inbox.push("p".into(), 1, update(None, "b", None));
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn session_coalescing_never_merges_two_sessions() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, update(Some("seg"), "old", None));
        inbox.push("p".into(), 2, update(Some("seg"), "new", None));

        assert_eq!(inbox.len(), 2);
        let (first_plugin, first_session, first_event) = inbox.pop().unwrap();
        let (second_plugin, second_session, second_event) = inbox.pop().unwrap();
        assert_eq!((first_plugin.as_str(), first_session), ("p", 1));
        assert_eq!((second_plugin.as_str(), second_session), ("p", 2));
        assert!(
            matches!(first_event, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "old")
        );
        assert!(
            matches!(second_event, SessionEvent::Message(message) if message.params["view"]["spans"][0]["text"] == "new")
        );
    }

    #[test]
    fn session_is_returned_by_pop_and_pop_exit() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 7, register());
        inbox.push("p".into(), 9, SessionEvent::Exit { code: Some(3) });

        let (plugin, session, event) = inbox.pop_exit().unwrap();
        assert_eq!((plugin.as_str(), session), ("p", 9));
        assert!(matches!(event, SessionEvent::Exit { code: Some(3) }));
        let (plugin, session, event) = inbox.pop().unwrap();
        assert_eq!((plugin.as_str(), session), ("p", 7));
        assert!(
            matches!(event, SessionEvent::Message(RpcMessage { method, .. }) if method == "register")
        );
    }

    #[test]
    fn focus_update_is_never_coalesced() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, focused_update(None));
        inbox.push("p".into(), 1, focused_update(Some(json!("ticket-1"))));
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn update_does_not_coalesce_across_a_request_barrier() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, update(Some("s"), "before", None));
        inbox.push("p".into(), 1, update(Some("other"), "request", Some(7)));
        inbox.push("p".into(), 1, update(Some("s"), "after", None));
        assert_eq!(inbox.len(), 3);
        assert!(
            matches!(inbox.pop().unwrap().2, SessionEvent::Message(message)
            if message.params["view"]["spans"][0]["text"] == "before")
        );
        assert!(
            matches!(inbox.pop().unwrap().2, SessionEvent::Message(message)
            if message.id == Some(7))
        );
        assert!(
            matches!(inbox.pop().unwrap().2, SessionEvent::Message(message)
            if message.params["view"]["spans"][0]["text"] == "after")
        );
    }

    #[test]
    fn update_does_not_coalesce_across_a_focus_barrier() {
        let inbox = PluginInbox::default();
        inbox.push("p".into(), 1, update(Some("s"), "before", None));
        inbox.push("p".into(), 1, focused_update(Some(json!("focus-1"))));
        inbox.push("p".into(), 1, update(Some("s"), "after", None));
        assert_eq!(inbox.len(), 3);
    }

    #[test]
    fn byte_cap_drops_notifications_and_rejects_requests_but_keeps_exit() {
        let inbox = PluginInbox::default();
        let payload = "x".repeat(1_000_000);
        for index in 0..8 {
            assert_eq!(
                inbox.push(
                    "p".into(),
                    1,
                    update(Some(&format!("s-{index}")), &payload, None)
                ),
                PushOutcome::Queued
            );
        }
        assert_eq!(
            inbox.push("p".into(), 1, update(Some("other"), &payload, None)),
            PushOutcome::Dropped
        );
        assert_eq!(
            inbox.push("p".into(), 1, update(Some("request"), &payload, Some(9))),
            PushOutcome::Rejected
        );
        assert_eq!(inbox.pop_rejected().unwrap().id, 9);
        assert_eq!(
            inbox.push("p".into(), 1, SessionEvent::Exit { code: None }),
            PushOutcome::Queued
        );
    }

    #[test]
    fn pop_releases_queued_bytes() {
        let inbox = PluginInbox::default();
        let event = update(Some("s"), "bytes", None);
        let expected = event_bytes(&event);
        inbox.push("p".into(), 1, event);
        {
            let state = lock(&inbox.state);
            assert_eq!(state.total_bytes, expected);
            assert_eq!(state.plugin_bytes.get("p"), Some(&expected));
        }
        inbox.pop().unwrap();
        let state = lock(&inbox.state);
        assert_eq!(state.total_bytes, 0);
        assert!(!state.plugin_bytes.contains_key("p"));
    }
}
