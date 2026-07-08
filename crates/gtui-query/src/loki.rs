//! `LokiSource` — queries a Loki HTTP API (`/loki/api/v1/query_range`) and parses
//! the JSON with [`crate::loki_parser::parse_loki_response`]. Runs on the query
//! engine's tokio task (never the UI thread).

use std::future::Future;
use std::pin::Pin;

use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::Frame;

use crate::loki_parser::parse_loki_response;

pub struct LokiSource {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl LokiSource {
    /// `base_url` e.g. `http://localhost:3100`; `token` may be empty (no auth).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::new(),
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
                let body = resp
                    .text()
                    .await
                    .map_err(|e| QueryError::Network(e.to_string()))?;
                frames.extend(parse_loki_response(&body)?);
            }
            Ok(frames)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_with_endpoint() {
        let s = LokiSource::new("http://localhost:3100", "tok");
        assert_eq!(s.base_url, "http://localhost:3100");
        assert_eq!(s.token, "tok");
    }
}
