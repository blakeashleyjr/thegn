# Tasks — crash reporting, error surfacing, log traceability

## 1. Crash foundation (thegn-core)

- [x] 1.1 Split `install_panic_hook` out of `log_trace::init`: install
      unconditionally at process start (host `main`, daemon entry, CLI path);
      keep the existing hook line/target so the e2e log-guard patterns still
      match. Unit-test hook installation is idempotent. _(Installed from
      `main()`; idempotency via a `HOOK_INSTALLED` swap-once atomic. The hook
      still logs `target: "thegn::panic"`.)_
- [x] 1.2 Add `log_trace::register_panic_restore` / `clear_panic_restore`
      (swap-once atomic; callback is `Fn() + Send + Sync`); the hook runs the
      callback first, before logging or delegating. Core stays substrate-free
      — the callback body lives in the host (`platform::unix::TerminalRestore`).
- [x] 1.3 Crash-report writer (`diagnostics.rs`): `crash/` dir (0700, files
      0600), full report body, `Backtrace::force_capture()`, ring tail, bounded
      retention with pruning, non-panicking best-effort writes (`try_lock` ring
      degradation). Pure formatting + retention logic unit-tested; the write
      path smoke-tested.
- [x] 1.4 Unacknowledged-crash detection (`.ack` marker sibling, no DB) + pure
      `unacknowledged` logic with unit tests.

## 2. Always-on ring layer

- [x] 2.1 `thegn_core::log::buffer` gets its production construction site: a
      minimal always-installed `RingLayer` holding a WARN+ ring (default 256) + the `thegn::panic` target (ERROR ≥ WARN), static `LevelFilter::WARN`.
      No file/terminal I/O until crash/bundle.
- [x] 2.2 `msg::{warn,error}` reach the ring when the TUI is active and no sink
      exists (via `tui_active → tracing → RingLayer`); non-TUI keeps the branded
      stderr fallback. Verified by the smoke (branded eprintln with `THEGN_LOG`
      unset; tracing sink with it set).
- [x] 2.3 CLAUDE.md instrumentation sentence amended ("no subscriber" → "no
      _sink_") with the ring's zero-I/O cost shape.

## 3. Host wiring: restore, notice, identity

- [x] 3.1 `run.rs` registers the restore callback right after raw mode + alt
      screen and clears it in teardown; the restore body writes the fixed escape
      teardown + a raw `libc::tcsetattr` on the saved termios directly to the
      tty fd (no termwiz calls); teardown shares the one path behind the
      swap-once guard.
- [x] 3.2 The `StderrGuard` hands the hook a dup of pre-redirect stderr for the
      one-line crash notice (`register_crash_notice`).
- [x] 3.3 Per-process run id + `proc=`/`run=` fmt fields; `main.rs` installs
      `Role::Cli` (stderr layer only when `THEGN_LOG`/`THEGN_LOG_LEVEL` set).
- [x] 3.4 Startup notification for an unacknowledged crash report (off-loop
      `crash-scan` thread; opens DB + records + acknowledges + pulses the waker).

## 4. Sink separation and bounds

- [x] 4.1 Daemon logs to `thegn-daemon.log` (`Role::Daemon`) built from
      `cfg.log` + env (fixing the pinned default). **Client-run-id-at-attach is
      DEFERRED** — it needs an IPC handshake field; the per-line `run=`/`proc=`
      identity and the separate daemon file both landed.
- [x] 4.2 Compositor `[log]` level reconciliation after config load via a
      type-erased reload handle (env overlay wins); env-vs-config precedence
      documented in `config/config.toml.example`.
- [x] 4.3 Startup rotation for `thegn-stderr.log` (new `[log] stderr_cap_mb`)
      and `audit.log` via `log_trace::rotate_if_over`; `over_cap` unit-tested.
- [x] 4.4 `logs tail --run <id>` added; **both** parsers strip the new
      `proc=`/`run=` tokens and `log_view::line_run_id` powers the filter.
      **Full type unification of `LogLine`/`ParsedLog` DEFERRED** — a large,
      risky host-panel refactor for marginal value; the functional goal
      (correlate + tail-by-run) is met without it.

## 5. Redaction

- [x] 5.1 A minimal LOCAL sensitive-key predicate + `log_redact` (key/value,
      env-map, argv shapes) with unit tests. **Marked `// TODO(unify)`** to
      consume `add-credential-broker`'s canonical list at review (per the
      shared-seam boundary — `mcp/docs.rs`/`secret.rs` untouched).
- [x] 5.2 Pane-argv DEBUG leak fixed in `daemon/service.rs`: program name +
      arg count at DEBUG, full argv only at TRACE through the redactor.
- [x] 5.3 Bundle-time redaction: `config.redacted.toml` via a `toml::Value`
      redactor (same key predicate as MCP) + `log_redact` over included log
      tails.

## 6. Error surfacing

- [x] 6.1 Daemon chip error state on heartbeat staleness (`snapshot` detects a
      stale registry row; `daemon_chip_state → Error`; red glyph; status-modal
      health note distinguishes crashed/wedged; unit-tested). Additive on the
      persistence chip slot, Full damage path.
- [ ] 6.2 Fallback notifications (daemon reattach/spawn, ssh-over-wss,
      native-exec) in `panes.rs`. **DEFERRED** — deeper `panes.rs` plumbing of
      db+notify_state; the chip error state + startup crash scan already make
      daemon degradation visible.
- [x] 6.3 Daemon-side crash reporting (`proc=daemon`) landed (panic hook +
      identity). Surfacing "daemon crashed" is covered by the shared crash dir
      (the startup `crash-scan` picks up a `proc=daemon` report) + the chip
      error state; the bespoke "on next attach names the path" message is
      DEFERRED with 6.2.

## 7. Doctor + bundle

- [x] 7.1 Doctor identification block (version/channel/build via embedded git
      sha, OS, daemon reachability + version, `[log]` sinks with path/size/cap,
      crash reports) in text and `--json`.
- [x] 7.2 `thegn doctor bundle [--out]`: tar.gz (hand-rolled ustar + flate2)
      with doctor.json, redacted config, bounded log tails, crash reports, and a
      printed MANIFEST.
- [x] 7.3 Catalog row `doctor.bundle` (`Verb::DoctorBundle`, admin scope,
      OPERATOR surface, off MCP/plugins; `SURFACE_GAPS` for http/grpc); pinned
      catalog + CLI-coverage tests extended.
- [x] 7.4 Config keys `[log] stderr_cap_mb` + `[diagnostics]` (`crash_reports`,
      `crash_retention`, `ring_size`, `crash_sink` reserved-and-rejected) —
      documented in `config.toml.example`; the drift test + generated
      config-reference page cover them.

## 8. Test infrastructure

- [ ] 8.1 e2e log copy into `e2e-results/<case>/`. **DEFERRED** — a `muse`
      (external tool) harness change; the e2e suite is itself deferred/broken in
      this repo, so this is non-blocking.
- [x] 8.2 Smoke: a deliberate-panic path (`THEGN_PANIC_TEST` test-only hook)
      asserting the crash report exists with version/proc/backtrace; bundle
      smoke asserting the manifest + no plaintext for a seeded token + redaction
      marker.

## 9. Gate

- [ ] 9.1 `just ci` — left for the architect's pre-PR run (per the box-discipline
      instructions, full-workspace gates were NOT run here). Scoped clippy
      (`just quick` on core + host), targeted `nextest` on the affected modules,
      and `test/smoke.sh` all pass; `shellcheck` clean.
