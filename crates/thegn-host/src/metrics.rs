//! Metrics scraper supervisor — runs off-thread, scrapes Prometheus endpoints,
//! and sends updates to the TUI via mpsc channel.

use std::io::Read;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use thegn_core::config::MetricsConfig;
use thegn_core::metrics::{MetricSample, filter_samples, parse_metrics};

/// One target's latest state.
#[derive(Debug, Clone)]
pub struct MetricTargetState {
    pub name: String,
    pub url: String,
    /// Latest samples (filtered to allowlist).
    pub samples: Vec<MetricSample>,
    /// Health state.
    pub health: MetricHealth,
    /// Last successful scrape timestamp (for stale detection).
    pub last_ok: Option<Instant>,
    /// Error message if unhealthy.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetricHealth {
    Up,
    Stale,
    Error,
}

/// All targets' latest state.
#[derive(Debug, Clone, Default)]
pub struct MetricsState {
    pub targets: Vec<MetricTargetState>,
}

impl PartialEq for MetricsState {
    fn eq(&self, other: &Self) -> bool {
        self.targets.len() == other.targets.len()
            && self.targets.iter().zip(other.targets.iter()).all(|(a, b)| {
                a.name == b.name
                    && a.url == b.url
                    && a.health == b.health
                    && a.error == b.error
                    && a.samples == b.samples
            })
    }
}

