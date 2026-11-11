//! Parsed view of the thegn plain-text log file.
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

/// One parsed line from the thegn log file.
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
        // Lift the leading identity tokens (`proc=`/`run=`/`wt=`) out of the
        // visible message — the formatter places them between the target and
        // the message body (see `log_trace::Brand`).
        message: strip_identity_tokens(message),
        raw: line.to_string(),
    })
}

/// Trim the leading run of `proc=`/`run=`/`wt=` identity tokens the formatter
/// injects, returning the human-readable message body.
fn strip_identity_tokens(message: &str) -> String {
    let mut rest = message.trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        if tok.starts_with("proc=") || tok.starts_with("run=") || tok.starts_with("wt=") {
            rest = rest[end..].trim_start();
        } else {
            break;
        }
    }
    rest.to_string()
}

/// The run id carried by a raw log line (`run=<id>`), if present. Used by
/// `thegn logs tail --run <id>` to filter to one process's lines.
///
/// Reads only the formatter's leading identity run (the `proc=`/`run=`/`wt=`
/// tokens right after the target), mirroring [`parse_log_line`]; a `run=`
/// token quoted later in the message is never attributed. The first non-empty
/// `run=` in that run wins. Returns `None` for a line without the Brand shape.
pub fn line_run_id(raw: &str) -> Option<&str> {
    let (_ts, rest) = raw.split_once("  ")?;
    let (_level, rest) = rest.split_once(' ')?;
    let (_target, message) = rest.trim_start().split_once("  ")?;
    let mut rest = message.trim_start();
    let mut run = None;
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        if let Some(id) = tok.strip_prefix("run=") {
            if run.is_none() && !id.is_empty() {
                run = Some(id);
            }
        } else if !(tok.starts_with("proc=") || tok.starts_with("wt=")) {
            break;
        }
        rest = rest[end..].trim_start();
    }
    run
}

/// Return the path to the thegn log file, honouring the `[log]` config.
pub fn log_file_path(cfg: &crate::config::LogConfig) -> std::path::PathBuf {
    cfg.dir_path().join("thegn.log")
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

/// Classification of a log line's worktree tag (`wt_slug` format).
///
/// [`crate::log_trace::wt_slug`] writes `""` for a host-global line,
/// `"<label>.<64 lowercase hex>"` for a worktree, and `"<label>.unbound"` when
/// the path exceeds the identity bound. `Legacy` and `Unbound` tags can never
/// be proven to belong to the active worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtTagClass {
    HostGlobal,
    Versioned,
    Unbound,
    Legacy,
}

