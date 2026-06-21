//! Request-body transforms applied before dispatch.
//!
//! Port of `ensureMaxTokens` / `applyBackendDefaults` / `requestHasTools` /
//! `estimatedRequestTokens` / `exceedsContextLimit` / `ensureStreamUsage` from
//! `main.go`. The Go versions take and return raw bytes (parsing each time); the
//! Rust proxy parses the body once into a [`serde_json::Value`] and applies these
//! transforms in place, re-serializing at dispatch.

use serde_json::{Map, Value, json};

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
}
