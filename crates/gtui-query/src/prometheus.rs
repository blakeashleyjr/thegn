//! `PrometheusSource` — queries a Prometheus HTTP API (`/api/v1/query_range`)
//! and parses the JSON with [`crate::prom_parser::parse_prometheus_response`].
//! Runs on the query engine's tokio task, so the blocking-forbidden UI thread is
//! never touched.

use std::future::Future;
use std::pin::Pin;

use gtui_core::datasource::{DataSource, Query, QueryError};
use gtui_core::frame::Frame;

use crate::prom_parser::parse_prometheus_response;

pub struct PrometheusSource {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl PrometheusSource {
    /// `base_url` e.g. `http://localhost:9090`; `token` may be empty (no auth).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
            client: reqwest::Client::new(),
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
                let body = resp
                    .text()
                    .await
                    .map_err(|e| QueryError::Network(e.to_string()))?;
                frames.extend(parse_prometheus_response(&body)?);
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
        let s = PrometheusSource::new("http://localhost:9090/", "");
        assert_eq!(s.base_url, "http://localhost:9090/");
    }
}
