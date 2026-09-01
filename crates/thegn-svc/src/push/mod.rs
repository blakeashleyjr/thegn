//! The push-provider seam — outbound delivery for the push-to-phone bridge.
//!
//! An object-safe async seam (`BoxFuture`, like the calendar/issue seams): ntfy
//! and the three webhook publishers are implemented, while
//! (`telegram`/`gotify`/`pushover`) remain reserved. The inbound half (the daemon's command subscription)
//! lives in [`subscriber`]; the router decision that authorises a push is the
//! pure `thegn_core::notification_route`, not here.
//!
//! Request/response shaping is split into **pure** functions (`publish_url`,
//! priority/tag mapping) so they unit-test without a live endpoint; the async
//! trait method is a thin reqwest wrapper with a bounded retry.

pub mod discord;
pub mod ntfy;
pub mod rate_limit;
pub mod slack;
pub mod subscriber;
pub mod webhook;

use std::time::Duration;

use thegn_core::config::{PushConfig, PushKind};
use thegn_core::config_push::PushSinkConfig;
use thegn_core::notification::Priority;
use thegn_core::notification_render::{MarkdownFlavor, RenderedNotification};
use thegn_core::seam::{Availability, BoxFuture, ErrorClass, ProbeReport, SeamError};

/// Provider capabilities exposed by `thegn doctor` and the host worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PushCaps {
    pub payload_limit: usize,
    pub markdown_flavor: &'static str,
    pub priority_colors: bool,
    pub dry_run: bool,
}

impl PushCaps {
    pub const fn new(
        payload_limit: usize,
        markdown_flavor: &'static str,
        priority_colors: bool,
    ) -> Self {
        Self {
            payload_limit,
            markdown_flavor,
            priority_colors,
            dry_run: true,
        }
    }

    pub const fn ntfy() -> Self {
        Self::new(0, "plain", false)
    }
}

/// A message to deliver to the phone. `priority` is the notification's effective
/// priority (the provider maps it to its own scale); `tags` become provider
/// hints (ntfy renders them as emoji/labels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushMessage {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub tags: Vec<String>,
}

/// A push delivery failure, classified for the seam vocabulary.
#[derive(Debug)]
pub enum PushError {
    /// The seam has no implementation for this op / kind.
    Unsupported(&'static str),
    /// No server/topic configured.
    NotConfigured,
    /// Credentials were rejected (401/403).
    Auth(String),
    /// Connect/timeout/5xx — a retry may succeed.
    Transient(String),
    /// The upstream asked us to retry after a bounded delay.
    RateLimited(Duration),
    /// Anything else (4xx, malformed URL, …).
    Other(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Unsupported(op) => write!(f, "push provider does not support {op}"),
            PushError::NotConfigured => write!(f, "push channel is not configured"),
            PushError::Auth(m) => write!(f, "push auth rejected: {m}"),
            PushError::Transient(m) => write!(f, "push transient failure: {m}"),
            PushError::RateLimited(delay) => {
                write!(f, "push rate limited; retry after {}s", delay.as_secs())
            }
            PushError::Other(m) => write!(f, "push failed: {m}"),
        }
    }
}

impl std::error::Error for PushError {}

impl SeamError for PushError {
    fn class(&self) -> ErrorClass {
        match self {
            PushError::Unsupported(_) => ErrorClass::Unsupported,
            PushError::NotConfigured => ErrorClass::NotConfigured,
            PushError::Auth(_) => ErrorClass::Auth,
            PushError::Transient(_) => ErrorClass::Transient,
            PushError::RateLimited(_) => ErrorClass::RateLimited,
            PushError::Other(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        PushError::Unsupported(op)
    }
}

/// A substitutable push backend. Object-safe: `publish` returns a [`BoxFuture`]
/// so a `Box<dyn PushProvider>` works and the trait never uses native
/// `async fn` (the provider-trait ratchet).
pub trait PushProvider: Send + Sync {
    /// The config kind this provider implements.
    fn kind(&self) -> PushKind;
    /// Deliver one message (best-effort; the impl owns its own bounded retry).
    fn publish<'a>(&'a self, msg: &'a PushMessage) -> BoxFuture<'a, Result<(), PushError>>;
    /// Deliver the provider-neutral rendered notification. The default keeps
    /// the old command-inbox callers source-compatible; webhook providers
    /// override it so their envelopes retain all generic event fields.
    fn publish_rendered<'a>(
        &'a self,
        notification: &'a RenderedNotification,
    ) -> BoxFuture<'a, Result<(), PushError>> {
        let msg = PushMessage {
            title: notification.title.clone(),
            body: notification.message.clone(),
            priority: notification.priority,
            tags: vec![notification.kind.as_str().to_string()],
        };
        Box::pin(async move { self.publish(&msg).await })
    }
    /// Provider caps are synchronous and safe to show in doctor.
    fn caps(&self) -> PushCaps {
        PushCaps::ntfy()
    }
    /// Describe this provider for `thegn doctor` (synchronous, no network).
    fn probe(&self) -> ProbeReport;
}

