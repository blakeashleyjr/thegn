//! OpenAI ↔ Anthropic protocol translation.
//!
//! Port of `openai_anthropic_bridge.go` + the translation half of `anthropic.go`.
//! This is what lets the proxy present one surface (either wire protocol) and
//! route to a backend speaking the other. The router stays protocol-agnostic:
//! Anthropic-surface backends (Kimi, MiniMax) are reached by translating an
//! OpenAI request in, then translating their response back out.
//!
//! The Go originals build `map[string]any` trees; here we build
//! [`serde_json::Value`] trees with the same shapes. Wall-clock fields
//! (`created`, generated message ids) are passed in as parameters so the
//! translation is pure and round-trip tests are deterministic; the
//! `superzej-proxy` I/O layer supplies real values.

use serde_json::{Map, Value, json};

// ── OpenAI → Anthropic (request) ────────────────────────────────────────────

/// Default `max_tokens` when the OpenAI request specifies none, matching the Go
/// bridge.
const BRIDGE_DEFAULT_MAX_TOKENS: i64 = 32768;

/// Translates an OpenAI `chat/completions` request body into an Anthropic
/// `messages` request body targeting `model`. Mirrors `openAIToAnthropic`.
pub fn openai_to_anthropic(raw: &[u8], model: &str) -> Result<Value, serde_json::Error> {
    let req: Value = serde_json::from_slice(raw)?;
    let msgs = req
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut max_tokens = BRIDGE_DEFAULT_MAX_TOKENS;
    if let Some(n) = req.get("max_completion_tokens").and_then(positive_int) {
        max_tokens = n;
    }
    if let Some(n) = req.get("max_tokens").and_then(positive_int) {
        max_tokens = n;
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("max_tokens".into(), json!(max_tokens));
    out.insert(
        "messages".into(),
        Value::Array(openai_messages_to_anthropic(&msgs)),
    );
    if req.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        out.insert("stream".into(), json!(true));
    }
    if let Some(t) = req.get("temperature").filter(|v| v.is_number()) {
        out.insert("temperature".into(), t.clone());
    }
    if let Some(t) = req.get("top_p").filter(|v| v.is_number()) {
        out.insert("top_p".into(), t.clone());
    }
    let system = openai_system(&msgs);
    if !system.is_empty() {
        out.insert("system".into(), json!(system));
    }
    let stops = openai_stops(req.get("stop"));
    if !stops.is_empty() {
        out.insert("stop_sequences".into(), json!(stops));
    }
    let tools = openai_tools_to_anthropic(req.get("tools"));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = openai_tool_choice(req.get("tool_choice")) {
        out.insert("tool_choice".into(), choice);
    }
    Ok(Value::Object(out))
}

/// Flattens an OpenAI message `content` (string, or array of `{text|input_text}`
/// parts) into plain text. Mirrors `openAIText`.
fn openai_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    let Some(parts) = c.as_array() else {
        return String::new();
    };
    let mut texts = Vec::new();
    for p in parts {
        if let Some(t) = p
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            texts.push(t.to_string());
        } else if let Some(t) = p
            .get("input_text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            texts.push(t.to_string());
        }
    }
    texts.join("\n")
}

