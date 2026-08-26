//! Crash capture and process identity — the always-on, zero-I/O half of the
//! diagnostics story.
//!
//! Two things live here, both free at idle:
//!
//! * **Process identity** — a per-process run id (short, sortable) and a
//!   process-kind discriminator (`host`/`daemon`/`cli`) that `log_trace` stamps
//!   on every log line so a session can be correlated across the compositor's
//!   and the daemon's files.
//! * **The always-on ring + crash reports** — a fixed-size in-memory ring of
//!   WARN-and-above events (fed by the minimal `log_trace` ring layer) that
//!   performs no I/O until a crash or a debug bundle reads it, plus the
//!   best-effort crash-report writer the panic hook calls. The report writer is
//!   pure formatting (unit-tested) over a snapshot; only the final write touches
//!   disk.

use crate::log::buffer::LogBuffer;
use crate::log::parser::{LogLevel, ParsedLog};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Which thegn process emitted a line / produced a crash report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcKind {
    Host,
    Daemon,
    Cli,
}

impl ProcKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcKind::Host => "host",
            ProcKind::Daemon => "daemon",
            ProcKind::Cli => "cli",
        }
    }
}

static RUN_ID: OnceLock<String> = OnceLock::new();
static PROC_KIND: OnceLock<ProcKind> = OnceLock::new();

/// This process's run id — minted once, lazily. Short and roughly sortable:
/// base-36 seconds-since-epoch followed by the base-36 pid, so files/lines sort
/// by start time and stay unique per process (`hx7f3a`-shaped).
pub fn run_id() -> &'static str {
    RUN_ID.get_or_init(mint_run_id).as_str()
}

fn mint_run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    format!("{}{}", to_base36(secs), to_base36(pid))
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Record which kind of process this is (host/daemon/cli). Called once at
/// startup, before the subscriber is installed.
pub fn set_proc_kind(kind: ProcKind) {
    let _ = PROC_KIND.set(kind);
}

/// This process's kind. Defaults to `Cli` if never set (a plain verb).
pub fn proc_kind() -> ProcKind {
    *PROC_KIND.get().unwrap_or(&ProcKind::Cli)
}

pub fn proc_kind_str() -> &'static str {
    proc_kind().as_str()
}

// ---------------------------------------------------------------------------
// Build/version identity — the host registers the full block (channel, git sha,
// build time); a process that never registers (a unit test) falls back to the
// crate version + platform constants.
// ---------------------------------------------------------------------------

