# Design — crash reporting, error surfacing, log traceability

## Context: the failure-class inventory

| Failure class                             | Today                                                                                                                                                                                                                                                                                                                      | Evidence                                                                               |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Main-thread panic mid-frame               | Terminal restore rides `BufferedTerminal` Drop during unwind; termwiz `UnixTerminal::drop` unwraps → possible abort with raw mode + alt screen left on. Panic text goes to `thegn-stderr.log` (fd 2 is redirected), invisible.                                                                                             | `run.rs` teardown is normal-return only; `platform/mod.rs::redirect_stderr_to_logfile` |
| Worker-thread panic                       | Survives via `catch_unwind` (PTY reader → synthetic Exit; hydrate → fallback model; gtui-embed latches `is_panicked`) or kills the thread silently. Hook line only if `THEGN_LOG` set.                                                                                                                                     | `pane_pty.rs`, `hydrate.rs`, `embed.rs`                                                |
| Any panic, `THEGN_LOG` unset              | No hook installed at all (`install_panic_hook` only called from `log_trace::init`). No backtrace ever (nothing reads/sets `RUST_BACKTRACE`; release `strip = true`). No crash report file exists anywhere.                                                                                                                 | `log_trace.rs`                                                                         |
| `msg::warn`/`msg::error` in a TUI session | Routed to `tracing` when `tui_active` → dropped when no subscriber (the default).                                                                                                                                                                                                                                          | `msg.rs`                                                                               |
| Daemon attach/spawn failure               | Silent fallback to in-process PTY + `tracing::warn!`.                                                                                                                                                                                                                                                                      | `panes.rs` reattach/ssh-wss/native-exec ladders                                        |
| Daemon crash                              | Stdio nulled (`util::detached`) + no subscriber by default → nothing anywhere. Status chip derives from DB heartbeats and has no error state — a crash is just a stale heartbeat.                                                                                                                                          | `daemon/mod.rs`, chip                                                                  |
| CLI verb diagnostics                      | `main.rs` installs no subscriber; `Role::Cli`/`Role::Watch` exist but are never constructed. `THEGN_LOG` has no effect on plain verbs.                                                                                                                                                                                     | `main.rs`, `log_trace.rs`                                                              |
| Log rotation                              | Compositor and daemon both write `thegn.log`; two `FileSink`s, two size counters, one rotation rename dance → racing renames. Daemon uses `LogConfig::default()` (ignores `THEGN_LOG_*`); host ignores `[log]` in config.toml (env-only, init happens before config load). `thegn-stderr.log` and `audit.log` have no cap. | `daemon/mod.rs`, `run.rs`, `log_trace.rs::audit`                                       |
| Secrets in logs                           | `daemon/service.rs` logs full pane argv at DEBUG. No redaction layer exists in the tracing path (the only redaction fn serves MCP config responses).                                                                                                                                                                       | `service.rs` "open session" line                                                       |
| Support/debugging                         | No bundle verb. Doctor omits thegn's version, OS, daemon version, log paths. e2e keeps `thegn.log` in `case_tmp` (lost) and never collects `thegn-stderr.log`.                                                                                                                                                             | `cmd/doctor.rs`, muse harness                                                          |

## D1 — The panic hook owns terminal restore (never Drop-during-unwind)

The hook is split from `init()` and installed unconditionally at process
start. Because `thegn-core` is substrate-free, it cannot know about termwiz:
the hook gains a **registered restore callback** —
`log_trace::register_panic_restore(f)` / `clear_panic_restore()` — that the
host sets immediately after entering raw mode + alternate screen and clears
in normal teardown. The callback is:

- **Idempotent and non-panicking**: it writes the fixed escape teardown
  sequence (`?1006l ?1002l ?7h <u ?25h`, exit-alt, cooked mode via raw
  `tcsetattr` on saved termios — not through termwiz methods that can
  `unwrap`) directly to the tty fd, guarded by a `swap`-once atomic so hook
  and normal teardown never double-run it.
- **First** in the hook, before logging or report writing, so even a failure
  later in the hook leaves a usable terminal.
