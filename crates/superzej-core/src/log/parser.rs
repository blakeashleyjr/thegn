use chrono::{DateTime, Local};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    pub fn parse(s: &str) -> Self {
        let lower = s.to_lowercase();
        if lower.contains("fat") {
            LogLevel::Fatal
        } else if lower.contains("err") {
            LogLevel::Error
        } else if lower.contains("warn") {
            LogLevel::Warn
        } else if lower.contains("inf") {
            LogLevel::Info
        } else if lower.contains("deb") {
            LogLevel::Debug
        } else if lower.contains("trac") {
            LogLevel::Trace
        } else {
            LogLevel::Info // Default
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLog {
    pub timestamp: String,
    pub level: LogLevel,
    pub message: String,
    pub original: String,
    /// The worktree this line was emitted for (`wt=<slug>` in the text sink or a
    /// `wt` JSON field), or `None` for host-global lines (startup, render, etc).
    /// The Logs panel filters on this: by default it shows this-worktree + global
    /// lines and hides ones tagged with a *different* worktree.
    pub worktree: Option<String>,
}

pub fn parse_log(line: &str) -> ParsedLog {
    let original = line.to_string();

    // Fast path: Try parsing as JSON first
    if let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(line) {
        let level = extract_level(&mut map).unwrap_or(LogLevel::Info);
        let worktree = extract_worktree(&mut map);
        let message = extract_message(&mut map).unwrap_or_default();
        let timestamp = extract_timestamp(&mut map).unwrap_or_else(|| Local::now().to_rfc3339());

        return ParsedLog {
            timestamp,
            level,
            message,
            original,
            worktree,
        };
    }

    // Fallback: Plain text logfmt heuristic (time level msg)
    // Very rudimentary fallback
    let parts: Vec<&str> = line.split_whitespace().collect();

    let (timestamp, level, message) = if parts.len() >= 3 {
        // Try to parse parts[1] as level
        let lvl = LogLevel::parse(parts[1]);
        (Local::now().to_rfc3339(), lvl, parts[2..].join(" "))
    } else {
        (Local::now().to_rfc3339(), LogLevel::Info, original.clone())
    };

    // Lift the ` wt=<slug> ` attribution token out of the message (see
    // `log_trace::Brand`), so it filters/displays as structure, not free text.
    let (worktree, message) = strip_wt_token(&message);

    ParsedLog {
        timestamp,
        level,
        message,
        original,
        worktree,
    }
}

/// Pull a `wt=<slug>` token out of a text-format message, returning the slug (if
/// any) and the message with that token removed. Tolerates the token appearing
/// anywhere (the formatter places it right after the target).
fn strip_wt_token(message: &str) -> (Option<String>, String) {
    let mut wt = None;
    let kept: Vec<&str> = message
        .split_whitespace()
        .filter(|tok| {
            if let Some(slug) = tok.strip_prefix("wt=") {
                if !slug.is_empty() {
                    wt = Some(slug.to_string());
                }
                false // drop the token from the visible message
            } else {
                true
            }
        })
        .collect();
    (wt, kept.join(" "))
}

fn extract_worktree(map: &mut serde_json::Map<String, Value>) -> Option<String> {
    map.remove("wt")
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
}

fn extract_level(map: &mut serde_json::Map<String, Value>) -> Option<LogLevel> {
    let keys = ["level", "lvl", "severity"];
    for k in keys {
        if let Some(v) = map.remove(k)
            && let Some(s) = v.as_str()
        {
            return Some(LogLevel::parse(s));
        }
    }
    None
}

fn extract_message(map: &mut serde_json::Map<String, Value>) -> Option<String> {
    let keys = ["msg", "message", "text"];
    for k in keys {
        if let Some(v) = map.remove(k)
            && let Some(s) = v.as_str()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_timestamp(map: &mut serde_json::Map<String, Value>) -> Option<String> {
    let keys = ["ts", "time", "timestamp"];
    for k in keys {
        if let Some(v) = map.remove(k)
            && let Some(s) = v.as_str()
            && let Ok(dt) = DateTime::parse_from_rfc3339(s)
        {
            return Some(dt.with_timezone(&Local).to_rfc3339());
        }
    }
    None
}

#[cfg(test)]
mod spec {
    use super::*;

    #[test]
    fn text_line_extracts_and_strips_wt_token() {
        // Matches the `Brand` text sink: `TS  LEVEL target  wt=<slug>  message`.
        let p = parse_log("2026-07-03T10:00:00  WARN  szhost::provision  wt=sz-solid-glen  boom");
        assert_eq!(p.level, LogLevel::Warn);
        assert_eq!(p.worktree.as_deref(), Some("sz-solid-glen"));
        // The wt token is lifted out of the visible message.
        assert!(
            !p.message.contains("wt="),
            "message still has token: {}",
            p.message
        );
        assert!(p.message.contains("boom"));
    }

    #[test]
    fn text_line_without_tag_is_global() {
        let p = parse_log("2026-07-03T10:00:00  INFO  szhost::startup  first frame");
        assert!(p.worktree.is_none());
        assert!(p.message.contains("first frame"));
    }

    #[test]
    fn json_line_extracts_wt_field() {
        let p = parse_log(r#"{"level":"error","wt":"app-feat","msg":"exploded"}"#);
        assert_eq!(p.level, LogLevel::Error);
        assert_eq!(p.worktree.as_deref(), Some("app-feat"));
        assert_eq!(p.message, "exploded");
    }

    #[test]
    fn empty_wt_slug_is_treated_as_global() {
        let (wt, msg) = strip_wt_token("wt= leftover");
        assert!(wt.is_none());
        assert_eq!(msg, "leftover");
    }
}
