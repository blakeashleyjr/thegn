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
  Mac and its CI job is re-enabled (opt-in) but has still never completed a run.
  - **Windows** got its first real CI runs in `0.1.0-alpha.1`, and has now had
    a full on-machine pass on real hardware — see
    [`docs/windows-native-audit.md`](docs/windows-native-audit.md). Until
    recently the repo could not even be _cloned_ on Windows:
    `crates/thegn-core/src/store/aux.rs` used a reserved DOS device name, so
    git refused it with `invalid path`. With that renamed,
    `cargo check --workspace` passes on msvc warning-free, the **release build
    completes**, the named-pipe daemon IPC tests pass (a bind through the
    pipe-name teardown window was mistaking its own predecessor for a live
    daemon), and the compositor **renders and runs** in Windows Terminal with
    ConPTY panes. Container sandboxing works too, via Podman/Docker Desktop —
    mount destinations are mapped into the WSL2 machine's `/mnt/<drive>/…` tree
    and linked-worktree git metadata is shimmed so `git` resolves inside the
    container. There is also a **native** backend that needs no WSL2:
    `appcontainer` runs the pane under a per-worktree AppContainer SID with
    deny-by-default filesystem access and capability-gated network, plus a Job
    Object for the pid/memory caps. It is an OS access-control boundary, not a
    kernel one — reported as its own `IsolationClass::OsAccessControl`, roughly
    `bwrap`'s feature level — and a pane whose toolchain the profile cannot
    reach degrades to `host` with the exact `icacls` command rather than
    starting broken. `jobobject` alone is **not** offered as isolation: it
    probes `Absent`, because it never applied containment to a pane and saying
    otherwise was a false security claim.

    **ConPTY teardown: settled — an ordinary pane close leaks nothing.**
    `crates/thegn-host/examples/conpty_teardown_windows.rs` measures it at **0
    threads, 0 handles and 0 orphaned `OpenConsole.exe` processes per close**,
    over 10 panes in each of two arms (child exits on its own; child terminated
    while alive), repeated.

    Counting the console host is the part that took two attempts. A
    pseudoconsole is hosted by a *separate process*, so an in-process-only
    thread/handle count reports a clean zero while whole processes leak. An
    earlier version of this note cited exactly that zero as proof teardown was
    clean, while a long session on this box accumulated **16 orphaned
    `OpenConsole.exe` processes** — every parent dead, aged 8–11 hours, each
    spinning **0.6–0.9 of a core**, about 10 of 12 cores.

    Those orphans were real but not thegn's teardown: every one followed a
    **force-kill** of the client, and `TerminateProcess` bypasses
    `ClosePseudoConsole`. That is worth knowing operationally — killing thegn,
    or killing a test run, leaves a spinning console host behind per pane, and
    it will not clean itself up. If a Windows box is inexplicably busy, check
    with `Get-CimInstance Win32_Process -Filter "Name='OpenConsole.exe'"` and
    compare each `ParentProcessId` against the live process list; anything whose
    parent is gone is an orphan and safe to stop.

    **Ctrl-C does not interrupt a pane's child on Windows.** Press it and the
    program keeps running. This is a real gap, not a rough edge, and it is the
    main reason the interactive checklist is still open.

    The control rules out the obvious explanations: plain typing reaches the
    child perfectly well (a `Read-Host` echoes it straight back), so the write
    path, ConPTY's input handling and the child's stdin all work. Ctrl-C
    specifically produces no interrupt — not as the raw `0x03` thegn's key
    encoder emits, not as the win32-input-mode key record ConPTY's own
    `ESC[?9001h` handshake asks for, not as both, against either PowerShell or
    `cmd`. No `CTRL_C_EVENT` is reaching the child, so the encoding is not the
    problem and changing it is not the fix. (`portable-pty` does not pass
    `CREATE_NEW_PROCESS_GROUP`, which would disable Ctrl-C on its own, so that
    is ruled out as well.) Unix is unaffected — the tty line discipline turns
    the same byte into SIGINT with no help from thegn.

    Reproduce with `cargo run -p thegn-host --example ctrl_c_windows`, or
    `cargo nextest run -E 'test(ctrl_c_interrupts_the_pane_child)'
    --run-ignored all`. That test is `#[ignore]`d rather than deleted precisely
    so the reproduction stays in the tree; un-ignore it when the gap closes.

    **Headless daemon sessions used to stall forever, and that was a real bug.**
    ConPTY opens every session with a DSR cursor query (`ESC[6n`) and withholds
    the child until a terminal answers. The compositor answers for an attached
    pane, so this never showed up interactively — but a daemon session with no
    client attached (the headless-agent case) had nobody to answer, and the
    agent never ran a single command. The daemon now answers for itself, which
    is correct on the merits: it owns the authoritative emulator and the client
    is only a viewer. Five `daemon::session` tests went from timing out at five
    minutes each to passing.

    **Git for Windows' `sh.exe` hangs on ~1.7% of spawns, and that is not
    thegn's bug.** Measured directly: `sh -c 'sleep 0.2; exit 0'` spawned 120
    times from a native process — **no PTY, no thegn** — and two never exited.
    The daemon test set alone spawns about ten shells per run, which is why
    roughly one full-workspace run in two used to carry a failure while every
    one of those tests passed in isolation. MSYS `fork()` is emulated rather
    than copy-on-write, and it loses address-space races; the same fault is
    already documented in `.config/nextest.toml` for `git rebase -i`.

    Two responses, both in that config. The concurrency caps were extended to
    the MSYS-spawning tests that were missing from them (`daemon::session`,
    `plugin::session`, the `plugin_example` binary — `daemon::session` had only
    the looser ConPTY cap, and first-match-wins gave it that one), which cut the
    daemon set from ~30 s to ~5 s. And that group now sets `retries = 2`,
    which is appropriate here specifically because the hazard is external and
    quantified: two retries take 1.7% per spawn to ~0.005%. It is scoped to that
    group alone — the rest of the suite has no retries and should not get any.
    nextest still *reports* a retried test as `flaky`, so this stays visible
    rather than silently passing.

    The cost, so it is not a surprise: a hung shell burns the full five-minute
    `slow-timeout` before its retry starts, so a run that hits one takes about
    five extra minutes. The tighter per-test timeout that would cut this cannot
    be scoped to these tests alone without also constraining the svc host probes
    in the same group, which legitimately budget up to two minutes.

    **Run the suite with `cargo nextest run` (i.e. `just test`), never bare
    `cargo test`.** The nextest profile bounds every test at five minutes;
    `cargo test` has no such bound, and a single wedged Windows test took a
    workspace run from ~5 minutes to a **6-hour** hang that looked like a slow
    build. The same run under nextest named the six offenders in five minutes
    each.

    Separately, and still true: the reap test's original hang was its own doing.
    It never drained its channel, so nothing answered the `ESC[6n` ConPTY opens
    every session with; the child stalls until something replies, and closing a
    stalled pseudoconsole does not complete. thegn always answers (the
    interactive loop, and `drain_until_exit`); only that one test did not, and it
    now does, passing on Windows in ~1s.

    Two things the harness pinned down that were not previously written anywhere:
    `PtyHandle`'s field order is load-bearing — dropping the ConPTY *input*
    before the master deadlocks on a terminated child, so `master` must stay
    declared before `writer` — and answering the DSR is mandatory on Windows for
    any code path that drives a pane, not merely polite.

    It stays unsupported on one substantive count — the Ctrl-C gap above —
    rather than on a checklist that is merely unrun. Resize storms **are** now
    proven: 300 resizes against a printing child (so ConPTY's asynchronous
    reflow genuinely races the reader thread) leave the pane un-panicked,
    un-wedged, correctly sized and its child alive. What remains unverified is
    visual: whether the frame tears mid-drag, and whether glyphs and the first
    frame look right — judgements a pipe cannot make. Windows requires a modern
    terminal (any VT-capable console; legacy non-VT conhost is refused), and
    publishes no binaries. Install with `.\install.ps1`.

    The idle-CPU figure previously cited here (~0.09 cores, "~1.6x Linux") was
    **warm-up, not steady state**, and is withdrawn.
    `crates/thegn-host/examples/idle_cpu_windows.rs` — a Windows port of the
    Linux `cpu-sample.sh`, which is `/proc`-only — measures the same 14-worktree
    fixture at **0.175 cores after a 2.5 s settle and 0.03–0.05 after 40 s**. The
    cost is startup catching up (`cpu_hydrate_ms` falls 130 → 26 per 2 s window,
    `cpu_diff_ms` to zero), not a steady-state spin: `idle_ratio` is 0.99
    throughout and the render path measures 2 ms p50 with no slow-frame warnings.
    Note the Linux number was taken with the **same 2.5 s settle**, so the
    original comparison was warm-up against warm-up.

    That re-measurement has now been done, and it settles the question the other
    way: with a **45 s settle**, Windows idles at **0.0367 cores** on the
    14-worktree fixture against Linux's ~0.056 — *below* Linux, not 1.6× above
    it. `idle_ratio` 0.955, `renders_per_s` 0.0 with every wake resolving to a
    render **skip**, `render_busy_ratio` 0.004, and the hot source is the 2 s
    refresh ticker, which is the wake the design intends. Idle CPU is not a
    Windows problem. One caveat for anyone repeating it: measure only through
    `idle_cpu_windows`, which uses a real ConPTY — sampling a run whose
    stdout/stderr are redirected to files gives ~0.25 cores, because stdin is
    then not a console and the loop never blocks in `poll_input(None)`. That
    figure measures the redirect.
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
    A second on-device pass fixed: the **`apple` sandbox backend, which could
    never start a container** (`container image exists` / `container pull` are
    both exit-64 non-commands — Apple puts image verbs under the `image` noun —
    and `--security-opt`/`--pids-limit` are flags its `run` rejects outright);
    a chain resolver that **re-walked the whole chain once per candidate**, so a
    Mac with podman/docker/`container` installed-but-dormant printed six
    identical host-fallback warnings per pane spawn; a **diff-watcher filter
    that panicked** on any worktree under a symlinked prefix (`/tmp`,
    `/var/folders`, `~/code → /Volumes/…`), because FSEvents delivers
    canonicalized paths and `matched_path_or_any_parents` asserts its argument
    is under the matcher root — taking the watcher thread down with it; a font
    picker that **scanned only the top level** of the macOS font directories and
    so found neither `/System/Library/Fonts/Supplemental` (290 faces) nor
    nix-darwin's `Nix Fonts/<hash>-<pkg>/share/fonts/…`; the bundled Alacritty
    profile **silently degrading itself** to 256-color/no-undercurl by forcing
    `TERM` without also identifying the emulator; a charge-capped Mac reporting
    **"not on AC"** while plugged in; and a host `timeout` call that failed with
    a bare `ENOENT` on any Mac without GNU coreutils.
    Added: thread QoS (`platform::qos`), so off-loop workers are eligible for
    the efficiency cores instead of competing with the render loop; `LC_TERMINAL`
    detection, the one terminal-identity signal that survives ssh; a macOS
    section in `thegn doctor`; and a darwin arm for the idle-CPU perf harness,
    which had never run on this platform at all.
    **Set your terminal to send Alt for Option.** macOS composes characters
    with Option by default, so thegn's Alt-based chords (`Alt-w`, `Alt-o`,
    `Alt-s`, `Alt-.`, every `Ctrl-Alt` toggle) type `∑`-style glyphs instead
    and read as dead keys. Terminal.app: Settings → Profiles → Keyboard → "Use
    Option as Meta key"; Ghostty: `macos-option-as-alt = true`; Alacritty:
    `[window] option_as_alt = "Both"`; kitty: `macos_option_as_alt yes`.
    `thegn doctor` names the setting for the terminal you are in. The
    profiles thegn ships now set this, so `tg --standalone` and the generated
    `thegn.app` are fine — the setting is for the terminal you launch thegn in.
    See the in-app help ([`docs/help/terminal-compatibility.md`](docs/help/terminal-compatibility.md)).
    What is still missing: **the macOS CI job has still never completed a run.**
    It was hard-disabled because it OOM-killed building `openspec`; that is fixed
    (it runs on a lean `devShells.ci` — toolchain + just + nextest, no
    openspec/muse), but it stays off by default on the same `extras` gate as
    windows/e2e while remote CI is paused, so nothing is enforced until someone
    runs `gh workflow run ci.yml --ref main -f extras=true`. **`just ci` also
    does not pass on a Mac**: it includes `e2e`, and all 45 committed muse
    baselines are `__linux` while `--ci` treats a missing baseline as a failure —
    so darwin baselines need recording (`just e2e-update`) or the leg needs to
    self-skip like `sandbox-e2e` does. The `apple` backend now emits correct
    argv, but has no automated coverage beyond unit tests — it still wants an
    on-device run with `container system start`. There are no CPU caps on macOS
    at all — not because `nice` is missing but because the wrapper that would
    apply it only ever wraps `bwrap` (Linux-only) or a local `Backend::None`
    (which never produces a spec), so no macOS pane is ever wrapped; `doctor`
    now reports that rather than naming a mechanism that cannot fire. Thermal
    sensors and the allocator trim both work now (IOHID and
    `malloc_zone_pressure_relief` respectively).
    Binaries ship from the next tag; and the
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
