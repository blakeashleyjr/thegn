//! Authoritative native-session absence. Transport/status/decoding failures are
//! never an empty roster. The Sprites list contract is documented at
//! <https://sprites.dev/api/sprites/exec> (GET /v1/sprites/{name}/exec).

use super::{CONTROL_TIMEOUT, Provider, SpritesProvider};
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionId {
    Text(String),
    Number(u64),
}

#[derive(Deserialize)]
struct SessionIdentity {
    id: SessionId,
}

fn roster_absent(body: &[u8], session: &str) -> Result<bool> {
    if session.is_empty() {
        bail!("cannot query an empty session identifier");
    }
    let roster: Vec<SessionIdentity> =
        serde_json::from_slice(body).context("sprites: invalid session roster")?;
    let mut found = false;
    for item in roster {
        let id = match item.id {
            SessionId::Text(id) if !id.is_empty() => id,
            SessionId::Number(id) => id.to_string(),
            _ => bail!("sprites: empty identifier in session roster"),
        };
        found |= id == session;
    }
    Ok(!found)
}

impl SpritesProvider {
    /// POST /v1/sprites/{id}/exec/{session}/kill?signal=SIGHUP&timeout=5s
    /// (OpenAPI "Kill Exec Session").
    ///
    /// A missing session is already in the requested state, so 404 is treated
    /// as success. The NDJSON response body is drained without exposing its
    /// contents and is capped at 64 KiB to prevent an unbounded control-plane
    /// response.
    pub async fn kill_exec_session(&self, id: &str, session: &str) -> Result<()> {
        let id = super::checked_sprite_name(id)?;
        let session = super::checked_exec_session(session)?;
        let mut url = reqwest::Url::parse(&self.api_base).context("sprites: bad API URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("sprites: API URL cannot hold a path"))?
            .pop_if_empty()
            .push("sprites")
            .push(id)
            .push("exec")
            .push(session)
            .push("kill");
        url.query_pairs_mut().append_pair("signal", "SIGHUP");

        let mut response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .context("sprites: kill exec session")?;
        const MAX_KILL_BODY_BYTES: usize = 64 * 1024;
        let mut body_len = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("sprites: read kill exec response")?
        {
            if chunk.len() > MAX_KILL_BODY_BYTES.saturating_sub(body_len) {
                bail!("sprites: kill exec response exceeds size limit");
            }
            body_len += chunk.len();
        }

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            bail!("sprites: kill exec session request failed ({status})");
        }
        Ok(())
    }

