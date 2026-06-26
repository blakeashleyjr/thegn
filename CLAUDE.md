# CLAUDE.md

Guidance for working in this repo. See `README.md` for the user-facing tour and
`tasks.md` for the roadmap / progress tracker.

## What this is

**superzej** (binary `szhost`, installed as `superzej` with `sj`/`szhost`
aliases) — a terminal-native git-worktree IDE that is its own terminal
multiplexer. One process, one session: each repo is a workspace, each git
**worktree** is a tab, and the chrome (sidebar tree, diff/PR panel, tabbar,
statusbar, pin strip) is rendered in-process. There is **no zellij, no WASM
plugins, no IPC** — all of that was stripped (Phase 0, commit `bb2ecd4`);
mentions of it in older docs/comments are historical.

The long game (see `tasks.md`): two tracks joined by one keystone — an
**AI-free workspace shell** (the current, shippable product) and an AI/agent
layer bridged by an **LLM proxy**. The shell must never hard-depend on the AI
layers; AI is strictly additive.

## Architecture

- **Cargo workspace, three crates:**
  - `crates/superzej-core` — substrate-agnostic, testable domain logic: layered
    config, SQLite DB, keymap registry, theme, sandbox backends, activity
    state machine, `gh` wrapper. No tokio/termwiz deps.
  - `crates/superzej-svc` — service trait seams with graceful degradation:
    `GitBackend` (gix-native reads, CLI fallback + writes), GitHub (octocrab /
    `gh`), SSH (russh / `ssh`). Native gaps always fall back to subprocess.
  - `crates/superzej-host` — the compositor: tokio runtime, portable-pty panes
    through a pluggable `PaneEmulator` (vt100 today), termwiz `Surface`
    diff-flush rendering, in-process chrome.
- **Event model (a hard invariant: ~0% idle CPU).** The loop blocks on termwiz
  `poll_input(None)` — no tick, no timeout. Every off-thread producer (PTY
  reader threads, model hydration on `spawn_blocking`, config/diff fs-watchers,
  the 2s refresh-ticker thread) sends on a tokio mpsc channel **and pulses the
  `TerminalWaker`**; the loop drains channels on wake and re-renders only when
  dirty. Never put blocking I/O (git, DB, subprocess) on the loop; never add a
  polling timeout.
- **Rendering** is a damage-region compositor (`src/render_plan.rs` + the
  `run.rs` render block). The loop tracks three damage channels — `full`
  (geometry), `chrome` (the master `dirty`: sidebar/panel/bars/overlays/model),
  and `dirty_panes` (per-pane PTY content) — and the **pure, unit-tested**
  `render_plan::plan()` maps them to the cheapest correct frame: `Skip` (idle),
  `Panes` (recompose + **bounded-diff** only the changed panes via
  `Surface::diff_region`), or `Full` (`render_tab` + whole-screen `diff_screens`).
  So a streaming-output frame costs ~one `compose_pane` + a one-rect diff, not a
  full chrome recompose. `render_tab` = `render_panes` (center) + `draw_chrome`,
  composed separately so each can repaint without the other.
- **State.** SQLite at `$XDG_STATE_HOME/superzej/superzej.db` (WAL, schema
  versioned via `user_version`): repos, workspaces, worktrees, PR cache,
  tab layouts, session + sidebar UI state. **git is the source of truth** for
  worktrees; the DB is a cache + resurrection layer.
- **Sandboxing.** Each worktree's interactive process can run in a container
  (`podman` → `docker` → `bwrap` → `none`); the worktree stays on the host,
  bind-mounted at its real path so host-side git reads keep working. Remote
  backend runs worktrees on another machine.

## Performance invariants

"Everything is instant": sub-300ms launch → first frame, <16ms render, 0% idle.

- **The render-decision invariants are ENFORCED in `just ci`** (not just measured).
  Wall-clock benchmarks are machine-dependent and excluded from CI; instead the
  render decision is a pure function (`render_plan::plan`) with exhaustive unit
  tests (`cargo test`, which `ci` runs) that lock the work-shape: an idle wake
  ⇒ `Skip` (the 0%-idle contract), pane output and nothing else ⇒ `Panes` (never
  recompose chrome), any chrome/overlay/geometry change ⇒ `Full`. A change that
  reintroduces a full recompose on pane output fails these tests. **When you
  touch the render path, keep these invariants and their tests green** — they are
  the regression gate, not the (advisory) wall-clock benches. The runtime
  `szhost::perf` rollup also emits a **slow-frame warning** (`render_p50_us` over
  `SUPERZEJ_FRAME_BUDGET_US`, default 16ms) and `render_busy_ratio`, which catch
  cost-per-frame regressions the idle-ratio/wake-count storm warning cannot see.

