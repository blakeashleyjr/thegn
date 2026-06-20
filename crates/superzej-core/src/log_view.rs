//! Parsed view of the szhost plain-text log file.
//!
//! The `Brand` formatter in `log.rs` writes each line as:
//!   `{timestamp}  {level:<5} {target}  {message}`
//!
//! This module provides the types and parsing logic for the Logs panel section.

/// Severity level of a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "ERROR" => Some(Self::Error),
            "WARN" => Some(Self::Warn),
            "INFO" => Some(Self::Info),
            "DEBUG" => Some(Self::Debug),
            "TRACE" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Error => "✗",
            Self::Warn => "!",
            Self::Info => "·",
            Self::Debug => "○",
            Self::Trace => "·",
        }
    }

    /// Cycle to the next level for the panel level-filter. Returns `None` after
    /// `Trace`, which the caller maps back to "show all".
    pub fn next_cycle(self) -> Option<Self> {
        match self {
            Self::Error => Some(Self::Warn),
            Self::Warn => Some(Self::Info),
            Self::Info => Some(Self::Debug),
            Self::Debug => Some(Self::Trace),
            Self::Trace => None,
        }
    }
}

/// One parsed line from the szhost log file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub timestamp: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    /// The original unparsed line, used for copy and export.
    pub raw: String,
}

/// Parse one line from the plain-text Brand-formatted log.
///
/// Format: `{ts}  {level:<5} {target}  {message}`
/// Returns `None` for blank lines, rotation markers, or any line that does
/// not match the four-field structure.
pub fn parse_log_line(line: &str) -> Option<LogLine> {
    // Step 1: split at first double-space to isolate the timestamp.
    let (ts, rest) = line.split_once("  ")?;
    // Step 2: level is the first whitespace-separated token.
    let (level_str, rest) = rest.split_once(' ')?;
    let level = LogLevel::parse(level_str.trim())?;
    // Step 3: skip any padding space between level and target, then find
    // the double-space delimiter that separates target from message.
    let (target, message) = rest.trim_start().split_once("  ")?;
    Some(LogLine {
        timestamp: ts.to_string(),
        level,
        target: target.trim().to_string(),
        message: message.to_string(),
        raw: line.to_string(),
    })
}

/// Return the path to the szhost log file, honouring the `[log]` config.
pub fn log_file_path(cfg: &crate::config::LogConfig) -> std::path::PathBuf {
    cfg.dir_path().join("szhost.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO_LINE: &str = "2026-06-05T12:00:00  INFO  superzej::db  connection opened";
    const ERROR_LINE: &str = "2026-06-05T12:00:01  ERROR superzej::host  fatal error occurred";
    const WARN_LINE: &str = "2026-06-05T12:00:02  WARN  superzej::panel  slow render";

    #[test]
    fn parse_log_line_valid_info() {
        let l = parse_log_line(INFO_LINE).unwrap();
        assert_eq!(l.timestamp, "2026-06-05T12:00:00");
        assert_eq!(l.level, LogLevel::Info);
        assert_eq!(l.target, "superzej::db");
        assert_eq!(l.message, "connection opened");
        assert_eq!(l.raw, INFO_LINE);
    }

    #[test]
    fn parse_log_line_valid_error() {
        let l = parse_log_line(ERROR_LINE).unwrap();
        assert_eq!(l.level, LogLevel::Error);
        assert_eq!(l.target, "superzej::host");
        assert_eq!(l.message, "fatal error occurred");
    }

    #[test]
    fn parse_log_line_valid_warn() {
        let l = parse_log_line(WARN_LINE).unwrap();
        assert_eq!(l.level, LogLevel::Warn);
        assert_eq!(l.target, "superzej::panel");
    }

    #[test]
    fn parse_log_line_rejects_blank() {
        assert!(parse_log_line("").is_none());
        assert!(parse_log_line("   ").is_none());
    }

    #[test]
    fn parse_log_line_rejects_short() {
        assert!(parse_log_line("2026-06-05T12:00:00  INFO").is_none());
        assert!(parse_log_line("not a log line at all").is_none());
    }

    #[test]
    fn parse_log_line_message_with_double_space() {
        let line = "2026-06-05T12:00:00  INFO  superzej::db  msg  with  spaces";
        let l = parse_log_line(line).unwrap();
        // split_once stops at the first double-space after target; everything
        // after that (including internal double-spaces) is the message.
        assert_eq!(l.message, "msg  with  spaces");
    }

    #[test]
    fn parse_log_line_all_levels() {
        let levels = [
            ("ERROR superzej::x  m", LogLevel::Error),
            ("WARN  superzej::x  m", LogLevel::Warn),
            ("INFO  superzej::x  m", LogLevel::Info),
            ("DEBUG superzej::x  m", LogLevel::Debug),
            ("TRACE superzej::x  m", LogLevel::Trace),
        ];
        for (suffix, expected) in levels {
            let line = format!("2026-06-05T12:00:00  {suffix}");
            let parsed = parse_log_line(&line).unwrap_or_else(|| panic!("failed to parse: {line}"));
            assert_eq!(parsed.level, expected, "line: {line}");
        }
    }

    #[test]
    fn log_level_cycle_wraps_to_none() {
        assert_eq!(LogLevel::Error.next_cycle(), Some(LogLevel::Warn));
        assert_eq!(LogLevel::Warn.next_cycle(), Some(LogLevel::Info));
        assert_eq!(LogLevel::Info.next_cycle(), Some(LogLevel::Debug));
        assert_eq!(LogLevel::Debug.next_cycle(), Some(LogLevel::Trace));
        assert_eq!(LogLevel::Trace.next_cycle(), None);
    }

    #[test]
    fn log_level_ordering() {
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
    }

    #[test]
    fn log_file_path_ends_with_szhost_log() {
        let cfg = crate::config::LogConfig::default();
        let p = log_file_path(&cfg);
        assert!(p.ends_with("szhost.log"), "got: {p:?}");
    }
}