/// Version/channel/build identity for the crash report and `doctor`.
#[derive(Clone, Debug)]
pub struct Identity {
    pub version: String,
    pub channel: String,
    /// Git sha and/or build time, when embedded at build time.
    pub build: Option<String>,
    pub os: String,
    pub arch: String,
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            channel: "unknown".to_string(),
            build: None,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// Register the installation identity (host does this at startup).
pub fn set_identity(id: Identity) {
    let _ = IDENTITY.set(id);
}

/// The registered identity, or a crate-version fallback.
pub fn identity() -> Identity {
    IDENTITY.get().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The always-on WARN+ ring. No I/O until snapshotted by a crash report/bundle.
// ---------------------------------------------------------------------------

/// Default ring capacity (events) when the host does not configure one.
pub const DEFAULT_RING_CAPACITY: usize = 256;

static RING: OnceLock<Mutex<LogBuffer>> = OnceLock::new();
static RING_CAP: AtomicUsize = AtomicUsize::new(DEFAULT_RING_CAPACITY);

/// Set the ring capacity. Must be called before the first event is captured
/// (i.e. before the subscriber is installed); ignored afterward.
pub fn set_ring_capacity(cap: usize) {
    RING_CAP.store(cap.max(1), Ordering::SeqCst);
}

fn ring() -> &'static Mutex<LogBuffer> {
    RING.get_or_init(|| Mutex::new(LogBuffer::new(RING_CAP.load(Ordering::SeqCst))))
}

/// Push a captured event into the ring. Best-effort and non-blocking: a
/// contended or poisoned lock drops the event rather than stalling the emitter
/// (which may be the panic hook holding no locks it must not re-take).
pub fn ring_push(event: ParsedLog) {
    if let Ok(mut g) = ring().try_lock() {
        g.push_parsed(event);
    }
}

/// Snapshot the ring's contents as rendered lines (oldest first). Best-effort:
/// a contended lock yields an empty tail rather than blocking the crash writer.
pub fn ring_snapshot() -> Vec<String> {
    match ring().try_lock() {
        Ok(g) => g.iter().map(render_ring_line).collect(),
        Err(_) => Vec::new(),
    }
}

fn render_ring_line(p: &ParsedLog) -> String {
    if p.original.is_empty() {
        format!("{}  {:?}  {}", p.timestamp, p.level, p.message)
    } else {
        p.original.clone()
    }
}

// ---------------------------------------------------------------------------
// Crash reports.
// ---------------------------------------------------------------------------

/// The crash directory (`$XDG_STATE_HOME/thegn/crash`).
pub fn crash_dir() -> PathBuf {
    crate::util::xdg_state_home().join("thegn/crash")
}

/// Default number of crash reports to retain.
pub const DEFAULT_CRASH_RETENTION: usize = 10;

static CRASH_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static CRASH_RETENTION: AtomicUsize = AtomicUsize::new(DEFAULT_CRASH_RETENTION);

/// Configure crash reporting from `[diagnostics]` (host, at startup). `enabled`
/// off makes the panic hook skip the report file entirely; `retain` bounds the
/// crash dir.
pub fn configure_crash(enabled: bool, retain: usize) {
    CRASH_ENABLED.store(enabled, Ordering::SeqCst);
    CRASH_RETENTION.store(retain.max(1), Ordering::SeqCst);
}

pub fn crash_enabled() -> bool {
    CRASH_ENABLED.load(Ordering::SeqCst)
}

pub fn crash_retention() -> usize {
    CRASH_RETENTION.load(Ordering::SeqCst)
}

/// Assemble and write a crash report for the current panic. Called by the panic
/// hook. Captures a backtrace regardless of `RUST_BACKTRACE`, the ring tail, and
/// the installation identity. Never panics; returns the report path or `None`.
pub fn report_panic(panic_line: &str) -> Option<PathBuf> {
    if !crash_enabled() {
        return None;
    }
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    let report = CrashReport {
        identity: identity(),
        run_id: run_id().to_string(),
        proc_kind: proc_kind(),
        thread,
        panic_line: panic_line.to_string(),
        backtrace: std::backtrace::Backtrace::force_capture().to_string(),
        ring_tail: ring_snapshot(),
    };
    write_crash_report(&report, crash_retention())
}

/// A fully-formed crash report, pure over its inputs so [`CrashReport::render`]
/// is unit-testable without touching disk.
#[derive(Clone, Debug)]
pub struct CrashReport {
    pub identity: Identity,
    pub run_id: String,
    pub proc_kind: ProcKind,
    pub thread: String,
    /// The `thread '…' panicked at …` line (same shape the default hook prints).
    pub panic_line: String,
    pub backtrace: String,
    pub ring_tail: Vec<String>,
}

impl CrashReport {
    /// Render the report body — deterministic given its fields.
    pub fn render(&self) -> String {
        let id = &self.identity;
        let mut s = String::new();
        s.push_str("thegn crash report\n");
        s.push_str("==================\n");
        s.push_str(&format!("version:   {}\n", id.version));
        s.push_str(&format!("channel:   {}\n", id.channel));
        s.push_str(&format!(
            "build:     {}\n",
            id.build.as_deref().unwrap_or("(unknown)")
        ));
        s.push_str(&format!("os/arch:   {}/{}\n", id.os, id.arch));
        s.push_str(&format!("process:   {}\n", self.proc_kind.as_str()));
        s.push_str(&format!("run:       {}\n", self.run_id));
        s.push_str(&format!("thread:    {}\n", self.thread));
        s.push('\n');
        s.push_str(&self.panic_line);
        if !self.panic_line.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("\nbacktrace:\n");
        s.push_str(&self.backtrace);
        if !self.backtrace.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("\nrecent warnings (ring tail):\n");
        if self.ring_tail.is_empty() {
            s.push_str("(none)\n");
        } else {
            for line in &self.ring_tail {
                s.push_str(line);
                s.push('\n');
            }
        }
        s
    }

