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
}

pub fn parse_log(line: &str) -> ParsedLog {
    let original = line.to_string();

    // Fast path: Try parsing as JSON first
    if let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(line) {
        let level = extract_level(&mut map).unwrap_or(LogLevel::Info);
        let message = extract_message(&mut map).unwrap_or_default();
        let timestamp = extract_timestamp(&mut map).unwrap_or_else(|| Local::now().to_rfc3339());

        return ParsedLog {
            timestamp,
            level,
            message,
            original,
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

    ParsedLog {
        timestamp,
        level,
        message,
        original,
    }
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
