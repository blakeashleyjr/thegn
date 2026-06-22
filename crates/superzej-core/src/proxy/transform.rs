//! Request-body transforms applied before dispatch.
//!
//! Port of `ensureMaxTokens` / `applyBackendDefaults` / `requestHasTools` /
//! `estimatedRequestTokens` / `exceedsContextLimit` / `ensureStreamUsage` from
//! `main.go`. The Go versions take and return raw bytes (parsing each time); the
//! Rust proxy parses the body once into a [`serde_json::Value`] and applies these
//! transforms in place, re-serializing at dispatch.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::proxy::compress::{Level, Limits, compress};

/// Minimum `max_tokens` the proxy will send. DeepSeek and others default to a
/// tiny limit (4096) that truncates tool calls, so a missing or too-small value
/// is bumped to this floor.
pub const MIN_MAX_TOKENS: i64 = 32768;

/// Ensures `max_tokens` is at least [`MIN_MAX_TOKENS`]. Reads either
/// `max_tokens` or `max_completion_tokens`; when bumping, normalizes onto
/// `max_tokens` and drops `max_completion_tokens`. Returns `true` if the body was
/// modified. Mirrors `ensureMaxTokens`.
pub fn ensure_max_tokens(body: &mut Value) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let current = obj
        .get("max_tokens")
        .and_then(positive_int)
        .or_else(|| obj.get("max_completion_tokens").and_then(positive_int))
        .unwrap_or(0);

    if current > 0 && current < MIN_MAX_TOKENS {
        obj.insert("max_tokens".into(), json!(MIN_MAX_TOKENS));
        obj.remove("max_completion_tokens");
        true
    } else if current == 0 {
        obj.insert("max_tokens".into(), json!(MIN_MAX_TOKENS));
        true
    } else {
        false
    }
}

