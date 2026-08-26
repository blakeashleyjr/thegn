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
use crate::diagnostics;
use crate::theme;
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
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

/// Is `size_bytes` over a `cap_mb`-MiB cap? (`cap_mb == 0` disables the cap.)
pub fn over_cap(size_bytes: u64, cap_mb: u64) -> bool {
    cap_mb > 0 && size_bytes > cap_mb.saturating_mul(1024 * 1024)
}

/// Rotate `path` aside (`path` → `path.1`, replacing any prior `.1`) at startup
/// if it exceeds `cap_mb` MiB. Best-effort and one-generation — used to bound
/// the otherwise-uncapped `thegn-stderr.log` and `audit.log` across restarts.
pub fn rotate_if_over(path: &Path, cap_mb: u64) {
    if let Ok(meta) = std::fs::metadata(path)
        && over_cap(meta.len(), cap_mb)
    {
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
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
    /// The pane daemon: its OWN file (`thegn-daemon.log`), so the compositor and
    /// the daemon never share one rotation state machine. No stderr layer (its
    /// stdio is nulled).
    Daemon,
}

impl Role {
    fn log_file(&self) -> String {
        match self {
            Role::Cli => "thegn.log".into(),
            Role::Watch { session } => format!("watch-{}.log", crate::util::slugify(session)),
            Role::Host => "thegn.log".into(),
            Role::Daemon => "thegn-daemon.log".into(),
        }
    }
    fn wants_stderr(&self) -> bool {
        matches!(self, Role::Cli)
    }
    /// The process-kind discriminator stamped on every line and crash report.
    fn proc_kind(&self) -> diagnostics::ProcKind {
        match self {
            Role::Host => diagnostics::ProcKind::Host,
            Role::Daemon => diagnostics::ProcKind::Daemon,
            Role::Cli | Role::Watch { .. } => diagnostics::ProcKind::Cli,
        }
    }
}

/// Baseline directive for records bridged in from the `log` crate.
///
/// `tokei` warns per file it cannot classify — `Unknown extension: crt`,
/// `Unknown MIME: …` — in bursts of ~500 within a single second, every time the
/// LOC scan runs. One session logged 14,917 of them. They are noise to us (the
/// scan's *result* is what we use), they drown real WARNs in the log, and they
/// land in the always-on in-memory ring that backs crash reports and the debug
/// bundle — a ring that is supposed to cost nothing while idle.
///
/// This is deliberately blunt: `tracing-log` gives every `log`-crate record the
/// target `log`, so there is no way to silence tokei specifically without
/// silencing other dependencies' records too. That is the right trade for a
/// diagnostics buffer about *thegn* — third-party ERRORs still pass, and
/// `THEGN_LOG` can put it back (`THEGN_LOG=log=warn`), since a user-supplied
/// filter replaces this default outright rather than merging with it.
const BRIDGED_LOG_DIRECTIVE: &str = "log=error";

fn level_filter(default: LogLevel) -> EnvFilter {
    // `THEGN_LOG` (tracing directives) wins; else the configured level.
    match std::env::var("THEGN_LOG") {
        Ok(s) if !s.trim().is_empty() => EnvFilter::builder().parse_lossy(s),
        _ => EnvFilter::new(format!("{},{BRIDGED_LOG_DIRECTIVE}", default.as_str())),
    }
}

/// Is a log level explicitly requested via the environment? `THEGN_LOG` (a
/// non-empty filter string) or `THEGN_LOG_LEVEL`. Used both to gate the CLI
/// stderr layer and to decide whether config's `[log] level` may reconcile in
/// (the environment always wins).
fn env_level_is_set() -> bool {
    std::env::var_os("THEGN_LOG").is_some_and(|v| !v.to_string_lossy().trim().is_empty())
        || std::env::var_os("THEGN_LOG_LEVEL").is_some()
}

// The reload closure is type-erased: `reload::Handle<EnvFilter, S>`'s `S` is the
// full (branch-specific) registry stack, so we box a closure that calls
// `handle.reload(..)` instead of naming the type.
static LEVEL_RELOAD: OnceLock<Box<dyn Fn(EnvFilter) + Send + Sync>> = OnceLock::new();

/// Reconcile the level filter from config after config load (compositor). A
/// no-op when no sink was installed (no reload handle) — the always-on ring
/// layer is fixed at WARN and never reloads.
pub fn reload_level(level: LogLevel) {
    if let Some(f) = LEVEL_RELOAD.get() {
        f(EnvFilter::new(level.as_str()));
    }
}

/// Install the global subscriber. Called unconditionally at process start: the
/// minimal WARN+ in-memory ring layer is ALWAYS installed (it does zero I/O
/// until a crash report or bundle reads it); a file/stderr sink is added only
/// when requested (`THEGN_LOG` / `[log]`), exactly as before. Idempotent and
/// best-effort — a second call, or a failure to open the log file, is swallowed
/// so logging never aborts a run.
pub fn install(role: Role, cfg: &LogConfig) {
    diagnostics::set_proc_kind(role.proc_kind());
    // Mint the run id now (before the first frame) so every line carries it.
    let _ = diagnostics::run_id();

    let stderr_ansi = io::stderr().is_terminal();
    let file_json = matches!(cfg.format, LogFormat::Json);

    // The always-on ring: WARN and above only, minus bridged `log`-crate noise
    // (see `BRIDGED_LOG_DIRECTIVE` — tokei alone contributed 14,917 WARNs in one
    // session, which would evict thegn's own records from a fixed-size ring that
    // exists to explain a crash).
    //
    // Still a WARN ceiling: `EnvFilter` reports a `max_level_hint`, which for
    // these directives is WARN, so it participates in the global max-level
    // computation exactly as the previous `LevelFilter::WARN` did. With no sink
    // present every sub-WARN callsite still resolves to a cached "never" — the
    // same order of cost as having no subscriber at all, and zero I/O. The extra
    // per-event target match is paid only at WARN and above.
    let ring = RingLayer.with_filter(EnvFilter::new(format!("warn,{BRIDGED_LOG_DIRECTIVE}")));

    // At most one sink per process (see `Role`): a file for host/daemon/watch,
    // stderr for a CLI verb (and then only when the env asked for logs).
    let want_file = cfg.file && matches!(role, Role::Host | Role::Daemon | Role::Watch { .. });
    let want_stderr = role.wants_stderr() && env_level_is_set();

    let (reload_filter, reload_handle) =
        tracing_subscriber::reload::Layer::new(level_filter(cfg.level));

    let mut has_sink = false;
    let installed = if want_file {
        match FileSink::open(cfg, &role.log_file()) {
            Ok(sink) => {
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(sink)
                    .event_format(Brand {
                        ansi: false,
                        // JSON carries its own `ts` field; the text sink prefixes
                        // a plain timestamp column.
                        timestamp: !file_json,
                        json: file_json,
                    })
                    .with_filter(reload_filter);
                let ok = tracing_subscriber::registry()
                    .with(ring)
                    .with(file_layer)
                    .try_init()
                    .is_ok();
                if ok {
                    let _ = LEVEL_RELOAD.set(Box::new(move |f| {
                        let _ = reload_handle.reload(f);
                    }));
                    has_sink = true;
                }
                ok
            }
            Err(e) => {
                // Can't log via tracing yet — say so on stderr directly.
                eprintln!("thegn: could not open log file: {e}");
                tracing_subscriber::registry().with(ring).try_init().is_ok()
            }
        }
    } else if want_stderr {
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .event_format(Brand {
                ansi: stderr_ansi,
                timestamp: false,
                json: false,
            })
            .with_filter(reload_filter);
        let ok = tracing_subscriber::registry()
            .with(ring)
            .with(stderr_layer)
            .try_init()
            .is_ok();
        if ok {
            let _ = LEVEL_RELOAD.set(Box::new(move |f| {
                let _ = reload_handle.reload(f);
            }));
            has_sink = true;
        }
        ok
    } else {
        // Ring only — no file, no stderr, no I/O until a crash/bundle reads it.
        tracing_subscriber::registry().with(ring).try_init().is_ok()
    };

    // `READY` means "a user-facing sink is installed" — `msg::*` consults it to
    // decide tracing vs a direct stderr write. The ring alone does NOT flip it,
    // so a plain CLI verb with logging off still prints branded errors to
    // stderr (its own surface) rather than swallowing them into the ring.
    if installed && has_sink {
        READY.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The panic hook: terminal restore first (never Drop-during-unwind), then a
// best-effort crash report + notice. See the module docs and `diagnostics`.
// ---------------------------------------------------------------------------

type RestoreFn = Box<dyn Fn() + Send + Sync + 'static>;
type NoticeFn = Box<dyn Fn(&str) + Send + Sync + 'static>;

static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static PANIC_RESTORE: OnceLock<Mutex<Option<RestoreFn>>> = OnceLock::new();
static RESTORE_DONE: AtomicBool = AtomicBool::new(false);
static CRASH_NOTICE: OnceLock<NoticeFn> = OnceLock::new();

/// Register the host's idempotent, non-panicking terminal-restore callback,
/// set immediately after entering raw mode + the alternate screen. Re-arms the
/// swap-once guard so the callback runs on the next panic/teardown.
pub fn register_panic_restore<F: Fn() + Send + Sync + 'static>(f: F) {
    let slot = PANIC_RESTORE.get_or_init(|| Mutex::new(None));
    if let Ok(mut g) = slot.lock() {
        *g = Some(Box::new(f));
    }
    RESTORE_DONE.store(false, Ordering::SeqCst);
}

/// Clear the restore callback on normal teardown (the compositor no longer owns
/// the screen).
pub fn clear_panic_restore() {
    if let Some(slot) = PANIC_RESTORE.get()
        && let Ok(mut g) = slot.lock()
    {
        *g = None;
    }
}

/// Run the registered restore callback AT MOST ONCE, whether reached from the
/// panic hook or from normal teardown. The swap-once atomic makes the second
/// caller a no-op. This function is panic-free by construction (a poisoned lock
/// is handled, the callback is contractually non-panicking), which is what lets
/// the hook call it during unwind without risking a double panic.
pub fn run_panic_restore_once() {
    if RESTORE_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(slot) = PANIC_RESTORE.get()
        && let Ok(g) = slot.lock()
        && let Some(f) = g.as_ref()
    {
        f();
    }
}

/// Register a writer for the one-line crash notice — the host hands over a dup
/// of the ORIGINAL (pre-redirect) stderr so the user sees the notice in their
/// terminal even though the session redirected fd 2 to the log file.
pub fn register_crash_notice<F: Fn(&str) + Send + Sync + 'static>(f: F) {
    let _ = CRASH_NOTICE.set(Box::new(f));
}

fn emit_crash_notice(s: &str) {
    if let Some(f) = CRASH_NOTICE.get() {
        f(s);
    }
}

/// Install the process-wide panic hook. Unconditional and independent of
/// `THEGN_LOG`: the hook restores the terminal first (via the registered
/// callback), writes a best-effort crash report, logs the panic line (so the
/// e2e log-guard `panicked` pattern still matches when a sink exists), prints a
/// notice to the original stderr, and delegates to the previous hook.
/// Idempotent: a second call is a no-op so the hook is not re-wrapped.
pub fn install_panic_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 1. Terminal restore FIRST, at most once, using only non-panicking
        //    writes — never relying on any type's Drop during unwind.
        run_panic_restore_once();

        // 2. Best-effort diagnostics. Wrapped in catch_unwind so a panic in the
        //    report/backtrace path cannot become a panic-while-panicking abort;
        //    the terminal is already restored above regardless of what fails.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let line = panic_line(info);
            let path = diagnostics::report_panic(&line);
            tracing::error!(target: "thegn::panic", "{line}");
            match &path {
                Some(p) => {
                    emit_crash_notice(&format!("\r\nthegn crashed — report: {}\r\n", p.display()))
                }
                None => emit_crash_notice("\r\nthegn crashed\r\n"),
            }
        }));

        // 3. Delegate to the previous hook (default prints to stderr → logfile).
        previous(info);
    }));
}

