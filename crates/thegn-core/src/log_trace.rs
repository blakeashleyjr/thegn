//! Diagnostics: a `tracing` subscriber with a compact, branded, human-readable
//! formatter and an optional rotating file sink.
//!
//! The stderr sink mirrors the historic `✦ thegn` look (coloured on a TTY).
//! The file sink (opt-in via `[log] file`) is plain + timestamped and rotates by
//! size with a hand-rolled writer (no `tracing-appender` — it only rotates by
//! time). `THEGN_LOG` is an env-filter directive string (e.g.
//! `debug,thegn::db=trace`) that overrides the configured default level.
//!
//! `msg::{info,warn,error}` route here once [`init`] has run (see [`ready`]);
//! before that — and for `msg::die` — they print straight to stderr so early
//! config diagnostics are never lost.
// The file-open-failure fallback writes to stderr directly (logging isn't up).
#![allow(clippy::disallowed_macros)]

use crate::config::{LogConfig, LogFormat, LogLevel};
use crate::theme;
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

static READY: AtomicBool = AtomicBool::new(false);

/// Whether [`init`] has installed the subscriber. `msg` consults this so its
/// functions fall back to direct stderr before logging is up.
pub fn ready() -> bool {
    READY.load(Ordering::SeqCst)
}

thread_local! {
    /// The worktree tag for the current thread — a short slug attached to every
    /// log line the thread emits while a [`WtGuard`] is in scope. The `fmt`
    /// subscriber formats an event on the thread that emitted it, so a
    /// thread-local set for the duration of a worktree-scoped `spawn_blocking`
    /// closure reliably tags all of that closure's logs. Empty ⇒ host-global.
    static CURRENT_WT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// RAII guard that tags the current thread's log lines with a worktree slug and
/// restores the previous tag (usually `None`) on drop. Attach it at the top of a
/// worktree-scoped unit of work (provisioning, pane spawn, a per-worktree
/// refresh) so its diagnostics are attributable — the Logs panel then filters to
/// the active worktree by default. See [`enter_wt`].
#[must_use = "the worktree log tag is cleared as soon as the guard is dropped"]
pub struct WtGuard(Option<String>);

/// Tag this thread's log lines with a worktree slug until the returned guard is
/// dropped. Use [`wt_slug`] to derive a stable slug from a worktree path so both
/// the emitter and the Logs-panel filter agree on the key.
pub fn enter_wt(slug: impl Into<String>) -> WtGuard {
    let slug = slug.into();
    let prev = CURRENT_WT.with(|c| c.replace(if slug.is_empty() { None } else { Some(slug) }));
    WtGuard(prev)
}

impl Drop for WtGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT_WT.with(|c| *c.borrow_mut() = prev);
    }
}

/// The current thread's worktree tag, if any.
fn current_wt() -> Option<String> {
    CURRENT_WT.with(|c| c.borrow().clone())
}

/// A stable, short worktree tag derived from a worktree path — the directory's
/// basename, slugified. Both the log emitter ([`enter_wt`]) and the Logs-panel
/// filter derive the key this way so they compare equal. Empty path ⇒ `""`.
pub fn wt_slug(path: &Path) -> String {
    path.file_name()
        .map(|n| crate::util::slugify(&n.to_string_lossy()))
        .unwrap_or_default()
}

pub fn audit(event: &str) {
    let dir = crate::util::thegn_dir();
    let audit_log = dir.join("audit.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_log)
    {
        use std::io::Write;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "[{now}] {event}");
    }
}

/// Initialize tracing. Fails silently if called twice (e.g., e2e tests).
/// daemon (whose stdout/stderr are nulled) skips the pointless stderr layer.
pub enum Role {
    Cli,
    Watch {
        session: String,
    },
    /// The native compositor: file sink only — stderr would write into the
    /// alternate screen and corrupt the frame.
    Host,
}

impl Role {
    fn log_file(&self) -> String {
        match self {
            Role::Cli => "thegn.log".into(),
            Role::Watch { session } => format!("watch-{}.log", crate::util::slugify(session)),
            Role::Host => "thegn.log".into(),
        }
    }
    fn wants_stderr(&self) -> bool {
        matches!(self, Role::Cli)
    }
}

fn level_filter(default: LogLevel) -> EnvFilter {
    // `THEGN_LOG` (tracing directives) wins; else the configured level.
    match std::env::var("THEGN_LOG") {
        Ok(s) if !s.trim().is_empty() => EnvFilter::builder().parse_lossy(s),
        _ => EnvFilter::new(default.as_str()),
    }
}

/// Install the global subscriber. Idempotent and best-effort: a second call (or
/// a failure to open the log file) is swallowed so logging never aborts a run.
pub fn init(role: Role, cfg: &LogConfig) {
    let filter = level_filter(cfg.level);
    let stderr_ansi = io::stderr().is_terminal();

    let stderr_layer = role.wants_stderr().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .event_format(Brand {
                ansi: stderr_ansi,
                timestamp: false,
            })
    });

    let file_layer = if cfg.file {
        match FileSink::open(cfg, &role.log_file()) {
            Ok(sink) => Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(sink)
                    .event_format(Brand {
                        ansi: false,
                        timestamp: matches!(cfg.format, LogFormat::Text),
                    }),
            ),
            Err(e) => {
                // Can't log via tracing yet — say so on stderr directly.
                eprintln!("thegn: could not open log file: {e}");
                None
            }
        }
    } else {
        None
    };

    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .is_ok();
    if installed {
        READY.store(true, Ordering::Relaxed);
    }
}

