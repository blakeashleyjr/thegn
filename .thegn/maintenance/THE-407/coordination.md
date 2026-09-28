# Coordination brief — THE-407 (primary)

## Scope

`thegn-core/src/config.rs`, `apps/registry.rs`, `apps/mod.rs`. Nothing else.

## The shape the primary expects

Two defects that share a cause — `BUILTIN_TABS` only knows `"work"`:

1. `[apps]` config cannot order or select the shipped `observe` tab.
2. Live reload never rebuilds `AppHost`, so a config change does not take effect
   until restart.

## The layering constraint — this is the part to get right

The issue itself flags a **core↔host catalog-layering** risk, and it is real.
`thegn-core` is substrate-free and owns config; `AppHost` lives in the host. **Do
not move the tab catalog across that seam in either direction** to make the fix
convenient.

This repo has one rule for exactly this situation: **one capability catalog** —
the control API, gRPC, CLI verbs, MCP tools and plugin host calls all project
`thegn_core::capability::CATALOG`. Before inventing a registry shape, check
whether the built-in tabs belong in that catalog or should be projected from it. If
they should, say so; if they genuinely should not, say why. That decision is worth
more than the code.

## Live reload

Rebuilding `AppHost` on reload must not blow the render invariants: a config change
is a chrome change, so it is a `Full` render, and the rebuild itself must not do
blocking I/O on the loop. If rebuilding needs I/O, it happens off-thread and hands
back over a channel.

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