/// One line in the same shape the default hook prints, so a single
/// `thread '.*' panicked` pattern matches both stderr and the log.
pub fn panic_line(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".into());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".into());
    format!("thread '{name}' panicked at {location}:\n{payload}")
}

/// The always-on ring layer: it captures WARN-and-above events (its per-layer
/// `LevelFilter::WARN`, applied at the `install` call site, gates it — panic
/// events log at ERROR so they are covered) into the in-memory ring, and does
/// nothing else. No I/O, no allocation beyond the bounded ring push; the crash
/// writer and the debug bundle are the only readers.
struct RingLayer;

impl<S> Layer<S> for RingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut v = RingVisitor::default();
        event.record(&mut v);
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let level = diagnostics::tracing_level_to_parser(meta.level());
        let wt = current_wt();
        // A Brand-shaped rendering so the ring tail in a crash report reads like
        // the log file. Identity (proc/run) is process-global at snapshot time,
        // so it is not repeated per line here.
        let mut original = format!("{ts}  {:<5} {}  ", meta.level().as_str(), meta.target());
        if let Some(w) = &wt {
            original.push_str(&format!("wt={w}  "));
        }
        original.push_str(&v.msg);
        diagnostics::ring_push(crate::log::parser::ParsedLog {
            timestamp: ts,
            level,
            message: v.msg,
            original,
            worktree: wt,
        });
    }
}

