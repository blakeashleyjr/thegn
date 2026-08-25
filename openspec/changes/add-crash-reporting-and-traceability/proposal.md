# Add crash reporting, error surfacing, and log traceability

Linear: THE-54

## Why

A deep trace of every failure path (verified against HEAD, 2026-08) shows that
when thegn breaks, the evidence is usually lost, invisible, or both:

- **A panic can leave the terminal wrecked.** The only panic hook
  (`thegn-core/src/log_trace.rs::install_panic_hook`) logs one line and
  delegates — it is only installed when `THEGN_LOG` is set (it hangs off
  `init()`), it captures **no backtrace**, and it restores **nothing**. A
  mid-frame panic on the main thread relies on `BufferedTerminal`'s Drop
  running during unwind, and termwiz's `UnixTerminal::drop` uses
  `unwrap`/`expect` — a failure there during unwind is a double panic and an
  abort with the user's terminal left in raw mode on the alternate screen.
  The compositor also redirects fd 2 to `thegn-stderr.log`
  (`platform/mod.rs::redirect_stderr_to_logfile`), so the panic message the
  default hook prints is invisible too. There is no crash-report file
  anywhere, and release builds `strip = true`, so even a captured trace would
  be bare addresses unless we record what's needed to interpret it.
- **Whole failure classes are silent by design.** With `THEGN_LOG` unset there
  is **no tracing subscriber at all** — so `msg::{info,warn,error}`, which
  route to `tracing` whenever the TUI is active (`msg.rs`), are dropped on the
  floor in exactly the sessions users run. Daemon attach/spawn failures fall
  back to in-process PTYs with only a `tracing::warn!`
  (`panes.rs` reattach/spawn ladders) — invisible without `THEGN_LOG`. The
  daemon status chip is derived from DB heartbeats and has **no error state**:
  a crashed daemon is just a stale heartbeat. A headless daemon has stdio
  nulled (`util::detached`), so with `THEGN_LOG` unset it dies without a
  trace. Plain CLI verbs never install any subscriber (`Role::Cli` is defined
  but never constructed) — `THEGN_LOG=debug thegn <verb>` logs nothing.
- **Logs are untraceable and race each other.** No log line carries a
  session/run id or a process discriminator; the daemon and the compositor
  write **the same `thegn.log`** with two independent `FileSink` size counters
  racing one rotation state machine, and the daemon passes
  `LogConfig::default()` instead of `log_config_from_env`, so its sink
  ignores every `THEGN_LOG_*` knob (pinned 5 MB × 5 in the default dir).
  `thegn-stderr.log` and `audit.log` have no cap and grow unbounded.
- **A secret leak, verified:** `daemon/service.rs` logs the full pane argv at
  DEBUG (`tracing::debug!(... argv = ?argv ... "open session")`) — a token on
  a pane command line lands verbatim in `thegn.log`. There is no redaction
  layer anywhere in the tracing path (the only redaction fn serves MCP config
  responses).
- **There is no support story.** No debug/diagnostics bundle verb exists, and
  `thegn doctor` does not even print thegn's own version, the OS, the daemon
  version, or where the logs are — the first three questions any bug report
  needs answered.

The linked reference (rustrak, a self-hosted Sentry-protocol error tracker) is
judged honestly in the design: forwarding crash data anywhere is opt-in
telemetry territory. What thegn needs first is local-first crash capture that
works with zero configuration; a forwarding seam is declared `reserved` and
default-off.

## What Changes

- **New capability `diagnostics`.** Crash handling, crash-only capture,
  log identity/rotation, redaction, error-surfacing contract, the debug
  bundle, and doctor identification.
- **Panic path owns terminal restore.** The panic hook is installed
  unconditionally (decoupled from `THEGN_LOG`/`init()`); the host registers an
  idempotent restore callback while it owns the raw/alternate screen, the hook
  runs it **before** anything else using only non-panicking writes, then
  prints a one-line crash notice to the real terminal (the saved pre-redirect
  stderr) — never relying on termwiz Drop during unwind.
- **Crash reports, always.** Every panic (main or worker thread) writes a
  best-effort report file under `$XDG_STATE_HOME/thegn/crash/`: version +
  channel + OS, run id, panic message/location, a force-captured backtrace
  (independent of `RUST_BACKTRACE`), thread name, and the tail of an
  always-on in-memory WARN+ ring buffer (reusing `thegn-core/src/log/buffer.rs`,
  which currently has no production construction site). Bounded retention;
  the next launch surfaces an unacknowledged crash. `panic = "unwind"` stays
  (abort would kill user shells — Cargo.toml records the measurement).
- **Always-on capture without breaking the perf contract.** `THEGN_LOG` unset
  still means: no file sink, no stderr layer, no per-frame work, no I/O. What
  becomes always-on is a minimal subscriber holding the WARN+ ring and the
  panic target only — disabled callsites stay a cached-interest check; the
  ring costs nothing at idle and is only ever flushed by the crash/bundle
  path. The honest trade is argued in the design (this amends the
  "no subscriber when unset" note in CLAUDE.md, not any spec).
