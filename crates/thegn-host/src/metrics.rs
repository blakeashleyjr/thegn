//! Metrics scraper supervisor — runs off-thread, scrapes Prometheus endpoints,
//! and publishes the latest state to the TUI through a latest-value
//! [`SnapshotSlot`] (whole-state replace, so only the newest scrape matters).

use std::io::Read;
use std::time::{Duration, Instant};

use std::sync::{Arc, Weak};

use crate::snapshot_slot::{PublishOutcome, SnapshotSlot};
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

/// Byte cap for the latest-value metrics slot. Generous: a handful of targets
/// with allowlisted samples is a few KiB.
const METRICS_SLOT_MAX_BYTES: usize = 256 * 1024;

/// The loop's end of the metrics slot. Dropping it (the loop ending) is what
/// stops the supervisor: the outbox sees its consumer token die.
pub(crate) struct MetricsInbox {
    slot: SnapshotSlot<MetricsState>,
    _consumer: Arc<()>,
}

impl MetricsInbox {
    /// Take the newest unconsumed state, if any.
    pub(crate) fn take(&self) -> Option<(u64, MetricsState)> {
        self.slot.take()
    }
}

/// The supervisor's end of the metrics slot: publishes with a per-scrape
/// generation and knows when the consumer has gone away.
pub(crate) struct MetricsOutbox {
    slot: SnapshotSlot<MetricsState>,
    consumer: Weak<()>,
    generation: u64,
}

impl MetricsOutbox {
    fn consumer_gone(&self) -> bool {
        self.consumer.strong_count() == 0
    }

    /// Publish one scrape; `None` when the consumer is gone (stop scraping).
    fn publish(&mut self, state: &MetricsState) -> Option<PublishOutcome> {
        if self.consumer_gone() {
            return None;
        }
        Some(publish_metrics(&self.slot, &mut self.generation, state))
    }
}

/// Build the metrics slot pair. `wake` runs once per empty-to-full change, so
/// the supervisor never pulses the loop for a value the loop has not taken.
pub(crate) fn metrics_slot(
    wake: impl Fn() + Send + Sync + 'static,
) -> (MetricsOutbox, MetricsInbox) {
    let slot = SnapshotSlot::with_waker(METRICS_SLOT_MAX_BYTES, wake);
    let consumer = Arc::new(());
    (
        MetricsOutbox {
            slot: slot.clone(),
            consumer: Arc::downgrade(&consumer),
            generation: 0,
        },
        MetricsInbox {
            slot,
            _consumer: consumer,
        },
    )
}

/// Publish one scrape under the next generation. Latest-wins: an unconsumed
/// earlier scrape is replaced, never queued.
fn publish_metrics(
    slot: &SnapshotSlot<MetricsState>,
    generation: &mut u64,
    state: &MetricsState,
) -> PublishOutcome {
    *generation = generation.saturating_add(1);
    let bytes = metrics_state_bytes(state);
    let outcome = slot.publish(*generation, state.clone(), bytes);
    if outcome == PublishOutcome::TooLarge {
        tracing::debug!(
            target: "thegn::metrics",
            bytes,
            "metrics state over the slot cap; keeping the last value"
        );
    }
    outcome
}

