//! Streaming relay core: the shared SSE plumbing for both client surfaces.
//!
//! Consumes an upstream `reqwest` byte stream, line-buffers SSE, and drives a
//! [`StreamSink`] that turns each line into client bytes while accumulating
//! usage. The reliability features are ported from the Go proxy's
//! `stream_relay.go` / `streamguard.go`:
//!
//! - **first-byte / peek timeout** — commit only once usable output is seen
//!   within the TTFB budget; an empty or timed-out stream falls through.
//! - **empty-completion peek** — buffer up to 1KB looking for real output before
//!   committing, so a reasoning-only/empty turn doesn't get streamed to the client.
//! - **idle watchdog** — terminate a stream that goes silent past the idle limit.
//! - **heartbeat** — emit a surface-specific keep-alive during upstream silence.
//! - **usage reconciliation** — parse the trailing `usage` chunk so cost/audit
//!   reflect real token counts (M1 logged zero for streams).
//!
//! The OpenAI surface passes lines through byte-for-byte (true passthrough); the
//! Anthropic surface translates OpenAI chunks into live Anthropic events (see
//! [`crate::anthropic_stream`]).

use std::time::Duration;

use axum::body::{Body, Bytes};
use futures::StreamExt;
use thegn_core::proxy::cost::Usage;
use tokio::time::{Instant, timeout};

/// Tunables for the relay (Go-compatible defaults).
#[derive(Clone, Copy, Debug)]
pub struct RelayConfig {
    /// Budget for first usable output before committing (TTFB / peek window).
    pub first_byte: Duration,
    /// Silence after which a committed stream is terminated.
    pub idle: Duration,
    /// Keep-alive cadence during upstream silence.
    pub heartbeat: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(45),
            idle: Duration::from_secs(120),
            heartbeat: Duration::from_secs(10),
        }
    }
}

/// A parsed OpenAI streaming chunk (the fields the relay/sinks care about).
#[derive(Debug, Default, Clone)]
pub struct OpenAiChunk {
    pub content: Option<String>,
    /// `(index, id, name, arguments_fragment)` for each tool-call delta.
    pub tool_calls: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    pub index: u64,
    pub id: Option<String>,
    pub name: Option<String>,
    pub args_fragment: String,
}

/// Parses the JSON payload of one `data:` SSE line into an [`OpenAiChunk`].
/// Returns `None` for non-data lines, comments, blanks, and `[DONE]`.
pub fn parse_sse_data_line(line: &[u8]) -> Option<OpenAiChunk> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let payload = s.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let mut chunk = OpenAiChunk::default();
    if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
        chunk.usage = Some(Usage {
            prompt_tokens: usage
                .get("prompt_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            completion_tokens: usage
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
        });
    }
    if let Some(choice) = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    {
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            chunk.finish_reason = Some(fr.to_string());
        }
        let delta = choice.get("delta");
        if let Some(c) = delta
            .and_then(|d| d.get("content"))
            .and_then(|x| x.as_str())
            && !c.is_empty()
        {
            chunk.content = Some(c.to_string());
        }
        if let Some(tcs) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|x| x.as_array())
        {
            for tc in tcs {
                chunk.tool_calls.push(ToolCallDelta {
                    index: tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0),
                    id: tc.get("id").and_then(|x| x.as_str()).map(String::from),
                    name: tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|x| x.as_str())
                        .map(String::from),
                    args_fragment: tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    Some(chunk)
}

/// Whether an SSE `data:` line carries usable output (text, a tool call, or a
/// tool-call finish). Port of `sseChunkHasOutput`.
pub fn sse_chunk_has_output(line: &[u8]) -> bool {
    match parse_sse_data_line(line) {
        Some(c) => {
            c.content.as_deref().is_some_and(|s| !s.trim().is_empty())
                || !c.tool_calls.is_empty()
                || c.finish_reason.as_deref() == Some("tool_calls")
        }
        None => false,
    }
}

/// Maps upstream SSE lines to client bytes while accumulating usage. Each surface
/// supplies one implementation.
pub trait StreamSink: Send + 'static {
    /// Keep-alive frame emitted during upstream silence.
    fn heartbeat_frame(&self) -> Vec<u8>;
    /// Process one complete SSE line (including its trailing `\n`), returning the
    /// bytes to send to the client (may be empty).
    fn process(&mut self, line: &[u8]) -> Vec<u8>;
    /// Emit any trailing frames once the upstream stream ends.
    fn finish(&mut self) -> Vec<u8>;
    /// The reconciled usage observed across the stream.
    fn usage(&self) -> Usage;
    /// The final `finish_reason` observed (for the audit row).
    fn finish_reason(&self) -> Option<String>;
}

