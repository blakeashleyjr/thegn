//! The upstream dispatch seam.
//!
//! Provider dispatch is a provider seam keyed by *wire protocol*, not vendor: an
//! object-safe [`ModelUpstream`] trait (BoxFuture methods so it stays
//! object-safe and dodges the async-fn-in-trait ratchet), capability bits, and
//! two implementations that cover the ecosystem:
//!
//! - [`AnthropicUpstream`] — the Anthropic `/v1/messages` surface (Claude).
//! - [`OpenAiCompatUpstream`] — the OpenAI `/v1/chat/completions` surface, which
//!   also covers OpenRouter, DeepSeek, Groq, and **local runtimes** (Ollama,
//!   vLLM) — they are just another base URL, no special case here.
//!
//! Vendor quirks (header shapes, translation) live only inside these impls; the
//! router stays in OpenAI space and never branches on a vendor name.

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use thegn_core::proxy::translate;

use crate::model::{Backend, Wire};
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

/// Capability bits a wire adapter advertises (caps ⇔ optional ops).
#[derive(Debug, Clone, Copy)]
pub struct UpstreamCaps {
    /// Supports a live SSE stream (`open_stream`) rather than buffered synthesis.
    pub streaming: bool,
    /// Passes tool/function-call definitions through unmodified.
    pub tool_passthrough: bool,
    /// Reports prompt-cache token counts in its usage block.
    pub cache_tokens: bool,
}

/// Object-safe upstream dispatch seam. One implementation per wire protocol.
pub trait ModelUpstream: Send + Sync {
    /// Which wire protocol this adapter speaks.
    fn wire(&self) -> Wire;
    /// The adapter's capability bits.
    fn caps(&self) -> UpstreamCaps;
    /// Non-streaming dispatch: takes an OpenAI-shaped request body and returns a
    /// normalized OpenAI-shaped response (translating around Anthropic backends).
    fn dispatch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        backend: &'a Backend,
        body_openai: &'a [u8],
    ) -> BoxFuture<'a, Result<BackendResponse>>;
    /// Opens a live streaming upstream response (OpenAI SSE). Adapters without
    /// `caps().streaming` return an error and the router uses buffered synthesis.
    fn open_stream<'a>(
        &'a self,
        client: &'a reqwest::Client,
        backend: &'a Backend,
        body: &'a [u8],
    ) -> BoxFuture<'a, Result<reqwest::Response>>;
}

/// Selects the wire adapter for a backend. Cheap; the adapters are stateless.
#[derive(Default)]
pub struct Upstreams {
    openai: OpenAiCompatUpstream,
    anthropic: AnthropicUpstream,
}

impl Upstreams {
    pub fn new() -> Self {
        Self::default()
    }
    /// The adapter for a backend's wire protocol.
    pub fn for_backend(&self, backend: &Backend) -> &dyn ModelUpstream {
        match backend.wire {
            Wire::OpenAi => &self.openai,
            Wire::Anthropic => &self.anthropic,
        }
    }
}

// ── OpenAI-compatible surface ───────────────────────────────────────────────

/// `/v1/chat/completions` — OpenAI and every openai-compatible endpoint
/// (OpenRouter, DeepSeek, Groq, Ollama, vLLM, …).
#[derive(Default)]
pub struct OpenAiCompatUpstream;

impl ModelUpstream for OpenAiCompatUpstream {
    fn wire(&self) -> Wire {
        Wire::OpenAi
    }
    fn caps(&self) -> UpstreamCaps {
        UpstreamCaps {
            streaming: true,
            tool_passthrough: true,
            cache_tokens: true,
        }
    }
    fn dispatch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        backend: &'a Backend,
        body: &'a [u8],
    ) -> BoxFuture<'a, Result<BackendResponse>> {
        Box::pin(async move {
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
        })
    }
    fn open_stream<'a>(
        &'a self,
        client: &'a reqwest::Client,
        backend: &'a Backend,
        body: &'a [u8],
    ) -> BoxFuture<'a, Result<reqwest::Response>> {
        Box::pin(async move {
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
        })
    }
}

// ── Anthropic surface ───────────────────────────────────────────────────────

