//! The push-to-phone publisher worker (host process).
//!
//! [`crate::notify::NotifyState::emit_push`] hands a [`PushJob`] to this
//! dedicated OS thread over a bounded channel; the thread owns a current-thread
//! tokio runtime and drives the async `PushProvider` seam
//! (`thegn_svc::push`). Best-effort by contract: a bounded queue
//! (drop-on-overflow with a counter on the sender side), the provider's own
//! ≤2-retry, and never a block on the event loop.
//!
//! It is wired at startup (`run.rs`) **only when `[notifications.push]` is
//! configured**; otherwise no worker exists and `emit_push` is a silent no-op.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use thegn_core::config::{PushConfig, PushKind};
use thegn_core::notification::Priority;
use thegn_core::notification_render::RenderedNotification;
use thegn_core::seam::{ErrorClass, SeamError};
use thegn_svc::push::{PushMessage, PushProvider};

use crate::notification_delivery::{DeliveryEvent, DeliverySnapshot};

/// A queued push, built at the emit site ([`crate::notify::NotifyState::emit_push`]).
pub struct PushJob {
    /// The configured sink name, never an endpoint.
    pub sink: String,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub kind: String,
    pub worktree: String,
    /// Provider-neutral rendering for webhook sinks. `None` is retained only
    /// for old unit-test/ntfy construction; routed jobs always carry it.
    pub rendered: Option<RenderedNotification>,
}

/// The bounded queue depth. Small on purpose: push is best-effort and a phone
/// needs no backlog; overflow drops (counted) rather than growing memory behind
/// a stalled server.
pub const QUEUE_DEPTH: usize = 64;

type Providers = BTreeMap<String, Box<dyn PushProvider>>;
type DeliveryRows = Vec<(String, String)>;

/// Spawn the publisher worker on a dedicated `Background`-QoS thread. Consumes
/// `rx` until the sender is dropped.
pub fn spawn(
    rx: Receiver<PushJob>,
    providers: BTreeMap<String, Box<dyn PushProvider>>,
    snapshot: DeliverySnapshot,
) {
    std::thread::Builder::new()
        .name("notify-push".into())
        .spawn(move || {
            // Housekeeping, not interactive: keep it off the perf cores (no-op
            // off Apple silicon).
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    // Drain so the bounded sender never wedges the emit site.
                    while rx.recv().is_ok() {}
                    return;
                }
            };
            while let Ok(job) = rx.recv() {
                let sink = job.sink.clone();
                let rendered = job.rendered.clone();
                let msg = to_message(job);
                let Some(provider) = providers.get(&sink) else {
                    snapshot.event(&sink, DeliveryEvent::DeadLetter);
                    continue;
                };
                // The provider owns its bounded retry; block this worker thread
                // (never the event loop) on the result.
                let result = if provider.kind() == PushKind::Ntfy {
                    rt.block_on(provider.publish(&msg))
                } else if let Some(notification) = rendered.as_ref() {
                    rt.block_on(provider.publish_rendered(notification))
                } else {
                    Err(thegn_svc::push::PushError::Other(
                        "rendered notification missing".into(),
                    ))
                };
                if let Err(e) = result {
                    match e.class() {
                        ErrorClass::RateLimited => {
                            snapshot.event(&sink, DeliveryEvent::RateLimitDrop)
                        }
                        ErrorClass::Transient => snapshot.event(&sink, DeliveryEvent::Retry),
                        _ => {}
                    }
                    snapshot.event(&sink, DeliveryEvent::DeadLetter);
                    tracing::debug!(target: "thegn::push", error = %e, "push delivery failed");
                } else {
                    snapshot.event(&sink, DeliveryEvent::Sent);
                }
            }
            // best-effort: push worker: a failed spawn just disables push this session; delivery failures are already logged below
        })
        .ok();
}

/// Build one provider per effective sink.  Keeping the scalar factory call
/// here preserves the existing ntfy/inbox service API while allowing the
/// service seam to grow named providers independently.
pub(crate) fn providers_for(cfg: &PushConfig) -> (Providers, DeliveryRows) {
    let sinks = cfg.effective_sinks();
    let mut providers = BTreeMap::new();
    let mut rows = Vec::new();
    for sink in sinks {
        if !sink.is_configured() {
            continue;
        }
        rows.push((sink.name.clone(), sink.kind.as_str().to_string()));
        if let Some(provider) = thegn_svc::push::provider_for_sink(&sink) {
            providers.insert(sink.name, provider);
        }
    }
    (providers, rows)
}

/// Shape a job into a provider message: the notification text is the title, the
/// worktree basename rides the body as context, the kind becomes a tag.
fn to_message(job: PushJob) -> PushMessage {
    let mut body = job.body;
    if !job.worktree.is_empty() {
        let base = std::path::Path::new(&job.worktree)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.worktree.clone());
        body = if body.is_empty() {
            base
        } else {
            format!("{body}\n{base}")
        };
    }
    PushMessage {
        title: job.title,
        body,
        priority: job.priority,
        tags: vec![job.kind],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_carries_worktree_basename_and_kind_tag() {
        let m = to_message(PushJob {
            sink: "phone".into(),
            title: "tests failed".into(),
            body: String::new(),
            priority: Priority::Alert,
            kind: "test_failed".into(),
            worktree: "/home/u/code/app".into(),
            rendered: None,
        });
        assert_eq!(m.title, "tests failed");
        assert_eq!(m.body, "app", "worktree basename as context");
        assert_eq!(m.tags, vec!["test_failed".to_string()]);
        assert_eq!(m.priority, Priority::Alert);
    }

    #[test]
    fn message_without_worktree_has_empty_body() {
        let m = to_message(PushJob {
            sink: "phone".into(),
            title: "hi".into(),
            body: String::new(),
            priority: Priority::Notice,
            kind: "info".into(),
            worktree: String::new(),
            rendered: None,
        });
        assert!(m.body.is_empty());
    }
}