/// Build the configured push provider, or `None` when push is unconfigured or
/// the selected kind is reserved (unimplemented in this build).
pub fn provider_for(cfg: &PushConfig) -> Option<Box<dyn PushProvider>> {
    cfg.effective_sinks().iter().find_map(provider_for_sink)
}

/// Build one already-validated named sink. Secret resolution happens here,
/// before the provider enters the worker, and an unresolved bearer URL simply
/// leaves that sink unavailable.
pub fn provider_for_sink(cfg: &PushSinkConfig) -> Option<Box<dyn PushProvider>> {
    if !cfg.is_configured() {
        return None;
    }
    match cfg.kind {
        PushKind::Ntfy => {
            let legacy = PushConfig {
                kind: cfg.kind,
                server: cfg.server.clone(),
                topic: cfg.topic.clone(),
                token: cfg.token.clone(),
                min_priority: cfg.min_priority.clone(),
                sinks: Vec::new(),
                inbox: Default::default(),
            };
            Some(Box::new(ntfy::NtfyPublisher::new(&legacy)))
        }
        PushKind::Webhook => webhook::WebhookPublisher::new(cfg)
            .ok()
            .map(|p| Box::new(p) as Box<dyn PushProvider>),
        PushKind::Discord => discord::DiscordPublisher::new(cfg)
            .ok()
            .map(|p| Box::new(p) as Box<dyn PushProvider>),
        PushKind::Slack => slack::SlackPublisher::new(cfg)
            .ok()
            .map(|p| Box::new(p) as Box<dyn PushProvider>),
        PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => None,
    }
}

/// Build the redacted offline report for one effective sink. This deliberately
/// does not call the provider constructor: a missing secret must be reported
/// distinctly from a reserved kind, without ever placing the endpoint in a
/// report or error.
pub fn probe_sink(cfg: &PushSinkConfig) -> ProbeReport {
    use thegn_core::seam::Kind;

    if cfg.kind.is_reserved() {
        return ProbeReport::new(
            "push",
            cfg.name.trim(),
            Availability::Unavailable(format!(
                "{} is reserved: accepted by config but not implemented in this build",
                cfg.kind.as_str()
            )),
        )
        .note(format!("kind: {}", cfg.kind.as_str()));
    }
    let (secret, url_ok) = match cfg.kind {
        PushKind::Ntfy => (
            true,
            !cfg.server.trim().is_empty() && !cfg.topic.trim().is_empty(),
        ),
        PushKind::Webhook | PushKind::Discord | PushKind::Slack => {
            let resolved = secret_ref_state(&cfg.url)
                .as_ref()
                .and_then(|s| thegn_core::config::expand_env_ref(s));
            let secret_present = resolved.is_some();
            let endpoint_valid = resolved.as_deref().is_some_and(valid_endpoint);
            (secret_present, endpoint_valid)
        }
        PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => (false, false),
    };
    let avail = if !secret || !url_ok {
        Availability::Unavailable(if !secret {
            "secret: missing".into()
        } else {
            "endpoint: unavailable or invalid".into()
        })
    } else {
        Availability::Ready
    };
    let flavor = flavor_for(cfg.kind);
    ProbeReport::new("push", cfg.name.trim(), avail)
        .with_caps(&caps_for(cfg.kind))
        .note(format!("kind: {}", cfg.kind.as_str()))
        .note(format!(
            "secret: {}",
            if secret { "resolved" } else { "missing" }
        ))
        .note(format!("markdown: {flavor}"))
        .note("dry-run: POST shape ready")
        .note("offline probe: no network request")
}

pub(crate) fn caps_for(kind: PushKind) -> PushCaps {
    match kind {
        PushKind::Ntfy => PushCaps::ntfy(),
        PushKind::Webhook => PushCaps::new(0, "common_mark", false),
        PushKind::Discord => PushCaps::new(2_000, "discord", true),
        PushKind::Slack => PushCaps::new(40_000, "slack", true),
        PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => {
            PushCaps::new(0, "plain", false)
        }
    }
}

pub(crate) fn flavor_for(kind: PushKind) -> &'static str {
    caps_for(kind).markdown_flavor
}

pub(crate) fn render_flavor(kind: PushKind) -> MarkdownFlavor {
    match kind {
        PushKind::Webhook => MarkdownFlavor::CommonMark,
        PushKind::Discord => MarkdownFlavor::Discord,
        PushKind::Slack => MarkdownFlavor::Slack,
        PushKind::Ntfy | PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => {
            MarkdownFlavor::Plain
        }
    }
}

/// Resolve an env/file-only endpoint and validate its scheme. The returned URL
/// is only held privately by a provider; all failure text is value-free.
pub(crate) fn resolve_endpoint(reference: &str) -> Result<String, PushError> {
    let Some(reference) = secret_ref_state(reference) else {
        return Err(PushError::Other(
            "webhook endpoint must be an env: or file: SecretRef".into(),
        ));
    };
    let Some(url) = thegn_core::config::expand_env_ref(&reference) else {
        return Err(PushError::NotConfigured);
    };
    if !valid_endpoint(&url) {
        return Err(PushError::Other(
            "webhook endpoint is not a valid HTTP URL".into(),
        ));
    }
    Ok(url)
}