/// Concatenates all `system`-role messages, separated by blank lines. Mirrors
/// `openAISystem`.
fn openai_system(msgs: &[Value]) -> String {
    let mut parts = Vec::new();
    for m in msgs {
        if m.get("role").and_then(Value::as_str) == Some("system") {
            let text = openai_text(m.get("content"));
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n\n")
}

/// Translates OpenAI messages into Anthropic message blocks. Mirrors
/// `openAIMessagesToAnthropic`, including the orphan-tool-result handling that
/// keeps strict Anthropic-surface backends (Kimi, MiniMax) from rejecting a
/// `tool_result` whose `tool_use` was dropped from history.
fn openai_messages_to_anthropic(msgs: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut emitted_tool_use_ids = std::collections::HashSet::new();

    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" => continue,
            "assistant" => {
                let mut blocks = Vec::new();
                let text = openai_text(m.get("content"));
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let ty = tc.get("type").and_then(Value::as_str).unwrap_or("");
                        if ty.is_empty() || ty == "function" {
                            let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                            if !id.is_empty() {
                                emitted_tool_use_ids.insert(id.to_string());
                            }
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let args = tc
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": openai_tool_input(args),
                            }));
                        }
                    }
                }
                if blocks.is_empty() {
                    // Strict backends reject an empty assistant turn; a single
                    // space is accepted.
                    blocks.push(json!({"type": "text", "text": " "}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            "tool" => {
                let tool_call_id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if !tool_call_id.is_empty() && emitted_tool_use_ids.contains(tool_call_id) {
                    out.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": openai_text(m.get("content")),
                        }],
                    }));
                } else {
                    let mut text = openai_text(m.get("content"));
                    if text.is_empty() {
                        text = "[tool result]".to_string();
                    }
                    out.push(json!({
                        "role": "user",
                        "content": [{"type": "text", "text": format!("Tool result: {text}")}],
                    }));
                }
            }
            _ => {
                // user or anything else collapses to a user text message.
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": openai_text(m.get("content"))}],
                }));
            }
        }
    }
    out
}

/// Parses an OpenAI tool-call `arguments` string into a JSON object, returning
/// `{}` for blank or non-object input. Mirrors `openAIToolInput`.
fn openai_tool_input(args: &str) -> Value {
    if args.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(args) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

/// Translates OpenAI tool definitions into Anthropic tool definitions. Mirrors
/// `openAIToolsToAnthropic`.
fn openai_tools_to_anthropic(tools: Option<&Value>) -> Vec<Value> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in tools {
        let ty = t.get("type").and_then(Value::as_str).unwrap_or("");
        if !ty.is_empty() && ty != "function" {
            continue;
        }
        let f = t.get("function");
        let name = f
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let desc = f
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let schema = f
            .and_then(|f| f.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        out.push(json!({"name": name, "description": desc, "input_schema": schema}));
    }
    out
}

/// Translates an OpenAI `tool_choice` into an Anthropic one. Mirrors
/// `openAIToolChoice`.
fn openai_tool_choice(raw: Option<&Value>) -> Option<Value> {
    let raw = raw?;
    if raw.is_null() {
        return None;
    }
    if let Some(s) = raw.as_str() {
        return match s {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            _ => None,
        };
    }
    if raw.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = raw
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    {
        return Some(json!({"type": "tool", "name": name}));
    }
    None
}

/// Normalizes an OpenAI `stop` field (string or array) into a list. Mirrors
/// `openAIStops`.
fn openai_stops(raw: Option<&Value>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };
    if raw.is_null() {
        return Vec::new();
    }
    if let Some(s) = raw.as_str() {
        return vec![s.to_string()];
    }
    raw.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ── Anthropic → OpenAI (completion response) ────────────────────────────────

/// Translates an Anthropic `messages` response into an OpenAI chat completion.
/// `now_unix` populates the `created` field. Mirrors `anthropicToOpenAICompletion`.
pub fn anthropic_to_openai_completion(
    raw: &[u8],
    model: &str,
    now_unix: i64,
) -> Result<Value, serde_json::Error> {
    let msg: Value = serde_json::from_slice(raw)?;
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for b in content {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        texts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    let args = if input.is_null() {
                        "{}".to_string()
                    } else {
                        serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                    };
                    calls.push(json!({
                        "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                            "arguments": args,
                        },
                    }));
                }
                _ => {}
            }
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(texts.join("\n")));
    if !calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(calls));
    }

    let usage = msg.get("usage");
    let prompt = anthropic_prompt_tokens(usage);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stop = msg.get("stop_reason").and_then(Value::as_str).unwrap_or("");

    Ok(json!({
        "id": msg.get("id").and_then(Value::as_str).unwrap_or(""),
        "object": "chat.completion",
        "created": now_unix,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": anthropic_stop_to_openai(stop),
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": output,
            "total_tokens": prompt + output,
        },
    }))
}

/// Anthropic prompt tokens = `input + cache_creation + cache_read`. Mirrors
/// `anthropicUsage.promptTokens`.
fn anthropic_prompt_tokens(usage: Option<&Value>) -> u64 {
    let Some(u) = usage else { return 0 };
    let f = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    f("input_tokens") + f("cache_creation_input_tokens") + f("cache_read_input_tokens")
}

