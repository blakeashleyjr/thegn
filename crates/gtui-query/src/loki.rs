//! `LokiSource` — queries a Loki HTTP API (`/loki/api/v1/query_range`) and parses
//! the JSON with [`crate::loki_parser::parse_loki_response`]. Runs on the query
//! engine's tokio task (never the UI thread).

use std::future::Future;
use std::pin::Pin;

use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::Frame;

use crate::http::read_limited;
use crate::loki_parser::parse_loki_response;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct LokiSource {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl LokiSource {
    /// `base_url` e.g. `http://localhost:3100`; `token` may be empty (no auth).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_timeouts(
            base_url,
            token,
            CONNECT_TIMEOUT,
            READ_TIMEOUT,
            TOTAL_TIMEOUT,
        )
    }

    fn with_timeouts(
        base_url: impl Into<String>,
        token: impl Into<String>,
        connect_timeout: std::time::Duration,
        read_timeout: std::time::Duration,
        total_timeout: std::time::Duration,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .read_timeout(read_timeout)
                .timeout(total_timeout)
                .build()
                .expect("valid Loki HTTP client settings"),
        }
    }
}

impl DataSource for LokiSource {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let base = self.base_url.trim_end_matches('/').to_string();
        let token = self.token.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let url = format!("{base}/loki/api/v1/query_range");
            let mut frames = Vec::new();
            for q in queries {
                // Loki wants nanosecond timestamps.
                let start_ns = q.time_range.from.timestamp_nanos_opt().unwrap_or(0);
                let end_ns = q.time_range.to.timestamp_nanos_opt().unwrap_or(0);
                let (start_s, end_s) = (start_ns.to_string(), end_ns.to_string());
                let mut req = client.get(&url).query(&[
                    ("query", q.expr.as_str()),
                    ("start", start_s.as_str()),
                    ("end", end_s.as_str()),
                    ("limit", "500"),
                    ("direction", "backward"),
                ]);
                if !token.is_empty() {
                    req = req.bearer_auth(&token);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| QueryError::Network(e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(QueryError::Network(format!(
                        "Loki HTTP status {}",
                        resp.status()
                    )));
                }
                let body = read_limited(resp, "Loki").await?;
                frames.extend(parse_loki_response(&body)?);
            }
            Ok(frames)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn constructs_with_endpoint() {
        let s = LokiSource::new("http://localhost:3100", "tok");
        assert_eq!(s.base_url, "http://localhost:3100");
        assert_eq!(s.token, "tok");
    }

    #[tokio::test]
    async fn stalled_response_body_hits_the_read_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0, "client closed before sending its request");
                request.extend_from_slice(&chunk[..count]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let source = LokiSource::with_timeouts(
            format!("http://{address}"),
            "",
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_secs(10),
        );
        let now = Utc::now();
        let started = tokio::time::Instant::now();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            source.query(vec![Query {
                ref_id: "A".into(),
                expr: "{job=\"test\"}".into(),
                time_range: gtui_core::datasource::TimeRange { from: now, to: now },
            }]),
        )
        .await
        .expect("read timeout did not bound the stalled body")
        .unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "read timeout took {:?}",
            started.elapsed()
        );
        assert!(matches!(error, QueryError::Network(_)));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
