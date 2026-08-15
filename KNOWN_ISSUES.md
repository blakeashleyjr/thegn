# Known issues — 0.1.0-alpha.1

This is a public **alpha**. The items below are known, tracked, and deferred to
a later release — each is a narrow edge case or a change whose fix carries more
regression risk than it's worth right before the first release. The pre-alpha
audit (73 verified findings) is otherwise fully remediated; full detail lives in
[`docs/superpowers/specs/alpha-audit-2026-08.md`](docs/superpowers/specs/alpha-audit-2026-08.md).

If you hit one of these it's a known limitation — but a reproducible report is
still welcome.

## Performance (event loop)

A few paths still do work on the event loop rather than off-thread. None block
indefinitely (the DB uses a 5s busy-timeout ceiling) and idle CPU stays ~0%, but
under contention they can cost a frame:

- **Crash respawn** re-resolves the sandbox on the loop (`pty_drain.rs`) — a slow
  container (re)create can stall the frame while a crashed pane respawns. Steady
  state is unaffected; only the exceptional respawn path.
- A **large paste** into a non-reading / flow-controlled local PTY does a
  blocking `write_all` on the loop (`pane.rs`). Bounded by the kernel PTY buffer
  (~64 KB); the daemon-backed transport already drops instead of blocking.
- `persist_session_layout` is a deliberately **synchronous** whole-session
  persist on structural changes (documented; the lightweight focus-change persist
  already runs off-loop). ~50–100 ms in release on a large session.
- The new-terminal wizard spawns its shell synchronously on submit (a one-shot,
  user-triggered action).

Moving these off-thread is mechanical but touches the ~18k-line loop; deferred to
keep the alpha's render invariants stable.

## Daemon / remote serving (`thegn serve`)

`thegn serve` (remote thin clients) is the newest surface. Hardened substantially
this release (loopback-default bind, owner-only socket + run-dir, control-plane
worktree-path confinement, protocol version-skew handshake, no idle-exit while
serving, no scrollback re-replay on reconnect, TOCTOU-safe socket election).
Remaining edge:

- Disabling the daemon (`[daemon] enabled = false` / `THEGN_NO_DAEMON`) while
  panes were persisted daemon-backed can duplicate a pane on the in-process
  fallback path.

## Concurrency

- A few best-effort persists (focus / active-tab pointer, corner-pane parsing)
  have benign unordered-writer races; last-writer-wins, no corruption. (The
  merge-gate cross-process race is now fixed with an flock.)

## CLI

- Several worktree-targeting verbs mix a `--worktree` flag with a positional
  argument; this will be unified in a later release.

## Config

- `thegn config validate` type-checks the common enum keys (sandbox, log, theme,
  merge_queue, pins, …) but not yet every one of the ~50 `config_enum!` types, so
  an out-of-range value for an uncovered enum can still pass `validate`. It is
  caught — and rolled back — by `thegn config set`, which re-parses the whole
  file after writing.

## Platform

- **Windows** support is best-effort: native panes only (no container
  sandboxing — that's a Linux/WSL2 feature), and requires a modern terminal
  (Windows Terminal; legacy conhost is refused).
- Cloud execution providers, remote worktrees over SSH, the Observe dashboards,
  the placement engine, and non-GitHub issue trackers are **dev-channel only** in
  this release (`THEGN_CHANNEL=dev`).

## Distribution

- Prebuilt binaries (all desktop platforms) and a Homebrew formula are wired via
  the release workflow but require a maintainer to cut the first tag; until then,
  install via Nix or `./install.sh`. `crates.io` / `cargo binstall` need the
  workspace crates made publishable first — see [`RELEASING.md`](RELEASING.md).