- `SUPERZEJ_LOG=info` writes a **startup waterfall** to
  `$XDG_STATE_HOME/superzej/logs/szhost.log` (`szhost::startup` events with
  `since_start_ms`). Frame/hydration timings: `SUPERZEJ_LOG=szhost::frame=debug`
  / `szhost::hydrate=debug`. No subscriber is installed when `SUPERZEJ_LOG` is
  unset — instrumentation is free.
- `just bench` (hyperfine) measures process baseline + real launch→first-frame
  via `SUPERZEJ_BENCH_FIRST_FRAME_EXIT=1`. Machine-dependent, so not in `ci`;
  perf commits should record before/after deltas.
- **Perf suite** (`docs/superpowers/specs/perf-suite.md`): runtime self-profiler
  (`SUPERZEJ_PERF=1` → `szhost::perf` rollup with wake-source + per-subsystem-CPU
  attribution + wake-storm warning), steady-state idle harness (`just bench-idle`,
  `SUPERZEJ_BENCH_RUN_MS`), criterion micro-benches (`just bench-micro`), a live
  Telemetry "LOOP" overlay, and an in-process flame-graph profiler (`just profile`,
  SIGUSR2, `profiling` feature). All free when off; none in `ci` (machine-dependent).
- Expensive setup belongs off-thread (see the diff fs-watcher: recursive
  inotify registration is ~1s on large worktrees and is done on a background
  thread, handed back over a channel).

## Source map

- `crates/superzej-host/src/main.rs` — clap tree; bare `szhost` launches the
  compositor, subcommands (`pr`, `issue`, `diff`, `list`, `repos`, `config`)
  run synchronously from `src/cmd/`.
- `crates/superzej-host/src/run.rs` — the event loop + startup.
- `crates/superzej-host/src/` — `chrome.rs` (widget rendering), `sidebar.rs`
  (tree model), `pins.rs` (`PinSupervisor` daemon panes), `center.rs`
  (pane-tree layout), `pane.rs`/`emulator.rs` (PTY + vt100), `session.rs`
  (persist/resurrect), `palette.rs`, `keymap.rs`, `copymode.rs`.
- `crates/superzej-core/src/` — `config.rs` (layered TOML, `config_enum!`),
  `db.rs`, `keymap.rs`, `theme.rs`, `sandbox.rs`, `activity.rs`, `log.rs`
  (branded tracing subscriber + rotating file sink).
- `config/config.toml.example` — every superzej key, documented.
- `docs/superpowers/{plans,specs}/` — design docs per feature.

## Development

Run inside `nix develop` (rust toolchain + tools).

```sh
just build           # cargo build --workspace (debug)
just test            # unit tests
just smoke           # hermetic end-to-end CLI test
just lint            # clippy -D warnings + shellcheck + yamllint + taplo
just coverage        # cargo llvm-cov, gated at 95% lines on the core
just bench           # startup benchmarks (hyperfine; not part of ci)
just start name=dev  # run the host with an isolated XDG_STATE_HOME
just ci              # fmt-check + lint + build + test + coverage + smoke + nix-build
```

Nix: `nix profile install .#default`; `nix develop` for the dev shell.

## Conventions & gotchas

- **Coverage gate: `superzej-core` only, 95% lines.** I/O / subprocess seams
  (the `cov_ignore` regex in the justfile) are excluded and exercised by
  `test/smoke.sh` instead. The host and svc crates carry their own unit tests
  but aren't gated. New core logic needs unit tests.
- **This shell often runs _inside_ a live superzej.** Anything that opens the
  DB or spawns the host in tests/benches must isolate `XDG_STATE_HOME`
  (`just start`/`just bench` already do).
- **`.pre-commit-config.yaml` is a generated Nix store symlink** (devenv /
  git-hooks.nix) — edit `devenv.nix`, then re-enter `devenv shell` to
  regenerate. `git add` new files before `nix flake check`.
- Commit/push only when asked; branch off `main` first. Conventional commit
  style (`feat(scope):`, `fix(scope):`) matches the history.