fn secret_ref_state(reference: &str) -> Option<String> {
    let reference = reference.trim();
    let valid = ["env:", "file:"].iter().any(|prefix| {
        reference
            .strip_prefix(prefix)
            .is_some_and(|operand| !operand.trim().is_empty())
    });
    valid.then(|| reference.to_string())
}

fn valid_endpoint(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

pub(crate) fn classify_response(
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
) -> PushError {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        PushError::Auth("upstream rejected credentials".into())
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        PushError::RateLimited(rate_limit::parse_retry_after(retry_after))
    } else if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
        PushError::Transient(format!("upstream returned {status}"))
    } else {
        PushError::Other(format!("upstream returned {status}"))
    }
}

pub(crate) fn transport_error(error: &reqwest::Error) -> PushError {
    // reqwest's Display includes the request URL. Keep diagnostics value-free.
    if error.is_timeout() {
        PushError::Transient("request timed out".into())
    } else if error.is_connect() {
        PushError::Transient("connection failed".into())
    } else if error.is_request() {
        PushError::Transient("request failed".into())
    } else {
        PushError::Other("HTTP request failed".into())
    }
}

pub(crate) fn legacy_rendered(msg: &PushMessage, _flavor: MarkdownFlavor) -> RenderedNotification {
    RenderedNotification {
        kind: thegn_core::notification::NotificationKind::Assigned,
        priority: msg.priority,
        source: String::new(),
        worktree: String::new(),
        timestamp: 0,
        title: msg.title.clone(),
        message: msg.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::seam::Kind;

    #[test]
    fn provider_built_only_for_configured_implemented_kind() {
        let mut cfg = PushConfig::default();
        assert!(provider_for(&cfg).is_none(), "no topic ⇒ inert");
        cfg.topic = "t".into();
        assert!(provider_for(&cfg).is_some(), "ntfy configured");
        cfg.kind = PushKind::Gotify; // reserved
        assert!(provider_for(&cfg).is_none(), "reserved ⇒ no provider");
    }

    #[test]
    fn kind_coverage_matches_reserved() {
        // The factory builds exactly the non-reserved kinds. Webhook sinks
        // resolve their endpoint SecretRef at construction, so use a private
        // temporary file as the configured URL for this exhaustive check.
        let endpoint = tempfile::NamedTempFile::new().expect("temporary endpoint ref");
        std::fs::write(endpoint.path(), "https://hooks.example.test/incoming").unwrap();
        let endpoint_ref = format!("file:{}", endpoint.path().display());
        crate::seam::kind_coverage::<PushKind, _>(|k| {
            let cfg = PushSinkConfig {
                name: k.as_str().into(),
                kind: k,
                topic: "t".into(),
                url: endpoint_ref.clone(),
                ..Default::default()
            };
            provider_for_sink(&cfg)
        });
        assert_eq!(PushKind::implemented().count(), 4);
    }

    #[test]
    fn push_error_classes() {
        assert_eq!(PushError::NotConfigured.class(), ErrorClass::NotConfigured);
        assert_eq!(PushError::Auth("x".into()).class(), ErrorClass::Auth);
        assert!(PushError::Transient("x".into()).is_transient());
        assert_eq!(
            PushError::RateLimited(Duration::from_secs(1)).class(),
            ErrorClass::RateLimited
        );
        assert_eq!(
            <PushError as SeamError>::unsupported("publish").class(),
            ErrorClass::Unsupported
        );
    }

    #[test]
    fn status_classification_is_retryable_and_redacted() {
        let limited = classify_response(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("999999"));
        assert_eq!(limited.class(), ErrorClass::RateLimited);
        assert_eq!(limited.to_string(), "push rate limited; retry after 30s");
        assert_eq!(
            classify_response(reqwest::StatusCode::UNAUTHORIZED, Some("1")).class(),
            ErrorClass::Auth
        );
        let failure = classify_response(reqwest::StatusCode::BAD_REQUEST, None);
        assert!(!failure.to_string().contains("http"));
    }

    #[test]
    fn named_probe_does_not_echo_secret_ref_or_endpoint() {
        let sink = PushSinkConfig {
            name: "oncall".into(),
            kind: PushKind::Discord,
            url: "env:THEGN_PUSH_PROBE_MISSING_437".into(),
            ..Default::default()
        };
        let report = probe_sink(&sink);
        assert_eq!(report.id, "oncall");
        assert!(report.availability.is_unavailable());
        let text = format!("{report:?}");
        assert!(text.contains("secret: missing"));
        assert!(!text.contains("THEGN_PUSH_PROBE_MISSING_437"));
        assert!(!text.contains("https://"));
    }
}
