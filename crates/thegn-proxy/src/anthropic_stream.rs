//! Incremental OpenAI-SSE → Anthropic-SSE translation.
//!
//! This is net-new versus the Go proxy (which buffers the whole completion and
//! synthesizes the event sequence afterward). [`AnthropicSink`] is a state machine
//! that emits live Anthropic `message_*` / `content_block_*` events as OpenAI
//! streaming chunks arrive, so an Anthropic-surface client (Claude Code with
//! `stream: true`) sees tokens as they're produced.
//!
//! Event sequence: `message_start` → for each content block
//! `content_block_start` / `content_block_delta…` / `content_block_stop` →
//! `message_delta` (stop_reason + output usage) → `message_stop`.

use std::collections::HashMap;

use serde_json::{Value, json};
use thegn_core::proxy::bridge::map_stop_reason;
use thegn_core::proxy::cost::Usage;

use crate::relay::{OpenAiChunk, StreamSink, parse_sse_data_line};

#[derive(PartialEq)]
enum Block {
    None,
    Text,
    Tool,
}

/// Translates an OpenAI stream into Anthropic SSE events.
pub struct AnthropicSink {
    msg_id: String,
    model: String,
    input_tokens_estimate: u64,
    started: bool,
    block: Block,
    /// Next Anthropic content-block index to allocate.
    next_index: u64,
    /// Index of the currently open Anthropic content block.
    open_index: u64,
    /// OpenAI tool-call index → allocated Anthropic block index.
    tool_block: HashMap<u64, u64>,
    usage: Usage,
    finish_reason: Option<String>,
    done: bool,
}

impl AnthropicSink {
    pub fn new(
        msg_id: impl Into<String>,
        model: impl Into<String>,
        input_tokens_estimate: u64,
    ) -> Self {
        Self {
            msg_id: msg_id.into(),
            model: model.into(),
            input_tokens_estimate,
            started: false,
            block: Block::None,
            next_index: 0,
            open_index: 0,
            tool_block: HashMap::new(),
            usage: Usage::default(),
            finish_reason: None,
            done: false,
        }
    }

    fn event(name: &str, data: Value) -> Vec<u8> {
        format!(
            "event: {name}\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap_or_default()
        )
        .into_bytes()
    }

    fn message_start(&self) -> Vec<u8> {
        Self::event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.input_tokens_estimate, "output_tokens": 0},
                },
            }),
        )
    }

    fn close_open_block(&mut self, out: &mut Vec<u8>) {
        if self.block != Block::None {
            out.extend_from_slice(&Self::event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": self.open_index}),
            ));
            self.block = Block::None;
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            out.extend_from_slice(&self.message_start());
            self.started = true;
        }
    }

    fn handle_chunk(&mut self, chunk: OpenAiChunk, out: &mut Vec<u8>) {
        self.ensure_started(out);

        // Text content → ensure a text block is open, then a text_delta.
        if let Some(text) = chunk.content {
            if self.block != Block::Text {
                self.close_open_block(out);
                self.open_index = self.next_index;
                self.next_index += 1;
                self.block = Block::Text;
                out.extend_from_slice(&Self::event(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": self.open_index,
                        "content_block": {"type": "text", "text": ""},
                    }),
                ));
            }
            out.extend_from_slice(&Self::event(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": self.open_index,
                    "delta": {"type": "text_delta", "text": text},
                }),
            ));
        }

        // Tool-call deltas → open a tool_use block per OpenAI tool index, then
        // stream its arguments as input_json_delta fragments.
        for tc in chunk.tool_calls {
            let new_block = !self.tool_block.contains_key(&tc.index);
            if new_block {
                self.close_open_block(out);
                let idx = self.next_index;
                self.next_index += 1;
                self.tool_block.insert(tc.index, idx);
                self.open_index = idx;
                self.block = Block::Tool;
                out.extend_from_slice(&Self::event(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {
                            "type": "tool_use",
                            "id": tc.id.clone().unwrap_or_default(),
                            "name": tc.name.clone().unwrap_or_default(),
                            "input": {},
                        },
                    }),
                ));
            } else if let Some(&idx) = self.tool_block.get(&tc.index) {
                // A continuation fragment for an already-open tool block.
                self.open_index = idx;
                self.block = Block::Tool;
            }
            if !tc.args_fragment.is_empty() {
                out.extend_from_slice(&Self::event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": self.open_index,
                        "delta": {"type": "input_json_delta", "partial_json": tc.args_fragment},
                    }),
                ));
            }
        }

        if let Some(fr) = chunk.finish_reason {
            self.finish_reason = Some(fr);
        }
        if let Some(u) = chunk.usage {
            self.usage = u;
        }
    }
}