/// Outcome of peeking an upstream stream before committing.
pub enum Peek<S: StreamSink> {
    /// Usable output seen; carries the buffered prefix bytes already read, the
    /// remaining upstream stream, and the sink (with prefix already processed).
    Commit {
        prefix_out: Vec<u8>,
        rest: ByteStream,
        sink: S,
    },
    /// Stream ended with no usable output — fall through (soft, no cooldown).
    Empty,
    /// No first byte within the TTFB budget — fall through (soft cooldown).
    TimedOut,
    /// Network/transport error before commit — fall through.
    Errored(String),
}

/// The boxed upstream byte stream type.
pub type ByteStream = std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Peeks the upstream stream: reads until usable output appears (commit), the
/// buffer hits 1KB (commit), the stream ends with nothing usable (empty), or the
/// TTFB budget elapses (timeout). On commit, the sink has already processed every
/// buffered line and `prefix_out` holds the client bytes to emit first.
pub async fn peek<S: StreamSink>(
    resp: reqwest::Response,
    mut sink: S,
    cfg: RelayConfig,
) -> Peek<S> {
    const PEEK_CAP: usize = 1024;
    let mut stream: ByteStream = Box::pin(resp.bytes_stream());
    let mut line_buf: Vec<u8> = Vec::new();
    let mut prefix_out: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut committed = false;

    let result = timeout(cfg.first_byte, async {
        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    total += chunk.len();
                    line_buf.extend_from_slice(&chunk);
                    // Drain complete lines, feeding the sink and scanning for output.
                    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = line_buf.drain(..=pos).collect();
                        if sse_chunk_has_output(&line) {
                            committed = true;
                        }
                        prefix_out.extend_from_slice(&sink.process(&line));
                    }
                    if committed || total >= PEEK_CAP {
                        return PeekInner::Commit;
                    }
                }
                Some(Err(e)) => return PeekInner::Errored(e.to_string()),
                None => {
                    // EOF: flush any trailing partial line, then decide.
                    if !line_buf.is_empty() {
                        let line = std::mem::take(&mut line_buf);
                        if sse_chunk_has_output(&line) {
                            committed = true;
                        }
                        prefix_out.extend_from_slice(&sink.process(&line));
                    }
                    return if committed {
                        PeekInner::Commit
                    } else {
                        PeekInner::Empty
                    };
                }
            }
        }
    })
    .await;

    match result {
        Ok(PeekInner::Commit) => Peek::Commit {
            prefix_out,
            rest: stream,
            sink,
        },
        Ok(PeekInner::Empty) => Peek::Empty,
        Ok(PeekInner::Errored(e)) => Peek::Errored(e),
        Err(_) => Peek::TimedOut,
    }
}

enum PeekInner {
    Commit,
    Empty,
    Errored(String),
}

