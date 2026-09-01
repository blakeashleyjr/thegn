//! Discord incoming-webhook provider and envelope shaping.

use std::time::Duration;

use thegn_core::config::PushKind;
use thegn_core::config_push::PushSinkConfig;
use thegn_core::notification::Priority;
use thegn_core::notification_render::{MarkdownFlavor, RenderedNotification, truncate_chars};
use thegn_core::seam::{Availability, BoxFuture, ProbeReport};

use super::{
    PushError, PushMessage, PushProvider, caps_for, classify_response, legacy_rendered,
    resolve_endpoint, transport_error,
};

const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(400);
pub const DISCORD_MESSAGE_LIMIT: usize = 2_000;

/// Build a valid Discord incoming-webhook JSON envelope. `content` is bounded
/// by visible Unicode scalar values, including the marker.
pub fn payload(notification: &RenderedNotification) -> serde_json::Value {
    serde_json::json!({
        "content": truncate_chars(&notification.message, DISCORD_MESSAGE_LIMIT, "…"),
        "embeds": [{
            "title": notification.title,
            "color": priority_color(notification.priority),
        }]
    })
}

pub fn priority_color(priority: Priority) -> u32 {
    match priority {
        Priority::Info => 0x6B7280,
        Priority::Notice => 0x3498DB,
        Priority::Alert => 0xED4245,
    }
}

pub struct DiscordPublisher {
    url: String,
    client: reqwest::Client,
}

impl DiscordPublisher {
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
            url: url.into(),
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

impl PushProvider for DiscordPublisher {
    fn kind(&self) -> PushKind {
        PushKind::Discord
    }

    fn publish<'a>(&'a self, msg: &'a PushMessage) -> BoxFuture<'a, Result<(), PushError>> {
        let notification = legacy_rendered(msg, MarkdownFlavor::Discord);
        Box::pin(async move { self.publish_inner(&notification).await })
    }

    fn publish_rendered<'a>(
        &'a self,
        notification: &'a RenderedNotification,
    ) -> BoxFuture<'a, Result<(), PushError>> {
        Box::pin(async move { self.publish_inner(notification).await })
    }

    fn caps(&self) -> super::PushCaps {
        caps_for(PushKind::Discord)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new("push", "discord", Availability::Ready)
            .with_caps(&self.caps())
            .note("secret: resolved")
            .note(format!(
                "markdown: {}",
                super::flavor_for(PushKind::Discord)
            ))
            .note("dry-run: POST shape ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;

    fn notification(message: &str) -> RenderedNotification {
        RenderedNotification {
            kind: NotificationKind::TestFailed,
            priority: Priority::Alert,
            source: "source".into(),
            worktree: "worktree".into(),
            timestamp: 7,
            title: "tests failed".into(),
            message: message.into(),
        }
    }

    #[test]
    fn payload_is_visible_unicode_bounded() {
        let value = payload(&notification(&"界".repeat(2_005)));
        let content = value["content"].as_str().unwrap();
        assert_eq!(content.chars().count(), DISCORD_MESSAGE_LIMIT);
        assert!(content.ends_with('…'));
        assert_eq!(priority_color(Priority::Alert), 0xED4245);
    }

    #[test]
    fn direct_url_errors_are_redacted() {
        let error = match DiscordPublisher::from_url("not a URL") {
            Ok(_) => panic!("invalid URL accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("not a URL"));
    }
}