/// Maps an Anthropic `stop_reason` to an OpenAI `finish_reason`. Mirrors
/// `anthropicStopToOpenAI`.
pub fn anthropic_stop_to_openai(stop: &str) -> &'static str {
    match stop {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
    }
}

// ── Anthropic → OpenAI (request) ────────────────────────────────────────────

/// Translates an Anthropic `messages` request body into an OpenAI
/// `chat/completions` body (non-streaming; the bridge synthesizes any
/// client-facing stream itself). Mirrors `anthropicToOpenAI`.
pub fn anthropic_to_openai(raw: &[u8]) -> Result<Value, serde_json::Error> {
    let req: Value = serde_json::from_slice(raw)?;
    let mut msgs = Vec::new();

    let system = anthropic_text(req.get("system"));
    if !system.is_empty() {
        msgs.push(json!({"role": "system", "content": system}));
    }

    if let Some(messages) = req.get("messages").and_then(Value::as_array) {
        for m in messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = m.get("content");

            // Plain string content → straight text message.
            if let Some(s) = content.and_then(Value::as_str) {
                msgs.push(json!({"role": role, "content": s}));
                continue;
            }
            let Some(blocks) = content.and_then(Value::as_array) else {
                continue;
            };

            if role == "assistant" {
                let mut text = Vec::new();
                let mut tool_calls = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                text.push(t.to_string());
                            }
                        }
                        Some("tool_use") => {
                            let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                            let args = if input.is_null() {
                                "{}".to_string()
                            } else {
                                serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                            };
                            tool_calls.push(json!({
                                "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                                "type": "function",
                                "function": {
                                    "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                                    "arguments": args,
                                },
                            }));
                        }
                        _ => {}
                    }
                }
                let mut am = Map::new();
                am.insert("role".into(), json!("assistant"));
                am.insert("content".into(), json!(text.join("\n")));
                if !tool_calls.is_empty() {
                    am.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                msgs.push(Value::Object(am));
                continue;
            }

            // user (or other): tool_result blocks become OpenAI tool messages;
            // remaining text becomes a user message.
            let mut user_text = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        msgs.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
                            "content": anthropic_text(b.get("content")),
                        }));
                    }
                    Some("text") => {
                        if let Some(t) = b
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            user_text.push(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            if !user_text.is_empty() {
                msgs.push(json!({"role": "user", "content": user_text.join("\n")}));
            }
        }
    }

    let mut body = Map::new();
    body.insert(
        "model".into(),
        req.get("model").cloned().unwrap_or(json!("")),
    );
    body.insert("messages".into(), Value::Array(msgs));
    body.insert("stream".into(), json!(false));
    if let Some(mt) = req.get("max_tokens").and_then(positive_int) {
        body.insert("max_tokens".into(), json!(mt));
    }
    if let Some(t) = req.get("temperature").filter(|v| v.is_number()) {
        body.insert("temperature".into(), t.clone());
    }
    if let Some(t) = req.get("top_p").filter(|v| v.is_number()) {
        body.insert("top_p".into(), t.clone());
    }
    if let Some(stops) = req
        .get("stop_sequences")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        body.insert("stop".into(), Value::Array(stops.clone()));
    }
    if let Some(tools) = req
        .get("tools")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let mut out_tools = Vec::new();
        for t in tools {
            let params = t
                .get("input_schema")
                .cloned()
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| json!({"type": "object"}));
            out_tools.push(json!({
                "type": "function",
                "function": {
                    "name": t.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": params,
                },
            }));
        }
        body.insert("tools".into(), Value::Array(out_tools));
        if let Some(tc) = anthropic_tool_choice(req.get("tool_choice")) {
            body.insert("tool_choice".into(), tc);
        }
    }
    Ok(Value::Object(body))
}

/// Flattens an Anthropic string-or-blocks field into plain text. Mirrors
/// `anthropicText`.
fn anthropic_text(raw: Option<&Value>) -> String {
    let Some(raw) = raw else { return String::new() };
    if let Some(s) = raw.as_str() {
        return s.to_string();
    }
    let Some(blocks) = raw.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for b in blocks {
        if b.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = b
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            parts.push(t.to_string());
        }
    }
    parts.join("\n")
}

