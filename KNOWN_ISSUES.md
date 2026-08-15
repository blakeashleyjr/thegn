# Known issues — 0.1.0-alpha.1

This is a public **alpha**. The items below are known, tracked, and deferred to
a later release — each is a narrow edge case or a deliberate design trade-off.
The pre-alpha audit (73 verified findings) is otherwise fully remediated; full
detail lives in
[`docs/superpowers/specs/alpha-audit-2026-08.md`](docs/superpowers/specs/alpha-audit-2026-08.md).

If you hit one of these it's a known limitation — but a reproducible report is
still welcome.

## Performance (event loop)

Idle CPU stays ~0% and pane bring-up (creation, crash respawn, the new-terminal
wizard) resolves sandboxes off-thread. Two narrow paths remain on the loop:

- `persist_session_layout` is a deliberately **synchronous** whole-session
  persist on structural changes (documented; the lightweight focus-change
  persist already runs off-loop). ~50–100 ms in release on a large session.
- The startup-shell **watchdog's clean-shell fallback** (fired only when a
  freshly materialized shell produces no output before its deadline) still
  resolves its launch spec synchronously — an exceptional failure-recovery
  path.

## Daemon / remote serving (`thegn serve`)

`thegn serve` (remote thin clients) is the newest surface. Hardened
substantially this release (loopback-default bind, owner-only socket + run-dir,
control-plane worktree-path confinement, protocol version-skew handshake, no
idle-exit while serving, no scrollback re-replay on reconnect, TOCTOU-safe
socket election), and disabling the daemon with persisted daemon-backed panes
now claims each persisted session exactly once (respawning it in-process and
stopping the daemon copy) instead of duplicating panes. No open issues are
tracked here at release time — treat surprises on this surface as reportable
bugs, not known limitations.

Note: launching with `THEGN_NO_DAEMON=1` while daemon-backed sessions are
persisted now actively stops those daemon sessions as it claims them (previously
they were silently orphaned).

## Concurrency

- A few best-effort persists (focus / active-tab pointer, corner-pane parsing)
  have benign unordered-writer races; last-writer-wins, no corruption. (The
  merge-gate cross-process race is now fixed with an flock.)

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
