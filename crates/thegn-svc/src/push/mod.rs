//! The push-provider seam — outbound delivery for the push-to-phone bridge.
//!
//! An object-safe async seam (`BoxFuture`, like the calendar/issue seams): one
//! implemented kind, `ntfy` ([`ntfy::NtfyPublisher`]), and four reserved
//! (`telegram`/`gotify`/`pushover`/`webhook`) that config accepts but this build
//! does not implement. The inbound half (the daemon's command subscription)
//! lives in [`subscriber`]; the router decision that authorises a push is the
//! pure `thegn_core::notification_route`, not here.
//!
//! Request/response shaping is split into **pure** functions (`publish_url`,
//! priority/tag mapping) so they unit-test without a live endpoint; the async
//! trait method is a thin reqwest wrapper with a bounded retry.

pub mod ntfy;
pub mod subscriber;

use thegn_core::config::{PushConfig, PushKind};
use thegn_core::notification::Priority;
use thegn_core::seam::{BoxFuture, ErrorClass, ProbeReport, SeamError};

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
    /// Describe this provider for `thegn doctor` (synchronous, no network).
    fn probe(&self) -> ProbeReport;
}

/// Build the configured push provider, or `None` when push is unconfigured or
/// the selected kind is reserved (unimplemented in this build).
pub fn provider_for(cfg: &PushConfig) -> Option<Box<dyn PushProvider>> {
    if !cfg.is_configured() {
        return None;
    }
    match cfg.kind {
        PushKind::Ntfy => Some(Box::new(ntfy::NtfyPublisher::new(cfg))),
        // Reserved kinds have no publisher.
        PushKind::Telegram | PushKind::Gotify | PushKind::Pushover | PushKind::Webhook => None,
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
        // The factory builds exactly the non-reserved kinds (topic supplied).
        crate::seam::kind_coverage::<PushKind, _>(|k| {
            let cfg = PushConfig {
                kind: k,
                topic: "t".into(),
                ..Default::default()
            };
            provider_for(&cfg)
        });
        // Sanity: ntfy is the only implemented one.
        assert_eq!(PushKind::implemented().count(), 1);
    }

    #[test]
    fn push_error_classes() {
        assert_eq!(PushError::NotConfigured.class(), ErrorClass::NotConfigured);
        assert_eq!(PushError::Auth("x".into()).class(), ErrorClass::Auth);
        assert!(PushError::Transient("x".into()).is_transient());
        assert_eq!(
            <PushError as SeamError>::unsupported("publish").class(),
            ErrorClass::Unsupported
        );
    }
}
