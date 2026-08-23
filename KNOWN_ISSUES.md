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

- **x86_64 Linux is the supported platform.** Prebuilt binaries ship for
  linux-gnu and linux-musl, and — from the next tagged release —
  aarch64-apple-darwin.
- **macOS is best-effort, Windows is unvalidated.** macOS has been run on a real
  Mac and its CI job is re-enabled (opt-in) but has still never completed a run;
  Windows has not been run interactively at all, and publishes no binaries.
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
  - **macOS** now builds, tests and runs on Apple silicon, but is not yet
    validated enough to support. What has been done on a real M-series Mac:
    `nix develop` builds (it previously could not be entered at all — `unar`,
    a yazi archive-preview helper, fails to link on aarch64-darwin and took the
    whole dev shell with it); `just build`/`test`/`lint`/`doc-check`/`smoke`/
    `check-cross` all pass; the release binary launches and reaches first frame
    in ~250-290ms; the pane daemon binds, serves and warm-reattaches; and the
    `daemon_panes` e2e spec passes. A first pass of real-device bugs is fixed —
    silent file-delivery corruption from GNU-only `stat -c`, host probes that
    reported every Mac as an idle 0 KB box, pairing URLs that advertised
    `localhost`, repo resolution broken by the `/tmp`→`/private/tmp` symlink, a
    pane-daemon socket that could exceed `sun_path`, a sandbox chain that
    selected stopped runtimes, and a `proc_listchildpids` count-vs-bytes bug
    that meant the relaunch hint never captured a foreground job.
    **Set your terminal to send Alt for Option.** macOS composes characters
    with Option by default, so thegn's Alt-based chords (`Alt-w`, `Alt-o`,
    `Alt-s`, `Alt-.`, every `Ctrl-Alt` toggle) type `∑`-style glyphs instead
    and read as dead keys. Ghostty: `macos-option-as-alt = true`; Alacritty:
    `[window] option_as_alt = "Both"`; kitty: `macos_option_as_alt yes`. The
    profiles thegn ships now set this, so `tg --standalone` and the generated
    `thegn.app` are fine — the setting is for the terminal you launch thegn in.
    See the in-app help ([`docs/help/terminal-compatibility.md`](docs/help/terminal-compatibility.md)).
    What is still missing: **the macOS CI job has still never completed a run.**
    It was hard-disabled because it OOM-killed building `openspec`; that is fixed
    (it runs on a lean `devShells.ci` — toolchain + just + nextest, no
    openspec/muse), but it stays off by default on the same `extras` gate as
    windows/e2e while remote CI is paused, so nothing is enforced until someone
    runs `gh workflow run ci.yml --ref main -f extras=true`. Binaries ship from the next tag; and the
    interactive half of the
    [on-device checklist](CONTRIBUTING.md#on-device-checklist) — resize by hand,
    pane restore across a real quit, opening a PR in a browser, the media badge,
    notifications and the chime firing visibly — still needs a human at a
    terminal. `just check-cross` covers every crate that builds without a darwin
    cross C toolchain, but `thegn-core`/`-svc`/`-host` still can't be checked
    from Linux, because their build scripts (`ring`, bundled sqlite) need a real
    darwin one.
- Cloud execution providers, remote worktrees over SSH, the Observe dashboards,
  the placement engine, and non-GitHub issue trackers are **dev-channel only** in
  this release (`THEGN_CHANNEL=dev`).

## Distribution

- Prebuilt binaries cover **x86_64 Linux (gnu + musl)** and
  **aarch64-apple-darwin**. The darwin leg is back in the matrix now that the
  target builds and the `macos` CI job is runnable again (opt-in per run with
  `extras: true`, on a lean dev shell so it no longer OOMs building openspec). windows-msvc is still
  out — that job has never executed. Nix
  (`nix profile install github:blakeashleyjr/thegn`) and `./install.sh` work on
  every supported platform.
- **macOS release archives are unsigned and unnotarized**, deliberately: see the
  decision in [`RELEASING.md`](RELEASING.md). Homebrew, Nix and the locally
  generated `thegn.app` are all unaffected (none of them quarantine). A tarball
  downloaded through a **browser** is quarantined and needs
  `xattr -dr com.apple.quarantine ./thegn` before its first launch.
- The Homebrew formula (`packaging/homebrew/thegn.rb`) is ready but needs a
  tagged release to point at: its `sha256` comes from the release's
  `*-aarch64-apple-darwin.sha256` asset, and the `blakeashleyjr/homebrew-tap`
  repo does not exist yet (RELEASING.md has the exact shape). Note that modern
  Homebrew refuses to install a formula from a file path, so trying it before
  the tap exists means `brew tap-new` + copying the formula in — the RELEASING
  steps spell that out.
- `crates.io` / `cargo binstall` need the workspace crates made publishable
  first — see [`RELEASING.md`](RELEASING.md).