/// Translates an Anthropic `tool_choice` into an OpenAI one. Mirrors the
/// `anthropicToolChoice` in `anthropic.go`.
fn anthropic_tool_choice(raw: Option<&Value>) -> Option<Value> {
    let raw = raw?;
    match raw.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("tool") => Some(json!({
            "type": "function",
            "function": {"name": raw.get("name").and_then(Value::as_str).unwrap_or("")},
        })),
        _ => None,
    }
}

// ── OpenAI → Anthropic (completion response) ────────────────────────────────

/// Maps an OpenAI `finish_reason` to an Anthropic `stop_reason`. Mirrors
/// `mapStopReason`.
pub fn map_stop_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

/// Turns an OpenAI completion's first choice into Anthropic content blocks
/// (text + tool_use). Mirrors `anthropicContentBlocks`.
pub fn anthropic_content_blocks(completion: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();
    let Some(msg) = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
    else {
        return blocks;
    };
    if let Some(text) = msg
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        blocks.push(json!({"type": "text", "text": text}));
    }
    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let input = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                "input": input,
            }));
        }
    }
    blocks
}

// ── Stream synthesis ────────────────────────────────────────────────────────

/// Removes the `stream` field from a request body. Mirrors
/// `openAIRequestWithoutStream`.
pub fn openai_request_without_stream(raw: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(mut obj)) => {
            obj.remove("stream");
            serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| raw.to_vec())
        }
        _ => raw.to_vec(),
    }
}

/// Synthesizes an OpenAI SSE byte stream from a buffered (non-streaming) OpenAI
/// completion, so an Anthropic-native or non-streaming backend can satisfy a
/// streaming client. `now_unix` and `fallback_id` fill missing fields. Mirrors
/// `openAICompletionToStream`. Returns `None` when the completion has no choices.
pub fn openai_completion_to_stream(
    raw: &[u8],
    now_unix: i64,
    fallback_id: &str,
) -> Option<Vec<u8>> {
    let resp: Value = serde_json::from_slice(raw).ok()?;
    let choice = resp
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())?;

    let created = match resp.get("created").and_then(Value::as_i64) {
        Some(c) if c != 0 => c,
        _ => now_unix,
    };
    let id = match resp.get("id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => fallback_id.to_string(),
    };
    let model = resp
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let index = choice.get("index").and_then(Value::as_i64).unwrap_or(0);

    let mut buf = String::new();
    let write_chunk = |buf: &mut String, choice: Value| {
        let chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [choice],
        });
        buf.push_str("data: ");
        buf.push_str(&serde_json::to_string(&chunk).unwrap_or_default());
        buf.push_str("\n\n");
    };

    // Opening role delta.
    write_chunk(
        &mut buf,
        json!({"index": index, "delta": {"role": "assistant"}, "finish_reason": Value::Null}),
    );

    let message = choice.get("message");
    if let Some(content) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        // Break content by line so incremental-streaming clients behave.
        for line in split_after_newline(content) {
            write_chunk(
                &mut buf,
                json!({"index": index, "delta": {"content": line}, "finish_reason": Value::Null}),
            );
        }
    }

    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for (i, tc) in tool_calls.iter().enumerate() {
            let ty = match tc.get("type").and_then(Value::as_str) {
                Some(s) if !s.is_empty() => s,
                _ => "function",
            };
            let delta = json!({"tool_calls": [{
                "index": i,
                "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                "type": ty,
                "function": {
                    "name": tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                    "arguments": tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or(""),
                },
            }]});
            write_chunk(
                &mut buf,
                json!({"index": index, "delta": delta, "finish_reason": Value::Null}),
            );
        }
    }

    let finish = match choice.get("finish_reason").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => "stop",
    };
    write_chunk(
        &mut buf,
        json!({"index": index, "delta": {}, "finish_reason": finish}),
    );

    // Final usage-only chunk so measured token counts survive into the SSE.
    if let Some(usage) = resp.get("usage") {
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let completion = usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let mut total = usage
            .get("total_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if total > 0 || prompt > 0 || completion > 0 {
            if total == 0 {
                total = prompt + completion;
            }
            let usage_chunk = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [],
                "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total},
            });
            buf.push_str("data: ");
            buf.push_str(&serde_json::to_string(&usage_chunk).unwrap_or_default());
            buf.push_str("\n\n");
        }
    }

    buf.push_str("data: [DONE]\n\n");
    Some(buf.into_bytes())
}

