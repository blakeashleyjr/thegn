# Contributing to thegn

Welcome! This gets you from clone to a running dev build in a few minutes.
For the product tour see [`README.md`](README.md); for architecture and
invariants see [the development reference](DEVELOPMENT.md).

## Start here

- Linux x86_64 is supported; macOS is best-effort; Windows is deferred (see the
  contributor notes below).
- Questions and bug reports go through [SUPPORT.md](SUPPORT.md) and the issue
  templates. Security reports go through [SECURITY.md](SECURITY.md), never a
  public issue.
- For a first change, read [AGENTS.md](AGENTS.md), enter the dev shell with
  `nix develop`, and iterate with `just quick <crate>` and targeted tests such
  as `cargo nextest run -p <crate> <filter>`. The [Dev loop](#dev-loop)
  explains when to leave the heavy gates until later: `just test` and
  `just smoke` are pre-push; `just ci` (including lint) is run before opening
  a PR.
- Use conventional-commit titles (`feat(scope):`, `fix(scope):`). The
  [PR template](.github/pull_request_template.md) has the checklist: new
  behavior needs tests, and changes to actions, keybinds, zones, or panel
  sections need the relevant `docs/help/` page updated in the same change.
- Contributions are dual-licensed MIT OR Apache-2.0 (see [License](#license)).

## Prerequisites

**The recommended path on every platform is [Nix](https://nixos.org/download/)
with flakes** — `nix develop` provides the exact toolchain, linters, and
runtime tools the project is built and CI-tested with. On macOS the
[Determinate installer](https://install.determinate.systems/) is the least
fuss (flakes on by default).

|           | Nix path (recommended)                         | Bare path                                                                                |
| --------- | ---------------------------------------------- | ---------------------------------------------------------------------------------------- |
| **macOS** | Xcode CLT (`xcode-select --install`), then Nix | Xcode CLT + [rustup](https://rustup.rs) (stable ≥ 1.96) + `brew install pkg-config just` |
| **Linux** | Nix                                            | rustup (stable ≥ 1.96) + `just` + a C toolchain                                          |

On macOS you can skip the table entirely and run `./setup-macos.sh` — it
checks each prerequisite and offers to install what's missing (nothing is
installed without asking).

> Intel Macs: the pinned nixpkgs (unstable) has dropped `x86_64-darwin`, so
> the Nix path is Apple-silicon only — use the bare rustup path instead.

Optional tools thegn shells out to when present: `gh` (PR/CI panels), `ssh`,
`lazygit`, `fzf`, `gum`, `delta`, `yazi` (file drawer). The Nix shell has all
of them; on a bare Mac: `brew install gh lazygit fzf gum git-delta yazi`.

## Quick start

```sh
git clone https://github.com/blakeashleyjr/thegn && cd thegn
nix develop        # or: direnv allow   (auto-enters the shell per-cd)
just build         # one-time debug build of the workspace
just start name=dev  # run the compositor with isolated state (safe to poke)
```

No Nix? The one-time `cargo build --workspace` and `cargo run -p thegn-host`
commands work too —
you just supply the tools above yourself. To install a real binary for daily
use, `./install.sh` (see the README's Install section).

If something is off, `just doctor` diagnoses the dev environment.

## Dev loop

Develop thegn in the dev instance (`just live` for a release build, `just devel` for a debug build), not in the stable instance you use for other projects. It has its own config, state and `THEGN_DIR`; seeding `~/.config/thegn-dev` is automatic on the first launch. See `thegn help two-instances` (`docs/help/two-instances.md`).

**Enter the dev shell once, right after cloning** (`nix develop`, or `direnv
allow` to auto-enter per-cd). The pre-commit / pre-push git hooks are generated
and installed by the dev shell — a bare clone that never enters it silently has
no local gates, so formatting and the heavy gates won't run until you push into
CI. The dev shell does not install a `post-checkout` hook: checkout and
worktree creation never execute scripts from the selected branch. Linked
worktrees may not have the generated, gitignored prek config; review the
worktree first and seed/configure it explicitly if you want optional local
prek hooks there. These developer hooks are convenience gates only; CI and
explicit merge gates remain authoritative.

If upgrading a checkout that already has the old Thegn `post-checkout` hook,
retire it explicitly after reviewing the path and its digest. The shell reports
the exact known legacy regular file in the canonical repository's ordinary
`.git/hooks` directory, but never deletes it. Do not inspect or remove a
custom, global, shared, symlink, directory, FIFO, or foreign hook automatically.
An old installed copy remains unsafe until a user explicitly removes it.

The heavy gates are full-workspace compiles — don't run them per-edit:

- **While iterating:** `just quick [crate]` — clippy on lib/bin code only,
  seconds not minutes; run a touched test with
  `cargo nextest run -p <crate> <filter>`.
- **Before pushing:** `just test` and `just smoke` (pre-push hooks run these).
- **Once, before opening a PR:** `just ci` — fmt-check + lint + build + test +
  coverage + smoke + nix-build. This is the merge gate; save it for the end.

> There is one dev shell — the flake's — and `.envrc` is a plain `use flake`, so
> `nix develop` and `direnv allow` land you in the same place CI runs every gate
> (`nix develop --command just <gate>`). If a gate behaves differently for you,
> check you aren't nested inside another Nix shell that shadowed the toolchain.

**Anything that renders** (chrome, panel, sidebar, keymap, input) also has an
end-to-end gate: `just e2e` drives the built binary in a real PTY with
[muse](https://github.com/blakeashleyjr/muse) through `test/muse/specs/` and
diffs text/styled snapshots against `test/muse/snapshots/` (see
[`docs/coverage.md`](docs/coverage.md#end-to-end-just-e2e)). After an intentional
UI change, `just e2e-update` re-records the baselines — review that diff like
code. To look at a change by hand, `muse session open -- target/debug/thegn`
and `snap`/`send`/`wait` it. [`docs/testing-with-muse.md`](docs/testing-with-muse.md)
is the full guide (environment, spec anatomy, the traps, artifacts, baselines,
macOS); the `tui-check` skill under `extensions/skills/` is the agent recipe.

### Committed Claude Code hooks

This repository commits `.claude/settings.json` `PreToolUse` hooks for
`test/heavy-guard.sh` and `test/heredoc-guard.py` as the dev-loop policy.
Claude Code's directory trust is a one-time decision: after a branch switch,
the next Bash call runs the hook files in that checkout before any permission
prompt. The accepted risk is that those files can change with the branch and
run with the interactive Claude process's authority. Review `.claude/` and
`test/*-guard*` when checking out a branch from someone else. Inspect effective
hooks with `/hooks` in Claude Code, or start it with
`claude --setting-sources user` to skip project settings. Thegn's autonomous
launches use that setting source. See the
[configuration help](docs/help/configuration.md) for the settings and
permissions that apply to autonomous launches.

### Cross-platform path semantics

Core path tests can exercise Linux, macOS, Windows, and fallback conventions
on one host through `util::{home_for, xdg_config_home_for, xdg_state_home_for}`.
Use `testenv::state_env_for` when checking one OS's environment mapping, and
`testenv::IsolatedState` for tests that mutate process environment or write
state; it sets both Unix/XDG and Windows variables and restores them on drop.
Test builds refuse to open state under `$HOME/.local/state` or the
`XDG_STATE_HOME` inherited before the test, with a `THE-744` panic.
`IsolatedState::new()` puts XDG state at `<root>/state`;
`with_real_home_layout()` is only for tests of that guard. Host tests use
`crate::testenv::IsolatedState`.
Host tests needing extra non-state variables use
`IsolatedState::with_env(&[...])`; state keys are refused. `config_home()` gives
the isolated config directory.
`testenv::symlinked_tempdir` supplies a link spelling and its canonical path
for tests where the OS resolves paths through a symlink. The per-crate
`state-env-*-ratchet.txt` allowlists track existing tests that still set these
variables directly; the ratchet is shrink-only as tests migrate to the helper.

`just` with no arguments lists every recipe. Commits follow conventional
style (`feat(scope):`, `fix(scope):`); branch off `main`.

Roadmap and specs: `tasks.md` is the roadmap index; behavior specs live in
`openspec/specs/` and in-flight changes in `openspec/changes/` (the
`openspec` CLI is in the dev shell; agent slash-commands regenerate with
`just openspec-setup`). Every config key is documented in
[`config/config.toml.example`](config/config.toml.example).

### Early integration of dependent branches

When parallel changes share a schema, generated surface, or caller/callee contract, the integrator checks the first combined checkout that contains both tips. Run `just integration-check <crate> '<nextest expression>' <first-tip> <second-tip>` for the affected behavior. Record the combined SHA and result in the existing issue handoff. Every remaining lane can use that evidence while the normal fold and PR gates run at their usual boundaries.

## Programme conventions (sandbox + remote, 2026-11)

The sandbox + remote programme is many parallel lanes landing in waves (W0
contracts, W1 truth fixes, W2 seam freeze, and so on). The rules below keep
those lanes from defining the same type, flag, directory or convention twice.
The programme record is
[`docs/superpowers/plans/2026-11-17-sandbox-remote-programme.md`](docs/superpowers/plans/2026-11-17-sandbox-remote-programme.md),
which also holds the hardware and accounts checklist. THE-791 (owner decision
2026-10-13) is its parent acceptance record.

- **Schema (B2).** Items write `migrate_vNEXT` and `verify_vNEXT_schema`;
  they never write a literal `SCHEMA_VERSION`. The integrator assigns the
  number when the batch folds. At most one bump per wave: if a wave's migration
  would land before its first consumer, the consumer item creates it and the
  next item of the same wave appends to the same `migrate_vN` function before
  the fold. Each migration item carries the daemon-restart release note.
- **openspec (B6).** Archive a change at the end of the wave that completes it,
  before the next change that MODIFIES the same requirement is opened. Rebase
  MODIFIED blocks on the current spec at archive time. Keep one change per
  capability requirement in flight at a time. A MODIFIED or REMOVED delta cannot
  target another change that is still unarchived; amend that change in place.
- **Freeze (B7, lead ruling L3).** The frozen files are
  `crates/thegn-core/src/sandbox*.rs`, `capabilities.rs`, `sandbox_matrix.rs`,
  `sandbox_backend.rs`, `sandbox_truth.rs` and `sandbox_support.rs`. The window
  is active only between WS02-P1-02b and WS02-P2-08. Inside the window no lane
  outside WS02 has a diff that touches a path matching
  `crates/thegn-core/src/sandbox*.rs`, whether it edits an existing file or adds
  a new one. New modules created during W2 by other lanes take names outside the
  glob (`confine_plan.rs`, `seatbelt_*.rs`, `win_sandbox*.rs`, `launch_kind.rs`;
  the THE-791 modules `launch_class.rs`, `admission.rs` and `workspace_mode.rs`
  are also outside it). The check is mechanical:
  `git diff --name-only <base>.. -- 'crates/thegn-core/src/sandbox*.rs'` is empty
  for every non-WS02 lane merged inside the window. Tickets cite symbols, not
  line numbers, for the frozen files; moves carry `git diff --color-moved`
  evidence.
- **Features (B3).** `Feature` variants are added only by WS00-P1-01. Items use
  their variant and never add one, and no item states an `ALL` length.
- **Recipes (B4).** Recipes live only under
  `config/recipes/{sandbox,provider,connector}/`, never a top-level `recipes/`.
  See [`config/recipes/README.md`](config/recipes/README.md).
- **CI (B9).** There is no new pull-request job for sandbox tests. Suites that
  need user namespaces, bwrap, systemd user units or podman go into the
  `test/userns-e2e.d/` registry (run by `just sandbox-userns-e2e`, a step of the
  dispatch/nightly `security-e2e` job) or into `sandbox-e2e-full`. Wall-clock
  numbers are advisory and never fail a gate. New binaries and crates keep
  `just check-cross` green and join the per-crate ratchet scans (platform-cfg,
  spawn-ownership, thread-name).
- **Packaging (B45).** Every new binary names its artifact owner item and the
  channels it ships in (tarball, flake, Windows zip, remote).
- **Spikes and on-device checks (B8).** No item whose design rests on an
  unverified external fact merges before its spike's decision record. A spike
  is a `P0`-phase item or carries an `s` suffix (for example `WS05-P0-06`,
  `WS04-P3-11s`); the dependent item lists it as a dependency. A check that needs a device (a Mac, a Windows VM, a
  vendor account) is recorded in the PR with the host and date.
- **tasks.md linkage.** `tasks.md` stays the roadmap index. The programme has
  one group row pointing at the programme record; each openspec change cites
  the group letter and item number in its proposal's Impact section, and each
  work item's id (for example `WS01-P1-02`) appears as a task in its change.
- **Config and catalog checklist (B10).** Every item that adds a config key,
  action or control op ticks these in its Ratchets line:

  ```text
  config-reference / help ratchet
  config_example test
  test/env-overlay-ratchet.txt  (pin with a reason or a knob; secrets pinned OUT of the overlay)
  hm_module_drift disposition   (rendered or OMITTED)
  marked_definition_count_is_pinned  (each new config_enum!)
  capability::CATALOG row + required_scope + cli_control_caps()  (every control/daemon op)
  real Surface values: Http | Grpc | Cli | Mcp | Plugin
  help claim and prose in the same lane as a new ACTION_SPECS id
  ```

To add a suite to the userns registry, drop an executable
`test/userns-e2e.d/NN-name.sh` that exits 0 (pass), 77 (skip, after printing the
reason) or anything else (fail). With `THEGN_REQUIRE_SECURITY_TESTS=1` a skip
fails the run.

## macOS notes

- **Any terminal works.** `thegn` / `tg-tui` run in whatever terminal you
  use (Ghostty, iTerm2, Terminal.app, …). Alacritty is only needed for the
  `tg` dedicated-window launcher, and is optional.
- **Sandboxing degrades gracefully.** The worktree-sandbox probe order is
  `podman → docker → apple → bwrap → host`. `apple` is Apple's `container`
  CLI (each container gets its own Linux VM — the strongest isolation any
  backend offers) and is probed only on macOS; `bwrap`/`systemd-run` are
  Linux-only. With no container runtime installed everything still works —
  panes just run directly on the host.
- **Cross-checks are partial — mind the gap.** `just check-cross` type-checks
  every crate that builds without a darwin cross C toolchain
  (`thegn-metrics`, `thegn-media`, `thegn-proc`, `tg-kit`, `gtui-core`, `gtui-render`,
  `gtui-app`) against `aarch64-apple-darwin`. It stops at `thegn-core`,
  `thegn-svc`, `thegn-host` and `gtui-query`, whose build scripts (`ring`,
  bundled sqlite, libgit2) compile C for the target — those **cannot** be
  checked from Linux, so a darwin break in the host crate is invisible to the
  routine gate. Cover it with the on-device checklist below, or the full macOS
  build+test job (`macos-15`), which is opt-in because GitHub bills those
  minutes at 10x. It runs only on a manual dispatch with `extras`:
  `gh workflow run ci.yml --ref <branch> -f extras=true`. (There is **no**
  `[ci-macos]` commit-message trigger — nor `[ci-windows]` or `[ci-e2e]`; the
  extras jobs are gated on `if: ${{ inputs.extras }}`. Pull requests run the
  lean CI set; pushes do not trigger remote CI.)
- **State paths** follow XDG conventions (`~/.config/thegn`,
  `~/.local/state/thegn`) on macOS too; set `XDG_CONFIG_HOME`/
  `XDG_STATE_HOME` if you prefer `~/Library`. Keep `XDG_STATE_HOME` shortish:
  the pane daemon's socket lives under it, and macOS caps a unix socket path at
  104 bytes (Linux allows 108).
- `just start-term` needs Ghostty on PATH (it opens a dedicated window; plain
  `just start` uses the current terminal). The font picker prefers `fc-list`
  and falls back to scanning `~/Library/Fonts`, `/Library/Fonts` and
  `/System/Library/Fonts` **recursively** when fontconfig isn't installed —
  macOS resolves those directories recursively too, and a flat scan missed both
  `/System/Library/Fonts/Supplemental` and nix-darwin's
  `/Library/Fonts/Nix Fonts/<hash>-<pkg>/share/fonts/…`.
- **`thegn doctor` has a macOS section**: the Option-as-Alt setting for your
  terminal, `RLIMIT_NOFILE` against `kern.maxfilesperproc`, whether `$TMPDIR`
  can shorten the pane-daemon socket, and which of `osascript` / `afplay` /
  `pbcopy` / `fc-list` / `mediaremote-adapter` are actually present. Each of
  those degrades silently at runtime, so check it before reporting a
  "missing feature".
- **`just ci` does NOT pass on a Mac yet.** Two legs self-skip cleanly and say
  so — the `check-cross` windows-gnu leg (the mingw cross-cc is gated to Linux)
  and the podman-backed `sandbox-e2e-*` tiers — but `e2e` does not: all 45
  committed muse baselines are `__linux`, and `--ci` makes a missing baseline a
  hard failure. Until darwin baselines are recorded (`just e2e-update` on a
  Mac), run the other gates individually rather than the `ci` aggregate.

### On-device checklist

Nothing above proves the compositor actually _runs_. Work through this on a real
Mac when touching anything platform-sensitive; it covers what neither
`check-cross` nor a headless CI job can:

1. `nix develop` — the dev shell builds (`openspec` is the heaviest derivation
   in it; it was OOM-killed on the 7 GB CI runner before its memory was capped).
2. Once, for final on-device validation: `just build && just test && just smoke && just lint`.
3. `just start name=dev` in a real terminal: panes spawn, render and resize;
   sidebar/statusbar/pin strip draw; `thegn doctor` reports sane termcaps.
4. Detach and re-attach a session — the pane daemon's socket resolves under
   `~/.local/state/thegn/run/` (macOS never sets `XDG_RUNTIME_DIR`).
5. Activity dots light for a busy worktree pane (the scanner is `sysinfo`-based
   off Linux, and macOS only exposes another process's cwd to the same user).
6. Pane restore: run something long-lived, quit, relaunch — the cwd and the
   relaunch hint come back.
7. Open a PR/issue from the panel — `open` fires; `$BROWSER` and
   `[forward] browser` still take precedence.
8. With Apple's `container` installed, `thegn doctor` shows `apple` present and
   `auto` picks it; without it, `auto` falls through to `host`.
9. The media badge, `osascript` notifications, the `afplay` chime, `pbcopy`
   copy-mode yank, and a Keychain secret round-trip all fire.

## Windows (native) notes

Native Windows is a build target **under development**, not a supported
platform, and no Windows binaries ship in the current public alpha. Current
repository state, from the first opt-in native run on 2026-11-23
([run 45091904082](https://github.com/blakeashleyjr/thegn/actions/runs/45091904082)):

- The MSVC workspace check (`cargo check --workspace --locked`) **passes** on
  windows-latest. The Linux-side `x86_64-pc-windows-gnu` cross-check still
  covers cfg/compile regressions ahead of that run.
- Native named-pipe daemon IPC tests exist and are the first gate after the
  check; that run never ran them, because the `thegn-svc` lib test target does
  not compile for Windows: `crates/thegn-svc/src/calendar/command_tests.rs:110`
  and `crates/thegn-svc/src/inbox/command_tests.rs:324` call `libc::kill`
  without a `cfg(unix)` guard, and `libc` is a unix-only dependency there.
  `cargo check --workspace` does not build test targets, which is why the step
  before it passed. The kernel tests after it (Job Objects, the `thegn.exe`
  build and the artifact smoke) have not run yet. They used to fail on
  `pipe_bind_is_the_lock_and_round_trips`: Windows keeps a pipe _name_ reserved
  for a few milliseconds after the last handle of an instance that carried a
  connection closes, and `bind_exclusive` read that window as a rival daemon.
  It now retries briefly (`crates/thegn-svc/src/ipc.rs`); the opt-in run has
  not been green since.
- No `thegn.exe` artifact exists yet, because the upload step follows the
  tests, and nobody has run thegn interactively on Windows. Those are remaining
  gaps before the platform can be called supported — see
  `openspec/changes/add-windows-compositor-validation`.
- The MSVC and artifact-smoke jobs are opt-in: manually dispatch `ci.yml` with
  `extras: true`.

No WSL is required. The dev experience differs from unix — nix and the justfile
don't apply:

- **Toolchain:** [rustup](https://rustup.rs) with the default
  `x86_64-pc-windows-msvc` toolchain + the Visual Studio Build Tools
  ("Desktop development with C++" — the C deps: bundled sqlite, libgit2).
  Then plain cargo: `cargo build`, `cargo run`, `cargo test`.
- **Terminal:** run thegn inside [Windows Terminal](https://aka.ms/terminal)
  (or another modern VT emulator — WezTerm, Alacritty). Legacy conhost.exe is
  refused at startup with a pointer here.
- **Before trusting the compositor on a new machine**, run the event-model
  spike: `cargo run -p thegn-host --example waker_spike` — expect one tick per
  second at ~0% CPU and instant key echo (see the file header for pass/fail).
- **Shells:** panes default to `pwsh` → `powershell` → `%COMSPEC%`; pins/tool
  commands run through the right dialect automatically
  (`thegn_core::shellinv`).
- **State paths:** `%APPDATA%\thegn` (config) and `%LOCALAPPDATA%\thegn`
  (state/DB/logs).
- **What's intentionally absent on Windows:** container sandboxing (Linux
  containers in a VM can't bind-mount the worktree at its real path — use
  WSL2 if you want sandboxed panes; native panes run on the host, scoped by
  kill-on-close Job Objects), the SIGUSR2 flamegraph profiler, and the
  merge-queue headless agent (POSIX quoting).
- **CI:** `just check-cross` includes the Linux-side
  `x86_64-pc-windows-gnu` workspace check. Pull requests run the lean CI set
  (fmt, clippy, nextest and smoke); heavy jobs run nightly or on dispatch. The
  full `windows-latest` MSVC job (check + IPC/Job-Object kernel tests, release
  build, and downloaded-artifact smoke) runs only when `ci.yml` is dispatched
  with `extras: true`.

### Running the tests under Wine (from Linux)

`just test-windows-wine [nextest args]` cross-compiles the test binaries for
`x86_64-pc-windows-gnu` and **runs** them under Wine, so a Windows regression
shows up as a failing test on a Linux box instead of only as a compile check.
With no arguments it runs the whole workspace; pass `-p <crate>` and/or a
nextest filter to narrow it (`just test-windows-wine -p thegn-proc fallback`).
It is x86_64 Linux only, a heavy gate (a separate full compile; the agent
heavy-guard treats it like `just test`), and not part of `just ci`.

How it is wired:

- **Wine** is the flake package `.#wine` (`wineWow64Packages.minimal`, ~790 MB
  closure), built on first use by the recipe. It is deliberately not in the dev
  shell, so nobody else downloads it.
- **`test/wine-runner.sh`** is the cargo target runner. It uses a private,
  headless prefix under `$XDG_CACHE_HOME/thegn/` (created once, ~30 s), with
  Mono/Gecko disabled and no display, and exits with the test's status. It
  exports `THEGN_UNDER_WINE=1`.
- **`test/mingw-link.sh`** wraps the mingw linker for this recipe only: the
  cdylib that `iroh-relay` declares exceeds the 65,535-entry PE export table
  in a debug build, so that one unused DLL is replaced by an empty stub.
- **Skips** live in the `wine` profile in `.config/nextest.toml`, each with
  its cause. The list is currently empty and only covers Wine artefacts. A test
  that fails because thegn is wrong on Windows, or because the test assumes
  Linux, is a bug to fix, not a skip.

What it covers: pure logic, the `cfg(windows)` code paths in the leaf crates
(`thegn-proc`, `thegn-metrics`, `thegn-media`, `tg-kit`, `gtui-*`), process
spawning through Win32 `CreateProcess`, and Windows path handling. What it does
NOT cover, so a green run here proves nothing about these: ConPTY behaviour
(Wine's is incomplete), Job Object kill-on-close and nested jobs, named-pipe
reservation timing (the `bind_exclusive` race above), NTFS ACLs, symlink
privilege rules, and anything MSVC-specific. The MSVC CI job is still the gate
for those.

Wine also exposes the host's Unix root as drive `Z:`, so a test that checks
`Path::new("/bin/sh").exists()` before spawning `sh` sees `true` under Wine and
`false` on real Windows. Prefer `cfg!(unix)` for that kind of guard.

Known gap: all crates' test targets compile for `x86_64-pc-windows-gnu`, but
`thegn-core`, `thegn-svc` and `thegn-host` do not yet have a clean Wine run.
The procfs and `/bin/sh` tests in the converted files now skip at compile time
(`cfg!(target_os = "linux")` / `cfg!(unix)`), and a ratchet keeps them that
way. Some svc and host tests (LSP, filter-process, monitor actions) still
probe `/bin/sh` at runtime, so they may run under Wine. A skipped test does
not establish Windows behavior. Scope a run with
`just test-windows-wine -p <crate>`.

## Testing on other platforms

Linux is the only platform the local gates run on. `just check-cross` proves
that macOS and Windows code compiles; only running it there proves it works.
Two opt-in recipes do that without paying for CI runners.

### Any ssh host: `just test-remote`

```sh
just test-remote host=mymac                      # nix develop --command just test, on mymac
just test-remote host=mymac thegn-core config::  # cargo nextest run -p thegn-core config::
```

`scripts/test-remote.sh` packs the worktree as git sees it (tracked files plus
untracked, non-ignored ones, so uncommitted edits are included and `target/` is
not). It unpacks the files into `<remote-dir>/src` on the host and commits a
throwaway snapshot there, because `build.rs` and some tests expect a `HEAD`.
It then runs the gate with `CARGO_TARGET_DIR=<remote-dir>/target`, streams
the output, and exits with the remote gate's status. Afterwards it deletes
`src/` and keeps `target/`, so the next run is a warm rebuild.
`THEGN_REMOTE_CLEAN=1` deletes the whole remote dir.

- **Auth** comes from your ssh config (`Host mymac` in `~/.ssh/config`). The
  repo stores no hosts, users or keys. The runner opens one multiplexed
  connection, so a hardware key prompts once per run, not once per step.
- **Remote OS** is detected (`echo %OS%` returns `Windows_NT` from cmd.exe,
  the Windows OpenSSH default shell). If the Windows default shell is
  PowerShell, set `THEGN_REMOTE_OS=windows`.
- **Knobs:** `THEGN_REMOTE_DIR` (default `.cache/thegn-remote/<worktree>` on
  unix, `tgr/<worktree>` on Windows, relative to the remote home directory).
  Absolute paths, drive-letter paths, and `.` or `..` path segments are
  refused. `THEGN_REMOTE_CMD` replaces the gate (a sh snippet on unix,
  PowerShell on Windows), and `THEGN_REMOTE_SSH_CONFIG` passes `ssh -F`.
  Each is described at the top of the script.
- `just test-remote-selftest` checks the runner itself against a local ssh
  shim. It needs no network and compiles nothing.

**Using a Mac (Apple silicon).** Do this once on the Mac. Install Nix with the
[Determinate installer](https://install.determinate.systems/), which enables
flakes by default. Turn on Remote Login (System Settings → General → Sharing).
Check that `ssh mymac nix --version` works from the Linux box. The flake
supports `aarch64-darwin`, so the remote `nix develop` builds the same dev
shell CI uses, and nothing else needs installing. The first run fetches that
shell and does a cold build, which takes a while. Later runs reuse the remote
`target/`. Intel Macs can't take the nix path (see Prerequisites). For an
Intel Mac, set `THEGN_REMOTE_CMD='cargo nextest run --workspace'` after
installing rustup and `cargo-nextest` there.

**Windows host.** Install OpenSSH Server, Git for Windows, the Visual Studio
Build Tools (C++ workload) and rustup's `x86_64-pc-windows-msvc` toolchain.
`test/windows-vm/provision.ps1` installs all four and also runs on a real
machine. The default gate is `cargo test --workspace --locked`, or
`cargo test -p <crate> --locked [filter]` when you pass a crate.

### Local Windows VM: `just test-windows-vm`

A real Windows 11 guest under KVM, managed with
[quickemu](https://github.com/quickemu-project/quickemu). quickemu and qemu
come from the flake's pinned nixpkgs through `nix shell --inputs-from .`
rather than the dev shell, because qemu is a ~1 GiB closure that only this
recipe uses. The recipe is Linux-only. Your user needs read/write access to
`/dev/kvm` (on NixOS, the `kvm` group). Neither step needs sudo or libvirt.

```sh
THEGN_WINVM_ACCEPT_LICENSES=1 just windows-vm-setup  # download ISO, prepare unattended install
just windows-vm-up        # first boot: unattended install + provisioning, ~30-60 min
just test-windows-vm      # up + the CI `windows` job's msvc check and test groups
just test-windows-vm thegn-svc ipc                   # or one crate/filter
just windows-vm-down      # clean shutdown;  windows-vm-status / windows-vm-destroy
```

- **State** is kept in `$XDG_DATA_HOME/thegn-dev/windows-vm`, never in the
  repo. It holds the ISO (~6 GiB), a sparse qcow2 disk that grows to roughly
  40–60 GiB once the toolchain and a `target/` are on it, a VM-only ssh key,
  and the generated `ssh_config` for host `thegn-winvm`. To reach the VM by
  name yourself, add `Include <state>/ssh_config` to `~/.ssh/config`. `setup`
  refuses to start with less than 80 GiB free (`THEGN_WINVM_MIN_FREE_GB`).
  `windows-vm-destroy` deletes this directory only when it contains the VM
  state marker; it refuses `/` and your home directory.
- **Provisioning** needs no clicks. quickget's unattended install creates a
  local admin `Quickemu`, and `setup` adds a first-logon step that runs
  `test/windows-vm/provision.ps1`. That script installs OpenSSH Server (key
  only, using the VM key, password login disabled), VS Build Tools, Git and
  rustup (stable msvc), then
  writes `C:\thegn-provision.done`, which `up` waits for. Its log is
  `C:\thegn-provision.log` in the guest.
- **Media and licences.** `quickget windows 11` downloads Microsoft's
  consumer Windows 11 ISO from Microsoft's own download API, not evaluation
  media. It runs unactivated, which is fine for testing, and it doesn't expire
  the way the 90-day Enterprise evaluation does. Using Windows still needs a
  licence, and the provisioning accepts the Build Tools licence unattended.
  Both are your decision, which is why `setup` requires
  `THEGN_WINVM_ACCEPT_LICENSES=1`. If you use a time-limited evaluation image
  instead, rebuild it when it expires with `just windows-vm-destroy`, then
  `setup` and `up` again. Rebuilding takes about an hour, unattended.
- **Exposure:** quickemu forwards the guest's ssh port on every host
  interface, not just loopback. That is why provisioning turns password login
  off: the unattended admin password is the well-known `quickemu`. Firewall
  port 22220 if this machine is on an untrusted network.
- **Tuning:** `THEGN_WINVM_CPUS` (8), `THEGN_WINVM_RAM` (16G),
  `THEGN_WINVM_DISK` (96G, sparse), `THEGN_WINVM_SSH_PORT` (22220), and
  `THEGN_WINVM_BOOT_TIMEOUT`.

## Where things live

The architecture (crates, invariants, and the gate behind each) is
`docs/ARCHITECTURE.md`; step-by-step recipes for adding things are
`docs/extending/`.

- `crates/thegn-core` — substrate-agnostic domain logic (config, DB, keymap,
  theme, sandbox). New core logic needs unit tests (95% line-coverage gate).
- `crates/thegn-svc` — service seams (git, GitHub, SSH) with subprocess
  fallbacks.
- `crates/thegn-host` — the compositor: event loop (`src/run.rs`), chrome,
  panes, handlers.

Read [the development reference](DEVELOPMENT.md) before touching the event loop or render path —
the 0%-idle and render-plan invariants are enforced by tests. Prefer sibling
modules over growing the large legacy files (run.rs, config.rs, …).

## License

thegn is dual-licensed under either [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. Unless you state otherwise, any
contribution you intentionally submit for inclusion in thegn is dual-licensed
under those same terms, with no additional terms or conditions (per the Apache
2.0 license, Section 5).
