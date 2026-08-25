# Diagnostics

## ADDED Requirements

### Requirement: A panic restores the terminal before anything else

The panic hook SHALL be installed unconditionally at process start,
independent of `THEGN_LOG` and of any tracing subscriber. While the
compositor owns the raw/alternate screen it MUST register an idempotent,
non-panicking terminal-restore callback with the hook (and clear it on
normal teardown); on a main-thread panic the hook MUST run that callback
first — before logging, report writing, or delegating — using only writes
that cannot themselves panic, and MUST NOT rely on any type's `Drop` running
during unwind to restore the terminal. After restoring, the hook SHALL print
a one-line crash notice naming the crash-report path to the original
(pre-redirect) stderr so the user sees it in their terminal. Worker-thread
panics skip terminal restore but still report.

#### Scenario: Mid-frame panic leaves a usable terminal

- **WHEN** the main thread panics while the compositor is mid-frame on the
  alternate screen in raw mode
- **THEN** the terminal is returned to cooked mode on the primary screen with
  the cursor visible, and a one-line notice with the crash-report path is
  printed to the user's terminal

#### Scenario: Restore is idempotent across hook and teardown

- **WHEN** the restore callback has already run (via the hook) and normal
  teardown subsequently runs, or vice versa
- **THEN** the restore sequence executes at most once and the second caller
  is a no-op

#### Scenario: Worker-thread panic does not touch the screen

- **WHEN** a background thread panics while the compositor is rendering
- **THEN** the terminal state is untouched, the panic is recorded, and the
  session continues where a recovery path exists today

### Requirement: Every panic produces a crash report

Every panic in any thegn process SHALL write a best-effort crash report file
under a dedicated crash directory in the state dir, containing: thegn
version, channel and build metadata, OS/platform, the process kind and run
id, the panicking thread's name, the panic message and location, a
backtrace captured regardless of the `RUST_BACKTRACE` environment variable,
and the tail of the in-memory warning ring. Report writing MUST NOT itself
panic or deadlock (degrading to a partial report instead), MUST work with
`THEGN_LOG` unset, and retention MUST be bounded (oldest reports pruned).
The next compositor launch SHALL surface an unacknowledged crash report via
a notification naming the report path, and `thegn doctor` SHALL list recent
reports.

#### Scenario: Crash report exists with logging disabled

- **WHEN** thegn panics in a session where `THEGN_LOG` is unset
- **THEN** a crash report file exists containing the version, run id, panic
  message and location, and a captured backtrace

#### Scenario: Next launch surfaces the crash

- **WHEN** the compositor starts and an unacknowledged crash report exists
- **THEN** a notification names the report path, and acknowledging it
  prevents re-surfacing on subsequent launches

#### Scenario: Retention is bounded

- **WHEN** more reports exist than the retention limit after a new crash
- **THEN** the oldest reports are pruned and the newest retained

### Requirement: Crash capture is always on; full logging stays opt-in

thegn SHALL always install a minimal diagnostics layer consisting of a
fixed-size in-memory ring capturing WARN-and-above events and the panic
target — and nothing else — regardless of `THEGN_LOG`. This layer MUST
perform no file or terminal I/O until a crash report or debug bundle is
requested, MUST add no polling or wake source, and sub-WARN callsites MUST
impose only a cached-interest check. File and stderr log sinks SHALL remain
opt-in exactly as today (`THEGN_LOG` / `[log]`): unset means no sink is
installed and no log file is written.

#### Scenario: No sink and no I/O when logging is off

- **WHEN** thegn runs with `THEGN_LOG` unset
- **THEN** no log file is created and the diagnostics layer performs no I/O,
  while WARN and ERROR events accumulate in the in-memory ring

#### Scenario: The ring feeds the crash report

- **WHEN** warnings were emitted before a panic in a session with no log
  sink
- **THEN** the crash report contains those warnings from the ring tail

### Requirement: Log lines carry process and run identity

Every log line SHALL carry a process discriminator (`host`, `daemon`, or
`cli`) and a per-process run id, in addition to the existing worktree tag.
The daemon SHALL log the connecting client's run id when a client attaches,
so one session can be correlated across the compositor's and the daemon's
log files. `thegn logs tail` SHALL support filtering by run id.

#### Scenario: Correlating a session across processes

- **WHEN** a compositor with run id R attaches to the daemon and both have
  file logging enabled
- **THEN** every compositor line carries `proc=host run=R`, and the daemon's
  file records an attach line naming client run id R

#### Scenario: Tailing one run

- **WHEN** a user runs `thegn logs tail` filtered to run id R
- **THEN** only lines from that run are shown

### Requirement: Each process owns its log file and every sink is bounded

The daemon SHALL write its own log file, distinct from the compositor's, so
no two processes ever share one rotation state machine, and the daemon's log
configuration SHALL honor the same environment overlay and `[log]` config
keys as the host rather than pinned defaults. The compositor SHALL apply the
`[log]` config section after config load for settings the environment
overlay did not set (environment wins), without losing startup-waterfall
capture. The stderr capture file and the audit log SHALL be size-bounded:
when over their cap at process start they are rotated before use.

#### Scenario: Concurrent processes cannot race rotation

