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
                })
                .collect();

            let start = filtered.len().saturating_sub(lines);
            for l in &filtered[start..] {
                if json {
                    let obj = serde_json::json!({
                        "timestamp": l.timestamp,
                        "level": l.level.label(),
                        "target": l.target,
                        "message": l.message,
                    });
                    println!("{obj}");
                } else {
                    println!("{}", l.raw);
                }
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
