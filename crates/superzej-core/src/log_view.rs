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

/// Build a bounded tail that always carries the recent ERROR lines.
///
/// A plain last-`context` slice of an append-only log rarely contains any ERROR
/// line (errors are sparse and scroll out), which left the notification → log
/// drilldown — opened error-gated — showing "no matching log lines" for an error
/// it had just counted. This returns the last `context` lines **plus** the most
/// recent `max_errors` ERROR lines that fall *before* that window, concatenated
/// in original file order (pre-window errors ascending, then the contiguous
/// window). The two index ranges are disjoint, so no dedup is needed. Callers
/// count errors over the same `all_lines`, so whenever the count is non-zero the
/// result contains at least one ERROR line. The payload is bounded at
/// `context + max_errors`.
pub fn error_inclusive_tail(
    all_lines: &[LogLine],
    context: usize,
    max_errors: usize,
) -> Vec<LogLine> {
    let window_start = all_lines.len().saturating_sub(context);
    // Most-recent ERROR indices that fall before the tail window.
    let mut pre_errors: Vec<usize> = all_lines[..window_start]
        .iter()
        .enumerate()
        .filter(|(_, l)| l.level == LogLevel::Error)
        .map(|(i, _)| i)
        .collect();
    // Keep only the newest `max_errors`, then restore ascending order.
    if pre_errors.len() > max_errors {
        pre_errors.drain(..pre_errors.len() - max_errors);
    }
    let mut out = Vec::with_capacity(pre_errors.len() + (all_lines.len() - window_start));
    out.extend(pre_errors.into_iter().map(|i| all_lines[i].clone()));
    out.extend_from_slice(&all_lines[window_start..]);
    out
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

    #[test]
    fn log_level_parse_accepts_lowercase_and_rejects_unknown() {
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("Error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Trace));
        assert_eq!(LogLevel::parse("FATAL"), None);
        assert_eq!(LogLevel::parse(""), None);
    }

    #[test]
    fn log_level_label_and_glyph_for_all_levels() {
        let all = [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ];
        for lvl in all {
            // label round-trips through parse.
            assert_eq!(LogLevel::parse(lvl.label()), Some(lvl), "label {lvl:?}");
            // glyph is a non-empty marker.
            assert!(!lvl.glyph().is_empty(), "glyph {lvl:?}");
        }
        assert_eq!(LogLevel::Error.glyph(), "✗");
        assert_eq!(LogLevel::Warn.glyph(), "!");
        assert_eq!(LogLevel::Error.label(), "ERROR");
        assert_eq!(LogLevel::Trace.label(), "TRACE");
    }

    #[test]
    fn parse_log_line_rejects_unknown_level_token() {
        // The structure is right but the level token is not a known level.
        let line = "2026-06-05T12:00:00  BOGUS superzej::db  msg";
        assert!(parse_log_line(line).is_none());
    }

    #[test]
    fn parse_log_line_rejects_missing_target_message_split() {
        // Has timestamp + level, but no double-space before the message.
        let line = "2026-06-05T12:00:00  INFO superzej::db message-no-double-space";
        assert!(parse_log_line(line).is_none());
    }

    /// Build a `LogLine` whose `raw` uniquely encodes its index, so ordering and
    /// dedup are directly assertable.
    fn line(level: LogLevel, i: usize) -> LogLine {
        LogLine {
            timestamp: "2026-06-05T12:00:00".to_string(),
            level,
            target: "superzej::test".to_string(),
            message: format!("line {i}"),
            raw: format!("raw-{i}"),
        }
    }

    fn error_count(lines: &[LogLine]) -> usize {
        lines.iter().filter(|l| l.level == LogLevel::Error).count()
    }

    #[test]
    fn error_inclusive_tail_includes_error_before_window() {
        let mut lines: Vec<_> = (0..1000).map(|i| line(LogLevel::Info, i)).collect();
        lines[10] = line(LogLevel::Error, 10);
        let out = error_inclusive_tail(&lines, 400, 200);
        // The pre-window error is carried and lands first (it predates the window).
        assert_eq!(out.first().unwrap().raw, "raw-10");
        assert_eq!(error_count(&out), 1);
        // Window contents preserved after it.
        assert_eq!(out.last().unwrap().raw, "raw-999");
        assert_eq!(out.len(), 1 + 400);
    }

    #[test]
    fn error_inclusive_tail_no_errors_is_plain_tail() {
        let lines: Vec<_> = (0..500).map(|i| line(LogLevel::Info, i)).collect();
        let out = error_inclusive_tail(&lines, 400, 200);
        assert_eq!(out, lines[100..].to_vec());
    }

    #[test]
    fn error_inclusive_tail_caps_pre_window_errors() {
        let lines: Vec<_> = (0..1000).map(|i| line(LogLevel::Error, i)).collect();
        let out = error_inclusive_tail(&lines, 400, 50);
        // 50 most-recent pre-window errors + the 400-line window.
        assert_eq!(out.len(), 450);
        // Newest-50 of indices 0..600 are 550..600, ascending.
        assert_eq!(out.first().unwrap().raw, "raw-550");
    }

    #[test]
    fn error_inclusive_tail_does_not_duplicate_error_in_window() {
        let mut lines: Vec<_> = (0..500).map(|i| line(LogLevel::Info, i)).collect();
        lines[5] = line(LogLevel::Error, 5); // before window (starts at 100)
        lines[480] = line(LogLevel::Error, 480); // inside window
        let out = error_inclusive_tail(&lines, 400, 200);
        assert_eq!(out.iter().filter(|l| l.raw == "raw-480").count(), 1);
        assert_eq!(out.len(), 401); // one pre-window error + 400-line window
    }

    #[test]
    fn error_inclusive_tail_empty_input() {
        assert!(error_inclusive_tail(&[], 400, 200).is_empty());
    }

    #[test]
    fn error_inclusive_tail_fewer_than_context() {
        let lines: Vec<_> = (0..10).map(|i| line(LogLevel::Info, i)).collect();
        let out = error_inclusive_tail(&lines, 400, 200);
        assert_eq!(out, lines);
    }

    #[test]
    fn error_inclusive_tail_preserves_original_order() {
        let levels = [
            LogLevel::Error,
            LogLevel::Info,
            LogLevel::Error,
            LogLevel::Warn,
        ];
        // context=2 so the first two lines are pre-window.
        let lines: Vec<_> = (0..4).map(|i| line(levels[i], i)).collect();
        let out = error_inclusive_tail(&lines, 2, 200);
        let order: Vec<_> = out.iter().map(|l| l.raw.clone()).collect();
        // pre-window error at idx 0, then the window (idx 2,3). idx 1 (Info) drops.
        assert_eq!(order, vec!["raw-0", "raw-2", "raw-3"]);
    }

    #[test]
    fn error_inclusive_tail_max_errors_zero_is_window_only() {
        let mut lines: Vec<_> = (0..500).map(|i| line(LogLevel::Info, i)).collect();
        lines[5] = line(LogLevel::Error, 5); // before window
        let out = error_inclusive_tail(&lines, 400, 0);
        assert_eq!(out, lines[100..].to_vec());
        assert_eq!(error_count(&out), 0);
    }
}
