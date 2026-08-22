# Known issues — 0.1.0-alpha.2

This is a public **alpha**. The items below are known, tracked, and deferred to
a later release — each is a narrow edge case or a deliberate design trade-off.
The pre-alpha audit (73 verified findings) is otherwise fully remediated; full
detail lives in
[`docs/superpowers/specs/alpha-audit-2026-08.md`](docs/superpowers/specs/alpha-audit-2026-08.md).
A second pass before `0.1.0-alpha.2` covered packaging, licensing, CI and gate
coverage rather than runtime behavior — see that release's
[`CHANGELOG.md`](CHANGELOG.md) entry. It left no known issues open.

If you hit one of these it's a known limitation — but a reproducible report is
still welcome.

## UI

- **Tab strip overflow.** When the center tab strip is narrow (panel open at
  ~100 columns) chips that don't fit are dropped from the strip without an
  overflow indicator; the tabs exist and `Alt-Left/Right` reach them, and they
  reappear when the strip widens. Tracked as tasks.md 745.
- **Keys during a tab's bring-up are dropped.** Typing into a brand-new tab
  before its shell has materialized loses the keys (by design — there is no
  pane yet); host chords still dispatch. `32-resurrect` waits for the prompt.

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
substantially in `0.1.0-alpha.1` (loopback-default bind, owner-only socket + run-dir,
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
  - **Windows** got its first real CI runs in `0.1.0-alpha.1`, and now compiles
    and passes its tests. Until recently the repo could not even be _cloned_ on
    Windows: `crates/thegn-core/src/store/aux.rs` used a reserved DOS device
    name, so git refused it with `invalid path`. With that renamed,
    `cargo check --workspace` passes on msvc; the named-pipe daemon IPC tests
    pass (a bind through the pipe-name teardown window was mistaking its own
    predecessor for a live daemon); and the Job-Object process-scoping tests
    pass. What has **not** completed is the release build, which the opt-in
    msvc job has so far run out of time on, and nobody has run thegn
    interactively on Windows — so no binaries ship and it stays unsupported.
    When it does land: native panes only (container sandboxing is a Linux/WSL2
    feature) and a modern terminal is required (Windows Terminal; legacy
    conhost is refused).
  - **macOS** has never been compiled end-to-end. Its CI job is opt-in
    (`[ci-macos]`) and has never got past building the dev shell, where the
    `openspec` derivation's `pnpm install` was OOM-killed on the 7 GB runner;
    that derivation now pins `NODE_OPTIONS`/pnpm child-concurrency to cap its
    peak, but the job has not been re-run to confirm. The darwin-side work that
    _is_ done: the flake's darwin outputs evaluate (the Linux-only OCI images
    and musl bridge are gated out, and the dropped `x86_64-darwin` is no longer
    declared), the dev-loop scripts no longer assume GNU userland, and the
    macOS runtime gaps are filled (`sysinfo` activity scanner, `open` instead of
    a hardcoded `xdg-open`, `apple` in the default sandbox chain, libproc-backed
    pane cwd/foreground capture). `just check-cross` now covers every crate that
    builds without a darwin cross C toolchain — but `thegn-core`/`-svc`/`-host`
    still can't be checked from Linux, because their build scripts (`ring`,
    bundled sqlite) need a real darwin one. The remaining proof is the
    on-device checklist in [`CONTRIBUTING.md`](CONTRIBUTING.md#on-device-checklist).
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
