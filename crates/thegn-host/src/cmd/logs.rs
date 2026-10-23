//! `thegn logs` — plugin/script API for reading the thegn log file.
//!
//! Reads thegn.log directly from disk; does not require a running host.

#![allow(clippy::disallowed_macros)]

use clap::Subcommand;
use thegn_core::log_view::{LogLevel, parse_log_line};

#[derive(Subcommand, Clone)]
pub enum Action {
    /// Tail the thegn log (like `tail -n N`, but with filtering).
    Tail {
        /// Output the last N lines (after filtering).
        #[arg(long, short = 'n', default_value = "50")]
        lines: usize,
        /// Minimum log level to show (error, warn, info, debug, trace).
        #[arg(long, short = 'l')]
        level: Option<String>,
        /// Case-insensitive substring filter on the full log line.
        #[arg(long, short = 'f')]
        filter: Option<String>,
        /// Show only lines from one process run id (`run=<id>`), correlating a
        /// session across the compositor's and the daemon's log files.
        #[arg(long)]
        run: Option<String>,
        /// Output as JSON (one object per line).
        #[arg(long)]
        json: bool,
        /// Print the log file path and exit.
        #[arg(long)]
        path: bool,
    },
    /// Count log lines, optionally filtered.
    Count {
        /// Minimum log level to count (error, warn, info, debug, trace).
        #[arg(long, short = 'l')]
        level: Option<String>,
        /// Case-insensitive substring filter.
        #[arg(long, short = 'f')]
        filter: Option<String>,
    },
}

pub fn run(cfg: &thegn_core::config::Config, action: Action) -> anyhow::Result<()> {
    match action {
        Action::Tail {
            lines,
            level,
            filter,
            run,
            json,
            path,
        } => {
            let log_path = thegn_core::log_view::log_file_path(&cfg.log);
            if path {
                println!("{}", log_path.display());
                return Ok(());
            }
            if !log_path.exists() {
                eprintln!(
                    "log file not found: {} (set THEGN_LOG to enable logging)",
                    log_path.display()
                );
                return Ok(());
            }

            let threshold = parse_level(level.as_deref())?;
            let filter_lc = filter.map(|f| f.to_lowercase());

            let content = std::fs::read_to_string(&log_path)?;
            let filtered: Vec<_> = content
                .lines()
                .filter_map(parse_log_line)
                .filter(|l| {
                    threshold.is_none_or(|thr| l.level <= thr)
                        && filter_lc
                            .as_deref()
                            .is_none_or(|f| l.raw.to_lowercase().contains(f))
                        && run
                            .as_deref()
                            .is_none_or(|r| thegn_core::log_view::line_run_id(&l.raw) == Some(r))
                })
                .collect();

            let start = filtered.len().saturating_sub(lines);
            for l in &filtered[start..] {
                println!("{}", tail_output(l, json));
            }
        }
        Action::Count { level, filter } => {
            let log_path = thegn_core::log_view::log_file_path(&cfg.log);
            if !log_path.exists() {
                println!("0");
                return Ok(());
            }

            let threshold = parse_level(level.as_deref())?;
            let filter_lc = filter.map(|f| f.to_lowercase());

            let content = std::fs::read_to_string(&log_path)?;
            let count = content
                .lines()
                .filter_map(parse_log_line)
                .filter(|l| {
                    threshold.is_none_or(|thr| l.level <= thr)
                        && filter_lc
                            .as_deref()
                            .is_none_or(|f| l.raw.to_lowercase().contains(f))
                })
                .count();
            println!("{count}");
        }
    }
    Ok(())
}

fn tail_output(line: &thegn_core::log_view::LogLine, json: bool) -> String {
    if json {
        let obj = serde_json::json!({
            "timestamp": line.timestamp,
            "level": line.level.label(),
            "target": line.target,
            "message": line.message,
        });
        thegn_core::pipeline_report::json_terminal_safe(&obj.to_string()).into_owned()
    } else {
        thegn_core::pipeline_report::display_cell(&line.raw)
    }
}

fn parse_level(s: Option<&str>) -> anyhow::Result<Option<LogLevel>> {
    match s {
        None => Ok(None),
        Some(s) => LogLevel::parse(s).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown log level {:?} — use error/warn/info/debug/trace",
                s
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::tail_output;
    use thegn_core::log_view::{LogLevel, LogLine};

    #[test]
    fn tail_output_neutralizes_controls_in_human_and_json() {
        let message = "hi \x1b]0;forged\x07 \u{9b}31m \u{202e}x\u{2028}y é 界";
        let line = LogLine {
            timestamp: "2026-10-22T00:00:00Z".into(),
            level: LogLevel::Info,
            target: "thegn::x".into(),
            message: message.into(),
            raw: format!("2026-10-22T00:00:00Z  INFO  thegn::x  {message}"),
        };

        let human = tail_output(&line, false);
        assert!(
            !human
                .chars()
                .any(|c| { c.is_control() || thegn_core::pipeline_report::is_format_control(c) })
        );
        assert!(human.contains('\u{fffd}'));
        assert!(human.contains("é 界"));

        let json = tail_output(&line, true);
        assert!(
            !json
                .chars()
                .any(|c| { c.is_control() || thegn_core::pipeline_report::is_format_control(c) })
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["message"], line.message);
    }

    #[test]
    fn tail_output_keeps_clean_lines_byte_identical() {
        // Positive control: clean output matched the base implementation byte for byte.
        let line = LogLine {
            timestamp: "2026-10-22T00:00:00Z".into(),
            level: LogLevel::Info,
            target: "thegn::x".into(),
            message: "hello é 界 😀".into(),
            raw: "2026-10-22T00:00:00Z  INFO  thegn::x  hello é 界 😀".into(),
        };
        let obj = serde_json::json!({
            "timestamp": line.timestamp,
            "level": line.level.label(),
            "target": line.target,
            "message": line.message,
        });
        assert_eq!(tail_output(&line, false), line.raw);
        assert_eq!(tail_output(&line, true), obj.to_string());
    }
}
