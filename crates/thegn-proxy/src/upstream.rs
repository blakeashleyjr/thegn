//! Upstream dispatch over reqwest. Translates OpenAI⇄Anthropic around
//! Anthropic-surface backends so the router stays in OpenAI space. Port of the
//! `callBackend*` family from `main.go` (minus the deferred subscription path).

use anyhow::{Context, Result};
use thegn_core::proxy::bridge;

use crate::model::Backend;
use crate::shared::now_unix;

/// Anthropic API version pinned for the `/v1/messages` surface.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A normalized backend reply in OpenAI shape. `headers` carries the upstream
/// response headers for cost / Retry-After extraction (see [`crate::headers`]).
pub struct BackendResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: reqwest::header::HeaderMap,
}

/// Issues a non-streaming request to a backend and returns the response as
/// OpenAI-shaped bytes (translating from Anthropic when needed).
pub async fn call_backend(
    client: &reqwest::Client,
    backend: &Backend,
    body_openai: &[u8],
) -> Result<BackendResponse> {
    if backend.anthropic {
        call_anthropic(client, backend, body_openai).await
    } else {
        call_openai(client, backend, body_openai).await
    }
}

async fn call_openai(
    client: &reqwest::Client,
    backend: &Backend,
    body: &[u8],
) -> Result<BackendResponse> {
    let url = format!(
        "{}/chat/completions",
        backend.base_url.trim_end_matches('/')
    );
    let mut req = client.post(&url).header("content-type", "application/json");
    if !backend.api_key.is_empty() {
        req = req.bearer_auth(&backend.api_key);
    }
    let resp = req
        .body(body.to_vec())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.context("read upstream body")?;
    Ok(BackendResponse {
        status,
        body: bytes.to_vec(),
        headers,
    })
}

async fn call_anthropic(
    client: &reqwest::Client,
    backend: &Backend,
    body: &[u8],
) -> Result<BackendResponse> {
    // Translate OpenAI → Anthropic, forcing non-streaming upstream (the client
    // stream is synthesized by the caller from the buffered completion).
    let anthropic_body =
        bridge::openai_to_anthropic(&bridge::openai_request_without_stream(body), &backend.model)
            .context("translate request to anthropic")?;
    let url = format!("{}/messages", backend.base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-api-key", &backend.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .body(serde_json::to_vec(&anthropic_body)?)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let raw = resp.bytes().await.context("read anthropic body")?;
    if !(200..300).contains(&status) {
        // Pass the upstream error through unchanged so the router can classify it.
        return Ok(BackendResponse {
            status,
            body: raw.to_vec(),
            headers,
        });
    }
    let openai = bridge::anthropic_to_openai_completion(&raw, &backend.model, now_unix())
        .context("translate anthropic response to openai")?;
    Ok(BackendResponse {
        status,
        body: serde_json::to_vec(&openai)?,
        headers,
    })
}

/// Opens a streaming request to an OpenAI-surface backend, returning the live
/// response for the caller to relay. Network/connection errors surface as `Err`.
pub async fn open_openai_stream(
    client: &reqwest::Client,
    backend: &Backend,
    body: &[u8],
) -> Result<reqwest::Response> {
    let url = format!(
        "{}/chat/completions",
        backend.base_url.trim_end_matches('/')
    );
    let mut req = client.post(&url).header("content-type", "application/json");
    if !backend.api_key.is_empty() {
        req = req.bearer_auth(&backend.api_key);
    }
    req.body(body.to_vec())
        .send()
        .await
        .with_context(|| format!("POST {url}"))
}
