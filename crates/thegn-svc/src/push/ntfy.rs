//! The ntfy publisher — POST a notification to `{server}/{topic}`.
//!
//! ntfy (`ntfy.sh`, self-hostable) is a store-and-forward pub/sub server: a
//! plain HTTP POST to a topic URL reaches every subscribed phone with no
//! inbound port and no companion app. The message is the request body; the
//! title, priority and tags ride headers.
//!
//! Request shaping is pure ([`NtfyPublisher::publish_url`], [`ntfy_priority`]);
//! [`NtfyPublisher::publish`] is a thin reqwest wrapper with a bounded retry.

use std::time::Duration;

use thegn_core::config::{PushConfig, PushKind};
use thegn_core::notification::Priority;
use thegn_core::seam::{Availability, BoxFuture, ProbeReport};

use super::{PushError, PushMessage, PushProvider};

/// Per-publish deadline. Bounds a hung POST so a stalled server can't wedge the
/// publisher worker (the shared client already bounds connection setup).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// Total attempts (1 initial + up to 2 retries), then the message is dropped.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// An ntfy topic publisher.
pub struct NtfyPublisher {
    /// Server base, trailing slash trimmed, e.g. `"https://ntfy.sh"`.
    server: String,
    topic: String,
    /// Resolved bearer token (SecretRef expanded), or `None` for a public topic.
    token: Option<String>,
    client: reqwest::Client,
}

impl NtfyPublisher {
    pub fn new(cfg: &PushConfig) -> Self {
        NtfyPublisher {
            server: cfg.server.trim().trim_end_matches('/').to_string(),
            topic: cfg.topic.trim().to_string(),
            token: cfg.resolved_token(),
            client: crate::provider::provider_http_client(),
        }
    }

    /// The publish endpoint: `{server}/{topic}`.
    pub fn publish_url(&self) -> String {
        format!("{}/{}", self.server, self.topic)
    }

    async fn post_once(&self, msg: &PushMessage) -> Result<(), PushError> {
        let tags = msg.tags.join(",");
        let mut req = self
            .client
            .post(self.publish_url())
            .timeout(PUBLISH_TIMEOUT)
            .header("Title", header_value(&msg.title))
            .header("Priority", ntfy_priority(msg.priority))
            .body(msg.body.clone());
        if !tags.is_empty() {
            req = req.header("Tags", header_value(&tags));
        }
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Ok(())
                } else if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    Err(PushError::Auth(format!("{status}")))
                } else if status.is_server_error() || status.as_u16() == 429 {
                    Err(PushError::Transient(format!("{status}")))
                } else {
                    Err(PushError::Other(format!("{status}")))
                }
            }
            Err(e) => Err(super::transport_error(&e)),
        }
    }
}

impl PushProvider for NtfyPublisher {
    fn kind(&self) -> PushKind {
        PushKind::Ntfy
    }

    fn publish<'a>(&'a self, msg: &'a PushMessage) -> BoxFuture<'a, Result<(), PushError>> {
        Box::pin(async move {
            let mut last = PushError::Other("no attempt made".into());
            for attempt in 0..MAX_ATTEMPTS {
                match self.post_once(msg).await {
                    Ok(()) => return Ok(()),
                    // Only transient failures are worth retrying; auth/4xx are final.
                    Err(e @ PushError::Transient(_)) => {
                        last = e;
                        if attempt + 1 < MAX_ATTEMPTS {
                            tokio::time::sleep(RETRY_BACKOFF * (attempt + 1)).await;
                        }
                    }
                    Err(other) => return Err(other),
                }
            }
            Err(last)
        })
    }

    fn probe(&self) -> ProbeReport {
        // A network provider: no offline round-trip (probes are cheap by
        // contract). Report the effective config so `doctor` shows where push
        // goes and whether a token is set.
        let avail = if self.topic.is_empty() || self.server.is_empty() {
            Availability::Unavailable("server/topic not configured".into())
        } else {
            Availability::Ready
        };
        ProbeReport::new("push", "ntfy", avail)
            .note(format!("publishes to {}", self.publish_url()))
            .note(if self.token.is_some() {
                "auth token: set"
            } else {
                "auth token: none (public topic)"
            })
            .note("network provider; not probed offline")
    }
}

/// Map thegn's priority onto ntfy's named priority scale.
/// `Alert → high`, `Notice → default`, `Info → low`.
pub fn ntfy_priority(p: Priority) -> &'static str {
    match p {
        Priority::Alert => "high",
        Priority::Notice => "default",
        Priority::Info => "low",
    }
}

/// Strip control characters (CR/LF especially) from a header value: ntfy header
/// fields are single-line, and a newline in a notification message must never
/// split into a spoofed second header.
fn header_value(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publisher(server: &str, topic: &str, token: Option<&str>) -> NtfyPublisher {
        NtfyPublisher {
            server: server.trim_end_matches('/').to_string(),
            topic: topic.to_string(),
            token: token.map(str::to_string),
            client: crate::provider::provider_http_client(),
        }
    }

    #[test]
    fn publish_url_joins_server_and_topic() {
        let p = publisher("https://ntfy.sh/", "thegn-alerts", None);
        assert_eq!(p.publish_url(), "https://ntfy.sh/thegn-alerts");
    }

    #[test]
    fn priority_mapping() {
        assert_eq!(ntfy_priority(Priority::Alert), "high");
        assert_eq!(ntfy_priority(Priority::Notice), "default");
        assert_eq!(ntfy_priority(Priority::Info), "low");
    }

    #[test]
    fn header_value_strips_newlines_to_prevent_injection() {
        assert_eq!(header_value("hello\r\nX-Evil: 1"), "helloX-Evil: 1");
        assert_eq!(header_value("  spaced  "), "spaced");
    }

    #[test]
    fn probe_reports_url_and_token_state() {
        let p = publisher("https://ntfy.sh", "t", Some("tok"));
        let r = p.probe();
        assert_eq!(r.seam, "push");
        assert_eq!(r.id, "ntfy");
        assert!(r.availability.is_ready());
        assert!(r.notes.iter().any(|n| n.contains("ntfy.sh/t")));
        assert!(r.notes.iter().any(|n| n.contains("token: set")));
        // No topic ⇒ unavailable.
        let p = publisher("https://ntfy.sh", "", None);
        assert!(p.probe().availability.is_unavailable());
    }
}
