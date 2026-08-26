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

use std::sync::mpsc::Receiver;

use thegn_core::notification::Priority;
use thegn_svc::push::{PushMessage, PushProvider};

/// A queued push, built at the emit site ([`crate::notify::NotifyState::emit_push`]).
pub struct PushJob {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub kind: String,
    pub worktree: String,
}

/// The bounded queue depth. Small on purpose: push is best-effort and a phone
/// needs no backlog; overflow drops (counted) rather than growing memory behind
/// a stalled server.
pub const QUEUE_DEPTH: usize = 64;

/// Spawn the publisher worker on a dedicated `Background`-QoS thread. Consumes
/// `rx` until the sender is dropped.
pub fn spawn(rx: Receiver<PushJob>, provider: Box<dyn PushProvider>) {
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
                let msg = to_message(job);
                // The provider owns its bounded retry; block this worker thread
                // (never the event loop) on the result.
                if let Err(e) = rt.block_on(provider.publish(&msg)) {
                    tracing::debug!(target: "thegn::push", error = %e, "push delivery failed");
                }
            }
        })
        .ok();
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
            title: "tests failed".into(),
            body: String::new(),
            priority: Priority::Alert,
            kind: "test_failed".into(),
            worktree: "/home/u/code/app".into(),
        });
        assert_eq!(m.title, "tests failed");
        assert_eq!(m.body, "app", "worktree basename as context");
        assert_eq!(m.tags, vec!["test_failed".to_string()]);
        assert_eq!(m.priority, Priority::Alert);
    }

    #[test]
    fn message_without_worktree_has_empty_body() {
        let m = to_message(PushJob {
            title: "hi".into(),
            body: String::new(),
            priority: Priority::Notice,
            kind: "info".into(),
            worktree: String::new(),
        });
        assert!(m.body.is_empty());
    }
}
