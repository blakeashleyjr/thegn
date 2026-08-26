//! The ntfy subscriber — the inbound half of the command inbox.
//!
//! ntfy exposes a topic as a **streaming** endpoint (`GET {server}/{topic}/json`
//! keeps the connection open and emits one JSON object per line). We consume
//! that stream, hand each `event: message` payload's `message` field (the raw
//! command envelope) to the daemon over an mpsc channel, and reconnect with
//! capped backoff + `since=` resume when the connection drops.
//!
//! Only transport lives here; the security decision (verify → freshness →
//! replay → admit) is the pure `thegn_core::push_inbox`. Line parsing is a pure
//! helper ([`parse_ntfy_line`]) so it unit-tests without a live server.
//!
//! *Transport note:* ntfy's `/json` newline-stream is the primary transport
//! rather than `/sse` — both are ntfy-native long-lived streams, but the
//! newline-JSON framing is trivial and robust to parse where SSE's
//! `event:`/`data:` framing is not. The reconnect/`since=` resume semantics are
//! identical.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::{Notify, mpsc};

/// Reconnect backoff bounds (design: "caps at minutes").
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// One decoded ntfy stream line we care about: a delivered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfyMessage {
    /// The message body — for the inbox, the raw command envelope JSON.
    pub message: String,
    /// The server's unix-second timestamp, used to resume with `since=`.
    pub time: i64,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(default)]
    event: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    time: Option<i64>,
}

/// Parse one line of ntfy's `/json` stream. Returns `Some` only for a delivered
/// `message` event carrying a body; `open`/`keepalive`/`poll_request` control
/// lines and blank lines return `None`.
pub fn parse_ntfy_line(line: &str) -> Option<NtfyMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let raw: RawLine = serde_json::from_str(line).ok()?;
    if raw.event != "message" {
        return None;
    }
    let message = raw.message?;
    Some(NtfyMessage {
        message,
        time: raw.time.unwrap_or(0),
    })
}

/// A long-lived subscription to an ntfy command topic.
pub struct NtfySubscriber {
    server: String,
    topic: String,
    token: Option<String>,
    client: reqwest::Client,
}

enum ConsumeEnd {
    /// The stream ended (EOF or error) — reconnect after backoff.
    Ended,
    /// Shutdown was requested — the run loop returns.
    Shutdown,
}

impl NtfySubscriber {
    pub fn new(server: &str, topic: &str, token: Option<String>) -> Self {
        NtfySubscriber {
            server: server.trim().trim_end_matches('/').to_string(),
            topic: topic.trim().to_string(),
            token,
            client: crate::provider::provider_http_client(),
        }
    }

    /// The stream URL, resuming from `since` (unix seconds) when reconnecting so
    /// messages published during a brief disconnect are not lost (duplicates
    /// from the overlap are dropped by the replay cache downstream).
    pub fn stream_url(&self, since: Option<i64>) -> String {
        let base = format!("{}/{}/json", self.server, self.topic);
        match since {
            Some(s) => format!("{base}?since={s}"),
            None => base,
        }
    }

    /// Run until `shutdown` fires: connect, stream messages to `tx`, reconnect
    /// with capped backoff. Never returns on its own except on shutdown.
    pub async fn run(&self, tx: mpsc::Sender<String>, shutdown: Arc<Notify>) {
        let mut backoff = BACKOFF_START;
        // Start from "now" (no history replay): stale commands would be dropped
        // by the freshness window anyway, and replaying a backlog on boot is
        // surprising. Resume points advance as messages arrive.
        let mut since: Option<i64> = None;
        loop {
            let opened = tokio::select! {
                _ = shutdown.notified() => return,
                r = self.open(since) => r,
            };
            match opened {
                Ok(resp) => {
                    backoff = BACKOFF_START; // reset on a successful connect
                    match self.consume(resp, &tx, &mut since, &shutdown).await {
                        ConsumeEnd::Shutdown => return,
                        ConsumeEnd::Ended => {}
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "thegn::push", error = %e, "inbox subscribe failed; will retry");
                }
            }
            tokio::select! {
                _ = shutdown.notified() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn open(&self, since: Option<i64>) -> reqwest::Result<reqwest::Response> {
        let mut req = self.client.get(self.stream_url(since));
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req.send().await?.error_for_status()
    }

    async fn consume(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<String>,
        since: &mut Option<i64>,
        shutdown: &Arc<Notify>,
    ) -> ConsumeEnd {
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        loop {
            let chunk = tokio::select! {
                _ = shutdown.notified() => return ConsumeEnd::Shutdown,
                c = stream.next() => c,
            };
            let Some(chunk) = chunk else {
                return ConsumeEnd::Ended; // stream closed cleanly
            };
            let Ok(bytes) = chunk else {
                return ConsumeEnd::Ended; // read error
            };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            // Emit each complete line; keep the trailing partial in `buf`.
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                if let Some(msg) = parse_ntfy_line(&line) {
                    // Resume just after this message on the next reconnect.
                    if msg.time > 0 {
                        *since = Some(msg.time);
                    }
                    if tx.send(msg.message).await.is_err() {
                        // The inbox loop is gone — nothing left to feed.
                        return ConsumeEnd::Shutdown;
                    }
                }
            }
            // Guard against an endless line (a hostile server never sending \n).
            if buf.len() > 1_048_576 {
                tracing::warn!(target: "thegn::push", "inbox stream line exceeded 1 MiB; resetting connection");
                return ConsumeEnd::Ended;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_events_only() {
        let open = r#"{"id":"x","time":100,"event":"open","topic":"t"}"#;
        assert_eq!(parse_ntfy_line(open), None);
        let keepalive = r#"{"id":"y","time":101,"event":"keepalive","topic":"t"}"#;
        assert_eq!(parse_ntfy_line(keepalive), None);
        let msg = r#"{"id":"z","time":102,"event":"message","topic":"t","message":"{\"v\":1}"}"#;
        assert_eq!(
            parse_ntfy_line(msg),
            Some(NtfyMessage {
                message: "{\"v\":1}".into(),
                time: 102
            })
        );
        assert_eq!(parse_ntfy_line(""), None);
        assert_eq!(parse_ntfy_line("not json"), None);
    }

    #[test]
    fn stream_url_appends_since() {
        let s = NtfySubscriber::new("https://ntfy.sh/", "cmd", None);
        assert_eq!(s.stream_url(None), "https://ntfy.sh/cmd/json");
        assert_eq!(
            s.stream_url(Some(123)),
            "https://ntfy.sh/cmd/json?since=123"
        );
    }
}
