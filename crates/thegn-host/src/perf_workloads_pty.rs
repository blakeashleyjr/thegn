//! Deterministic PTY-shaped producers aimed at the real terminal writer, with
//! an in-memory sink so timing and failure modes can be inspected safely.

use crate::frame_writer::FrameWriter;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOG_BURST_LINES: usize = 32;
const LOG_BURST_RATE: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProducerKind {
    ContinuousText,
    EscapeHeavy,
    SmallChunks,
    FlooderPlusQuiet,
    OutputThenExit,
}

impl ProducerKind {
    const ALL: [ProducerKind; 5] = [
        Self::ContinuousText,
        Self::EscapeHeavy,
        Self::SmallChunks,
        Self::FlooderPlusQuiet,
        Self::OutputThenExit,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ContinuousText => "continuous_text",
            Self::EscapeHeavy => "escape_heavy",
            Self::SmallChunks => "small_chunks",
            Self::FlooderPlusQuiet => "flooder_plus_quiet",
            Self::OutputThenExit => "output_then_exit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SinkMode {
    Fast,
    Throttled { per_write: Duration },
    FailAfter { writes: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoadFactors {
    log_burst: bool,
    recording: bool,
    hydration: bool,
}

impl LoadFactors {
    const fn none() -> Self {
        Self {
            log_burst: false,
            recording: false,
            hydration: false,
        }
    }

    const fn all() -> Self {
        Self {
            log_burst: true,
            recording: true,
            hydration: true,
        }
    }

    const fn log_burst() -> Self {
        Self {
            log_burst: true,
            ..Self::none()
        }
    }

    const fn recording() -> Self {
        Self {
            recording: true,
            ..Self::none()
        }
    }

    const fn hydration() -> Self {
        Self {
            hydration: true,
            ..Self::none()
        }
    }

    fn labels(self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        if self.log_burst {
            labels.push("log_burst");
        }
        if self.recording {
            labels.push("recording");
        }
        if self.hydration {
            labels.push("hydration");
        }
        labels
    }

    fn label(self) -> String {
        let labels = self.labels();
        if labels.is_empty() {
            "none".to_string()
        } else {
            labels.join("+")
        }
    }

    fn from_labels(labels: impl Iterator<Item = &'static str>) -> Option<Self> {
        let mut factors = Self::none();
        let mut found = false;
        for label in labels {
            match label {
                "log_burst" => factors.log_burst = true,
                "recording" => factors.recording = true,
                "hydration" => factors.hydration = true,
                "none" | "baseline" => {}
                _ => return None,
            }
            found = true;
        }
        found.then_some(factors)
    }

    fn push_unique(cases: &mut Vec<Self>, factors: Self) {
        if !cases.contains(&factors) {
            cases.push(factors);
        }
    }

    fn audit_cases() -> Vec<Self> {
        let default = || {
            vec![
                Self::log_burst(),
                Self::recording(),
                Self::hydration(),
                Self::all(),
            ]
        };
        let Ok(raw) = std::env::var("THEGN_AUDIT_FACTORS") else {
            return default();
        };
        let mut cases = Vec::new();
        for item in raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            if item == "alone" {
                for factors in [Self::log_burst(), Self::recording(), Self::hydration()] {
                    Self::push_unique(&mut cases, factors);
                }
            } else if item == "all" {
                Self::push_unique(&mut cases, Self::all());
            } else {
                let labels = item.split('+').map(|label| match label {
                    "log_burst" => "log_burst",
                    "recording" => "recording",
                    "hydration" => "hydration",
                    "none" | "baseline" => "none",
                    _ => "unknown",
                });
                if let Some(factors) = Self::from_labels(labels) {
                    Self::push_unique(&mut cases, factors);
                }
            }
        }
        if cases.is_empty() { default() } else { cases }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct FactorCounts {
    log_burst_lines: usize,
    recording_bytes: usize,
    hydration_calls: usize,
}

struct LoadRuntime {
    stop: Arc<AtomicBool>,
    log_lines: Arc<AtomicUsize>,
    log_thread: Option<std::thread::JoinHandle<()>>,
    recording: Option<Arc<Mutex<tempfile::NamedTempFile>>>,
    _tempdir: Option<tempfile::TempDir>,
    recording_bytes: Arc<AtomicUsize>,
}

impl LoadRuntime {
    fn start(factors: LoadFactors) -> Self {
        let tempdir = factors
            .log_burst
            .then_some(tempfile::tempdir().expect("load-factor tempdir"))
            .or_else(|| {
                factors
                    .recording
                    .then_some(tempfile::tempdir().expect("recording tempdir"))
            });
        let stop = Arc::new(AtomicBool::new(false));
        let log_lines = Arc::new(AtomicUsize::new(0));
        let log_thread = if factors.log_burst {
            let path = tempdir
                .as_ref()
                .expect("log burst tempdir")
                .path()
                .join("synthetic.log");
            let stop = Arc::clone(&stop);
            let log_lines = Arc::clone(&log_lines);
            Some(thegn_core::thread_name::spawn_named(
                "thegn-wl-log",
                move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    let mut file = std::fs::File::create(path).expect("synthetic log file");
                    for line in 0..LOG_BURST_LINES {
                        writeln!(file, "synthetic log burst line {line:04}")
                            .expect("synthetic log write");
                        log_lines.fetch_add(1, Ordering::SeqCst);
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(LOG_BURST_RATE);
                    }
                },
            ))
        } else {
            None
        };
        let recording = if factors.recording {
            Some(Arc::new(Mutex::new(
                tempfile::NamedTempFile::new_in(
                    tempdir.as_ref().expect("recording tempdir").path(),
                )
                .expect("recording tempfile"),
            )))
        } else {
            None
        };
        Self {
            stop,
            log_lines,
            log_thread,
            recording,
            _tempdir: tempdir,
            recording_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn recording_sink(&self) -> Option<Arc<Mutex<tempfile::NamedTempFile>>> {
        self.recording.as_ref().map(Arc::clone)
    }

    fn finish(mut self) -> (FactorCounts, usize) {
        let thread = self.log_thread.take();
        let joined = usize::from(thread.is_some());
        if let Some(thread) = thread {
            thread.join().expect("log burst thread");
        }
        (
            FactorCounts {
                log_burst_lines: self.log_lines.load(Ordering::SeqCst),
                recording_bytes: self.recording_bytes.load(Ordering::SeqCst),
                hydration_calls: 0,
            },
            joined,
        )
    }
}

impl Drop for LoadRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.log_thread.take() {
            thread.join().expect("log burst thread");
        }
    }
}

impl SinkMode {
    fn label(self) -> String {
        match self {
            Self::Fast => "fast".to_string(),
            Self::Throttled { per_write } => format!("throttled_{}ms", per_write.as_millis()),
            Self::FailAfter { writes } => format!("fail_after_{writes}_writes"),
        }
    }
}

#[derive(Clone, Debug)]
struct Chunk {
    producer: u8,
    seq: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct Observation {
    kind: &'static str,
    seed: u64,
    fingerprint: u64,
    mode: String,
    factors: String,
    produced_chunks: usize,
    produced_bytes: usize,
    accepted_chunks: usize,
    refused_chunks: usize,
    sink_writes: usize,
    sink_bytes: usize,
    ordered: bool,
    admitted_order: Vec<(u8, u32)>,
    quiet_progress: Option<usize>,
    factor_counts: FactorCounts,
    writer_status: String,
    elapsed_ms: u128,
    joined_producers: usize,
    joined_load_threads: usize,
}

struct XorShift64Star(u64);

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn text_bytes(len: usize, seq: u32, salt: u64) -> Vec<u8> {
    let line = format!("pane output {seq:08} printable text {salt:016x}\r\n");
    line.bytes().cycle().take(len).collect()
}

fn push_chunks(chunks: &mut Vec<Chunk>, producer: u8, size: usize, seq: &mut u32, salt: u64) {
    chunks.push(Chunk {
        producer,
        seq: *seq,
        bytes: text_bytes(size, *seq, salt),
    });
    *seq = seq.saturating_add(1);
}

fn generate(kind: ProducerKind, seed: u64, total_bytes: usize) -> Vec<Vec<Chunk>> {
    let mut rng = XorShift64Star::new(seed);
    match kind {
        ProducerKind::ContinuousText => {
            let mut chunks = Vec::new();
            let mut seq = 0;
            let mut bytes = 0;
            while bytes < total_bytes {
                let size = 4096.min(total_bytes.saturating_sub(bytes).max(1));
                push_chunks(&mut chunks, 0, size, &mut seq, seed);
                bytes += size;
            }
            vec![chunks]
        }
        ProducerKind::EscapeHeavy => {
            const UNITS: [&[u8]; 4] = [b"\x1b[38;2;", b"\x1b[H", b"\x1b[2K", b"text"];
            let mut chunks = Vec::new();
            let mut current = Vec::with_capacity(1024);
            let mut seq = 0;
            let mut generated_bytes = 0;
            while generated_bytes + current.len() < total_bytes {
                let unit_index = (rng.next() as usize) % UNITS.len();
                let mut unit = UNITS[unit_index].to_vec();
                if unit_index == 0 {
                    unit.extend_from_slice(
                        format!(
                            "{};{};{}m",
                            rng.next() % 256,
                            rng.next() % 256,
                            rng.next() % 256
                        )
                        .as_bytes(),
                    );
                }
                if current.len() + unit.len() > 1024 && !current.is_empty() {
                    generated_bytes += current.len();
                    chunks.push(Chunk {
                        producer: 0,
                        seq,
                        bytes: std::mem::take(&mut current),
                    });
                    seq = seq.saturating_add(1);
                    current = Vec::with_capacity(1024);
                }
                current.extend_from_slice(&unit);
            }
            if !current.is_empty() {
                chunks.push(Chunk {
                    producer: 0,
                    seq,
                    bytes: current,
                });
            }
            vec![chunks]
        }
        ProducerKind::SmallChunks => {
            let mut chunks = Vec::new();
            let mut seq = 0;
            let mut bytes = 0;
            while bytes < total_bytes {
                let size = 1 + (rng.next() % 16) as usize;
                push_chunks(&mut chunks, 0, size, &mut seq, seed);
                bytes += size;
            }
            vec![chunks]
        }
        ProducerKind::FlooderPlusQuiet => {
            let mut flooder = Vec::new();
            let mut quiet = Vec::new();
            let mut flood_seq = 0;
            let mut quiet_seq = 0;
            let mut bytes = 0;
            while bytes < total_bytes {
                push_chunks(&mut flooder, 0, 4096, &mut flood_seq, seed);
                bytes += 4096;
                if flood_seq.is_multiple_of(16) {
                    push_chunks(&mut quiet, 1, 64, &mut quiet_seq, seed);
                }
            }
            vec![flooder, quiet]
        }
        ProducerKind::OutputThenExit => {
            let mut chunks = Vec::new();
            let mut seq = 0;
            let mut bytes = 0;
            while bytes < total_bytes {
                let size = 512.min(total_bytes.saturating_sub(bytes).max(1));
                push_chunks(&mut chunks, 0, size, &mut seq, seed);
                bytes += size;
            }
            chunks.push(Chunk {
                producer: 0,
                seq,
                bytes: b"\x1b[0m[exit 0]\r\n".to_vec(),
            });
            vec![chunks]
        }
    }
}

fn fingerprint(chunks: &[Vec<Chunk>]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for chunk in chunks.iter().flatten() {
        for byte in [chunk.producer]
            .into_iter()
            .chain(chunk.seq.to_le_bytes())
            .chain(chunk.bytes.iter().copied())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn run_case(
    kind: ProducerKind,
    seed: u64,
    mode: SinkMode,
    total_bytes: usize,
    factors: LoadFactors,
) -> Observation {
    let started = Instant::now();
    let chunks = generate(kind, seed, total_bytes);
    let fingerprint = fingerprint(&chunks);
    let produced_chunks = chunks.iter().map(Vec::len).sum();
    let produced_bytes = chunks.iter().flatten().map(|chunk| chunk.bytes.len()).sum();
    let load = LoadRuntime::start(factors);
    let recording = load.recording_sink();
    let recording_bytes = Arc::clone(&load.recording_bytes);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(AtomicUsize::new(0));
    let sink_seen = Arc::clone(&seen);
    let sink_writes = Arc::clone(&writes);
    let sink = move |bytes: &[u8]| {
        let write = sink_writes.fetch_add(1, Ordering::SeqCst) + 1;
        let mut output = sink_seen.lock().unwrap_or_else(|error| error.into_inner());
        output.extend_from_slice(bytes);
        drop(output);
        if let Some(recording) = recording.as_ref() {
            let mut file = recording.lock().unwrap_or_else(|error| error.into_inner());
            file.write_all(bytes).expect("recording sink write");
            recording_bytes.fetch_add(bytes.len(), Ordering::SeqCst);
        }
        match mode {
            SinkMode::Throttled { per_write } => std::thread::sleep(per_write),
            SinkMode::FailAfter { writes } if write > writes => {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            SinkMode::Fast | SinkMode::FailAfter { .. } => {}
        }
        Ok(())
    };
    let writer = Arc::new(FrameWriter::spawn_with(false, true, |_| {}, sink));
    let admitted = Arc::new(Mutex::new(Vec::<(u8, u32)>::new()));
    let submission = Arc::new(Mutex::new(()));
    let mut producers = Vec::with_capacity(chunks.len());
    for producer_chunks in chunks.iter().cloned() {
        let writer = Arc::clone(&writer);
        let admitted = Arc::clone(&admitted);
        let submission = Arc::clone(&submission);
        producers.push(thegn_core::thread_name::spawn_named(
            "thegn-wl-prod",
            move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                let mut accepted = 0;
                let mut refused = 0;
                for chunk in producer_chunks {
                    let _submission = submission.lock().unwrap_or_else(|error| error.into_inner());
                    let mut bytes = chunk.bytes.clone();
                    let mut submitted = false;
                    let retry_deadline = Instant::now() + Duration::from_secs(10);
                    let mut attempts = 0;
                    loop {
                        match writer.try_submit_oob(crate::frame_writer::OobKind::Other, bytes) {
                            Ok(()) => {
                                admitted
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .push((chunk.producer, chunk.seq));
                                accepted += 1;
                                submitted = true;
                                break;
                            }
                            Err(returned) => {
                                bytes = returned;
                                if matches!(
                                    writer.take_status(),
                                    crate::frame_writer::WriterStatus::Fatal(_)
                                ) || Instant::now() >= retry_deadline
                                {
                                    break;
                                }
                                if attempts < 8 {
                                    std::thread::yield_now();
                                } else {
                                    std::thread::sleep(Duration::from_micros(50));
                                }
                                attempts += 1;
                            }
                        }
                    }
                    if !submitted {
                        refused += 1;
                    }
                }
                (accepted, refused)
            },
        ));
    }
    let mut accepted_chunks = 0;
    let mut refused_chunks = 0;
    let mut joined_producers = 0;
    for producer in producers {
        let (accepted, refused) = producer.join().expect("producer thread");
        accepted_chunks += accepted;
        refused_chunks += refused;
        joined_producers += 1;
    }
    let admitted = admitted
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let expected = admitted
        .iter()
        .filter_map(|(producer, seq)| {
            chunks
                .get(usize::from(*producer))?
                .iter()
                .find(|chunk| chunk.producer == *producer && chunk.seq == *seq)
                .map(|chunk| chunk.bytes.as_slice())
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    let status = format!("{:?}", writer.take_status());
    if !matches!(mode, SinkMode::FailAfter { .. }) {
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap_or_else(|error| error.into_inner()).len() < expected.len()
            && Instant::now() < drain_deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    drop(writer);
    let seen = seen
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mut last_seq = [None, None];
    let increasing = admitted.iter().all(|(producer, seq)| {
        let previous = &mut last_seq[usize::from(*producer)];
        let result = previous.is_none_or(|value| *seq > value);
        *previous = Some(*seq);
        result
    });
    let ordered = seen.len() <= expected.len()
        && expected.starts_with(&seen)
        && (matches!(mode, SinkMode::FailAfter { .. }) || seen == expected)
        && increasing;
    let quiet_progress = (kind == ProducerKind::FlooderPlusQuiet).then(|| {
        admitted
            .iter()
            .filter(|(producer, _)| *producer == 1)
            .count()
    });
    let (factor_counts, joined_load_threads) = load.finish();
    let admitted_order = admitted;
    Observation {
        kind: kind.label(),
        seed,
        fingerprint,
        mode: mode.label(),
        factors: factors.label(),
        produced_chunks,
        produced_bytes,
        accepted_chunks,
        refused_chunks,
        sink_writes: writes.load(Ordering::SeqCst),
        sink_bytes: seen.len(),
        ordered,
        admitted_order,
        quiet_progress,
        factor_counts,
        writer_status: status,
        elapsed_ms: started.elapsed().as_millis(),
        joined_producers,
        joined_load_threads,
    }
}

fn audit_manifest(seed: u64, total_bytes: usize, cases: &[LoadFactors]) -> serde_json::Value {
    let factor_cases = cases
        .iter()
        .map(|factors| factors.label())
        .collect::<Vec<_>>();
    serde_json::json!({
        "harness": "perf_workloads_pty",
        "slice": "b",
        "seed": seed,
        "total_bytes": total_bytes,
        "os": std::env::consts::OS,
        "crate_version": env!("CARGO_PKG_VERSION"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "git_rev": std::env::var("THEGN_AUDIT_GIT_REV").unwrap_or_else(|_| "unknown".to_string()),
        "rustc": option_env!("RUSTC_VERSION").unwrap_or("unknown"),
        "cpus": std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1),
        "kernel": match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            Ok(kernel) if !kernel.trim().is_empty() => kernel.trim().to_string(),
            Ok(_) | Err(_) => "unknown".to_string(),
        },
        "factors": factor_cases,
        "factor_list": ["log_burst", "recording", "hydration"],
        "factor_support": {
            "log_burst": {"supported": true},
            "recording": {"supported": true},
            "hydration": {
                "supported": false,
                "reason": "the existing hydration workload is an ignored test without a callable seam"
            }
        },
        "scope": "synthetic producers into FrameWriter::spawn_with with controlled sink and opt-in load factors; no PTY child, no host loop, no native terminal; elapsed_ms is harness wall time, not a terminal measurement"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_gives_same_fingerprint_for_every_kind() {
        for kind in ProducerKind::ALL {
            assert_eq!(
                fingerprint(&generate(kind, 7, 8192)),
                fingerprint(&generate(kind, 7, 8192))
            );
        }
    }

    #[test]
    fn different_seed_changes_the_fingerprint_for_random_kinds() {
        for kind in [
            ProducerKind::ContinuousText,
            ProducerKind::EscapeHeavy,
            ProducerKind::SmallChunks,
            ProducerKind::FlooderPlusQuiet,
        ] {
            assert_ne!(
                fingerprint(&generate(kind, 7, 8192)),
                fingerprint(&generate(kind, 8, 8192))
            );
        }
    }

    #[test]
    fn escape_heavy_chunks_never_split_a_sequence() {
        for chunk in &generate(ProducerKind::EscapeHeavy, 11, 8192)[0] {
            let Some(last_escape) = chunk.bytes.iter().rposition(|byte| *byte == 0x1b) else {
                continue;
            };
            assert_eq!(chunk.bytes.get(last_escape + 1), Some(&b'['));
            assert!(chunk.bytes.get(last_escape + 2..).is_some_and(|tail| {
                tail.iter()
                    .take_while(|byte| **byte != 0x1b)
                    .any(|byte| (0x40..=0x7e).contains(byte))
            }));
        }
    }

    #[test]
    fn output_then_exit_ends_with_the_marker() {
        let chunks = generate(ProducerKind::OutputThenExit, 11, 8192);
        assert_eq!(
            chunks[0].last().map(|chunk| chunk.bytes.as_slice()),
            Some(&b"\x1b[0m[exit 0]\r\n"[..])
        );
    }

    #[test]
    fn flooder_plus_quiet_has_two_producers_with_rising_seq() {
        let chunks = generate(ProducerKind::FlooderPlusQuiet, 11, 70_000);
        assert_eq!(chunks.len(), 2);
        for producer in &chunks {
            assert!(producer.windows(2).all(|pair| pair[0].seq < pair[1].seq));
        }
    }

    #[test]
    fn fast_sink_preserves_order_and_accounting() {
        for kind in ProducerKind::ALL {
            let observation = run_case(kind, 11, SinkMode::Fast, 8 * 1024, LoadFactors::none());
            assert!(
                observation.ordered,
                "{} fast stream preserves admitted order",
                kind.label()
            );
            assert_eq!(observation.refused_chunks, 0);
            assert_eq!(observation.sink_bytes, observation.produced_bytes);
            assert_eq!(
                observation.accepted_chunks + observation.refused_chunks,
                observation.produced_chunks
            );
            assert_eq!(
                observation.joined_producers,
                generate(kind, 11, 8 * 1024).len()
            );
        }
    }

    #[test]
    fn throttled_sink_keeps_order_and_accounting() {
        for kind in [ProducerKind::ContinuousText, ProducerKind::FlooderPlusQuiet] {
            let observation = run_case(
                kind,
                11,
                SinkMode::Throttled {
                    per_write: Duration::from_millis(1),
                },
                16 * 1024,
                LoadFactors::none(),
            );
            assert!(
                observation.ordered,
                "{} throttled stream preserves admitted order",
                kind.label()
            );
            assert!(
                observation.elapsed_ms < 5_000,
                "throttled case returns within a generous bound: {}ms",
                observation.elapsed_ms
            );
            assert_eq!(
                observation.accepted_chunks + observation.refused_chunks,
                observation.produced_chunks
            );
            assert_eq!(
                observation.joined_producers,
                generate(kind, 11, 16 * 1024).len()
            );
        }
    }

    #[test]
    fn failing_sink_reports_a_degraded_writer_and_returns() {
        let observation = run_case(
            ProducerKind::ContinuousText,
            11,
            SinkMode::FailAfter { writes: 1 },
            16 * 1024,
            LoadFactors::none(),
        );
        assert!(
            observation.ordered,
            "failed stream remains an admitted-order prefix"
        );
        assert!(observation.sink_writes >= 1);
        assert!(
            observation.elapsed_ms < 5_000,
            "failure case returns within a generous bound: {}ms",
            observation.elapsed_ms
        );
        assert_eq!(observation.joined_producers, 1);
    }

    #[test]
    fn run_case_joins_every_producer() {
        for kind in [ProducerKind::FlooderPlusQuiet, ProducerKind::ContinuousText] {
            let total = 16 * 1024;
            let observation = run_case(kind, 11, SinkMode::Fast, total, LoadFactors::none());
            assert_eq!(
                observation.joined_producers,
                generate(kind, 11, total).len()
            );
        }
    }

    #[test]
    fn flooder_plus_quiet_fast_sink_admits_quiet_output() {
        let observation = run_case(
            ProducerKind::FlooderPlusQuiet,
            11,
            SinkMode::Fast,
            128 * 1024,
            LoadFactors::none(),
        );
        assert!(observation.quiet_progress.is_some_and(|chunks| chunks > 0));
        assert!(
            observation
                .admitted_order
                .iter()
                .any(|(producer, _)| *producer == 1)
        );
    }

    /// Run with `cargo test -p thegn-host --release perf_workloads_pty --
    /// --ignored --nocapture` to emit raw, machine-readable observations.
    #[test]
    #[ignore = "controlled PTY-shaped sink workload; opt-in, machine-dependent"]
    #[allow(
        clippy::disallowed_macros,
        reason = "opt-in fixture emits measured artifact"
    )]
    fn controlled_pty_sink_matrix() {
        let seed = std::env::var("THEGN_AUDIT_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let total_bytes = std::env::var("THEGN_AUDIT_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(262144);
        let modes = [
            SinkMode::Fast,
            SinkMode::Throttled {
                per_write: Duration::from_millis(2),
            },
            SinkMode::FailAfter { writes: 3 },
        ];
        let factor_cases = LoadFactors::audit_cases();
        let observations = ProducerKind::ALL
            .into_iter()
            .flat_map(|kind| {
                modes.into_iter().flat_map(|mode| {
                    factor_cases
                        .iter()
                        .copied()
                        .map(move |factors| run_case(kind, seed, mode, total_bytes, factors))
                })
            })
            .collect::<Vec<_>>();
        let artifact = serde_json::json!({
            "manifest": audit_manifest(seed, total_bytes, &factor_cases),
            "observations": observations
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&artifact).expect("serialize workload artifact")
        );
    }

    #[test]
    fn same_seed_same_fingerprint_with_factors_on() {
        let factors = LoadFactors::all();
        let first = run_case(ProducerKind::EscapeHeavy, 7, SinkMode::Fast, 8192, factors);
        let second = run_case(ProducerKind::EscapeHeavy, 7, SinkMode::Fast, 8192, factors);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.factors, "log_burst+recording+hydration");
    }

    #[test]
    fn factors_join_load_threads() {
        let observation = run_case(
            ProducerKind::FlooderPlusQuiet,
            11,
            SinkMode::Fast,
            16 * 1024,
            LoadFactors::all(),
        );
        assert_eq!(observation.joined_load_threads, 1);
        assert!(observation.quiet_progress.is_some_and(|chunks| chunks > 0));
        assert!(observation.factor_counts.recording_bytes > 0);
        assert!(observation.factor_counts.log_burst_lines > 0);
        let mut samples = Vec::new();
        crate::platform::thread_census::sample_self(&mut samples, 256);
        assert!(
            !samples
                .iter()
                .any(|sample| sample.comm_str() == "thegn-wl-log")
        );
    }

    #[test]
    fn failing_sink_with_factors_still_returns() {
        let observation = run_case(
            ProducerKind::ContinuousText,
            11,
            SinkMode::FailAfter { writes: 1 },
            16 * 1024,
            LoadFactors::all(),
        );
        assert!(observation.elapsed_ms < 5_000);
        assert_eq!(observation.joined_load_threads, 1);
        assert!(observation.factor_counts.recording_bytes > 0);
    }

    #[test]
    fn manifest_contains_slice_b_factor_and_host_keys() {
        let manifest = audit_manifest(11, 8192, &[LoadFactors::log_burst(), LoadFactors::all()]);
        let object = manifest.as_object().expect("manifest object");
        for key in [
            "git_rev",
            "rustc",
            "cpus",
            "kernel",
            "factors",
            "factor_list",
            "factor_support",
            "slice",
        ] {
            assert!(object.contains_key(key), "manifest key {key}");
        }
        assert_eq!(object.get("slice"), Some(&serde_json::json!("b")));
        assert_eq!(
            object
                .get("factor_support")
                .and_then(|support| support.get("hydration"))
                .and_then(|hydration| hydration.get("supported")),
            Some(&serde_json::json!(false))
        );
    }
}