/// The compact branded formatter, shared by both sinks.
///
/// stderr (tty): `✦ thegn  WARN  thegn::worktree  created sz/foo`
/// file:         `2026-06-05T12:00:00  WARN  thegn::worktree  created sz/foo`
struct Brand {
    ansi: bool,
    timestamp: bool,
}

impl Brand {
    fn hue(level: &Level) -> &'static str {
        match *level {
            Level::ERROR => theme::RED,
            Level::WARN => theme::AMBER,
            Level::INFO => theme::DIM,
            Level::DEBUG => theme::FAINT,
            Level::TRACE => theme::GHOST,
        }
    }
}

impl<S, N> FormatEvent<S, N> for Brand
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let level = meta.level();
        let target = meta.target();

        if self.timestamp {
            // Local wall-clock via the already-present `chrono` dep.
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            write!(writer, "{ts}  ")?;
        }

        if self.ansi {
            // Branded prefix: faint magenta star + level-hued level tag.
            write!(
                writer,
                "\x1b[38;2;{}m\u{2726}\x1b[0m \x1b[38;2;{}m{:<5}\x1b[0m \x1b[38;2;{}m{}\x1b[0m  ",
                theme::MAGENTA,
                Self::hue(level),
                level.as_str(),
                theme::FAINT,
                target,
            )?;
        } else {
            write!(writer, "{:<5} {}  ", level.as_str(), target)?;
        }

        // Worktree attribution: when the emitting thread is inside a `WtGuard`,
        // tag the line so the Logs panel can filter to the active worktree. The
        // ` wt=<slug>  ` token sits between target and message in a fixed spot the
        // parser (`log::parser::parse_log`) extracts and strips from the message.
        if let Some(wt) = current_wt() {
            if self.ansi {
                write!(writer, "\x1b[38;2;{}mwt={wt}\x1b[0m  ", theme::FAINT)?;
            } else {
                write!(writer, "wt={wt}  ")?;
            }
        }

        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// A size-capped log writer: one-step rotation (`.log` → `.log.1` → … →
/// `.log.<max>`) once the active file passes `cap_bytes`. Cheap to clone (an
/// `Arc<Mutex<…>>`); locks per write.
#[derive(Clone)]
struct FileSink(Arc<Mutex<Rotating>>);

struct Rotating {
    path: PathBuf,
    cap_bytes: u64,
    max_files: usize,
    file: File,
    size: u64,
}

impl FileSink {
    fn open(cfg: &LogConfig, name: &str) -> io::Result<FileSink> {
        let dir = cfg.dir_path();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(FileSink(Arc::new(Mutex::new(Rotating {
            path,
            cap_bytes: cfg.rotation_size_mb.max(1) * 1024 * 1024,
            max_files: cfg.max_files.max(1),
            file,
            size,
        }))))
    }
}

impl Rotating {
    fn rotate(&mut self) -> io::Result<()> {
        // Shift .log.(n-1) → .log.n, dropping the oldest, then .log → .log.1.
        for n in (1..self.max_files).rev() {
            let from = self.numbered(n);
            let to = self.numbered(n + 1);
            if from.exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }
        let _ = std::fs::rename(&self.path, self.numbered(1));
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = 0;
        Ok(())
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }
}

impl Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self.0.lock().map_err(|_| io::Error::other("log lock"))?;
        if g.size + buf.len() as u64 > g.cap_bytes {
            g.rotate()?;
        }
        let n = g.file.write(buf)?;
        g.size += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("log lock"))?
            .file
            .flush()
    }
}

// `MakeWriter` for the fmt layer: hand back a cheap clone per event.
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileSink {
    type Writer = FileSink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogConfig;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sz-log-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn file_sink_writes_and_rotates() {
        let dir = tmp("rot");
        let cfg = LogConfig {
            file: true,
            dir: dir.to_string_lossy().into_owned(),
            rotation_size_mb: 1, // cap is forced below via direct field poke
            max_files: 3,
            ..LogConfig::default()
        };
        let mut sink = FileSink::open(&cfg, "t.log").unwrap();
        // Shrink the cap so we don't write a megabyte in a test.
        sink.0.lock().unwrap().cap_bytes = 64;
        for _ in 0..10 {
            sink.write_all(b"0123456789ABCDEF0123456789\n").unwrap();
        }
        // Active file + at least one rotated file exist.
        assert!(dir.join("t.log").exists());
        assert!(dir.join("t.log.1").exists());
        // Never keep more than max_files rotations.
        assert!(!dir.join("t.log.4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_filter_prefers_thegn_log() {
        // Just ensure construction doesn't panic for both paths.
        let _ = level_filter(LogLevel::Info);
    }

    #[test]
    fn wt_slug_is_basename_slug() {
        assert_eq!(
            wt_slug(Path::new("/home/me/wt/sz-solid-glen")),
            "sz-solid-glen"
        );
        assert_eq!(wt_slug(Path::new("/repo/app feat")), "app-feat");
        assert_eq!(wt_slug(Path::new("")), "");
    }

    #[test]
    fn enter_wt_sets_and_restores_thread_tag() {
        assert!(current_wt().is_none());
        {
            let _g = enter_wt("wt-a");
            assert_eq!(current_wt().as_deref(), Some("wt-a"));
            {
                // Nested guard overrides, then restores the outer tag on drop.
                let _g2 = enter_wt("wt-b");
                assert_eq!(current_wt().as_deref(), Some("wt-b"));
            }
            assert_eq!(current_wt().as_deref(), Some("wt-a"));
        }
        assert!(current_wt().is_none());
        // An empty slug is a no-op tag (host-global).
        let _g = enter_wt("");
        assert!(current_wt().is_none());
    }
}