pub fn classify_wt_tag(tag: Option<&str>) -> WtTagClass {
    let Some(tag) = tag.filter(|t| !t.is_empty()) else {
        return WtTagClass::HostGlobal;
    };
    if tag.ends_with(".unbound") {
        return WtTagClass::Unbound;
    }
    match tag.rsplit_once('.') {
        Some((_, hex))
            if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            WtTagClass::Versioned
        }
        _ => WtTagClass::Legacy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_wt_tag_recognises_the_wt_slug_shapes() {
        use crate::log_trace::wt_slug;
        use std::path::Path;
        let versioned = wt_slug(Path::new("/repos/a/feat_x"));
        assert_eq!(classify_wt_tag(Some(&versioned)), WtTagClass::Versioned);
        let global = wt_slug(Path::new(""));
        assert_eq!(classify_wt_tag(Some(&global)), WtTagClass::HostGlobal);
        assert_eq!(classify_wt_tag(None), WtTagClass::HostGlobal);
        let long = format!("/{}", "x".repeat(20_000));
        let unbound = wt_slug(Path::new(&long));
        assert_eq!(classify_wt_tag(Some(&unbound)), WtTagClass::Unbound);
        let hex63 = format!("feat.{}", "a".repeat(63));
        let bad64 = format!("feat.{}G", "a".repeat(63));
        for legacy in ["feat-x", "feat-x.abc", hex63.as_str(), bad64.as_str()] {
            assert_eq!(
                classify_wt_tag(Some(legacy)),
                WtTagClass::Legacy,
                "{legacy}"
            );
        }
    }

    const INFO_LINE: &str = "2026-06-05T12:00:00  INFO  thegn::db  connection opened";
    const ERROR_LINE: &str = "2026-06-05T12:00:01  ERROR thegn::host  fatal error occurred";
    const WARN_LINE: &str = "2026-06-05T12:00:02  WARN  thegn::panel  slow render";

    #[test]
    fn parse_log_line_valid_info() {
        let l = parse_log_line(INFO_LINE).unwrap();
        assert_eq!(l.timestamp, "2026-06-05T12:00:00");
        assert_eq!(l.level, LogLevel::Info);
        assert_eq!(l.target, "thegn::db");
        assert_eq!(l.message, "connection opened");
        assert_eq!(l.raw, INFO_LINE);
    }

    #[test]
    fn parse_log_line_valid_error() {
        let l = parse_log_line(ERROR_LINE).unwrap();
        assert_eq!(l.level, LogLevel::Error);
        assert_eq!(l.target, "thegn::host");
        assert_eq!(l.message, "fatal error occurred");
    }

    #[test]
    fn parse_log_line_valid_warn() {
        let l = parse_log_line(WARN_LINE).unwrap();
        assert_eq!(l.level, LogLevel::Warn);
        assert_eq!(l.target, "thegn::panel");
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
        let line = "2026-06-05T12:00:00  INFO  thegn::db  msg  with  spaces";
        let l = parse_log_line(line).unwrap();
        // split_once stops at the first double-space after target; everything
        // after that (including internal double-spaces) is the message.
        assert_eq!(l.message, "msg  with  spaces");
    }

    #[test]
    fn parse_log_line_all_levels() {
        let levels = [
            ("ERROR thegn::x  m", LogLevel::Error),
            ("WARN  thegn::x  m", LogLevel::Warn),
            ("INFO  thegn::x  m", LogLevel::Info),
            ("DEBUG thegn::x  m", LogLevel::Debug),
            ("TRACE thegn::x  m", LogLevel::Trace),
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
    fn log_file_path_ends_with_thegn_log() {
        let cfg = crate::config::LogConfig::default();
        let p = log_file_path(&cfg);
        assert!(p.ends_with("thegn.log"), "got: {p:?}");
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
        let line = "2026-06-05T12:00:00  BOGUS thegn::db  msg";
        assert!(parse_log_line(line).is_none());
    }

    #[test]
    fn parse_log_line_rejects_missing_target_message_split() {
        // Has timestamp + level, but no double-space before the message.
        let line = "2026-06-05T12:00:00  INFO thegn::db message-no-double-space";
        assert!(parse_log_line(line).is_none());
    }

    /// Build a `LogLine` whose `raw` uniquely encodes its index, so ordering and
    /// dedup are directly assertable.
    fn line(level: LogLevel, i: usize) -> LogLine {
        LogLine {
            timestamp: "2026-06-05T12:00:00".to_string(),
            level,
            target: "thegn::test".to_string(),
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

    const RUN_ID_PREFIX: &str = "2026-07-03T10:00:00  WARN  thegn::git  ";

    #[test]
    fn line_run_id_reads_the_leading_identity_run() {
        let a = format!("{RUN_ID_PREFIX}proc=host run=r1  wt=t  fetched");
        assert_eq!(line_run_id(&a), Some("r1"));
        let b = format!("{RUN_ID_PREFIX}wt=t proc=host run=r2  body");
        assert_eq!(line_run_id(&b), Some("r2"));
        let c = format!("{RUN_ID_PREFIX}run=first run=second body");
        assert_eq!(line_run_id(&c), Some("first"));
    }

    #[test]
    fn line_run_id_ignores_a_quoted_run_token_in_the_message() {
        let with_run = format!("{RUN_ID_PREFIX}proc=host run=r1  wt=t  saw run=zzz in output");
        assert_eq!(line_run_id(&with_run), Some("r1"));
        let without_run = format!("{RUN_ID_PREFIX}wt=t  saw run=zzz in output");
        assert_eq!(line_run_id(&without_run), None);
    }

    #[test]
    fn line_run_id_is_none_without_a_leading_run_token() {
        let quoted = format!("{RUN_ID_PREFIX}restored run=abc from backup");
        assert_eq!(line_run_id(&quoted), None);
        let empty = format!("{RUN_ID_PREFIX}run=  body");
        assert_eq!(line_run_id(&empty), None);
        assert_eq!(line_run_id("not a log line run=x"), None);
    }
}
