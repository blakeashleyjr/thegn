//! Generic JSON webhook push provider.

use std::time::Duration;

use thegn_core::config::PushKind;
use thegn_core::config_push::PushSinkConfig;
use thegn_core::notification_render::{MarkdownFlavor, RenderedNotification};
use thegn_core::seam::{Availability, BoxFuture, ProbeReport};

use super::{
    PushError, PushMessage, PushProvider, caps_for, classify_response, legacy_rendered,
    render_flavor, resolve_endpoint, transport_error,
};

const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// The stable, versioned generic webhook envelope.
pub fn payload(notification: &RenderedNotification) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "kind": notification.kind.as_str(),
        "priority": notification.priority.as_str(),
        "message": notification.message,
        "source": notification.source,
        "worktree": notification.worktree,
        "ts": notification.timestamp,
    })
}

/// A configured generic webhook publisher. The URL is intentionally private:
/// it is a bearer credential and must not appear in diagnostics.
pub struct WebhookPublisher {
    url: String,
    client: reqwest::Client,
}

impl WebhookPublisher {
    pub fn new(cfg: &PushSinkConfig) -> Result<Self, PushError> {
        Ok(Self {
            url: resolve_endpoint(&cfg.url)?,
            client: crate::provider::provider_http_client(),
        })
    }

    pub fn from_url(url: &str) -> Result<Self, PushError> {
        if !super::valid_endpoint(url) {
            return Err(PushError::Other(
                "webhook endpoint is not a valid HTTP URL".into(),
            ));
        }
        Ok(Self {
            url: url.to_string(),
            client: crate::provider::provider_http_client(),
        })
    }

    async fn post_once(&self, notification: &RenderedNotification) -> Result<(), PushError> {
        let response = self
            .client
            .post(&self.url)
            .timeout(PUBLISH_TIMEOUT)
            .json(&payload(notification))
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        if response.status().is_success() {
            Ok(())
        } else {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok());
            Err(classify_response(response.status(), retry_after))
        }
    }

    async fn publish_inner(&self, notification: &RenderedNotification) -> Result<(), PushError> {
        let mut last = PushError::Other("no attempt made".into());
        for attempt in 0..MAX_ATTEMPTS {
            match self.post_once(notification).await {
                Ok(()) => return Ok(()),
                Err(error @ PushError::Transient(_)) => {
                    last = error;
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(RETRY_BACKOFF * (attempt + 1)).await;
                    }
                }
                Err(error @ PushError::RateLimited(delay)) => {
                    last = error;
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }
}

impl PushProvider for WebhookPublisher {
    fn kind(&self) -> PushKind {
        PushKind::Webhook
    }

    fn publish<'a>(&'a self, msg: &'a PushMessage) -> BoxFuture<'a, Result<(), PushError>> {
        let notification = legacy_rendered(msg, MarkdownFlavor::CommonMark);
        Box::pin(async move { self.publish_inner(&notification).await })
    }

    fn publish_rendered<'a>(
        &'a self,
        notification: &'a RenderedNotification,
    ) -> BoxFuture<'a, Result<(), PushError>> {
        Box::pin(async move { self.publish_inner(notification).await })
    }

    fn caps(&self) -> super::PushCaps {
        caps_for(PushKind::Webhook)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new("push", "webhook", Availability::Ready)
            .with_caps(&self.caps())
            .note("secret: resolved")
            .note(format!(
                "markdown: {}",
                render_flavor(PushKind::Webhook).name()
            ))
            .note("dry-run: POST shape ready")
    }
}

trait FlavorName {
    fn name(self) -> &'static str;
}

impl FlavorName for MarkdownFlavor {
    fn name(self) -> &'static str {
        match self {
            MarkdownFlavor::CommonMark => "common_mark",
            MarkdownFlavor::Discord => "discord",
            MarkdownFlavor::Slack => "slack",
            MarkdownFlavor::Plain => "plain",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;

    fn rendered() -> RenderedNotification {
        RenderedNotification::new(
            NotificationKind::TestFailed,
            thegn_core::notification::Priority::Alert,
            "tests failed",
            "ci:run",
            "/repo",
            42,
            MarkdownFlavor::CommonMark,
        )
    }

    #[test]
    fn generic_schema_is_versioned_and_stable() {
        let value = payload(&rendered());
        assert_eq!(value["v"], 1);
        assert_eq!(value["kind"], "test_failed");
        assert_eq!(value["priority"], "alert");
        assert_eq!(value["ts"], 42);
        assert!(value.get("source").is_some() && value.get("worktree").is_some());
    }

    #[test]
    fn direct_constructor_rejects_non_http_urls() {
        let error = match WebhookPublisher::from_url("javascript:alert(1)") {
            Ok(_) => panic!("non-http URL accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("javascript"));
    }
}
