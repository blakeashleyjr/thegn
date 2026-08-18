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
wizard) resolves sandboxes off-thread — the seconds-to-minutes container work is
gone from the loop. A few small, deliberately **synchronous** DB writes remain
on structural events (this is the sanctioned best-effort-persist family; git,
not the DB, is the source of truth):

- `persist_session_layout` — a whole-session persist on structural changes
  (documented; the lightweight focus-change persist already runs off-loop).
  ~50–100 ms in release on a large session.
- Tiny per-event upserts on the loop: the new-terminal wizard's terminal-row
  persist and the sidebar `ui_state` write. Each is a single `Db::open` + one
  small upsert (a few ms), bounded by the DB's 5 s busy-timeout ceiling under a
  concurrent writer.
- The startup-shell **watchdog's clean-shell fallback** (fired only when a
  freshly materialized shell produces no output before its deadline) still
  resolves its launch spec synchronously — an exceptional failure-recovery
  path.

None of these block indefinitely, and all are one-shot reactions to a
user-driven structural change (a switch/open/close), not steady-state work.

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
- Closing a **local** pane whose child both ignores `SIGHUP` and has stopped
  reading its stdin (a wedged process with a full input queue) can leave its
  per-pane writer thread parked on the final flush, holding one file descriptor,
  until the child dies. Bounded (one thread + fd per such pane) and strictly
  better than the pre-alpha behavior (which blocked the whole UI on that flush);
  the daemon-backed transport (the default) is unaffected.

## Platform

- **Only x86_64 Linux is supported.** Prebuilt binaries ship for linux-gnu and
  linux-musl.
- **macOS and Windows are unvalidated.** No binaries are published for either,
  and neither has ever been run interactively.
  - **Windows** got its first real CI run for this release. Until now the repo
    could not even be _cloned_ on Windows: `crates/thegn-core/src/store/aux.rs`
    used a reserved DOS device name, so git refused it with `invalid path`.
    With that renamed, `cargo check --workspace` **passes on msvc** — the port
    compiles. What still fails is the named-pipe daemon IPC:
    `ipc::tests::pipe_bind_is_the_lock_and_round_trips` (`thegn-svc/src/ipc.rs`),
    where a second `bind_exclusive` does not report the endpoint as already
    held. The Job-Object tests and the release build have not been reached yet.
    The msvc job is opt-in (`[ci-windows]`) until it passes. When Windows does
    land: native panes only (container sandboxing is a Linux/WSL2 feature) and
    a modern terminal is required (Windows Terminal; legacy conhost is refused).
  - **macOS** has never been compiled at all. Its CI job is opt-in
    (`[ci-macos]`) and has never executed, and darwin cannot be cross-checked
    from Linux — `just check-cross` covers only the C-dep-free leaf crates,
    because `thegn-host`'s build scripts (`ring`, bundled sqlite) need a real
    darwin C toolchain.
- Cloud execution providers, remote worktrees over SSH, the Observe dashboards,
  the placement engine, and non-GitHub issue trackers are **dev-channel only** in
  this release (`THEGN_CHANNEL=dev`).

## Distribution

- Prebuilt binaries cover **x86_64 Linux (gnu + musl) only** — the macOS and
  windows-msvc legs were removed from the release matrix because those targets
  have never been built (see Platform above). Nix
  (`nix profile install github:blakeashleyjr/thegn`) and `./install.sh` are the
  other Linux paths.
- The Homebrew formula (`packaging/homebrew/thegn.rb`) is staged but inert: it
  needs macOS release assets, and the `blakeashleyjr/homebrew-tap` repo does not
  exist yet.
- `crates.io` / `cargo binstall` need the workspace crates made publishable
  first — see [`RELEASING.md`](RELEASING.md).