    /// The report file's base name: `<UTC-ts>-<run_id>.txt`, sortable by time.
    pub fn file_name(&self, now: chrono::DateTime<chrono::Utc>) -> String {
        format!("{}-{}.txt", now.format("%Y%m%dT%H%M%S%3f"), self.run_id)
    }
}

/// Write `report` to the crash dir best-effort, prune to `retain`, and return
/// the path written. Never panics; a failure returns `None`. The directory is
/// created `0700` and the report `0600`.
pub fn write_crash_report(report: &CrashReport, retain: usize) -> Option<PathBuf> {
    let dir = crash_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let _ = crate::fsperm::restrict_dir_to_owner(&dir);
    let name = report.file_name(chrono::Utc::now());
    let path = dir.join(&name);
    std::fs::write(&path, report.render()).ok()?;
    let _ = crate::fsperm::restrict_to_owner(&path);
    prune_reports(&dir, retain.max(1));
    Some(path)
}

/// List report file names (basenames) in `dir`, newest last (name-sorted).
fn report_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".txt"))
        .collect();
    names.sort();
    names
}

/// Which report basenames to prune to retain the newest `keep` (pure).
pub fn reports_to_prune(mut names: Vec<String>, keep: usize) -> Vec<String> {
    names.sort();
    if names.len() <= keep {
        return Vec::new();
    }
    let cut = names.len() - keep;
    names.into_iter().take(cut).collect()
}

/// Remove all but the newest `keep` reports (and their `.ack` markers).
fn prune_reports(dir: &Path, keep: usize) {
    for name in reports_to_prune(report_names(dir), keep) {
        let _ = std::fs::remove_file(dir.join(&name));
        let _ = std::fs::remove_file(dir.join(format!("{name}.ack")));
    }
}

// ---------------------------------------------------------------------------
// Unacknowledged-crash detection (marker-file scheme, no DB).
// ---------------------------------------------------------------------------

/// Report basenames in `names` that have no acknowledgement marker in
/// `ack_names` (pure). A report `X.txt` is acknowledged by a sibling `X.txt.ack`.
pub fn unacknowledged(names: &[String], ack_names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| n.ends_with(".txt"))
        .filter(|n| !ack_names.iter().any(|a| a == &format!("{n}.ack")))
        .cloned()
        .collect()
}

/// Paths of crash reports not yet acknowledged (best-effort; empty if the dir is
/// absent). Newest last.
pub fn unacknowledged_reports() -> Vec<PathBuf> {
    let dir = crash_dir();
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let acks: Vec<String> = entries
        .iter()
        .filter(|n| n.ends_with(".ack"))
        .cloned()
        .collect();
    let reports: Vec<String> = entries
        .iter()
        .filter(|n| n.ends_with(".txt"))
        .cloned()
        .collect();
    unacknowledged(&reports, &acks)
        .into_iter()
        .map(|n| dir.join(n))
        .collect()
}

/// Mark a crash report acknowledged so it is not surfaced again (writes an empty
/// `<report>.ack` sibling). Best-effort.
pub fn acknowledge(report: &Path) {
    let mut ack = report.as_os_str().to_os_string();
    ack.push(".ack");
    let _ = std::fs::write(PathBuf::from(ack), b"");
}

/// All retained crash reports, newest last (for `doctor` / bundle).
pub fn list_reports() -> Vec<PathBuf> {
    let dir = crash_dir();
    report_names(&dir)
        .into_iter()
        .map(|n| dir.join(n))
        .collect()
}

