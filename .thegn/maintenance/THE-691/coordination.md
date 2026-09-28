# Coordination brief — THE-691 (primary)

## Scope

The `config get` and `automations test` command paths, and the assertion in
`crates/thegn-host/tests/static_cli_support/unix.rs` that currently encodes the
wrong behaviour. Nothing else.

## The shape the primary expects

`api call` already reads its config through an **admitted capture**; these two
commands read the source directly, so a non-regular source (a FIFO, a device)
hangs them instead of being refused. So this is **routing two callers onto an
existing path**, not building a new one.

Read how `api call` does it and reuse that exactly. If the existing path does not
quite fit, say why rather than forking a second capture.

## The property that matters

A non-regular config source must produce a **prompt refusal with a specific
reason**, never a hang and never a silent empty read. "It no longer hangs" is not
the acceptance criterion — the criterion is that it _refuses and says what it
saw_. Assert the refusal, not a timeout: a test that passes because something
completed within N seconds is a test that will flake and will not catch a
regression back to blocking.

Do **not** widen the admission seam to make this easier. If admission needs a new
capability, that is a finding to report.

## Constraints that apply to every lane in this batch

- **`thegn-core` is substrate-free** (no tokio, termwiz, portable-pty, HTTP, forge
  SDK) and gated at **95% lines**. New core logic needs unit tests in the same
  change.
- **Never blocking I/O on the event loop** (git, DB, subprocess, D-Bus, network),
  and never before the first frame. Off-thread producers send on a channel _and_
  pulse the `TerminalWaker`.
- **A new `section.key` in config trips FOUR gates**, not three: the
  `config.toml.example` key-coverage test, the env-overlay ratchet, strict config
  validation, and — for anything expressed in seconds/days —
  `config_duration.rs`'s `direct_environment_checks_match_complete_schema_policy`,
  which asserts the direct env-overlay checks and the whole-config checks report
  the same error count. That fourth one is not in CLAUDE.md and took a land-gate
  failure to find.
- **Per-OS code belongs in `src/platform/`.** The `test/platform-cfg-*-ratchet.txt`
  allowlists are shrink-only, so a platform `#[cfg]` in a file not already listed
  **fails the build**. For a test that genuinely cannot compile off-Unix, prefer a
  runtime skip through an existing seam (`sandbox_backend::host_os()`,
  `thegn_core::fsperm::make_executable_for_test`) over `#[cfg(unix)]` — the test
  then still type-checks on Windows.
- **Tests must isolate `XDG_STATE_HOME`.** This shell often runs inside a live
  thegn; anything opening the DB or spawning the host must not touch real state.
- **Ignored `Result`s must be deliberate.** `let _ =` / `.ok()` needs a short
  `// best-effort: <why>` unless it is obviously a cache/waker/cleanup path, and
  never on the primary path of a user-invoked action.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run. Withholding a verdict is the correct outcome when you cannot build — several
lanes did that last batch and it was right every time.

## Deliverable

A plan the primary reviews and greenlights before any implementation. **No
production edits in this stage.** If you find the issue's premise is wrong, or a
dependency it does not mention, say so — two workers overturned a primary decision
last batch and were right both times.
