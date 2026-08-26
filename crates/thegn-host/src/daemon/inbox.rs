//! The push command inbox — the daemon-hosted inbound command surface.
//!
//! **Hard-off by default.** When `[notifications.push.inbox] enabled = true`
//! (and only then), the daemon subscribes to the configured command topic,
//! feeds each raw message through the pure `thegn_core::push_inbox::evaluate`
//! (HMAC → freshness → replay → `allow ∩ scopes ∩ unconditional-admin-deny`),
//! and runs an accepted envelope through the **same** control-API dispatch
//! (`thegn_svc::control::http::dispatch_local`) — one capability catalog, never
//! a second policy table, never a shell.
//!
//! The subscriber survives UI detach because it lives here (the daemon), not in
//! the compositor: a phone command must not require an attached UI. When
//! `[daemon] enabled = false` there is no daemon, so there is no inbox — doctor
//! says so.
//!
//! Security posture (see the change's design + `thegn_core::push_inbox`):
//! - The secret is a SecretRef, resolved once at start; a shape/resolution
//!   failure refuses to start the inbox and names the fix.
//! - Every message is MAC-verified before the replay cache is touched.
//! - A per-minute execution cap bounds command throughput even for valid
//!   messages; replies are truncated so a listing can't exfiltrate unbounded
//!   state.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use thegn_core::config::Config;
use thegn_core::push_inbox::{self, Counters, Outcome, RateLimiter, ReplayGuard};
use thegn_core::store::ControlStore;
use thegn_svc::control::PushedNote;
use thegn_svc::control::http::{ControlState, dispatch_local};
use thegn_svc::control::routes::build_call;
use thegn_svc::push::{PushMessage, PushProvider, subscriber::NtfySubscriber};

use super::service::{DaemonService, SharedDb};

/// Commands actually executed per minute (excess valid commands are dropped
/// with a counter — DoS bound on a stolen topic+secret).
const MAX_EXEC_PER_MINUTE: u32 = 30;
/// Backlog of raw messages buffered between the subscriber and the executor.
const RAW_BUFFER: usize = 256;