- **WHEN** the compositor and the daemon both log heavily at the same time
- **THEN** each writes and rotates only its own file and no rotation rename
  from one process affects the other's file

#### Scenario: Daemon honors configured rotation

- **WHEN** the daemon starts with a rotation size configured via environment
  or `[log]`
- **THEN** the daemon's log file rotates at the configured size, not the
  built-in default

#### Scenario: Oversized stderr log is rotated at startup

- **WHEN** the compositor starts and the stderr capture file exceeds its
  configured cap
- **THEN** the file is rotated aside before stderr is redirected into a
  fresh one

### Requirement: Logs, crash reports, and bundles never contain secret values

No secret value SHALL appear in any log line, crash report, or debug
bundle. Command argvs and environment maps MUST pass a redaction chokepoint
before being logged at any level: values for keys on the shared
sensitive-key list and values of secret-bearing flags are replaced with a
redaction placeholder, and at DEBUG level a spawned command is logged by
program name and argument count only, never full argv. The sensitive-key
list is a single shared definition also used by credential-audit events.
Redaction is best-effort pattern matching; the primary rule is that
secret-bearing shapes are not logged at all.

#### Scenario: Pane argv with a token is redacted

- **WHEN** a pane is spawned through the daemon with an argv containing
  `--token <value>` or a `FOO_TOKEN=<value>` assignment and DEBUG logging is
  enabled
- **THEN** the log records the program name and argument count, and any
  fuller argv rendering carries a redaction placeholder in place of the
  secret value

#### Scenario: Bundle redacts configured secrets

- **WHEN** a debug bundle is produced from a config containing a plaintext
  token value
- **THEN** the bundled config carries a redaction placeholder where the
  token value was

### Requirement: Errors are surfaced, never silently dropped

Every failure SHALL have a named surface. An error on the primary path of a
user-invoked action MUST reach the user (status line, message, or CLI
stderr) — never only a log. Branded messages emitted while the TUI is
active MUST always reach the in-memory ring, and additionally the log when a
sink is enabled; they MUST NOT be dropped when no subscriber sink exists. A
background fallback that changes user-visible behavior MUST raise a
notification naming what degraded and why, in addition to any log line. CLI
invocations SHALL install a stderr tracing layer when `THEGN_LOG` is set, so
diagnostics work outside the TUI.

#### Scenario: A TUI-session error is not lost without a sink

- **WHEN** `msg::error` fires while the TUI is active and `THEGN_LOG` is
  unset
- **THEN** the message is present in the in-memory ring and appears in a
  subsequent crash report or debug bundle

#### Scenario: THEGN_LOG works for plain CLI verbs

- **WHEN** a user runs a non-TUI verb with `THEGN_LOG=debug`
- **THEN** tracing output at that level appears on stderr

### Requirement: One verb produces a redacted debug bundle

`thegn doctor bundle` SHALL write a single archive containing: the doctor
JSON report (including the installation identification block), the
effective config with secret values redacted, bounded tails of every log
sink (compositor, daemon, stderr capture, audit), and retained crash
reports — and SHALL print a manifest of exactly what was included. The verb
SHALL be a `thegn_core::capability::CATALOG` row on the operator surface
set (CLI + control API; not MCP or plugins), gated by `required_scope` — no
second policy table. Log content included in the bundle passes the same
redaction chokepoint.

#### Scenario: Bundle contents and manifest

- **WHEN** a user runs `thegn doctor bundle`
- **THEN** an archive is written containing doctor JSON, redacted config,
  bounded log tails, and crash reports, and the command prints a manifest
  listing every included file

#### Scenario: The bundle verb is catalog-projected

- **WHEN** the capability catalog is enumerated
- **THEN** the bundle verb appears as a row on the operator surface set with
  its required scope, and it is not exposed via MCP or plugin surfaces

### Requirement: Doctor identifies the installation

`thegn doctor` (text and `--json`) SHALL report thegn's own version, release
channel and build metadata, the OS/platform, the daemon's version and
reachability, and the logging configuration in effect including each sink's
path, current size, and rotation cap. Doctor remains information, not
failure: it still exits 0.

#### Scenario: Doctor names its own version

- **WHEN** a user runs `thegn doctor --json`
- **THEN** the output contains thegn's version, channel, build metadata, and
  OS, alongside the daemon version or its unreachability

#### Scenario: Doctor locates the logs

- **WHEN** a user runs `thegn doctor`
- **THEN** the output lists each log sink's path, size, and cap

### Requirement: Crash-report forwarding is reserved and off by default

Forwarding crash reports or errors to an external tracking service SHALL be
a provider-seam kind declared `reserved` (Sentry-protocol shaped, covering
self-hosted implementations) and MUST be off by default: the default
configuration SHALL perform zero network I/O for diagnostics, and no
diagnostic data leaves the machine without explicit opt-in configuration.

#### Scenario: Default configuration sends nothing

- **WHEN** thegn runs with default configuration and a crash occurs
- **THEN** the crash report is written locally and no network request is
  made by any diagnostics component

#### Scenario: The sink kind is reserved

- **WHEN** a config declares a crash-sink kind
- **THEN** it is rejected as `reserved` (not yet implemented) rather than
  silently ignored