/// Merges a backend's default body params for keys the caller did not set, so
/// explicit caller values win. Returns `true` if anything changed. Mirrors
/// `applyBackendDefaults`.
pub fn apply_backend_defaults(body: &mut Value, defaults: &Map<String, Value>) -> bool {
    if defaults.is_empty() {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for (k, v) in defaults {
        if !obj.contains_key(k) {
            obj.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    changed
}

/// Whether the request carries a non-empty `tools` array (the caller wants
/// function calling). Mirrors `requestHasTools`.
pub fn request_has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
}

/// Rough upper-bound estimate of a request's prompt size (whole serialized body
/// divided by ~4 chars/token). Slightly over-counts, which is the safe direction
/// for a context-window skip guard. Mirrors `estimatedRequestTokens`.
pub fn estimated_request_tokens(serialized_len: usize) -> usize {
    serialized_len / 4
}

/// Whether the request is too large for a backend's known context window. A
/// `context_limit` of 0 (unknown) is never skipped. Mirrors `exceedsContextLimit`.
pub fn exceeds_context_limit(context_limit: usize, est_tokens: usize) -> bool {
    context_limit > 0 && est_tokens > context_limit
}

/// Sets `stream_options.include_usage = true` so OpenAI-compatible upstreams
/// emit a final usage chunk on streamed requests. Mirrors `ensureStreamUsage`.
pub fn ensure_stream_usage(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let opts = obj.entry("stream_options").or_insert_with(|| json!({}));
    if let Some(opts_obj) = opts.as_object_mut() {
        opts_obj.insert("include_usage".into(), json!(true));
    } else {
        *opts = json!({ "include_usage": true });
    }
}

/// Converts a JSON value to an `i64` only for positive integers (matching Go's
/// `toPositiveInt`, which accepts integral floats too).
fn positive_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                (i > 0).then_some(i)
            } else if let Some(f) = n.as_f64() {
                if f > 0.0 && f.fract() == 0.0 {
                    Some(f as i64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

// ── In-flight tool-output compression (group W) ─────────────────────────────

/// Policy controlling tool-output token reduction. Built by the proxy from
/// config; pure data so the transform stays testable.
#[derive(Debug, Default, Clone)]
pub struct CompressPolicy {
    pub level: Level,
    pub limits: Limits,
    /// Tool names never compressed (per-command bypass, W 304).
    pub bypass_tools: Vec<String>,
    /// If set, ONLY these tool names are compressed (e.g. route file-reads
    /// through it, W 305).
    pub only_tools: Option<Vec<String>>,
    /// Custom pre-filters applied before compression (W 308): each regex match
    /// is replaced by its paired string.
    pub filters: Vec<(Regex, String)>,
}

impl CompressPolicy {
    /// A disabled policy (no compression).
    pub fn off() -> Self {
        Self {
            level: Level::Off,
            ..Default::default()
        }
    }

    pub fn enabled(&self) -> bool {
        self.level != Level::Off
    }

    /// Whether a tool message with the given (resolved) tool name should be
    /// compressed under this policy.
    fn should_compress(&self, tool_name: Option<&str>) -> bool {
        match tool_name {
            Some(name) => {
                if self.bypass_tools.iter().any(|b| b == name) {
                    return false;
                }
                match &self.only_tools {
                    Some(only) => only.iter().any(|o| o == name),
                    None => true,
                }
            }
            // Unknown tool name: compress unless an allow-list is in force.
            None => self.only_tools.is_none(),
        }
    }
}

/// Applies a policy's custom regex filters to a string, in order.
fn apply_filters(s: &str, filters: &[(Regex, String)]) -> String {
    let mut out = s.to_string();
    for (re, rep) in filters {
        out = re.replace_all(&out, rep.as_str()).into_owned();
    }
    out
}

/// Compresses the content of `role == "tool"` messages in place, returning the
/// total characters removed. Only tool messages are touched — the system prompt,
/// tool definitions, and assistant turns (the cacheable prefix) are left
/// byte-identical. Mirrors the pure-transform style of the rest of this module.
pub fn compress_tool_messages(body: &mut Value, policy: &CompressPolicy) -> usize {
    if !policy.enabled() {
        return 0;
    }
    // First pass (immutable): map tool_call_id → tool name from assistant turns,
    // so bypass/allow-list decisions can key off the originating command.
    let mut name_by_id: HashMap<String, String> = HashMap::new();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for m in messages {
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    if let (Some(id), Some(name)) = (
                        tc.get("id").and_then(Value::as_str),
                        tc.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str),
                    ) {
                        name_by_id.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut saved = 0usize;
    for m in messages.iter_mut() {
        if m.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let tool_name = m
            .get("tool_call_id")
            .and_then(Value::as_str)
            .and_then(|id| name_by_id.get(id))
            .map(String::as_str);
        if !policy.should_compress(tool_name) {
            continue;
        }
        match m.get_mut("content") {
            Some(Value::String(s)) => {
                saved += compress_in_place(s, policy);
            }
            Some(Value::Array(parts)) => {
                for part in parts.iter_mut() {
                    if let Some(Value::String(t)) = part.get_mut("text") {
                        saved += compress_in_place(t, policy);
                    }
                }
            }
            _ => {}
        }
    }
    saved
}

/// Filters then compresses a single content string in place, returning chars saved.
fn compress_in_place(s: &mut String, policy: &CompressPolicy) -> usize {
    let original = s.chars().count();
    let filtered = apply_filters(s, &policy.filters);
    let result = compress(&filtered, policy.level, policy.limits);
    let saved = original.saturating_sub(result.text.chars().count());
    *s = result.text;
    saved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn injects_default_when_unset() {
        let mut b = json!({"model": "x"});
        assert!(ensure_max_tokens(&mut b));
        assert_eq!(b["max_tokens"], json!(MIN_MAX_TOKENS));
    }

    #[test]
    fn bumps_too_small_and_drops_completion_variant() {
        let mut b = json!({"max_completion_tokens": 4096});
        assert!(ensure_max_tokens(&mut b));
        assert_eq!(b["max_tokens"], json!(MIN_MAX_TOKENS));
        assert!(b.get("max_completion_tokens").is_none());
    }

    #[test]
    fn leaves_large_enough_untouched() {
        let mut b = json!({"max_tokens": 100000});
        assert!(!ensure_max_tokens(&mut b));
        assert_eq!(b["max_tokens"], json!(100000));
    }

    #[test]
    fn defaults_do_not_override_caller() {
        let mut b = json!({"reasoning_effort": "low"});
        let mut defaults = Map::new();
        defaults.insert("reasoning_effort".into(), json!("high"));
        defaults.insert("verbosity".into(), json!("high"));
        assert!(apply_backend_defaults(&mut b, &defaults));
        assert_eq!(b["reasoning_effort"], json!("low")); // caller wins
        assert_eq!(b["verbosity"], json!("high")); // injected
    }

    #[test]
    fn empty_defaults_noop() {
        let mut b = json!({"a": 1});
        assert!(!apply_backend_defaults(&mut b, &Map::new()));
    }

    #[test]
    fn tools_detection() {
        assert!(request_has_tools(&json!({"tools": [{"type": "function"}]})));
        assert!(!request_has_tools(&json!({"tools": []})));
        assert!(!request_has_tools(&json!({})));
    }

    #[test]
    fn context_guard() {
        assert!(exceeds_context_limit(1000, 1001));
        assert!(!exceeds_context_limit(1000, 1000));
        assert!(!exceeds_context_limit(0, 999_999)); // unknown == never skip
        assert_eq!(estimated_request_tokens(4000), 1000);
    }

    #[test]
    fn stream_usage_merges_existing() {
        let mut b = json!({"stream_options": {"foo": 1}});
        ensure_stream_usage(&mut b);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
        assert_eq!(b["stream_options"]["foo"], json!(1));
    }

    #[test]
    fn stream_usage_creates_when_missing() {
        let mut b = json!({"model": "x"});
        ensure_stream_usage(&mut b);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn non_object_bodies_are_left_alone() {
        let mut arr = json!([1, 2]);
        assert!(!ensure_max_tokens(&mut arr));
        assert!(!apply_backend_defaults(&mut arr, &{
            let mut m = Map::new();
            m.insert("a".into(), json!(1));
            m
        }));
        ensure_stream_usage(&mut arr); // no panic, no change
        assert_eq!(arr, json!([1, 2]));
    }

    #[test]
    fn float_and_non_integer_max_tokens() {
        // Integral float is accepted as the current value (≥ min → untouched).
        let mut b = json!({"max_tokens": 40000.0});
        assert!(!ensure_max_tokens(&mut b));
        // A non-integral / non-positive value is treated as unset → injects default.
        let mut b = json!({"max_tokens": 0});
        assert!(ensure_max_tokens(&mut b));
        assert_eq!(b["max_tokens"], json!(MIN_MAX_TOKENS));
    }

    #[test]
    fn stream_options_replaced_when_not_object() {
        let mut b = json!({"stream_options": "weird"});
        ensure_stream_usage(&mut b);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
    }

    fn balanced_policy() -> CompressPolicy {
        CompressPolicy {
            level: Level::Balanced,
            ..Default::default()
        }
    }

    #[test]
    fn compresses_tool_content_only() {
        let mut b = json!({
            "messages": [
                {"role": "system", "content": "sys  with  spaces"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "\u{1b}[31mERR\u{1b}[0m\n\n\n\n\nout"},
            ]
        });
        let saved = compress_tool_messages(&mut b, &balanced_policy());
        assert!(saved > 0);
        // Tool content compressed (ANSI gone, blanks collapsed).
        assert_eq!(b["messages"][2]["content"], json!("ERR\n\nout"));
        // System prompt left byte-identical (cacheable prefix untouched).
        assert_eq!(b["messages"][0]["content"], json!("sys  with  spaces"));
    }

    #[test]
    fn disabled_policy_is_noop() {
        let mut b = json!({"messages": [{"role": "tool", "tool_call_id": "x", "content": "\u{1b}[31mhi\u{1b}[0m"}]});
        let before = b.clone();
        assert_eq!(compress_tool_messages(&mut b, &CompressPolicy::off()), 0);
        assert_eq!(b, before);
    }

    #[test]
    fn bypass_tool_passes_through() {
        let mut b = json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "read_file"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "\u{1b}[31mkeep\u{1b}[0m"},
            ]
        });
        let policy = CompressPolicy {
            level: Level::Conservative,
            bypass_tools: vec!["read_file".into()],
            ..Default::default()
        };
        assert_eq!(compress_tool_messages(&mut b, &policy), 0);
        assert_eq!(
            b["messages"][1]["content"],
            json!("\u{1b}[31mkeep\u{1b}[0m")
        );
    }

    #[test]
    fn only_tools_allow_list() {
        let mut b = json!({
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "a", "function": {"name": "bash"}},
                    {"id": "b", "function": {"name": "grep"}}
                ]},
                {"role": "tool", "tool_call_id": "a", "content": "x  y"},
                {"role": "tool", "tool_call_id": "b", "content": "x  y"},
            ]
        });
        let policy = CompressPolicy {
            level: Level::Aggressive,
            only_tools: Some(vec!["bash".into()]),
            ..Default::default()
        };
        compress_tool_messages(&mut b, &policy);
        assert_eq!(b["messages"][1]["content"], json!("x y")); // bash compressed
        assert_eq!(b["messages"][2]["content"], json!("x  y")); // grep untouched
    }

    #[test]
    fn custom_filter_applied_before_compression() {
        let mut b = json!({"messages": [{"role": "tool", "tool_call_id": "x", "content": "SECRET=abc123 done"}]});
        let policy = CompressPolicy {
            level: Level::Conservative,
            filters: vec![(Regex::new(r"SECRET=\S+").unwrap(), "SECRET=***".into())],
            ..Default::default()
        };
        compress_tool_messages(&mut b, &policy);
        assert_eq!(b["messages"][0]["content"], json!("SECRET=*** done"));
    }

    #[test]
    fn compresses_array_content_text_parts() {
        let mut b = json!({
            "messages": [{"role": "tool", "tool_call_id": "x", "content": [
                {"type": "text", "text": "a\n\n\n\n\nb"}
            ]}]
        });
        compress_tool_messages(&mut b, &balanced_policy());
        assert_eq!(b["messages"][0]["content"][0]["text"], json!("a\n\nb"));
    }
}