/// Map a `tracing` level to the parser's `LogLevel` (for ring capture).
pub fn tracing_level_to_parser(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::ERROR => LogLevel::Error,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::TRACE => LogLevel::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_identity() -> Identity {
        Identity {
            version: "9.9.9".into(),
            channel: "dev".into(),
            build: Some("abc1234".into()),
            os: "linux".into(),
            arch: "x86_64".into(),
        }
    }

    fn sample_report() -> CrashReport {
        CrashReport {
            identity: sample_identity(),
            run_id: "hx7f3a".into(),
            proc_kind: ProcKind::Host,
            thread: "main".into(),
            panic_line: "thread 'main' panicked at src/x.rs:1:1:\nboom".into(),
            backtrace: "0: frame_a\n1: frame_b".into(),
            ring_tail: vec![
                "2026-01-01T00:00:00  WARN  thegn::x  something odd".into(),
                "2026-01-01T00:00:01  ERROR thegn::y  it broke".into(),
            ],
        }
    }

    #[test]
    fn run_id_is_stable_and_nonempty() {
        let a = run_id();
        let b = run_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn base36_roundtrips_shape() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
    }

    #[test]
    fn proc_kind_strings() {
        assert_eq!(ProcKind::Host.as_str(), "host");
        assert_eq!(ProcKind::Daemon.as_str(), "daemon");
        assert_eq!(ProcKind::Cli.as_str(), "cli");
    }

    #[test]
    fn render_contains_all_sections() {
        let r = sample_report();
        let out = r.render();
        assert!(out.contains("version:   9.9.9"));
        assert!(out.contains("channel:   dev"));
        assert!(out.contains("build:     abc1234"));
        assert!(out.contains("os/arch:   linux/x86_64"));
        assert!(out.contains("process:   host"));
        assert!(out.contains("run:       hx7f3a"));
        assert!(out.contains("thread:    main"));
        assert!(out.contains("panicked at src/x.rs"));
        assert!(out.contains("backtrace:"));
        assert!(out.contains("frame_a"));
        assert!(out.contains("recent warnings (ring tail):"));
        assert!(out.contains("it broke"));
    }

    #[test]
    fn render_handles_empty_ring() {
        let mut r = sample_report();
        r.ring_tail.clear();
        assert!(r.render().contains("(none)"));
    }

    #[test]
    fn file_name_is_timestamped_and_sortable() {
        let r = sample_report();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let name = r.file_name(ts);
        assert!(name.starts_with("20260102T030405"));
        assert!(name.ends_with("-hx7f3a.txt"));
    }

    #[test]
    fn prune_keeps_newest() {
        let names = vec![
            "20260101T000001000-a.txt".to_string(),
            "20260101T000002000-b.txt".to_string(),
            "20260101T000003000-c.txt".to_string(),
        ];
        let prune = reports_to_prune(names, 2);
        assert_eq!(prune, vec!["20260101T000001000-a.txt".to_string()]);
    }

    #[test]
    fn prune_noop_when_under_limit() {
        let names = vec!["a.txt".to_string(), "b.txt".to_string()];
        assert!(reports_to_prune(names, 10).is_empty());
    }

    #[test]
    fn unacknowledged_filters_acked() {
        let reports = vec!["r1.txt".to_string(), "r2.txt".to_string()];
        let acks = vec!["r1.txt.ack".to_string()];
        let out = unacknowledged(&reports, &acks);
        assert_eq!(out, vec!["r2.txt".to_string()]);
    }

    #[test]
    fn prune_and_ack_over_a_temp_dir() {
        // Exercise the on-disk prune/ack helpers over an explicit temp dir
        // (no global-env mutation — hermetic under both nextest and cargo test).
        let dir = std::env::temp_dir().join(format!("tg-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            let name = format!("2026010{}T000000000-run{i}.txt", i + 1);
            std::fs::write(dir.join(&name), b"x").unwrap();
        }
        prune_reports(&dir, 2);
        assert_eq!(report_names(&dir).len(), 2, "pruned to newest 2");

        // Acknowledge the oldest remaining; the other stays unacknowledged.
        let names = report_names(&dir);
        let all_ack: Vec<String> = Vec::new();
        assert_eq!(unacknowledged(&names, &all_ack).len(), 2);
        acknowledge(&dir.join(&names[0]));
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        let acks: Vec<String> = entries
            .iter()
            .filter(|n| n.ends_with(".ack"))
            .cloned()
            .collect();
        assert_eq!(unacknowledged(&names, &acks).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