impl StreamSink for AnthropicSink {
    fn heartbeat_frame(&self) -> Vec<u8> {
        b"event: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec()
    }

    fn process(&mut self, line: &[u8]) -> Vec<u8> {
        let Some(chunk) = parse_sse_data_line(line) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        self.handle_chunk(chunk, &mut out);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        self.ensure_started(&mut out);
        self.close_open_block(&mut out);
        let stop = map_stop_reason(self.finish_reason.as_deref().unwrap_or("stop"));
        out.extend_from_slice(&Self::event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
                "usage": {"output_tokens": self.usage.completion_tokens},
            }),
        ));
        out.extend_from_slice(&Self::event(
            "message_stop",
            json!({"type": "message_stop"}),
        ));
        out
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

    fn drive(lines: &[&[u8]]) -> String {
        let mut sink = AnthropicSink::new("msg_1", "claude-x", 10);
        let mut out = Vec::new();
        for l in lines {
            out.extend_from_slice(&sink.process(l));
        }
        out.extend_from_slice(&sink.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn text_only_sequence() {
        let sse = drive(&[
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            b"data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n",
        ]);
        // Ordered event sequence.
        let order: Vec<&str> = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
        .to_vec();
        let mut idx = 0;
        for line in sse.lines().filter(|l| l.starts_with("event: ")) {
            let name = line.trim_start_matches("event: ");
            assert_eq!(name, order[idx], "event {idx}");
            idx += 1;
        }
        assert_eq!(idx, order.len());
        assert!(sse.contains("\"text\":\"Hel\""));
        assert!(sse.contains("\"text\":\"lo\""));
        assert!(sse.contains("\"stop_reason\":\"end_turn\""));
        assert!(sse.contains("\"output_tokens\":2"));
        assert!(sse.contains("\"input_tokens\":10"));
    }

    #[test]
    fn tool_call_sequence() {
        let sse = drive(&[
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"f\",\"arguments\":\"\"}}]}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n",
            b"data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n",
        ]);
        assert!(sse.contains("\"type\":\"tool_use\""));
        assert!(sse.contains("\"name\":\"f\""));
        assert!(sse.contains("\"id\":\"t1\""));
        assert!(sse.contains("input_json_delta"));
        assert!(sse.contains("\"partial_json\":\"{\\\"a\\\":\""));
        assert!(sse.contains("\"partial_json\":\"1}\""));
        assert!(sse.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn text_then_tool_closes_text_block() {
        let sse = drive(&[
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t\",\"function\":{\"name\":\"f\"}}]}}]}\n",
            b"data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n",
        ]);
        // Two content blocks: index 0 (text) and index 1 (tool_use), each stopped.
        assert!(sse.contains("\"index\":0"));
        assert!(sse.contains("\"index\":1"));
        let stops = sse
            .lines()
            .filter(|l| *l == "event: content_block_stop")
            .count();
        assert_eq!(stops, 2);
    }

    #[test]
    fn empty_stream_still_well_formed() {
        let sse = drive(&[]);
        assert!(sse.contains("message_start"));
        assert!(sse.contains("message_delta"));
        assert!(sse.contains("message_stop"));
        assert!(sse.contains("\"stop_reason\":\"end_turn\""));
    }

    #[test]
    fn heartbeat_is_ping() {
        let sink = AnthropicSink::new("m", "x", 0);
        assert_eq!(
            sink.heartbeat_frame(),
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec()
        );
    }
}
