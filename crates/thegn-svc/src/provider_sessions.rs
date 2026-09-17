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
}
