# Coordination brief — THE-455 (primary)

## Scope

`calendar/ics.rs` (both the svc and core copies) and `hydrate_calendar.rs`.
**THE-459 is a sibling lane in this batch and also edits `hydrate_calendar.rs`** —
keep your diff there minimal and list the functions you touch, so the two can be
sequenced.

## The two distinct defects — do not conflate them

1. **No bounds.** Local ICS/vdir ingestion has no byte, file-count or time limit.
2. **Cap applied before filtering.** The 20,000-entry cap is applied _before_
   selecting `.ics` files, so _which_ files get ingested depends on directory
   order — the selection is nondeterministic.

(2) is the subtler one and the one a "just add a limit" fix leaves in place.
**Filter first, then sort, then cap**, so the same directory always yields the same
set. A deterministic selection is the acceptance criterion, not just a bounded one.

## What the tests must pin

- A directory whose non-`.ics` files outnumber the cap still ingests the `.ics`
  ones — the case the current ordering breaks.
- The same directory, presented in a different readdir order, yields the **same**
  selection. (Construct the order explicitly; do not rely on the filesystem.)
- An oversized single file, and a directory over the file-count bound, are each
  **refused with a specific reason** — and the refusal says what was exceeded, not
  "invalid calendar".
- Bounds are expressed as named constants, and the test asserts against the
  constant rather than a literal. A hard-coded bound rots the moment the constant
  moves; that exact drift cost a round last batch.

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
