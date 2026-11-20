//! The sprite memory policy: `[env.<n>.provider] min_ram_gb > 0` sets
//! `limit_mb = min_ram_gb × 1024` (autoscale unset) once, right after create, with a
//! GET read-back. It is NOT re-applied on reuse, resume or a warm-spare claim: the
//! Sprites API stores it per sprite. `thegn fleet inspect <sprite> --env <S> --policy`
//! reads it back (UNVERIFIED: whether that GET wakes a cold sprite). It is not a
//! cgroup limit inside the guest.

use anyhow::{Context, Result, bail};

use super::SpritesHttp;
use crate::provider::CONTROL_TIMEOUT;

/// Largest policy response body read.
const MAX_POLICY_BYTES: usize = 64 * 1024;

fn policy_url(http: &SpritesHttp<'_>) -> Result<String> {
    crate::vps::registry::checked_name(http.name)
        .map_err(|_| anyhow::anyhow!("sprites: invalid sprite name"))?;
    Ok(format!(
        "{}/sprites/{}/policy/resources",
        http.base.trim_end_matches('/'),
        http.name
    ))
}

/// Apply the memory policy, then read it back; a mismatch is an error.
pub(crate) async fn apply(http: &SpritesHttp<'_>, limit_mb: u32) -> Result<()> {
    let name = http.name;
    let response = http
        .client
        .post(policy_url(http)?)
        .bearer_auth(http.token)
        .timeout(CONTROL_TIMEOUT)
        .json(&policy_body(limit_mb))
        .send()
        .await
        .map_err(|error| {
            crate::provider::redacted_transport_error("sprites", "set memory policy", &error)
                .context(format!(
                    "sprites: setting the memory policy of `{name}` failed"
                ))
        })?;
    let status = response.status();
    if !status.is_success() {
        bail!("sprites: setting the memory policy of `{name}` returned HTTP {status}");
    }
    let got = read(http).await?;
    if got != Some(limit_mb) {
        bail!("sprites: memory policy of `{name}` reads back {got:?} MiB, expected {limit_mb} MiB");
    }
    Ok(())
}

/// The sprite's current memory limit in MiB (`None` = no limit set).
pub(crate) async fn read(http: &SpritesHttp<'_>) -> Result<Option<u32>> {
    let name = http.name;
    let response = http
        .client
        .get(policy_url(http)?)
        .bearer_auth(http.token)
        .timeout(CONTROL_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            crate::provider::redacted_transport_error("sprites", "memory policy", &error).context(
                format!("sprites: reading the memory policy of `{name}` failed"),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        bail!("sprites: reading the memory policy of `{name}` returned HTTP {status}");
    }
    let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
    let bytes = crate::http::read_bounded_response(response, deadline, MAX_POLICY_BYTES)
        .await
        .map_err(|_| anyhow::anyhow!("sprites: memory policy response of `{name}` unreadable"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("sprites: memory policy of `{name}` is not JSON"))?;
    Ok(parse_limit(&value))
}

/// `memory.limit_mb` from a policy document.
fn parse_limit(v: &serde_json::Value) -> Option<u32> {
    v.get("memory")?
        .get("limit_mb")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
}

/// The policy request body: `{"memory":{"limit_mb":N}}`.
pub fn policy_body(limit_mb: u32) -> serde_json::Value {
    serde_json::json!({ "memory": { "limit_mb": limit_mb } })
}

impl crate::provider::SpritesProvider {
    /// The memory limit (MiB) of sprite `name` from `GET …/policy/resources`;
    /// `None` = no limit set. Read only; UNVERIFIED whether it wakes a cold sprite.
    pub async fn memory_policy_mb(&self, name: &str) -> Result<Option<u32>> {
        let base = self.sprites_http()?;
        let http = super::SpritesHttp { name, ..base };
        read(&http).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_limit_reads_memory_limit_mb() {
        assert_eq!(
            parse_limit(&serde_json::json!({"memory":{"limit_mb":16384}})),
            Some(16384)
        );
        assert_eq!(parse_limit(&serde_json::json!({})), None);
    }

    #[test]
    fn policy_url_refuses_a_slash() {
        let client = reqwest::Client::new();
        let http = SpritesHttp {
            client: &client,
            base: "https://api.sprites.dev/v1",
            token: "tok-secret",
            name: "a/b",
        };
        assert!(policy_url(&http).is_err());
    }

    async fn serve(responses: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                sock.read(&mut buf).await.unwrap_or_default(); // best-effort: drain the request
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn apply_mismatch_is_an_error_and_match_is_ok() {
        let client = reqwest::Client::new();
        let base = serve(vec!["{}".into(), r#"{"memory":{"limit_mb":1024}}"#.into()]).await;
        let http = SpritesHttp {
            client: &client,
            base: &base,
            token: "tok-secret",
            name: "sp1",
        };
        let err = apply(&http, 2048).await.unwrap_err().to_string();
        assert!(err.contains("sp1") && !err.contains("tok-secret"), "{err}");
        let base = serve(vec!["{}".into(), r#"{"memory":{"limit_mb":2048}}"#.into()]).await;
        let http = SpritesHttp {
            client: &client,
            base: &base,
            token: "tok-secret",
            name: "sp1",
        };
        apply(&http, 2048).await.unwrap();
    }

    #[tokio::test]
    async fn memory_policy_mb_reads_another_name() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let count = sock.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..count]);
            assert!(
                request.starts_with("GET /sprites/another/policy/resources"),
                "{request}"
            );
            let body = r#"{"memory":{"limit_mb":2048}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(response.as_bytes()).await.unwrap();
        });
        let base = format!("http://{addr}");
        let provider = crate::provider::SpritesProvider::new(&base, "tok-secret", "owned");
        assert_eq!(
            provider.memory_policy_mb("another").await.unwrap(),
            Some(2048)
        );
        server.await.unwrap();
    }

    #[test]
    fn policy_body_sets_only_the_memory_limit() {
        assert_eq!(
            policy_body(2048),
            serde_json::json!({ "memory": { "limit_mb": 2048 } })
        );
    }
}
