//! `PrometheusSource` — queries a Prometheus HTTP API (`/api/v1/query_range`)
//! and parses the JSON with [`crate::prom_parser::parse_prometheus_response`].
//! Runs on the query engine's tokio task, so the blocking-forbidden UI thread is
//! never touched.

use std::future::Future;
use std::pin::Pin;

use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::Frame;

use crate::http::read_limited;
use crate::prom_parser::parse_prometheus_response;

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct PrometheusSource {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl PrometheusSource {
    /// `base_url` e.g. `http://localhost:9090`; `token` may be empty (no auth).
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
                .expect("valid Prometheus HTTP client settings"),
        }
    }
}

impl DataSource for PrometheusSource {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
        let base = self.base_url.trim_end_matches('/').to_string();
        let token = self.token.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let url = format!("{base}/api/v1/query_range");
            let mut frames = Vec::new();
            for q in queries {
                let start = q.time_range.from.timestamp();
                let end = q.time_range.to.timestamp();
                // ~240 points across the window, min 15s resolution.
                let step = ((end - start) / 240).max(15);
                let (start_s, end_s, step_s) =
                    (start.to_string(), end.to_string(), format!("{step}"));
                let mut req = client.get(&url).query(&[
                    ("query", q.expr.as_str()),
                    ("start", start_s.as_str()),
                    ("end", end_s.as_str()),
                    ("step", step_s.as_str()),
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
                        "Prometheus HTTP status {}",
                        resp.status()
                    )));
                }
                let body = read_limited(resp, "Prometheus").await?;
                frames.extend(parse_prometheus_response(&body)?);
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
        let s = PrometheusSource::new("http://localhost:9090/", "");
        assert_eq!(s.base_url, "http://localhost:9090/");
    }

    #[tokio::test]
    async fn non_success_http_status_is_reported_before_body_parsing() {
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
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let source = PrometheusSource::new(format!("http://{address}"), "");
        let now = Utc::now();
        let error = source
            .query(vec![Query {
                ref_id: "A".into(),
                expr: "up".into(),
                time_range: gtui_core::datasource::TimeRange { from: now, to: now },
            }])
            .await
            .unwrap_err();
        assert!(matches!(error, QueryError::Network(message) if message.contains("503")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_content_length_over_four_mib_is_rejected() {
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
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4194305\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let source = PrometheusSource::new(format!("http://{address}"), "");
        let now = Utc::now();
        let error = source
            .query(vec![Query {
                ref_id: "A".into(),
                expr: "up".into(),
                time_range: gtui_core::datasource::TimeRange { from: now, to: now },
            }])
            .await
            .unwrap_err();
        assert!(matches!(error, QueryError::Other(message) if message.contains("4 MiB")));
        server.await.unwrap();
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
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
        let source = PrometheusSource::with_timeouts(
            format!("http://{address}"),
            "",
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_secs(10),
        );
        let now = Utc::now();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            source.query(vec![Query {
                ref_id: "A".into(),
                expr: "up".into(),
                time_range: gtui_core::datasource::TimeRange { from: now, to: now },
            }]),
        )
        .await
        .expect("read timeout did not bound the stalled body")
        .unwrap_err();
        assert!(matches!(error, QueryError::Network(_)));
        server.await.unwrap();
    }
}
