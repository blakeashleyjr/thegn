//! The sprite memory policy: `[env.<n>.provider] min_ram_gb > 0` sets
//! `limit_mb = min_ram_gb × 1024` (autoscale unset) after create and again,
//! with a GET readback, at each provision. Sprites have a fixed 8 vCPU, so
//! there is no CPU knob.

use anyhow::{Context, Result, bail};

use super::SpritesHttp;
use crate::provider::CONTROL_TIMEOUT;

/// Largest policy response body read.
const MAX_POLICY_BYTES: usize = 64 * 1024;

fn policy_url(http: &SpritesHttp<'_>) -> String {
    format!(
        "{}/sprites/{}/policy/resources",
        http.base.trim_end_matches('/'),
        http.name
    )
}

/// Apply the memory policy, then read it back; a mismatch is an error.
pub(crate) async fn apply(http: &SpritesHttp<'_>, limit_mb: u32) -> Result<()> {
    let name = http.name;
    let response = http
        .client
        .post(policy_url(http))
        .bearer_auth(http.token)
        .timeout(CONTROL_TIMEOUT)
        .json(&policy_body(limit_mb))
        .send()
        .await
        .with_context(|| format!("sprites: setting the memory policy of `{name}` failed"))?;
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
        .get(policy_url(http))
        .bearer_auth(http.token)
        .timeout(CONTROL_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("sprites: reading the memory policy of `{name}` failed"))?;
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

    async fn serve(responses: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
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

    #[test]
    fn policy_body_sets_only_the_memory_limit() {
        assert_eq!(
            policy_body(2048),
            serde_json::json!({ "memory": { "limit_mb": 2048 } })
        );
    }
}