/// Start the command inbox if (and only if) it is enabled and validly
/// configured. A no-op — with a logged reason — otherwise. Spawns two daemon
/// tasks (the SSE-style subscriber and the executor) that stop on `shutdown`.
pub fn spawn(
    cfg: &Config,
    svc: Arc<DaemonService>,
    db: SharedDb,
    shutdown: Arc<Notify>,
    server_label: String,
) {
    let push = &cfg.notifications.push;
    let inbox = &push.inbox;
    // Hard-off default: absent or disabled ⇒ no subscription exists at all.
    if !inbox.enabled {
        return;
    }
    // Enabling demands a SecretRef secret, a non-empty allow list, and known
    // non-admin capabilities. A failure refuses to start and names the fix.
    if let Some(reason) = inbox.startup_block_reason() {
        tracing::error!(target: "thegn::push", "push command inbox NOT started: {reason}");
        return;
    }
    let Some(secret) = inbox.resolved_secret() else {
        tracing::error!(
            target: "thegn::push",
            "push command inbox NOT started: inbox_secret did not resolve"
        );
        return;
    };
    let allow = inbox.allow_set();
    let ceiling = inbox.ceiling();
    let allow_count = allow.len();

    // The inbox rides the same server (and token) as outbound push; its topic is
    // separate.
    let subscriber = NtfySubscriber::new(&push.server, &inbox.topic, push.resolved_token());

    // Optional reply publisher, pointed at the reply topic.
    let reply_provider: Option<Box<dyn PushProvider>> = if inbox.reply_topic.trim().is_empty() {
        None
    } else {
        let mut rc = push.clone();
        rc.topic = inbox.reply_topic.clone();
        thegn_svc::push::provider_for(&rc)
    };

    let state = ControlState {
        api: svc.clone(),
        store: db.clone() as Arc<Mutex<dyn ControlStore + Send>>,
        // The inbox IS the authenticator: HMAC + freshness + replay + the
        // `allow ∩ scopes ∩ admin-deny` admission all ran in `evaluate` before a
        // message reaches dispatch. `local_admin` therefore only satisfies the
        // in-process handler's transport-auth — it grants no policy the
        // admission didn't already permit.
        local_admin: true,
        require_approval: false,
        server_label,
    };

    let (raw_tx, raw_rx) = mpsc::channel::<String>(RAW_BUFFER);
    let sub_shutdown = shutdown.clone();
    tokio::spawn(async move {
        subscriber.run(raw_tx, sub_shutdown).await;
    });
    tokio::spawn(execute_loop(
        raw_rx,
        secret,
        allow,
        ceiling,
        state,
        reply_provider,
        shutdown,
    ));

    tracing::info!(
        target: "thegn::push",
        topic = %inbox.topic,
        allowed = allow_count,
        "push command inbox listening"
    );
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn execute_loop(
    mut rx: mpsc::Receiver<String>,
    secret: String,
    allow: std::collections::BTreeSet<String>,
    ceiling: thegn_core::control::ScopeSet,
    state: ControlState,
    reply: Option<Box<dyn PushProvider>>,
    shutdown: Arc<Notify>,
) {
    let mut replay = ReplayGuard::default_guard();
    let mut counters = Counters::default();
    let mut limiter = RateLimiter::per_minute(MAX_EXEC_PER_MINUTE);

    loop {
        let raw = tokio::select! {
            _ = shutdown.notified() => break,
            msg = rx.recv() => match msg {
                Some(m) => m,
                None => break, // subscriber gone
            },
        };
        let now = now_secs();
        let outcome =
            push_inbox::evaluate(&raw, secret.as_bytes(), &allow, ceiling, now, &mut replay);
        counters.record(&outcome);

        let acc = match outcome {
            Outcome::Accepted(a) => a,
            Outcome::Rejected(reason) => {
                tracing::warn!(
                    target: "thegn::push",
                    reason = reason.as_str(),
                    "inbox message rejected"
                );
                continue;
            }
        };

        // Per-minute execution cap: bound throughput even for valid commands.
        if !limiter.allow(now) {
            counters.rate_limited += 1;
            tracing::warn!(target: "thegn::push", cap = %acc.cap, "inbox execution rate cap hit — dropped");
            continue;
        }

        // Dispatch through the SAME control router the API serves. The catalog
        // id → (method, path, body) mapping is the shared `build_call` spine.
        let params = acc.params.as_object().cloned().unwrap_or_default();
        let (method, path, body) = match build_call(&acc.cap, params) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(target: "thegn::push", cap = %acc.cap, error = %e, "inbox dispatch build failed");
                continue;
            }
        };
        let (status, result) = dispatch_local(state.clone(), method, &path, body).await;
        tracing::info!(
            target: "thegn::push",
            cap = %acc.cap,
            status = status.as_u16(),
            "inbox command executed"
        );

        // Audit: surface the execution in the notification inbox (best-effort),
        // so a phone-initiated command is visible on the desktop too.
        record_audit(&state, &acc.cap, status.is_success()).await;

        // Truncated reply, if a reply topic is configured.
        if let Some(rp) = &reply {
            let reply_body = if status.is_success() {
                push_inbox::build_reply(&acc.id, Ok(&result), push_inbox::REPLY_CAP_BYTES)
            } else {
                let err = result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("command failed")
                    .to_string();
                push_inbox::build_reply(&acc.id, Err(&err), push_inbox::REPLY_CAP_BYTES)
            };
            publish_reply(rp.as_ref(), reply_body).await;
        }
    }

    tracing::info!(
        target: "thegn::push",
        accepted = counters.accepted,
        rejected = counters.rejected_total(),
        rate_limited = counters.rate_limited,
        "push command inbox stopped"
    );
}

/// Record a phone-initiated command in the notification inbox for auditability.
/// Best-effort: a failure here must never disrupt the inbox.
async fn record_audit(state: &ControlState, cap: &str, ok: bool) {
    let note = PushedNote {
        title: format!(
            "phone command: {cap} ({})",
            if ok { "ok" } else { "failed" }
        ),
        body: String::new(),
        urgency: None,
        source: Some("push_inbox".into()),
    };
    // best-effort: the audit row is a nicety, not the command's success path.
    let _ = state.api.notify_push(note).await;
}

/// Publish a (already-truncated) reply body to the reply topic. Best-effort.
async fn publish_reply(provider: &dyn PushProvider, body: String) {
    let msg = PushMessage {
        title: "thegn reply".into(),
        body,
        priority: thegn_core::notification::Priority::Notice,
        tags: vec!["reply".into()],
    };
    // best-effort: a failed reply publish never affects command execution.
    let _ = provider.publish(&msg).await;
}