/// `/v1/messages` — the proxy translates OpenAI⇄Anthropic around it so the
/// router stays in OpenAI space. Streaming is buffered-then-synthesized (no live
/// stream), so `caps().streaming` is false.
#[derive(Default)]
pub struct AnthropicUpstream;

impl ModelUpstream for AnthropicUpstream {
    fn wire(&self) -> Wire {
        Wire::Anthropic
    }
    fn caps(&self) -> UpstreamCaps {
        UpstreamCaps {
            streaming: false,
            tool_passthrough: true,
            cache_tokens: true,
        }
    }
    fn dispatch<'a>(
        &'a self,
        client: &'a reqwest::Client,
        backend: &'a Backend,
        body_openai: &'a [u8],
    ) -> BoxFuture<'a, Result<BackendResponse>> {
        Box::pin(async move {
            // OpenAI → Anthropic, forcing non-streaming upstream.
            let anthropic_body = translate::openai_to_anthropic(
                &translate::openai_request_without_stream(body_openai),
                &backend.model,
            )
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
                // Pass the upstream error through unchanged so the router classifies it.
                return Ok(BackendResponse {
                    status,
                    body: raw.to_vec(),
                    headers,
                });
            }
            let mut openai =
                translate::anthropic_to_openai_completion(&raw, &backend.model, now_unix())
                    .context("translate anthropic response to openai")?;
            // Thread Anthropic cache-token counts into the OpenAI usage block so
            // accounting sees them (translate keeps only prompt/completion).
            merge_anthropic_cache_tokens(&raw, &mut openai);
            Ok(BackendResponse {
                status,
                body: serde_json::to_vec(&openai)?,
                headers,
            })
        })
    }
    fn open_stream<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _backend: &'a Backend,
        _body: &'a [u8],
    ) -> BoxFuture<'a, Result<reqwest::Response>> {
        Box::pin(async move {
            anyhow::bail!("anthropic upstream does not support live streaming (buffered synthesis)")
        })
    }
}

/// Copies Anthropic `usage.cache_read_input_tokens` /
/// `cache_creation_input_tokens` into the translated OpenAI `usage` object under
/// the keys the router's usage parser reads.
fn merge_anthropic_cache_tokens(anthropic_raw: &[u8], openai: &mut serde_json::Value) {
    let Ok(a): Result<serde_json::Value, _> = serde_json::from_slice(anthropic_raw) else {
        return;
    };
    let Some(usage) = a.get("usage") else { return };
    let read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let create = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if read == 0 && create == 0 {
        return;
    }
    if let Some(u) = openai
        .get_mut("usage")
        .and_then(serde_json::Value::as_object_mut)
    {
        u.insert("cache_read_tokens".into(), serde_json::json!(read));
        u.insert("cache_creation_tokens".into(), serde_json::json!(create));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_dispatches_by_wire() {
        let ups = Upstreams::new();
        let mut b = Backend {
            name: "p".into(),
            key_id: String::new(),
            base_url: "http://x".into(),
            model: "m".into(),
            api_key: "k".into(),
            wire: Wire::OpenAi,
            context_limit: 0,
            defaults: serde_json::Map::new(),
            rate: thegn_core::proxy::ratelimit::RatePolicy {
                rpm: 60.0,
                burst: 5.0,
            },
            inflight_cap: 0,
            pool: None,
        };
        assert_eq!(ups.for_backend(&b).wire(), Wire::OpenAi);
        assert!(ups.for_backend(&b).caps().streaming);
        b.wire = Wire::Anthropic;
        assert_eq!(ups.for_backend(&b).wire(), Wire::Anthropic);
        assert!(!ups.for_backend(&b).caps().streaming);
    }

    #[test]
    fn merges_anthropic_cache_tokens() {
        let raw = br#"{"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":40,"cache_creation_input_tokens":3}}"#;
        let mut openai = serde_json::json!({"usage":{"prompt_tokens":10,"completion_tokens":5}});
        merge_anthropic_cache_tokens(raw, &mut openai);
        assert_eq!(openai["usage"]["cache_read_tokens"], 40);
        assert_eq!(openai["usage"]["cache_creation_tokens"], 3);
    }
}