/// Collects a `tracing` event's `message` field (plus any structured kv) into a
/// single string for the ring. Best-effort formatting — the ring is evidence,
/// not a wire format.
#[derive(Default)]
struct RingVisitor {
    msg: String,
}

impl tracing::field::Visit for RingVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.msg, "{value:?}");
        } else {
            if !self.msg.is_empty() {
                self.msg.push(' ');
            }
            let _ = write!(self.msg, "{}={value:?}", field.name());
        }
    }
}

/// The compact branded formatter, shared by both sinks.
///
/// stderr (tty): `✦ thegn  WARN  thegn::worktree  created tg/foo`
/// file:         `2026-06-05T12:00:00  WARN  thegn::worktree  created tg/foo`
struct Brand {
    ansi: bool,
    timestamp: bool,
    /// Emit a machine-readable JSON object per line (`[log] format = "json"`)
    /// instead of the branded text column. Mutually exclusive with `timestamp`
    /// (JSON carries its own `ts` field) and `ansi`.
    json: bool,
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

        if self.json {
            return self.format_json(ctx, writer, event, level, target);
        }

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

        // Process + run identity: a per-process discriminator (`host`/`daemon`/
        // `cli`) and a per-process run id, so a session can be correlated across
        // the compositor's and the daemon's log files. These sit alongside the
        // `wt=` token and are stripped from the visible message by both parsers.
        let proc = diagnostics::proc_kind_str();
        let run = diagnostics::run_id();
        if self.ansi {
            write!(
                writer,
                "\x1b[38;2;{}mproc={proc} run={run}\x1b[0m  ",
                theme::FAINT
            )?;
        } else {
            write!(writer, "proc={proc} run={run}  ")?;
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

impl Brand {
    /// Emit one newline-terminated JSON object: `{"ts":…,"level":…,"target":…,
    /// ["wt":…,]"msg":…}`. Keys mirror what `log::parser::parse_log` extracts
    /// (`ts`/`level`/`wt`/`msg`), so the Logs panel and `thegn logs` read it
    /// back losslessly. The message is the event's rendered fields (the default
    /// text field formatter), JSON-string-escaped.
    fn format_json<S, N>(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
        level: &Level,
        target: &str,
    ) -> std::fmt::Result
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
    {
        // Render the event's fields (message + kv) into a scratch buffer, then
        // escape as a JSON string value.
        let mut fields = String::new();
        ctx.field_format()
            .format_fields(Writer::new(&mut fields), event)?;

        let ts = chrono::Local::now().to_rfc3339();
        write!(
            writer,
            "{{\"ts\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"proc\":\"{}\",\"run\":\"{}\"",
            json_escape(&ts),
            json_escape(level.as_str()),
            json_escape(target),
            json_escape(diagnostics::proc_kind_str()),
            json_escape(diagnostics::run_id()),
        )?;
        if let Some(wt) = current_wt() {
            write!(writer, ",\"wt\":\"{}\"", json_escape(&wt))?;
        }
        write!(writer, ",\"msg\":\"{}\"}}", json_escape(&fields))?;
        writeln!(writer)
    }
}

/// Minimal JSON string escaping for the file JSON sink (control chars, quotes,
/// backslashes). Keeps the sink dependency-free; the reader uses `serde_json`.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
        let d = std::env::temp_dir().join(format!("tg-log-{}-{tag}", std::process::id()));
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
    fn json_escape_handles_specials() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("l1\nl2\ttab"), "l1\\nl2\\ttab");
        // Control char below 0x20 → \u escape.
        assert_eq!(json_escape("\u{0007}"), "\\u0007");
    }

    #[test]
    fn json_sink_emits_parseable_json_with_timestamp() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(BufWriter(buf.clone()))
            .event_format(Brand {
                ansi: false,
                timestamp: false,
                json: true,
            });
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let _g = enter_wt("app-feat");
            tracing::error!(target: "thegn::db", "exploded");
        });

        let line = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let line = line.trim();
        // A real JSON object (regression: the old code emitted plain text and
        // dropped the timestamp for format = "json").
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {e}: {line:?}"));
        assert_eq!(v["level"], "ERROR");
        assert_eq!(v["target"], "thegn::db");
        assert_eq!(v["wt"], "app-feat");
        assert!(v["msg"].as_str().unwrap().contains("exploded"));
        // Timestamp is present and RFC3339-parseable.
        let ts = v["ts"].as_str().expect("ts field");
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "ts={ts}");

        // And the parser round-trips it back into a ParsedLog.
        let parsed = crate::log::parser::parse_log(line);
        assert_eq!(parsed.level, crate::log::parser::LogLevel::Error);
        assert_eq!(parsed.worktree.as_deref(), Some("app-feat"));
        assert!(parsed.message.contains("exploded"));
    }

    #[test]
    fn env_filter_prefers_thegn_log() {
        // Just ensure construction doesn't panic for both paths.
        let _ = level_filter(LogLevel::Info);
    }

    #[test]
    fn over_cap_respects_mib_boundary() {
        assert!(!over_cap(0, 5));
        assert!(!over_cap(5 * 1024 * 1024, 5)); // exactly at the cap
        assert!(over_cap(5 * 1024 * 1024 + 1, 5));
        assert!(!over_cap(u64::MAX, 0)); // cap 0 disables
    }

    #[test]
    fn rotate_if_over_renames_when_large() {
        let dir = tmp("rot-over");
        let path = dir.join("big.log");
        std::fs::write(&path, vec![0u8; 2 * 1024 * 1024]).unwrap();
        rotate_if_over(&path, 1); // 2 MiB > 1 MiB cap
        assert!(!path.exists());
        assert!(dir.join("big.log.1").exists());
        // A small file under the cap is left alone.
        std::fs::write(&path, b"small").unwrap();
        rotate_if_over(&path, 1);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wt_slug_is_basename_slug() {
        assert_eq!(
            wt_slug(Path::new("/home/me/wt/tg-solid-glen")),
            "tg-solid-glen"
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

    #[test]
    fn panic_line_has_thread_location_and_payload() {
        let caught = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = caught.clone();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            *sink.lock().unwrap() = panic_line(info);
        }));
        let r = std::thread::Builder::new()
            .name("boom-thread".into())
            .spawn(|| {
                let v: Vec<u8> = Vec::new();
                let _ = v[3];
            })
            .unwrap()
            .join();
        std::panic::set_hook(prev);
        assert!(r.is_err());
        let line = caught.lock().unwrap().clone();
        assert!(
            line.starts_with("thread 'boom-thread' panicked at "),
            "{line}"
        );
        assert!(line.contains("log_trace.rs"), "{line}");
        assert!(line.contains("index out of bounds"), "{line}");
        // the e2e guard's patterns match
        let re = regex::Regex::new("thread '.*' panicked|index out of bounds").unwrap();
        assert!(re.is_match(&line));
    }
}