/// Splits a string after each `\n`, keeping the newline on each piece (like Go's
/// `strings.SplitAfter(s, "\n")`).
fn split_after_newline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        cur.push(ch);
        if ch == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Positive-integer coercion shared with the request transforms.
fn positive_int(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        (i > 0).then_some(i)
    } else if let Some(f) = n.as_f64() {
        (f > 0.0 && f.fract() == 0.0).then_some(f as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_to_anthropic_basic() {
        let raw = br#"{
            "model": "gpt-x",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 100,
            "stream": true,
            "temperature": 0.5
        }"#;
        let out = openai_to_anthropic(raw, "claude-x").unwrap();
        assert_eq!(out["model"], json!("claude-x"));
        assert_eq!(out["max_tokens"], json!(100));
        assert_eq!(out["system"], json!("be brief"));
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["temperature"], json!(0.5));
        // System message is stripped from messages; only the user remains.
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(msgs[0]["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn default_max_tokens_when_unset() {
        let raw = br#"{"model":"m","messages":[]}"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        assert_eq!(out["max_tokens"], json!(BRIDGE_DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn assistant_tool_calls_become_tool_use() {
        let raw = br#"{
            "model": "m",
            "messages": [
                {"role": "assistant", "content": "calling", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"x\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "result text"}
            ]
        }"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][1]["type"], json!("tool_use"));
        assert_eq!(msgs[0]["content"][1]["id"], json!("call_1"));
        assert_eq!(msgs[0]["content"][1]["input"]["x"], json!(1));
        // tool result references an emitted id → tool_result.
        assert_eq!(msgs[1]["content"][0]["type"], json!("tool_result"));
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], json!("call_1"));
    }

    #[test]
    fn orphan_tool_result_becomes_user_text() {
        let raw = br#"{
            "model": "m",
            "messages": [
                {"role": "tool", "tool_call_id": "missing", "content": "orphaned"}
            ]
        }"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(msgs[0]["content"][0]["type"], json!("text"));
        assert_eq!(
            msgs[0]["content"][0]["text"],
            json!("Tool result: orphaned")
        );
    }

    #[test]
    fn empty_assistant_turn_gets_space() {
        let raw = br#"{"model":"m","messages":[{"role":"assistant","content":""}]}"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["text"], json!(" "));
    }

    #[test]
    fn tool_choice_mappings() {
        assert_eq!(
            openai_tool_choice(Some(&json!("auto"))),
            Some(json!({"type": "auto"}))
        );
        assert_eq!(
            openai_tool_choice(Some(&json!("required"))),
            Some(json!({"type": "any"}))
        );
        assert_eq!(
            openai_tool_choice(Some(
                &json!({"type": "function", "function": {"name": "f"}})
            )),
            Some(json!({"type": "tool", "name": "f"}))
        );
        assert_eq!(openai_tool_choice(Some(&json!("none"))), None);
        assert_eq!(openai_tool_choice(Some(&Value::Null)), None);
    }

    #[test]
    fn content_parts_array_flattens() {
        let raw = br#"{"model":"m","messages":[
            {"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}
        ]}"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        assert_eq!(out["messages"][0]["content"][0]["text"], json!("a\nb"));
    }

    #[test]
    fn anthropic_completion_to_openai() {
        let raw = br#"{
            "id": "msg_1",
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_1", "name": "f", "input": {"a": 1}}
            ],
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 7}
        }"#;
        let out = anthropic_to_openai_completion(raw, "claude-x", 1234).unwrap();
        assert_eq!(out["id"], json!("msg_1"));
        assert_eq!(out["created"], json!(1234));
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(out["choices"][0]["message"]["content"], json!("hello"));
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["id"],
            json!("tu_1")
        );
        // prompt = 10 + 0 + 5 = 15; total = 15 + 7.
        assert_eq!(out["usage"]["prompt_tokens"], json!(15));
        assert_eq!(out["usage"]["completion_tokens"], json!(7));
        assert_eq!(out["usage"]["total_tokens"], json!(22));
    }

    #[test]
    fn stop_reason_maps() {
        assert_eq!(anthropic_stop_to_openai("tool_use"), "tool_calls");
        assert_eq!(anthropic_stop_to_openai("max_tokens"), "length");
        assert_eq!(anthropic_stop_to_openai("end_turn"), "stop");
        assert_eq!(map_stop_reason("length"), "max_tokens");
        assert_eq!(map_stop_reason("tool_calls"), "tool_use");
        assert_eq!(map_stop_reason("content_filter"), "end_turn");
        assert_eq!(map_stop_reason(""), "end_turn");
    }

    #[test]
    fn anthropic_request_to_openai_roundtrips_tools() {
        let raw = br#"{
            "model": "claude-x",
            "system": "sys",
            "max_tokens": 50,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {"x": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "done"}
                ]}
            ],
            "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"}
        }"#;
        let out = anthropic_to_openai(raw).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[1]["content"], json!("hi"));
        assert_eq!(msgs[2]["tool_calls"][0]["id"], json!("t1"));
        assert_eq!(msgs[3]["role"], json!("tool"));
        assert_eq!(msgs[3]["tool_call_id"], json!("t1"));
        assert_eq!(out["tool_choice"], json!("auto"));
        assert_eq!(out["tools"][0]["function"]["name"], json!("f"));
        assert_eq!(out["max_tokens"], json!(50));
    }

    #[test]
    fn completion_to_stream_synthesizes_sse() {
        let raw = br#"{
            "id": "c1",
            "model": "m",
            "created": 99,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "line1\nline2"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        }"#;
        let sse =
            String::from_utf8(openai_completion_to_stream(raw, 1, "fallback").unwrap()).unwrap();
        assert!(sse.contains("\"role\":\"assistant\""));
        assert!(sse.contains("\"content\":\"line1\\n\""));
        assert!(sse.contains("\"content\":\"line2\""));
        assert!(sse.contains("\"finish_reason\":\"stop\""));
        assert!(sse.contains("\"total_tokens\":7"));
        assert!(sse.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn completion_to_stream_none_without_choices() {
        assert!(openai_completion_to_stream(br#"{"choices":[]}"#, 1, "f").is_none());
    }

    #[test]
    fn request_without_stream_strips_field() {
        let out = openai_request_without_stream(br#"{"model":"m","stream":true}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("stream").is_none());
        assert_eq!(v["model"], json!("m"));
    }

    #[test]
    fn anthropic_content_blocks_from_completion() {
        let c = json!({
            "choices": [{"message": {"content": "hi", "tool_calls": [
                {"id": "x", "function": {"name": "f", "arguments": "{\"a\":2}"}}
            ]}}]
        });
        let blocks = anthropic_content_blocks(&c);
        assert_eq!(blocks[0], json!({"type": "text", "text": "hi"}));
        assert_eq!(blocks[1]["type"], json!("tool_use"));
        assert_eq!(blocks[1]["input"]["a"], json!(2));
    }

    #[test]
    fn anthropic_content_blocks_empty_without_choices() {
        assert!(anthropic_content_blocks(&json!({"choices": []})).is_empty());
        assert!(anthropic_content_blocks(&json!({})).is_empty());
    }

    #[test]
    fn openai_to_anthropic_all_optional_fields() {
        // Exercises top_p, stop array, tools, tool_choice, and the
        // max_completion_tokens branch.
        let raw = br#"{
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "input_text", "input_text": "hey"}]}],
            "max_completion_tokens": 8000,
            "top_p": 0.9,
            "stop": ["X", "Y"],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": "auto"
        }"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        assert_eq!(out["max_tokens"], json!(8000));
        assert_eq!(out["top_p"], json!(0.9));
        assert_eq!(out["stop_sequences"], json!(["X", "Y"]));
        assert_eq!(out["tools"][0]["name"], json!("f"));
        assert_eq!(out["tool_choice"], json!({"type": "auto"}));
        // input_text part flattened.
        assert_eq!(out["messages"][0]["content"][0]["text"], json!("hey"));
    }

    #[test]
    fn openai_stops_single_string() {
        let raw = br#"{"model":"m","messages":[],"stop":"STOP"}"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        assert_eq!(out["stop_sequences"], json!(["STOP"]));
    }

    #[test]
    fn float_max_tokens_coerced() {
        let raw = br#"{"model":"m","messages":[],"max_tokens":2048.0}"#;
        let out = openai_to_anthropic(raw, "c").unwrap();
        assert_eq!(out["max_tokens"], json!(2048));
    }

    #[test]
    fn anthropic_request_full_options() {
        // User message as a text-block array, a tool with no input_schema,
        // tool_choice "any", plus temperature/top_p/stop_sequences.
        let raw = br#"{
            "model": "c",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}
            ],
            "temperature": 0.3,
            "top_p": 0.8,
            "stop_sequences": ["END"],
            "tools": [{"name": "f", "description": "d"}],
            "tool_choice": {"type": "any"}
        }"#;
        let out = anthropic_to_openai(raw).unwrap();
        assert_eq!(out["temperature"], json!(0.3));
        assert_eq!(out["top_p"], json!(0.8));
        assert_eq!(out["stop"], json!(["END"]));
        // text blocks joined into a user message.
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.last().unwrap()["content"], json!("a\nb"));
        // tool with no input_schema gets a default object schema.
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
        assert_eq!(out["tool_choice"], json!("required"));
    }

    #[test]
    fn anthropic_tool_choice_tool_variant() {
        let raw = br#"{"model":"c","messages":[],"tools":[{"name":"f"}],"tool_choice":{"type":"tool","name":"f"}}"#;
        let out = anthropic_to_openai(raw).unwrap();
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "function": {"name": "f"}})
        );
    }

    #[test]
    fn anthropic_string_assistant_content() {
        let raw = br#"{"model":"c","messages":[{"role":"assistant","content":"plain"}]}"#;
        let out = anthropic_to_openai(raw).unwrap();
        assert_eq!(out["messages"][0]["content"], json!("plain"));
    }

    #[test]
    fn anthropic_text_flattens_blocks_and_handles_missing() {
        assert_eq!(anthropic_text(None), "");
        assert_eq!(anthropic_text(Some(&json!(42))), ""); // not string/array
    }

    #[test]
    fn completion_to_stream_with_tool_calls() {
        let raw = br#"{
            "id": "c1",
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "tool_calls": [
                {"id": "t1", "function": {"name": "f", "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"}]
        }"#;
        // created=0 and no usage exercises the now_unix fallback + skipped usage chunk.
        let sse = String::from_utf8(openai_completion_to_stream(raw, 7, "fb").unwrap()).unwrap();
        assert!(sse.contains("\"tool_calls\""));
        assert!(sse.contains("\"name\":\"f\""));
        assert!(sse.contains("\"finish_reason\":\"tool_calls\""));
        assert!(sse.contains("\"created\":7")); // fallback now_unix
        assert!(!sse.contains("usage")); // no usage chunk
    }

    #[test]
    fn completion_to_stream_uses_fallback_id() {
        let raw = br#"{"choices":[{"index":0,"message":{"content":"hi"}}]}"#;
        let sse =
            String::from_utf8(openai_completion_to_stream(raw, 1, "fallback-id").unwrap()).unwrap();
        assert!(sse.contains("fallback-id"));
        // missing finish_reason defaults to "stop".
        assert!(sse.contains("\"finish_reason\":\"stop\""));
    }

    #[test]
    fn request_without_stream_passthrough_on_nonobject() {
        // A non-object body is returned unchanged.
        assert_eq!(openai_request_without_stream(b"[1,2]"), b"[1,2]");
    }

    #[test]
    fn anthropic_completion_tool_use_null_input() {
        let raw = br#"{"id":"m","content":[{"type":"tool_use","id":"t","name":"f","input":null}]}"#;
        let out = anthropic_to_openai_completion(raw, "x", 0).unwrap();
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            json!("{}")
        );
    }
}