/// Approximate retained size of a metrics state for the slot's byte cap.
fn metrics_state_bytes(state: &MetricsState) -> usize {
    std::mem::size_of_val(state)
        + state
            .targets
            .iter()
            .map(|target| {
                std::mem::size_of_val(target)
                    + target.name.len()
                    + target.url.len()
                    + target.error.as_ref().map_or(0, String::len)
                    + target
                        .samples
                        .iter()
                        .map(|sample| {
                            std::mem::size_of_val(sample)
                                + sample.name.len()
                                + sample
                                    .labels
                                    .iter()
                                    .map(|(k, v)| k.len() + v.len())
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

/// Spawn the metrics supervisor. It owns a plain OS thread so HTTP and sleeping
/// never touch the render/event loop and do not require an event-loop timeout.
pub(crate) fn spawn_metrics_supervisor(config: MetricsConfig, outbox: MetricsOutbox) {
    if config.targets.is_empty() {
        return;
    }

    // best-effort: supervisor thread: a failed spawn just disables metrics this session
    thegn_core::thread_name::try_spawn_named("thegn-metrics", move || {
        run_supervisor(config, outbox)
    })
    .ok();
}

fn run_supervisor(config: MetricsConfig, mut outbox: MetricsOutbox) {
    // Housekeeping: a sampler that sleeps between blocking HTTP scrapes and
    // that nothing ever waits on — it must not compete for a P-core.
    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
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

    if outbox.publish(&state).is_none() {
        return; // consumer gone (session ended) before the first scrape
    }

    loop {
        let now = Instant::now();
        for (i, target_cfg) in config.targets.iter().enumerate() {
            if outbox.consumer_gone() {
                return; // consumer gone (session ended): stop promptly
            }
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

        // The slot owns the empty-to-full wake; no explicit pulse here.
        if outbox.publish(&state).is_none() {
            return; // consumer gone (session ended): stop the supervisor
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

/// Run a command collector: spawn `argv` (never a shell) through the shared
/// bounded runner (`thegn_metrics::run_command_bounded`): one total deadline
/// across spawn, read, exit and reap, a stdout cap, its own process group
/// SIGKILLed as a unit on any non-clean exit, and no reader thread. EOF on
/// stdout is not treated as exit: success needs a zero exit inside the same
/// deadline, so a collector that closes stdout and sleeps times out instead of
/// wedging the supervisor.
fn collect_command(argv: &[String], timeout: Duration, max_bytes: usize) -> Result<String, String> {
    use std::ffi::OsStr;
    let (prog, args) = argv.split_first().ok_or_else(|| "empty argv".to_string())?;
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let buf = thegn_metrics::run_command_bounded(OsStr::new(prog), &args, timeout, max_bytes)
        .map_err(|e| format!("{prog}: {e}"))?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_metrics_bumps_generation_and_replaces_unconsumed_state() {
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&wakes);
        let (mut outbox, inbox) = metrics_slot(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let first = MetricsState::default();
        let latest = MetricsState {
            targets: vec![MetricTargetState {
                name: "svc".into(),
                url: "http://127.0.0.1:1/metrics".into(),
                samples: Vec::new(),
                health: MetricHealth::Up,
                last_ok: None,
                error: None,
            }],
        };
        assert_eq!(
            publish_metrics(&outbox.slot, &mut outbox.generation, &first),
            PublishOutcome::Stored
        );
        assert_eq!(outbox.generation, 1);
        assert_eq!(
            publish_metrics(&outbox.slot, &mut outbox.generation, &latest),
            PublishOutcome::Replaced
        );
        assert_eq!(outbox.generation, 2);
        // Two publishes, no take: one wake, exactly one (the newest) value.
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
        let (taken_generation, state) = inbox.take().expect("latest metrics state");
        assert_eq!(taken_generation, 2);
        assert_eq!(state, latest);
        assert!(inbox.take().is_none());
        // The next scrape after a take refills the slot with a fresh wake.
        assert_eq!(outbox.publish(&first), Some(PublishOutcome::Stored));
        assert_eq!(outbox.generation, 3);
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn metrics_outbox_stops_once_the_inbox_is_dropped() {
        let (mut outbox, inbox) = metrics_slot(|| {});
        assert!(!outbox.consumer_gone());
        drop(inbox);
        assert!(outbox.consumer_gone());
        assert_eq!(outbox.publish(&MetricsState::default()), None);
    }

    #[test]
    fn metrics_state_bytes_grows_with_samples() {
        let empty = MetricsState::default();
        let one = MetricsState {
            targets: vec![MetricTargetState {
                name: "svc".into(),
                url: "u".into(),
                samples: vec![thegn_core::metrics::MetricSample {
                    name: "requests".into(),
                    value: 1.0,
                    labels: [("k".to_owned(), "v".to_owned())].into_iter().collect(),
                }],
                health: MetricHealth::Up,
                last_ok: None,
                error: None,
            }],
        };
        assert!(metrics_state_bytes(&one) > metrics_state_bytes(&empty));
        assert!(metrics_state_bytes(&one) < METRICS_SLOT_MAX_BYTES);
    }

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

    #[cfg(unix)]
    #[test]
    fn collect_command_times_out_when_stdout_closes_but_process_lives() {
        // Closes stdout immediately, then sleeps: EOF is not exit.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "exec >&-; sleep 30".to_string(),
        ];
        let t = Instant::now();
        let err = collect_command(&argv, Duration::from_millis(300), 4096).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_does_not_hang_on_descendant_held_stdout() {
        // A backgrounded descendant inherits stdout and outlives the leader.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 30 & sleep 30".to_string(),
        ];
        let t = Instant::now();
        let err = collect_command(&argv, Duration::from_millis(300), 4096).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn collect_command_rejects_nonzero_exit() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'x 1\\n'; exit 3".to_string(),
        ];
        let err = collect_command(&argv, Duration::from_secs(5), 4096).unwrap_err();
        assert!(err.contains("exit"), "{err}");
    }
}