/// Stats handed back when a committed stream finishes.
#[derive(Debug, Clone)]
pub struct RelayStats {
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// Spawns the relay loop for a committed stream and returns the client [`Body`].
/// `on_finish` is invoked (off the request path) with the reconciled stats once
/// the stream completes, for cost/spend/audit finalization.
pub fn spawn_relay<S, F>(
    prefix_out: Vec<u8>,
    mut rest: ByteStream,
    mut sink: S,
    cfg: RelayConfig,
    on_finish: F,
) -> Body
where
    S: StreamSink,
    F: FnOnce(RelayStats) + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);

    tokio::spawn(async move {
        if !prefix_out.is_empty() && tx.send(Bytes::from(prefix_out)).await.is_err() {
            return; // client gone
        }
        let mut line_buf: Vec<u8> = Vec::new();
        let mut last_data = Instant::now();
        let mut hb = tokio::time::interval(cfg.heartbeat);
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        hb.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                biased;
                next = rest.next() => match next {
                    Some(Ok(chunk)) => {
                        last_data = Instant::now();
                        line_buf.extend_from_slice(&chunk);
                        while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = line_buf.drain(..=pos).collect();
                            let out = sink.process(&line);
                            if !out.is_empty() && tx.send(Bytes::from(out)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                },
                _ = hb.tick() => {
                    if last_data.elapsed() >= cfg.idle {
                        // Idle watchdog: upstream went silent — terminate.
                        tracing::warn!("stream idle timeout — terminating");
                        break;
                    }
                    let frame = sink.heartbeat_frame();
                    if !frame.is_empty() && tx.send(Bytes::from(frame)).await.is_err() {
                        return;
                    }
                }
            }
        }

        // Flush any trailing partial line, then the sink's closing frames.
        if !line_buf.is_empty() {
            let out = sink.process(&line_buf);
            if !out.is_empty() {
                let _ = tx.send(Bytes::from(out)).await;
            }
        }
        let tail = sink.finish();
        if !tail.is_empty() {
            let _ = tx.send(Bytes::from(tail)).await;
        }
        on_finish(RelayStats {
            usage: sink.usage(),
            finish_reason: sink.finish_reason(),
        });
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|b| (Ok::<Bytes, std::io::Error>(b), rx))
    });
    Body::from_stream(stream)
}

// ── OpenAI passthrough sink ─────────────────────────────────────────────────

/// Passes SSE lines through byte-for-byte while parsing usage on the side.
#[derive(Default)]
pub struct OpenAiSink {
    usage: Usage,
    finish_reason: Option<String>,
}

impl StreamSink for OpenAiSink {
    fn heartbeat_frame(&self) -> Vec<u8> {
        b": keep-alive\n\n".to_vec()
    }
    fn process(&mut self, line: &[u8]) -> Vec<u8> {
        if let Some(chunk) = parse_sse_data_line(line) {
            if let Some(u) = chunk.usage {
                // Last non-null usage wins (mirrors parseOpenAIStreamingUsage).
                self.usage = u;
            }
            if let Some(fr) = chunk.finish_reason {
                self.finish_reason = Some(fr);
            }
        }
        line.to_vec() // true passthrough
    }
    fn finish(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn usage(&self) -> Usage {
        self.usage
    }
    fn finish_reason(&self) -> Option<String> {
        self.finish_reason.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_delta() {
        let c = parse_sse_data_line(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n")
            .unwrap();
        assert_eq!(c.content.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_usage_and_finish() {
        let c = parse_sse_data_line(
            b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}",
        )
        .unwrap();
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert_eq!(c.usage.unwrap().total(), 7);
    }

    #[test]
    fn parses_tool_call_delta() {
        let c = parse_sse_data_line(
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}",
        )
        .unwrap();
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].id.as_deref(), Some("t1"));
        assert_eq!(c.tool_calls[0].name.as_deref(), Some("f"));
        assert_eq!(c.tool_calls[0].args_fragment, "{\"a\":");
    }

    #[test]
    fn non_data_lines_return_none() {
        assert!(parse_sse_data_line(b": comment\n").is_none());
        assert!(parse_sse_data_line(b"data: [DONE]\n").is_none());
        assert!(parse_sse_data_line(b"\n").is_none());
    }

    #[test]
    fn output_detection() {
        assert!(sse_chunk_has_output(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"
        ));
        assert!(!sse_chunk_has_output(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}"
        ));
        assert!(!sse_chunk_has_output(
            b"data: {\"choices\":[{\"delta\":{}}]}"
        ));
        assert!(sse_chunk_has_output(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}"
        ));
    }

    #[test]
    fn openai_sink_passes_through_and_tracks_usage() {
        let mut s = OpenAiSink::default();
        let line = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
        assert_eq!(s.process(line), line.to_vec());
        s.process(b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":5}}\n");
        assert_eq!(s.usage().total(), 7);
        assert_eq!(s.finish_reason().as_deref(), Some("stop"));
        assert!(s.finish().is_empty());
        assert_eq!(s.heartbeat_frame(), b": keep-alive\n\n");
    }
}
