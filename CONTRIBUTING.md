# Contributing to thegn

Welcome! This gets you from clone to a running dev build in a few minutes.
For the product tour see [`README.md`](README.md); for architecture and
invariants see [`CLAUDE.md`](CLAUDE.md).

## Prerequisites

**The recommended path on every platform is [Nix](https://nixos.org/download/)
with flakes** — `nix develop` provides the exact toolchain, linters, and
runtime tools the project is built and CI-tested with. On macOS the
[Determinate installer](https://install.determinate.systems/) is the least
fuss (flakes on by default).

|           | Nix path (recommended)                         | Bare path                                                                                |
| --------- | ---------------------------------------------- | ---------------------------------------------------------------------------------------- |
| **macOS** | Xcode CLT (`xcode-select --install`), then Nix | Xcode CLT + [rustup](https://rustup.rs) (stable ≥ 1.89) + `brew install pkg-config just` |
| **Linux** | Nix                                            | rustup (stable ≥ 1.89) + `just` + a C toolchain                                          |

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
just build         # debug build of the workspace
just start name=dev  # run the compositor with isolated state (safe to poke)
```

No Nix? `cargo build --workspace` and `cargo run -p thegn-host` work too —
you just supply the tools above yourself. To install a real binary for daily
use, `./install.sh` (see the README's Install section).

If something is off, `just doctor` diagnoses the dev environment.

## Dev loop

**Enter the dev shell once, right after cloning** (`nix develop`, or `direnv
allow` to auto-enter per-cd). The pre-commit / pre-push git hooks are generated
and installed by the dev shell — a bare clone that never enters it silently has
no hooks, so formatting and the heavy gates won't run until you push into CI.

The heavy gates are full-workspace compiles — don't run them per-edit:

- **While iterating:** `just quick [crate]` — clippy on lib/bin code only,
  seconds not minutes.
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

`just` with no arguments lists every recipe. Commits follow conventional
style (`feat(scope):`, `fix(scope):`); branch off `main`.

Roadmap and specs: `tasks.md` is the roadmap index; behavior specs live in
`openspec/specs/` and in-flight changes in `openspec/changes/` (the
`openspec` CLI is in the dev shell; agent slash-commands regenerate with
`just openspec-setup`). Every config key is documented in
[`config/config.toml.example`](config/config.toml.example).

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
  (`thegn-metrics`, `thegn-media`, `tg-kit`, `gtui-core`, `gtui-render`,
  `gtui-app`) against `aarch64-apple-darwin`. It stops at `thegn-core`,
  `thegn-svc`, `thegn-host` and `gtui-query`, whose build scripts (`ring`,
  bundled sqlite, libgit2) compile C for the target — those **cannot** be
  checked from Linux, so a darwin break in the host crate is invisible to the
  routine gate. Cover it with the on-device checklist below, or the full macOS
  build+test job (`macos-15`), which is opt-in because GitHub bills those
  minutes at 10x. It runs only on a manual dispatch with `extras`:
  `gh workflow run ci.yml --ref <branch> -f extras=true`. (There is **no**
  `[ci-macos]` commit-message trigger — nor `[ci-windows]` or `[ci-e2e]`; the
  jobs are gated purely on `if: ${{ inputs.extras }}`, and remote CI is paused
  besides, so nothing runs on push at all.)
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
2. `just build && just test && just smoke && just lint`.
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
platform, and no Windows binaries ship in `v0.1.0-alpha.1`. Current state — the
CI claims plus an on-machine pass on real hardware (Windows 11, msvc), written
up in [`docs/windows-native-audit.md`](docs/windows-native-audit.md):

- `cargo check --workspace` **passes** — the port compiles, warning-free.
- The **release build completes** (~18 min cold on a 12-core box). The opt-in
  CI job had never got past this; its 90-minute budget is not the constraint,
  an uncached runner is.
- The named-pipe daemon IPC tests **pass**. They used to fail on
  `pipe_bind_is_the_lock_and_round_trips`: Windows keeps a pipe _name_ reserved
  for a few milliseconds after the last handle of an instance that carried a
  connection closes, and `bind_exclusive` read that window as a rival daemon.
  It now retries briefly (`crates/thegn-svc/src/ipc.rs`).
- The Job-Object process-scoping tests **pass**.
- The **compositor runs**: it renders a full first frame in Windows Terminal,
  picks Unicode glyphs, spawns ConPTY panes, and brings up the daemon.
- Two known gaps keep it from "supported":
  1. **Idle CPU is ~23% of one core**, against the ~0% invariant. It is not the
     render path (p50 2 ms, `idle_ratio` 0.97, no slow-frame warnings) — it is
     thread churn, and every `cpu_*_ms` counter reads 0.0, so thegn's own
     attribution cannot see it.
  2. The interactive checklist (resize storms, `^C` passthrough) is still
     unproven — see `openspec/changes/add-windows-compositor-validation`.
- The msvc job is opt-in: dispatch, or `[ci-windows]` in the commit subject.
  Careful — the marker is matched anywhere in the commit _message_, so merely
  mentioning it in a body will trigger the job.

No WSL is required. The dev experience differs from unix — nix and the justfile
don't apply:

- **Setup is declarative** — the closest native analogue to `nix develop`,
  split across two files because Windows splits the job:

  ```powershell
  winget configure dev/windows.dsc.yaml   # prerequisites (idempotent)
  .\devshell.ps1                          # the environment
  ```

  `dev/windows.dsc.yaml` is a WinGet Configuration (PowerShell DSC) declaring
  git, rustup, the VS Build Tools C++ workload + Windows SDK, and Windows
  Terminal. `dev/scoop.json` declares the same set for `scoop import` (minus
  the Build Tools, which Scoop cannot install). If winget errors with
  `0x8a15000f`, its index is corrupt — `winget source reset --force` from an
  elevated prompt.

  The **Rust toolchain is not in either manifest**: `rust-toolchain.toml` pins
  the channel and components (`clippy`, `rustfmt`, `llvm-tools`) and rustup
  applies it on first `cargo` invocation, on every platform. That file is inert
  under `nix develop`, where the flake's pin wins — keep the two in step.

  Nothing else is needed: no cmake, no perl, no nasm. The dependency graph has
  no `cmake` crate, no `aws-lc-sys`, and no OpenSSL; every C dep (bundled
  sqlite, libgit2, LMDB, zlib, tree-sitter, ring) builds with plain `cc`.
- **`devshell.ps1` is the `nix develop` analogue.** It assembles the half a
  package manager cannot: it puts `~/.cargo/bin` and **Git for Windows'
  `usr\bin`** on the PATH, mirrors the flake's `CARGO_BUILD_JOBS` cap and
  sccache wiring, installs the cargo dev tools (`cargo-nextest`,
  `cargo-llvm-cov`), and drops you in a shell. `-Command "..."` runs one thing
  and exits (like `nix develop --command`); `-Check` reports and changes
  nothing.

  That `usr\bin` entry is load-bearing, not cosmetic: the test fixtures spawn
  `sh`, `cat`, `printf`, `sha256sum`, `stty` and friends, none of which exist
  on Windows outside the MSYS userland Git ships. Without it ~40 tests fail
  with "program not found". It is applied to the **session only** — never the
  User or Machine PATH — because it also contains `find.exe` and `sort.exe`,
  which shadow the Windows built-ins.
- Then plain cargo: `cargo build`, `cargo run`, `cargo test`. The justfile does
  not apply (its recipes are bash).
- **Installing:** `.\install.ps1` is the Windows counterpart to `install.sh` —
  per-user, no admin. It builds a release binary, drops `thegn.exe` plus `tg` /
  `tg-tui` shims into `%LOCALAPPDATA%\Programs\thegn`, adds that to the user
  PATH, and writes a Start Menu entry that opens thegn *inside Windows
  Terminal* (a plain shortcut would land in conhost, which thegn refuses).
  `-DryRun` prints the plan; `-NoBuild` installs a binary you already have
  (e.g. the CI artifact); `-BinDir` relocates it.
- **Test parallelism:** `git rebase -i` runs its sequence editor through Git
  for Windows' bundled MSYS `sh.exe`, whose emulated `fork()` loses races under
  heavy parallelism and dies with
  `sh.exe: *** fatal error - add_item (...)`. `.config/nextest.toml` caps those
  tests at 2 concurrent on Windows. With plain `cargo test` (which does not
  read that config) pass `-- --test-threads=4` or lower if you see it.
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
- **CI:** every PR cross-checks the whole workspace for
  `x86_64-pc-windows-gnu` on Linux (`just check-cross`); the full
  `windows-latest` msvc job (check + IPC/Job-Object kernel tests) is opt-in —
  add `[ci-windows]` to a commit message or dispatch the workflow.

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

Read [`CLAUDE.md`](CLAUDE.md) before touching the event loop or render path —
the 0%-idle and render-plan invariants are enforced by tests. Prefer sibling
modules over growing the large legacy files (run.rs, config.rs, …).

## License

thegn is dual-licensed under either [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. Unless you state otherwise, any
contribution you intentionally submit for inclusion in thegn is dual-licensed
under those same terms, with no additional terms or conditions (per the Apache
2.0 license, Section 5).
