//! Kaneo OAuth 2.0 device-authorization flow — the engine behind
//! `thegn kaneo login`.
//!
//! Ported from Kaneo's own CLI/MCP reference (`packages/mcp/src/auth/
//! device-flow.ts`). Two steps against `{base_url}/api/auth/device/*`:
//!   1. `POST /device/code` `{ client_id }` → a `user_code` + `verification_uri`
//!      the user opens in a browser to approve.
//!   2. Poll `POST /device/token` (device-code grant) until it returns an
//!      `access_token` (or a terminal error).
//!
//! The default `client_id` `kaneo-cli` is allowlisted by Kaneo out of the box
//! (`DEVICE_AUTH_CLIENT_IDS` defaults to `kaneo-cli,kaneo-mcp`).

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use reqwest::Client;
use serde::Deserialize;

/// The device-flow client id Kaneo allowlists by default.
pub const DEFAULT_CLIENT_ID: &str = "kaneo-cli";

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// `POST /device/code` response.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// A URL with the code pre-filled, when the server provides one.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Seconds to wait between polls (default 5 when the server omits it).
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Seconds until the device code expires.
    #[serde(default)]
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
}

fn device_url(base_url: &str, leaf: &str) -> String {
    format!("{}/api/auth/device/{leaf}", base_url.trim_end_matches('/'))
}

/// Step 1: request a device + user code.
pub async fn request_device_code(base_url: &str, client_id: &str) -> Result<DeviceCode> {
    let resp = client()
        .post(device_url(base_url, "code"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&serde_json::json!({ "client_id": client_id }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("device/code failed (HTTP {status}): {body}");
    }
    let code: DeviceCode = serde_json::from_str(&body)
        .map_err(|e| anyhow!("device/code: unexpected response ({e}): {body}"))?;
    if code.device_code.is_empty() || code.user_code.is_empty() || code.verification_uri.is_empty()
    {
        bail!("device/code: missing device_code/user_code/verification_uri: {body}");
    }
    Ok(code)
}

/// Classification of one `POST /device/token` poll response.
#[derive(Debug, PartialEq, Eq)]
enum TokenPoll {
    Token(String),
    Pending,
    SlowDown,
    Denied,
    Expired,
    Other(String),
}

/// Pure interpreter of a token-endpoint response body (status + JSON), factored
/// out so the OAuth error vocabulary is unit-tested without a live server.
fn classify_token_response(ok: bool, body: &serde_json::Value) -> TokenPoll {
    if ok && let Some(tok) = body.get("access_token").and_then(|t| t.as_str()) {
        return TokenPoll::Token(tok.to_string());
    }
    match body.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending") => TokenPoll::Pending,
        Some("slow_down") => TokenPoll::SlowDown,
        Some("access_denied") => TokenPoll::Denied,
        Some("expired_token") => TokenPoll::Expired,
        Some(other) => TokenPoll::Other(other.to_string()),
        None => TokenPoll::Other(body.to_string()),
    }
}

/// Step 2: poll the token endpoint until the user approves (returns the access
/// token), the request is denied/expired, or `timeout` elapses.
///
/// `on_wait` is invoked once per pending poll so the caller can show progress.
pub async fn poll_access_token(
    base_url: &str,
    client_id: &str,
    device_code: &str,
    interval_secs: u64,
    timeout: Duration,
    mut on_wait: impl FnMut(),
) -> Result<String> {
    let http = client();
    let started = Instant::now();
    let mut interval = Duration::from_secs(interval_secs.max(1));
    let url = device_url(base_url, "token");
    let mut first = true;
    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;
        if started.elapsed() >= timeout {
            bail!("device authorization timed out waiting for approval");
        }

        let resp = http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&serde_json::json!({
                "grant_type": DEVICE_GRANT,
                "device_code": device_code,
                "client_id": client_id,
            }))
            .send()
            .await;
        let (ok, body) = match resp {
            Ok(r) => {
                let ok = r.status().is_success();
                let v = r
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null);
                (ok, v)
            }
            // A transient network error: keep polling until the timeout.
            Err(_) => {
                on_wait();
                continue;
            }
        };
        match classify_token_response(ok, &body) {
            TokenPoll::Token(t) => return Ok(t),
            TokenPoll::Pending => on_wait(),
            TokenPoll::SlowDown => {
                interval += Duration::from_secs(5);
                on_wait();
            }
            TokenPoll::Denied => bail!("device authorization was denied"),
            TokenPoll::Expired => bail!("device code expired; start login again"),
            TokenPoll::Other(msg) => bail!("device/token failed: {msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn device_url_normalizes_trailing_slash() {
        assert_eq!(
            device_url("https://k.example.com/", "code"),
            "https://k.example.com/api/auth/device/code"
        );
        assert_eq!(
            device_url("https://k.example.com", "token"),
            "https://k.example.com/api/auth/device/token"
        );
    }

    #[test]
    fn device_code_deserializes_with_defaults() {
        let c: DeviceCode = serde_json::from_value(json!({
            "device_code": "dc", "user_code": "WXYZ-1234",
            "verification_uri": "https://k/device"
        }))
        .unwrap();
        assert_eq!(c.interval, 5, "interval defaults to 5");
        assert_eq!(c.expires_in, 0);
        assert!(c.verification_uri_complete.is_none());
    }

    #[test]
    fn classify_covers_the_oauth_vocabulary() {
        assert_eq!(
            classify_token_response(true, &json!({ "access_token": "tok" })),
            TokenPoll::Token("tok".into())
        );
        // A 4xx with a pending error is a keep-polling signal, not a failure.
        assert_eq!(
            classify_token_response(false, &json!({ "error": "authorization_pending" })),
            TokenPoll::Pending
        );
        assert_eq!(
            classify_token_response(false, &json!({ "error": "slow_down" })),
            TokenPoll::SlowDown
        );
        assert_eq!(
            classify_token_response(false, &json!({ "error": "access_denied" })),
            TokenPoll::Denied
        );
        assert_eq!(
            classify_token_response(false, &json!({ "error": "expired_token" })),
            TokenPoll::Expired
        );
        // An unknown error and an empty body both surface as Other.
        assert!(matches!(
            classify_token_response(false, &json!({ "error": "weird" })),
            TokenPoll::Other(_)
        ));
        assert!(matches!(
            classify_token_response(false, &json!({})),
            TokenPoll::Other(_)
        ));
        // A 200 with no access_token but no error also falls through to Other.
        assert!(matches!(
            classify_token_response(true, &json!({})),
            TokenPoll::Other(_)
        ));
    }
}
