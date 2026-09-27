# Coordination brief — THE-492 (primary)

## Scope

`config_media.rs`, `thegn-media/src/mpd.rs`, `secret_scan.rs`. Nothing else.

## The shape the primary expects

`media.mpd.password` currently sits in plaintext config, bypassing the typed
**secret-broker chokepoint** that THE-122 established. The job is to route it
through that existing seam — **not** to design a second way to hold a secret.

Read THE-122's implementation first and follow it exactly. If the MPD case cannot
use it as-is, report precisely why; "MPD is different" is not sufficient, since the
whole point of a chokepoint is that callers are not special.

## What must be true when you are done

- The password resolves through the broker, with keyring support and whatever
  audit the broker already provides.
- **`secret_scan` knows the new shape**, so a plaintext password left in config is
  _detected_ rather than silently ignored. A migration that makes the old form
  invisible is worse than leaving it — the point is that an operator finds out.
- **Existing configs keep working, or fail loudly.** Decide which, say which, and
  test it. Silently ignoring a previously-honoured password would break someone's
  MPD connection with no diagnostic.
- Config-key changes trip the four config gates listed below.

Never log or interpolate the secret — not in an error, not in a debug line. A
refusal names the _component_, never the value.

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
