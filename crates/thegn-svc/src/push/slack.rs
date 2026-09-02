//! Slack incoming-webhook provider and mrkdwn envelope shaping.

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
/// Slack section text is limited to 3,000 Unicode characters. Keep the
/// fallback text within the same bound so both envelope paths are valid.
pub const SLACK_TEXT_LIMIT: usize = 3_000;

/// Build Slack's incoming-webhook envelope: text remains useful to clients
/// that do not render blocks, while the section gives Slack mrkdwn structure.
pub fn payload(notification: &RenderedNotification) -> serde_json::Value {
    let message = truncate_chars(&notification.message, SLACK_TEXT_LIMIT, "…");
    serde_json::json!({
        "text": message,
        "blocks": [{
            "type": "section",
            "text": { "type": "mrkdwn", "text": message }
        }],
        "attachments": [{ "color": priority_color(notification.priority) }]
    })
}

pub fn priority_color(priority: Priority) -> &'static str {
    match priority {
        Priority::Info => "#6B7280",
        Priority::Notice => "#3498DB",
        Priority::Alert => "#ED4245",
    }
}

pub struct SlackPublisher {
    url: String,
    client: reqwest::Client,
}

impl SlackPublisher {
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

impl PushProvider for SlackPublisher {
    fn kind(&self) -> PushKind {
        PushKind::Slack
    }

    fn publish<'a>(&'a self, msg: &'a PushMessage) -> BoxFuture<'a, Result<(), PushError>> {
        let notification = legacy_rendered(msg, MarkdownFlavor::Slack);
        Box::pin(async move { self.publish_inner(&notification).await })
    }

    fn publish_rendered<'a>(
        &'a self,
        notification: &'a RenderedNotification,
    ) -> BoxFuture<'a, Result<(), PushError>> {
        Box::pin(async move { self.publish_inner(notification).await })
    }

    fn caps(&self) -> super::PushCaps {
        caps_for(PushKind::Slack)
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new("push", "slack", Availability::Ready)
            .with_caps(&self.caps())
            .note("secret: resolved")
            .note("markdown: slack")
            .note("dry-run: POST shape ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::notification::NotificationKind;

    #[test]
    fn payload_has_text_section_and_priority_color() {
        let n = RenderedNotification {
            kind: NotificationKind::AgentFailed,
            priority: Priority::Alert,
            source: "s".into(),
            worktree: "w".into(),
            timestamp: 1,
            title: "agent failed".into(),
            message: "failure\nnext".into(),
        };
        let v = payload(&n);
        assert_eq!(v["text"], "failure\nnext");
        assert_eq!(v["blocks"][0]["text"]["type"], "mrkdwn");
        assert_eq!(v["attachments"][0]["color"], "#ED4245");
    }

    #[test]
    fn colors_cover_all_priorities() {
        assert_ne!(
            priority_color(Priority::Info),
            priority_color(Priority::Alert)
        );
        assert_eq!(priority_color(Priority::Notice), "#3498DB");
    }

    #[test]
    fn payload_truncates_section_text_by_visible_unicode_chars() {
        let n = RenderedNotification {
            kind: NotificationKind::AgentFailed,
            priority: Priority::Alert,
            source: "s".into(),
            worktree: "w".into(),
            timestamp: 1,
            title: "agent failed".into(),
            message: "界".repeat(SLACK_TEXT_LIMIT + 5),
        };
        let value = payload(&n);
        let text = value["blocks"][0]["text"]["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), SLACK_TEXT_LIMIT);
        assert!(text.ends_with('…'));
        assert_eq!(value["text"].as_str().unwrap(), text);
    }
}
