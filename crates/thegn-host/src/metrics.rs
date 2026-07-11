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
                url: t.url.clone(),
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
                match &client {
                    Ok(client) => {
                        scrape_target(client, &target_cfg.url, config.max_body_bytes.max(1))
                    }
                    Err(e) => Err(format!("http client: {e}")),
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
}