- **Traceability.** Every log line carries a process discriminator
  (`host`/`daemon`/`cli`) and a per-process run id; the daemon records the
  client's run id at attach, so a session can be correlated across both files.
- **One file per process; every sink bounded.** The daemon writes
  `thegn-daemon.log` (ending the two-writers rotation race) and honors
  `THEGN_LOG_*` env plus `[log]` config like the host; `thegn-stderr.log` is
  rotated at startup when over a configurable cap; `audit.log` is capped.
  The compositor reconciles the `[log]` file section after config load
  instead of ignoring it (env still wins; the startup waterfall stays
  captured).
- **Redaction chokepoint.** Argv/env values pass a shared redaction filter
  before reaching any log line or bundle; the pane-argv DEBUG leak is fixed;
  the sensitive-key list is shared with `add-credential-broker`'s
  audit-without-values rule (one list, one crate).
- **Error-surfacing contract.** Every failure class names its surface:
  user-invoked primary-path errors reach `msg`/status (spec'd, was
  convention); `msg::*` while the TUI is active always reaches the ring and,
  when enabled, the log — never nothing; a background fallback that changes
  user-visible behavior (daemon → in-process pane) raises a notification; the
  daemon status chip gains an error state driven by stale heartbeats; CLI
  verbs construct `Role::Cli` so `THEGN_LOG` works outside the TUI.
- **`thegn doctor bundle`.** One verb writes a redacted support archive:
  extended `doctor --json`, versions (thegn/daemon/OS), redacted config,
  bounded log tails from every sink, crash reports — and prints a manifest of
  what it included. A `thegn_core::capability::CATALOG` row on the operator
  surface set, gated by `required_scope(verb)`. (`thegn debug` is taken by
  the BugStalker debugger, so the bundle lives under `doctor`.)
- **Doctor identifies the installation:** thegn version/channel/build, OS,
  daemon version + reachability, and the `[log]` configuration in effect with
  sink paths and sizes.
- **Crash forwarding is `reserved`.** A `crash_sink` seam kind (`sentry`
  protocol — covers rustrak) is declared reserved and default-off; the
  default configuration performs zero network I/O for diagnostics.

## Non-goals

- **Toast UX / diagnostics center.** Roadmap AI 749–753 owns the toast
  machinery (tiers, TTL, dedup, inbox folding). This change specs the
  _contract_ that errors are never dropped and produces the notifications;
  the richer rendering lands there.
- **Implementing an error-tracking client.** No Sentry SDK dependency, no
  DSN config, no network path — the seam is reserved.
- **Symbolication.** Release builds stay stripped; reports record enough to
  be interpreted against a build (version + backtrace as captured), not
  resolved symbols.
- **Tamper-evident/exportable audit trails** (AN 484/487) — the audit.log cap
  here is hygiene, not that feature.

## Impact

- **Roadmap:** AI **749/753** (in-app diagnostic surfacing / startup
  diagnostics gate — this delivers the never-dropped contract they build on),
  AN **486** (retention: sink caps/rotation), AO **490** (doctor/diagnostics
  command — extended). Complements shipped item 739 (panics reach the log).
- **Specs:** new `diagnostics` capability; **ADDED** requirement in
  `control-plane` (daemon failures surfaced, not silent).
- **In-flight changes reconciled:** `make-daemon-default` (its statusbar chip
  is the _persistence_ chip; the error state here composes with it — same
  chip slot, additive state), `add-credential-broker` (shares the
  sensitive-key list and the "audit events carry names, never values" rule;
  whichever lands first hosts the shared list), `add-cli-namespaces-and-remote-open`
  (the `doctor bundle` verb follows its namespace conventions),
  `add-windows-daemon-ipc` / `add-windows-parity` (per-process log files and
  the restore callback are platform-neutral; the fd-2 redirect stays behind
  `platform/`), `add-observability-dashboards` (unrelated: observe-\* is
  external data sources, not thegn's own logs).
- **Code (indicative):** `thegn-core/src/log_trace.rs` (hook decoupled from
  init, restore registration, crash writer, identity fields, redaction
  chokepoint), `thegn-core/src/log/buffer.rs` (ring gets its production
  construction site), `thegn-host/src/run.rs` + `platform/mod.rs` (restore
  callback registration, stderr rotation), `thegn-host/src/daemon/mod.rs` +
  `daemon/service.rs` (own log file, env/config honor, argv redaction),
  `cmd/doctor.rs` (+ `bundle`), `cmd/logs.rs` (run-id filter). No SQLite
  schema change; no new TUI action, keybind, zone, or panel section (the chip
  error state extends an existing chrome element; new `[log]`/`[diagnostics]`
  keys are documented in `config/config.toml.example` and appear in the
  generated config-reference help page).
