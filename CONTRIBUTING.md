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
  `podman → docker → bwrap → host`; `bwrap`/`systemd-run` are Linux-only, so
  with no container runtime installed everything still works — panes just run
  directly on the host. `[sandbox] backend = "apple"` selects the macOS
  `container` backend explicitly.
- **Cross-checks are partial — mind the gap.** `just check-cross` type-checks
  only the C-dep-free leaf crates (`thegn-metrics`, `thegn-media`) against
  `aarch64-apple-darwin`; `thegn-host` itself **cannot** be checked from Linux,
  because its build scripts (`ring`, bundled sqlite) need a real darwin C
  toolchain. So a darwin break in the host crate is invisible to the routine
  gate. The full macOS build+test job (`macos-15`) is opt-in because GitHub
  bills those minutes at 10x: add `[ci-macos]` to a commit message (or dispatch
  the workflow manually) for any platform-sensitive change — and note it has
  never actually run, so macOS is unvalidated as of `v0.1.0-alpha.1`.
- **State paths** follow XDG conventions (`~/.config/thegn`,
  `~/.local/state/thegn`) on macOS too; set `XDG_CONFIG_HOME`/
  `XDG_STATE_HOME` if you prefer `~/Library`.
- A few justfile recipes are Linux-centric (`start-term` assumes Ghostty on
  PATH; font tooling uses `fc-list`) — none are needed for the core loop.

## Windows (native) notes

Native Windows is a build target **under development**, not a supported
platform — as of `v0.1.0-alpha.1` `thegn-host` has never been compiled or run
on Windows (the msvc CI job has never executed), and no Windows binaries ship.
The per-OS code paths below are written and reviewed but unproven; getting a
first green msvc run is open work — see
`openspec/changes/add-windows-native-compile`. No WSL is required. The dev
experience differs from unix — nix/devenv and the justfile don't apply:

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
- **CI:** every PR cross-checks the whole workspace for
  `x86_64-pc-windows-gnu` on Linux (`just check-cross`); the full
  `windows-latest` msvc job (check + IPC/Job-Object kernel tests) is opt-in —
  add `[ci-windows]` to a commit message or dispatch the workflow.

## Where things live

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