/// Format a float metric value compactly for the sidebar.
pub fn format_sample_value(value: f64) -> String {
    if value.abs() >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value.abs() >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// Spawn the metrics supervisor. It owns a plain OS thread so HTTP and sleeping
/// never touch the render/event loop and do not require an event-loop timeout.
pub fn spawn_metrics_supervisor(
    config: MetricsConfig,
    tx: mpsc::UnboundedSender<MetricsState>,
    waker: termwiz::terminal::TerminalWaker,
) {
    if config.targets.is_empty() {
        return;
    }

    std::thread::Builder::new()
        .name("thegn-metrics".into())
        .spawn(move || run_supervisor(config, tx, waker))
        .ok();
}

fn run_supervisor(
    config: MetricsConfig,
    tx: mpsc::UnboundedSender<MetricsState>,
    waker: termwiz::terminal::TerminalWaker,
) {
    let interval = Duration::from_secs_f64(config.interval_secs.max(1.0));
    let timeout = Duration::from_millis(config.timeout_ms.clamp(100, 30_000));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build();

    let mut state = MetricsState {
        targets: config
            .targets
            .iter()
            .map(|t| MetricTargetState {
                name: t.name.clone(),
                url: target_display(t),
                samples: Vec::new(),
                health: MetricHealth::Error,
                last_ok: None,
                error: Some("initializing".into()),
            })
            .collect(),
    };

    let _ = tx.send(state.clone());
    let _ = waker.wake();

    loop {
        let now = Instant::now();
        for (i, target_cfg) in config.targets.iter().enumerate() {
            let result = {
                let _g = crate::perf::measure(crate::perf::Subsys::Metrics);
                match target_cfg.command_argv() {
                    // A command collector: run the argv (no shell) with the same
                    // timeout + body cap as a scrape. Off-loop already (this is a
                    // plain OS thread), so a wedged collector degrades its own
                    // target rather than blocking the loop or its siblings.
                    Some(argv) => collect_command(argv, timeout, config.max_body_bytes.max(1)),
                    None => match &client {
                        Ok(client) => {
                            scrape_target(client, &target_cfg.url, config.max_body_bytes.max(1))
                        }
                        Err(e) => Err(format!("http client: {e}")),
                    },
                }
            };

            let target_state = &mut state.targets[i];
            match result {
                Ok(body) => {
                    let all_samples = parse_metrics(&body);
                    target_state.samples =
                        filter_samples(&all_samples, &target_cfg.metrics, &target_cfg.labels);
                    target_state.health = MetricHealth::Up;
                    target_state.last_ok = Some(now);
                    target_state.error = None;
                }
                Err(e) => {
                    target_state.health = match target_state.last_ok {
                        Some(_) => MetricHealth::Stale,
                        None => {
                            target_state.samples.clear();
                            MetricHealth::Error
                        }
                    };
                    target_state.error = Some(e);
                }
            }
        }

        if tx.send(state.clone()).is_ok() {
            let _ = waker.wake();
        }
        std::thread::sleep(interval);
    }
}

/// Scrape a single target, enforcing the max response size while reading.
fn scrape_target(
    client: &reqwest::blocking::Client,
    url: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let response = client
        .get(url)
        .header("Accept", "text/plain; version=0.0.4")
        .send()
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;

    let mut limited = response.take(max_bytes as u64 + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "response too large: {} > {}",
            bytes.len(),
            max_bytes
        ));
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// The sidebar/JSON display string for a target: its URL for a scrape, the
/// argv for a command collector, so a `kind = "command"` row reads honestly.
fn target_display(t: &thegn_core::config::MetricsTarget) -> String {
    match t.command_argv() {
        Some(argv) => format!("$ {}", argv.join(" ")),
        None => t.url.clone(),
    }
}

/// Run a command collector: spawn `argv` (never a shell), read its stdout up to
/// `max_bytes`, and enforce `timeout`. A wedged or slow collector is killed and
/// reported as an error, exactly like a failed scrape — it never blocks the
/// supervisor thread past the timeout, nor the event loop (this runs off it).
///
/// The stdout is read on a helper thread so a child that never writes and never
/// exits can't block the read past the deadline: on timeout the child is killed,
/// which closes its stdout and releases the reader.
fn collect_command(argv: &[String], timeout: Duration, max_bytes: usize) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let (prog, args) = argv.split_first().ok_or_else(|| "empty argv".to_string())?;
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {prog}: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout pipe".to_string())?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("thegn-metrics-collect".into())
        .spawn(move || {
            let mut buf = Vec::new();
            // Cap the read at the same body limit as a scrape; +1 so we can tell
            // "exactly at the cap" from "over it".
            let res = stdout.take(max_bytes as u64 + 1).read_to_end(&mut buf);
            let _ = tx.send(res.map(|_| buf));
        })
        .ok();

    match rx.recv_timeout(timeout) {
        Ok(Ok(buf)) => {
            let _ = child.wait();
            if buf.len() > max_bytes {
                return Err(format!("output too large: > {max_bytes} bytes"));
            }
            String::from_utf8(buf).map_err(|e| e.to_string())
        }
        Ok(Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("read: {e}"))
        }
        Err(_) => {
            // Timeout (or the reader is still blocked): kill the child; its
            // stdout close releases the detached reader thread.
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("timed out after {}ms", timeout.as_millis()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_state_equality_tracks_sample_values() {
        let a = MetricsState {
            targets: vec![MetricTargetState {
                name: "svc".into(),
                url: "http://127.0.0.1:1/metrics".into(),
                samples: vec![thegn_core::metrics::MetricSample {
                    name: "requests".into(),
                    value: 1.0,
                    labels: Default::default(),
                }],
                health: MetricHealth::Up,
                last_ok: None,
                error: None,
            }],
        };
        let mut b = a.clone();
        assert_eq!(a, b);
        b.targets[0].samples[0].value = 2.0;
        assert_ne!(a, b);
    }

    #[test]
    fn format_sample_value_is_sidebar_friendly() {
        assert_eq!(format_sample_value(42.0), "42");
        assert_eq!(format_sample_value(12.25), "12.25");
        assert_eq!(format_sample_value(1_500_000.0), "1.5M");
    }

    #[test]
    fn target_display_prefers_argv_for_command_collectors() {
        use thegn_core::config::{MetricsTarget, MetricsTargetKind};
        let scrape = MetricsTarget {
            name: "svc".into(),
            url: "http://127.0.0.1:9091/metrics".into(),
            kind: MetricsTargetKind::Prometheus,
            command: Vec::new(),
            metrics: Vec::new(),
            labels: Default::default(),
        };
        assert_eq!(target_display(&scrape), "http://127.0.0.1:9091/metrics");
        let cmd = MetricsTarget {
            name: "gpu".into(),
            url: String::new(),
            kind: MetricsTargetKind::Command,
            command: vec!["vendor-smi".into(), "--prometheus".into()],
            metrics: Vec::new(),
            labels: Default::default(),
        };
        assert_eq!(target_display(&cmd), "$ vendor-smi --prometheus");
    }

    // The collector runs a real argv, so these are unix-gated smoke tests using
    // `sh -c` purely as a stand-in program that prints to stdout. `collect_command`
    // itself never introduces a shell — the argv is whatever the config supplies.
    #[cfg(unix)]
    #[test]
    fn collect_command_captures_stdout_and_parses() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'vendorx_gpu_busy 42\\n'".to_string(),
        ];
        let out = collect_command(&argv, Duration::from_secs(5), 4096).expect("collector runs");
        let samples = parse_metrics(&out);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "vendorx_gpu_busy");
        assert_eq!(samples[0].value, 42.0);
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_times_out_a_wedged_collector() {
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let err = collect_command(&argv, Duration::from_millis(150), 4096).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_enforces_the_output_cap() {
        // Print 5000 bytes with a 100-byte cap → refused.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "head -c 5000 /dev/zero | tr '\\0' 'x'".to_string(),
        ];
        let err = collect_command(&argv, Duration::from_secs(5), 100).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_reports_a_missing_program() {
        let argv = vec!["thegn-no-such-collector-xyz".to_string()];
        assert!(collect_command(&argv, Duration::from_secs(1), 4096).is_err());
    }
}