    pub async fn exec_session_absent(&self, id: &str, session: &str) -> Result<bool> {
        let mut url = reqwest::Url::parse(&self.api_base).context("sprites: bad API URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("sprites: API URL cannot hold a path"))?
            .pop_if_empty()
            .push("sprites")
            .push(id)
            .push("exec");
        let mut response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .context("sprites: query session roster")?;
        if response.status() != reqwest::StatusCode::OK {
            bail!(
                "sprites: session roster request failed ({})",
                response.status()
            );
        }
        const MAX_ROSTER_BYTES: usize = 1024 * 1024;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("sprites: read session roster")?
        {
            if chunk.len() > MAX_ROSTER_BYTES.saturating_sub(bytes.len()) {
                bail!("sprites: session roster exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        roster_absent(&bytes, session)
    }
}

impl Provider {
    /// Kill a persisted provider exec session when the provider owns one.
    /// Providers without a persistent server-side exec session to kill return
    /// successfully without making a request.
    pub async fn kill_exec_session(&self, id: &str, session: &str) -> Result<()> {
        match self {
            Provider::Sprites(provider) => provider.kill_exec_session(id, session).await,
            _ => Ok(()),
        }
    }

    /// Only an authoritative, successfully decoded roster may prove absence.
    /// Providers without persistent native session identities stay conservative.
    pub async fn exec_session_absent(&self, id: &str, session: &str) -> Result<bool> {
        match self {
            Provider::Sprites(provider) => provider.exec_session_absent(id, session).await,
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_requires_complete_valid_identity_list() {
        assert!(roster_absent(b"[]", "12").unwrap());
        assert!(!roster_absent(br#"[{"id":12}]"#, "12").unwrap());
        assert!(!roster_absent(br#"[{"id":"12","is_active":false}]"#, "12").unwrap());
        assert!(roster_absent(br#"[{"id":"13"}]"#, "12").unwrap());
        for malformed in [
            b"null".as_slice(),
            b"{}",
            b"[{\"command\":\"sh\"}]",
            b"[{\"id\":null}]",
            b"[{\"id\":\"\"}]",
            b"[{\"id\":-1}]",
            b"[{\"id\":1.5}]",
            b"[{\"id\":\"12\"},{}]",
            b"[{\"id\":12}]x",
        ] {
            assert!(roster_absent(malformed, "12").is_err());
        }
    }

    #[tokio::test]
    async fn roster_http_errors_never_authorize_fresh_exec() {
        use axum::http::StatusCode;
        use axum::{Router, routing::get};
        for (status, body, expected) in [
            (StatusCode::OK, "[]", Some(true)),
            (StatusCode::OK, r#"[{"id":"live"}]"#, Some(false)),
            (StatusCode::OK, r#"{"error":"bad"}"#, None),
            (StatusCode::UNAUTHORIZED, "[]", None),
            (StatusCode::FORBIDDEN, "[]", None),
            (StatusCode::NOT_FOUND, "[]", None),
            (StatusCode::INTERNAL_SERVER_ERROR, "[]", None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/v1/sprites/test/exec",
                        get(move |headers: axum::http::HeaderMap| async move {
                            assert_eq!(headers["authorization"], "Bearer test-token");
                            (status, body)
                        }),
                    ),
                )
                .await
                .unwrap();
            });
            let provider =
                SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
            let result = provider.exec_session_absent("test", "live").await;
            assert_eq!(result.ok(), expected, "status {status}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn kill_exec_session_handles_statuses_and_request_shape() {
        use axum::Router;
        use axum::extract::Request;
        use axum::routing::any;
        use std::sync::{Arc, Mutex};

        for (status, expected_ok) in [
            (axum::http::StatusCode::OK, true),
            (axum::http::StatusCode::NOT_FOUND, true),
            (axum::http::StatusCode::UNAUTHORIZED, false),
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, false),
        ] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().fallback(any(move |request: Request| {
                        let seen = Arc::clone(&seen);
                        async move {
                            seen.lock().unwrap().push((
                                request.method().clone(),
                                request.uri().clone(),
                                request
                                    .headers()
                                    .get("authorization")
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_owned),
                            ));
                            (status, "ignored response body")
                        }
                    })),
                )
                .await
                .unwrap();
            });

            let provider =
                SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
            let result = provider.kill_exec_session("test", "live").await;
            assert_eq!(result.is_ok(), expected_ok, "status {status}");

            let requests = requests.lock().unwrap();
            let (method, uri, authorization) = requests.first().expect("kill request recorded");
            assert_eq!(method, axum::http::Method::POST);
            assert_eq!(uri.path(), "/v1/sprites/test/exec/live/kill");
            let query = reqwest::Url::parse(&format!("http://localhost{uri}"))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect::<Vec<_>>();
            assert!(query.contains(&("signal".into(), "SIGHUP".into())));
            assert_eq!(authorization.as_deref(), Some("Bearer test-token"));
            drop(requests);
            server.abort();
        }
    }

    #[tokio::test]
    async fn kill_exec_session_rejects_multi_segment_session_before_request() {
        use axum::Router;
        use axum::routing::any;
        use std::sync::{Arc, atomic::AtomicUsize};

        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(move || {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { "unexpected request" }
                })),
            )
            .await
            .unwrap();
        });

        let provider = SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
        assert!(provider.kill_exec_session("test", "a/b").await.is_err());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        server.abort();
    }
}
