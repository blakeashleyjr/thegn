# Tasks — crash reporting, error surfacing, log traceability

## 1. Crash foundation (thegn-core)

- [ ] 1.1 Split `install_panic_hook` out of `log_trace::init`: install
      unconditionally at process start (host `main`, daemon entry, CLI path);
      keep the existing hook line/target so the e2e log-guard patterns still
      match. Unit-test hook installation is idempotent.
- [ ] 1.2 Add `log_trace::register_panic_restore` / `clear_panic_restore`
      (swap-once atomic; callback is `Fn() + Send + Sync`); the hook runs the
      callback first, before logging or delegating. Core stays substrate-free
      — the callback body lives in the host. Unit-test run-at-most-once
      semantics (hook then teardown, teardown then hook).
- [ ] 1.3 Crash-report writer: `crash/` dir under the state dir (0700, files
      0600), report body (version/channel/build, OS, proc kind, run id,
      thread, panic line, `Backtrace::force_capture()`, ring tail), bounded
      retention with pruning, non-panicking best-effort writes (`try_lock`
      degradation for the ring). Pure formatting + retention logic
      unit-tested (95% core gate); the actual write path smoke-tested.
- [ ] 1.4 Unacknowledged-crash detection (marker/mtime scheme, no DB) +
      pure "which reports are new" logic with unit tests.

## 2. Always-on ring layer

- [ ] 2.1 Give `thegn_core::log::buffer` its production construction site: a
      minimal always-installed layer holding a WARN+ ring (default 256
      events) + the `thegn::panic` target; static max-level filtering so
      sub-WARN callsites resolve to cached interest. No file/terminal I/O
      until crash/bundle. Unit-test ring capture + bounded size.
- [ ] 2.2 Route `msg::{warn,error}` into the ring when the TUI is active and
      no sink exists (fixes the drop); keep current stderr behavior for
      non-TUI, non-subscriber use. Unit tests on the routing matrix.
- [ ] 2.3 Amend the CLAUDE.md instrumentation sentence ("no subscriber" →
      "no sink") and note the ring's cost shape.

## 3. Host wiring: restore, notice, identity

- [ ] 3.1 In `run.rs`: register the restore callback right after raw
      mode + alt screen, clear it in teardown; restore body writes the fixed
      escape teardown + `tcsetattr` on saved termios directly to the tty fd
      (no termwiz calls that can panic); share the one restore path with
      normal teardown behind the swap-once guard.
- [ ] 3.2 Keep a dup of pre-redirect stderr in the `StderrGuard`
      (`platform/mod.rs`) and hand it to the hook for the one-line crash
      notice after restore.
- [ ] 3.3 Mint a run id per process; stamp `proc=`/`run=` fields in the fmt
      layer next to the existing `wt=` tag; construct `Role::Cli` in
      `main.rs` when `THEGN_LOG` is set so plain verbs get a stderr layer.
- [ ] 3.4 Startup notification for an unacknowledged crash report (existing
      notification path; off-loop scan + waker pulse — no work on the loop
      before first frame).

## 4. Sink separation and bounds

- [ ] 4.1 Daemon logs to `thegn-daemon.log`; build its `LogConfig` from
      `log_config_from_env` + `[log]` instead of `LogConfig::default()`
      (`daemon/mod.rs`). Daemon logs the client run id at attach.
- [ ] 4.2 Compositor `[log]` reconciliation after config load via a reload
      handle (env overlay wins); document the env-vs-config precedence in
      `config/config.toml.example`.
- [ ] 4.3 Startup rotation for `thegn-stderr.log` (new `[log] stderr_cap_mb`)
      and `audit.log` via a shared rotate-if-over helper; unit-test the
      rotate/naming logic (pure part).
- [ ] 4.4 Unify the two log-line parsers (`log_view::parse_log_line` /
      `log::parser::parse_log`) into one that knows the new `proc`/`run`
      fields and the full `LogLevel` set (incl. `Fatal`); Logs panel and
      `thegn logs tail` both use it; add `logs tail --run <id>`.

## 5. Redaction

- [ ] 5.1 Move the MCP `SENSITIVE` list into `thegn-core` as the shared
      sensitive-key list (coordinate with `add-credential-broker` — whichever
      lands first hosts it); add `log_redact` for key/value, env-map, and
      argv shapes (`--token`-style flags). Unit tests on the shapes (pure,
      core-gated).
- [ ] 5.2 Fix the pane-argv DEBUG leak in `daemon/service.rs`: program name +
      arg count at DEBUG, full argv only at TRACE through the redactor.
      Sweep other argv/env log sites through the chokepoint.
- [ ] 5.3 Bundle-time redaction pass reusing the MCP config redactor +
      `log_redact` over included log tails.

## 6. Error surfacing

- [ ] 6.1 Daemon chip error state on heartbeat staleness (threshold from the
      existing cadence); probe detail on activate distinguishes dead socket
      vs alive-but-stale; chrome-only change on the Full damage path;
      reconcile rendering with `make-daemon-default`'s persistence chip.
- [ ] 6.2 Fallback notifications: daemon reattach/spawn, ssh-over-wss, and
      native-exec fallback ladders in `panes.rs` raise a notification naming
      the degradation + cause (existing NotificationKind machinery; new kind
      only if none fits).
- [ ] 6.3 Daemon-side crash reporting (`proc=daemon`) + "daemon crashed"
      surfacing on next attach/probe with the report path.

## 7. Doctor + bundle

- [ ] 7.1 Doctor identification block: thegn version/channel/build (embed git
      sha at build time when available), OS, daemon version + reachability
      (version over the control handshake), `[log]` sinks with path/size/cap;
      in both text and `--json`.
- [ ] 7.2 `thegn doctor bundle [--out]`: tar.gz with doctor.json, redacted
      config, bounded log tails (all sinks), crash reports, printed MANIFEST.
- [ ] 7.3 Catalog row `doctor.bundle` on the operator surface set (CLI +
      control API, off MCP/plugins), `required_scope`-gated; extend the
      pinned catalog tests.
- [ ] 7.4 Config keys: `[log] stderr_cap_mb`, `[diagnostics]`
      (`crash_reports`, retention, ring size, `crash_sink` kind rejected as
      `reserved`) — all documented in `config/config.toml.example`; verify
      they appear in the generated config-reference help page.

## 8. Test infrastructure

- [ ] 8.1 e2e: copy `thegn.log` and `thegn-stderr.log` from `case_tmp` into
      `e2e-results/<case>/` on failure so panic evidence survives cleanup
      (muse harness change only; no baseline re-record).
- [ ] 8.2 Smoke: a deliberate-panic path (test-only env hook) asserting the
      crash report exists and the terminal restore sequence was emitted;
      bundle smoke asserting manifest + no secret value for a seeded token.

## 9. Gate

- [ ] 9.1 Run `just ci` once (includes `openspec validate --all --strict`)
      when the implementation is complete.