- Followed by a one-line crash notice written to the **saved pre-redirect
  stderr** (the `StderrGuard` keeps the original fd; the hook gets a dup), so
  the user sees `thegn crashed — report: <path>` in their terminal instead of
  nothing.

Worker-thread panics skip restore (they don't own the screen) but still
report. The existing `catch_unwind` recovery sites keep their behavior; the
hook runs before `catch_unwind` returns, so recovered panics still produce a
report and a notification rather than vanishing.

`panic = "unwind"` stays. Cargo.toml already records why abort was rejected
(a PTY-reader or hydration panic currently kills only that thread — abort
would take the multiplexer down with the user's shells in it). This design
removes the last reason to _want_ abort (the unwrap-in-Drop hazard) by never
reaching those Drops with the terminal still raw.

## D2 — Crash reports: always on, local-first

`$XDG_STATE_HOME/thegn/crash/<timestamp>-<run_id>.txt`, written best-effort
by the hook (never panics, never blocks on locks it might already hold — the
ring is lock-free to read-snapshot or uses `try_lock` with a degraded
report):

- thegn version + channel + git build info, OS/arch
- run id, process kind, thread name
- panic message + location (`panic_line`, same shape as today)
- `std::backtrace::Backtrace::force_capture()` — works regardless of
  `RUST_BACKTRACE`. Honest limitation: release builds are `strip = true`, so
  frames are module-relative addresses; the report records the version so the
  trace can be interpreted against the matching build. Symbolication is a
  non-goal; we do not un-strip release builds for this.
- the WARN+ ring tail (D3) — the last ~256 warn/error events leading up to
  the crash, which is the part of the report that usually explains it.

Retention: keep the newest N (default 10) reports; the hook prunes. On next
compositor launch, an unacknowledged report raises a notification with the
path (acknowledged = a marker file / touched mtime — no DB involvement, the
crash dir is self-contained). `thegn doctor` lists recent reports.

The e2e log guard keeps working: the hook still emits the
`thread '…' panicked` line to `tracing` when a subscriber exists.

## D3 — Always-on ring vs "no subscriber when unset": the honest argument

CLAUDE.md states "No subscriber is installed when `THEGN_LOG` is unset —
instrumentation is free." That contract's _substance_ is: zero file I/O, zero
per-frame work, zero formatting cost at idle. The letter of it ("no
subscriber") is what makes `msg::error` vanish and crash reports empty.

Decision: install one minimal always-on subscriber layer holding

- a fixed-size in-memory ring (reusing `thegn_core::log::buffer` — it exists
  and currently has no production construction site) at **WARN and above**
  plus the `thegn::panic` target, and nothing else.

Cost shape, argued: `tracing` caches per-callsite `Interest`; with a static
max-level filter at WARN, every DEBUG/INFO/TRACE callsite resolves to a
cached "never" — a load and branch, the same order of cost as today's
no-dispatcher check. Enabled events are WARN/ERROR, which are rare by
definition, and the ring write is a bounded memcpy — no allocation steady-
state, no I/O ever (the ring is only read by the crash writer and the
bundle). The 0%-idle contract is untouched: nothing polls, nothing wakes.
The spec locks this shape ("no file or terminal I/O until a crash report or
bundle is requested; sub-WARN callsites impose only a cached-interest
check"), and the CLAUDE.md sentence is amended to "no _sink_ is installed…"
as part of this change. `THEGN_LOG` semantics are otherwise unchanged; the
perf-suite spec (profiler gating) is unaffected.

Rejected alternatives: an always-on WARN file sink (background I/O and disk
growth in every session — real cost for rare value, and the ring reaches the
same evidence at crash time); crash-only stderr parsing (stderr is redirected
and unstructured); doing nothing (the status quo this change exists to fix).

## D4 — Identity: run ids and process discriminators

- Every process mints a **run id** at startup (short, sortable:
  timestamp+pid encoded, e.g. `hx7f3a`); `log_trace` stamps `run=<id>` and
  `proc=host|daemon|cli` on every line (fields on the fmt layer, same place
  `wt=` is injected today).
- The daemon additionally logs the **client's** run id at attach/handshake
  (one line per session attach: `client_run=<id> session=<sid>`), so a
  support bundle from the compositor can be correlated with the daemon file.
  `daemon_id` keeps its current line; it is the daemon's run id.
- `thegn logs tail --run <id>` filters. The two duplicated log-line parsers
  (`log_view::parse_log_line` vs `log::parser::parse_log`) are unified while
  adding the fields — one parser, one `LogLevel` (the panel's `Fatal` variant
  included), used by both the Logs panel and `logs tail`.

## D5 — Sinks: one file per process, everything bounded, config honored

- **Daemon → `thegn-daemon.log`** in the same logs dir. This ends the
  two-writers rotation race structurally (no coordination protocol needed —
  rejected: advisory file locks around rotation, which add a syscall per
  write batch to defend a layout we don't want anyway). The daemon builds its
  `LogConfig` from `log_config_from_env` + `[log]` (it loads config anyway),
  fixing the pinned-5MB×5 bug.
- **Compositor `[log]` reconciliation:** init stays env-built and early (the
  startup waterfall must capture config load itself); after config load the
  host applies `[log]` file settings (dir/rotation/level) via a reload handle
  (`tracing_subscriber::reload`) unless the env overlay set them — env wins,
  matching every other knob. Level-only reload; the ring layer is not
  reloadable.
- **`thegn-stderr.log`:** rotated at startup (rename to `.1`, one
  generation) when over `[log] stderr_cap_mb` (default 5). In-session growth
  is accepted and documented — the redirect is an fd-level dup2 and stderr is
  quiet in healthy sessions; a mid-session reopen ladder was rejected as
  machinery without a demonstrated need.
- **`audit.log`:** same startup-rotation treatment via the shared helper.
- The Logs panel tails only the compositor's own file (unchanged); `thegn
logs tail` and the bundle know about all sinks.

## D6 — Redaction chokepoint

One function in `thegn-core` (`log_redact`): given key/value or argv-shaped
input, redacts values whose key matches the shared sensitive list (the MCP
`SENSITIVE` list moves to core; `add-credential-broker` consumes the same
list for its audit events — whichever lands first hosts it) and
`--token`/`--password`-style flag values. Applied:

- at the pane-argv log site (`service.rs` logs `argv[0]` + arg count at
  DEBUG; full argv only at TRACE **through the redactor**),
- to env maps anywhere they are logged,
- to the bundle's config and log passes (belt and braces: the bundle also
  scrubs values it can recognize from resolved `env:`/`file:` refs at bundle
  time).

Honest limits, stated in the spec: pattern redaction is best-effort; the
primary rule is _don't log secret-bearing shapes at all_ (names and counts at
DEBUG, never values). No claim of entropy detection.

## D7 — Error-surfacing contract

Per failure class, the surface is named and spec'd:

| Class                                                                                     | Surface                                                                                                                                                                                                                                             |
| ----------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| User-invoked action fails (primary path)                                                  | `msg`/status line — never swallowed (promotes the CLAUDE.md convention to a requirement)                                                                                                                                                            |
| `msg::*` while TUI active                                                                 | always the ring; the log when a sink is enabled; toast machinery is AI 749–753's to build on top                                                                                                                                                    |
| Behavior-changing fallback (daemon → in-process, ssh-wss → local, native-exec → fallback) | a notification through the existing `NotificationKind` path naming what degraded and why, plus the warn                                                                                                                                             |
| Daemon health                                                                             | chip error state on stale heartbeat (threshold = existing heartbeat cadence × small factor); click keeps the on-demand probe and now shows the probe error text; composes with `make-daemon-default`'s persistence chip (same slot, additive state) |
| Daemon crash                                                                              | daemon-side hook writes a crash report (`proc=daemon`); the next attach/probe surfaces "daemon restarted/crashed, report at <path>"                                                                                                                 |
| CLI verbs                                                                                 | `main.rs` constructs `Role::Cli` when `THEGN_LOG` is set → stderr layer works outside the TUI; unset stays silent (stderr belongs to the verb's own output)                                                                                         |

Render/damage note (house rule): the chip error state is chrome → existing
`Full` damage on state change, driven by the heartbeat check that already
runs off-loop; notifications ride the existing event-bus → waker path. No new
wake source, no polling; the 0%-idle contract is untouched. No SQLite schema
change (crash dir is files; `user_version` unchanged). No new help context:
no new action/zone/panel section; new config keys surface in the generated
config-reference page.

## D8 — `thegn doctor bundle`

`thegn doctor bundle [--out <path>]` writes `thegn-bundle-<ts>.tar.gz`
(default: cwd) containing:

- `doctor.json` — the existing `--json` extended with the identification
  block (D9),
- `config.redacted.toml` — the effective config through the same redactor
  the MCP `thegn://config/current` resource uses,
- bounded tails (default last 500 lines each) of `thegn.log`,
  `thegn-daemon.log`, `thegn-stderr.log`, `audit.log`,
- crash reports (all retained),
- a `MANIFEST` the command also prints, so the user can see exactly what
  they are about to share.

Catalog: one new `thegn_core::capability::CATALOG` row (`doctor.bundle`) on
the operator surface set (CLI + control API; off MCP and plugins, matching
the pinned admin-caps convention), gated by `required_scope` — no second
policy table. The verb lives under `doctor` because `thegn debug` is the
BugStalker debugger namespace; naming follows
`add-cli-namespaces-and-remote-open` conventions.

## D9 — Doctor identifies the installation

Doctor (text + `--json`) gains: thegn version + channel + build metadata
(git sha when embedded), OS/platform, daemon version + reachability (via the
existing probe; version over the control socket handshake), and a `[log]`
section: effective level, sink paths, current sizes, rotation caps. Doctor
still always exits 0.

## D10 — Error-tracking forwarding: judged, reserved

rustrak (the issue's link) is a self-hosted, Sentry-protocol error tracker
(Rust server, SQLite/Postgres, integrates via standard Sentry SDKs pointed
at its DSN — ~105 stars, active). Wiring it in would mean adopting a Sentry
SDK dependency and a network path for crash data. Judgment: that is opt-in
telemetry territory even self-hosted, and thegn's actual gap is local
capture, which this change closes with zero configuration and zero network.
So: `[diagnostics] crash_sink` is declared as a provider-seam kind
(`sentry` — the protocol name; covers rustrak and Sentry itself) that is
**`reserved`** per the house implemented-or-`reserved` rule, and the spec
pins that the default configuration performs no network I/O for
diagnostics. If it is ever implemented it is a seam (`thegn_core::seam`,
Probe in doctor), default-off, sending only what the crash report contains
post-redaction.

## Security

- **Secrets:** the redaction chokepoint (D6) is the load-bearing piece —
  pane argv at DEBUG is today a verified plaintext-token path into
  `thegn.log`. Rules become requirements: no secret value in any log line,
  crash report, or bundle; argv/env logged only through the redactor;
  sensitive-key list shared with `add-credential-broker`. The ring holds
  only already-redacted event text.
- **Crash reports** can contain paths, hostnames, branch names — the crash
  dir is `0700` under the state dir, reports are created `0600`, and nothing
  leaves the machine (D10). The bundle is explicitly user-initiated sharing:
  it prints its manifest, and redaction is applied at bundle time again.
- **New write surfaces:** the crash dir and the bundle output path. The
  bundle verb is catalog-gated (`required_scope`) on the operator surface —
  not reachable from MCP or plugins, so an agent in a pane cannot exfiltrate
  a bundle through thegn's own API.
- **Sandbox:** panes never see the logs dir or crash dir unless the user's
  mounts already expose the state dir; no change to sandbox defaults.
- **Blast radius:** the always-on layer runs in-process only; a bug in the
  ring cannot write disk. The panic hook writes only under the state dir and
  to the saved stderr fd.

## Open questions

- Should the chip error state also cover a _stale_ daemon (heartbeat old but
  socket alive, e.g. wedged event loop)? Initial answer: yes — the probe
  distinguishes "dead socket" from "alive but stale" in the message; both
  render the error state.
- `[log] level` reconciliation (D5) after config load: level-only reload is
  proposed; is dir/rotation reload worth the churn given env already covers
  it? Default: dir/rotation honored from config at _daemon_ start (it loads
  config first), env-only for the compositor's early init, documented.
- Ring size (256 events) and stderr cap (5 MB) defaults — tune during
  implementation; both configurable.
