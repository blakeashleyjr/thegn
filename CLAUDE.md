# CLAUDE.md

Guidance for working in this repo. See `README.md` for the user-facing tour and
`tasks.md` for the roadmap / progress tracker.

## What this is

**thegn** (binary `thegn`, with a short `tg` alias) — a terminal-native git-worktree IDE that is its own terminal
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
  - `crates/thegn-core` — substrate-agnostic, testable domain logic: layered
    config, SQLite DB, keymap registry, theme, sandbox backends, activity
    state machine, `gh` wrapper. No tokio/termwiz deps.
  - `crates/thegn-svc` — service trait seams with graceful degradation:
    `GitBackend` (gix-native reads, CLI fallback + writes), GitHub (octocrab /
    `gh`), SSH (russh / `ssh`). Native gaps always fall back to subprocess.
  - `crates/thegn-host` — the compositor: tokio runtime, portable-pty panes
    through a pluggable `PaneEmulator` (vt100 today), termwiz `Surface`
    diff-flush rendering, in-process chrome.
- **Event model (a hard invariant: ~0% idle CPU).** When idle, the loop blocks
  on termwiz `poll_input(None)` — no tick, no timeout. (One sanctioned
  exception: while there is already work in hand — `dirty`, queued input, or an
  exhausted frame budget — the loop polls with a short 8ms timeout to _batch_
  bursty input before the next flush. That is a busy-time heuristic; the
  invariant is that an **idle** loop never polls.) Every off-thread producer
  (PTY reader threads, model hydration on `spawn_blocking`, config/diff
  fs-watchers, the 2s refresh-ticker thread) sends on a tokio mpsc channel
  **and pulses the `TerminalWaker`**; the loop drains channels on wake and
  re-renders only when dirty. Never put blocking I/O (git, DB, subprocess) on
  the loop; never add a polling timeout to the idle path.
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
- **Terminal compatibility / graceful degradation.** The outer terminal's
  capabilities (`thegn_core::termcaps`: color depth, glyph level, undercurl,
  mouse) are detected purely from the environment (with an optional startup
  DA/XTVERSION probe, `src/probe.rs`), folded with `[theme] color`/`glyphs`
  config, and installed into a render-time holder (`src/caps.rs`, same pattern as
  the undercurl atomic / chrome `PALETTE`). The frame is always composed in
  truecolor + Unicode; degradation happens at the edges — **color** quantizes
  truecolor→256→16→mono (or drops, for `NO_COLOR`) at the single `wire.rs`
  `color_spec` chokepoint, and **glyphs** swap Unicode↔ASCII via
  `caps::active_glyphs()` at the borders/chrome/pins/logotype call sites. Chrome
  layout widths use display width (`unicode-width`), not char count. `thegn
doctor` prints the resolved capabilities. Detection logic is pure + unit-tested
  in core; never assume truecolor/Unicode at a draw site — go through `caps`.
- **State.** SQLite at `$XDG_STATE_HOME/thegn/thegn.db` (WAL, schema
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
  `thegn::perf` rollup also emits a **slow-frame warning** (`render_p50_us` over
  `THEGN_FRAME_BUDGET_US`, default 16ms) and `render_busy_ratio`, which catch
  cost-per-frame regressions the idle-ratio/wake-count storm warning cannot see.

- `THEGN_LOG=info` writes a **startup waterfall** to
  `$XDG_STATE_HOME/thegn/logs/thegn.log` (`thegn::startup` events with
  `since_start_ms`). Frame/hydration timings: `THEGN_LOG=thegn::frame=debug`
  / `thegn::hydrate=debug`. No subscriber is installed when `THEGN_LOG` is
  unset — instrumentation is free.
- `just bench` (hyperfine) measures process baseline + real launch→first-frame
  via `THEGN_BENCH_FIRST_FRAME_EXIT=1`. Machine-dependent, so not in `ci`;
  perf commits should record before/after deltas.
- **Perf suite** (`docs/superpowers/specs/perf-suite.md`): runtime self-profiler
  (`THEGN_PERF=1` → `thegn::perf` rollup with wake-source + per-subsystem-CPU
  attribution + wake-storm warning), steady-state idle harness (`just bench-idle`,
  `THEGN_BENCH_RUN_MS`), criterion micro-benches (`just bench-micro`), a live
  Telemetry "LOOP" overlay, and an in-process flame-graph profiler (`just profile`,
  SIGUSR2, `profiling` feature). All free when off; none in `ci` (machine-dependent).
- Expensive setup belongs off-thread (see the diff fs-watcher: recursive
  inotify registration is ~1s on large worktrees and is done on a background
  thread, handed back over a channel).

## Source map

- `crates/thegn-host/src/main.rs` — clap tree; bare `thegn` launches the
  compositor, subcommands (`pr`, `issue`, `diff`, `list`, `repos`, `config`)
  run synchronously from `src/cmd/`.
- `crates/thegn-host/src/run.rs` — the event loop + startup.
- `crates/thegn-host/src/` — `chrome.rs` (widget rendering), `sidebar.rs`
  (tree model), `pins.rs` (`PinSupervisor` daemon panes), `center.rs`
  (pane-tree layout), `pane.rs`/`emulator.rs` (PTY + vt100), `session.rs`
  (persist/resurrect), `palette.rs`, `keymap.rs`, `copymode.rs`.
- `crates/thegn-core/src/` — `config.rs` (layered TOML, `config_enum!`),
  `db.rs`, `keymap.rs`, `theme.rs`, `sandbox.rs`, `activity.rs`, `log.rs`
  (branded tracing subscriber + rotating file sink).
- `config/config.toml.example` — every thegn key, documented.
- `docs/superpowers/{plans,specs}/` — design docs per feature.

## Development

Run inside `nix develop` (rust toolchain + tools).

```sh
just quick [crate]   # fast inner-loop: clippy on lib/bin only (no test targets)
just build           # cargo build --workspace (debug)
just test            # unit tests
just smoke           # hermetic end-to-end CLI test
just lint            # clippy -D warnings + shellcheck + yamllint + taplo
just coverage        # cargo llvm-cov, gated at 95% lines on the core
just bench           # startup benchmarks (hyperfine; not part of ci)
just start name=dev  # run the host with an isolated XDG_STATE_HOME
just ci              # fmt-check + lint + build + test + openspec-validate + coverage + smoke + nix-build
```

**Dev-loop policy — don't peg the machine.** The heavy gates (`just test`,
`just coverage`, `just lint`, `just ci`) are full-workspace compiles; running
them after every edit is what saturates the CPU. **While iterating, use
`just quick`** (clippy on lib/bin code only — no test/bench targets, no tests,
no coverage; `just quick thegn-host` scopes to one crate). Run the heavy
gates **once, when preparing to push or open a PR** — not per-edit. The tiers
enforce this automatically:

- **pre-commit** (cheap, no compile): treefmt + shellcheck + yamllint + the
  god-file ratchet.
- **pre-push**: clippy + `cargo test` + smoke.
- **CI-only** (`just ci`): coverage (`cargo llvm-cov` — the heaviest gate,
  instrumented recompile), cross-check, docs, e2e, nix-build. Run `just coverage`
  locally on demand before a PR if you want the gate early.

The dev shell also **caps `CARGO_BUILD_JOBS`** (leaves ~2 cores free) and wires
**sccache** (`RUSTC_WRAPPER`, `CARGO_INCREMENTAL=0`) so cold worktrees / branch
switches reuse compiled crates instead of rebuilding from scratch.

Nix: `nix profile install .#default`; `nix develop` for the dev shell.

## Spec-driven development (OpenSpec)

thegn's **own development** is managed with [OpenSpec](https://github.com/Fission-AI/OpenSpec)
(spec-driven development for AI agents). This is a dev-process tool — it is **not**
part of the shipped `thegn` binary.

- **Source of truth:** `openspec/specs/<capability>/spec.md` describes how the
  system behaves _today_ (behavior-first: `### Requirement:` with SHALL/MUST +
  `#### Scenario:` WHEN/THEN). `openspec/config.yaml` holds the schema + the
  project context injected into every artifact the AI generates.
- **In-flight work:** each change is a self-contained folder under
  `openspec/changes/<name>/` (proposal.md, design.md, tasks.md, and delta specs
  using `## ADDED/MODIFIED/REMOVED Requirements`). On completion, deltas merge
  into `openspec/specs/` and the change is archived.
- **Workflow (Claude Code slash commands):** `/opsx:explore` → `/opsx:propose`
  → `/opsx:apply` → `/opsx:sync` → `/opsx:archive`. The `.claude/` commands +
  skills are gitignored; regenerate them per checkout with `just openspec-setup`
  (the dev shell also seeds them on first entry).
- **tasks.md stays the roadmap index** (groups A–AX, phased). When starting work,
  link the `tasks.md` item(s) to the openspec change (cite group letter + number
  in the proposal's Impact). OpenSpec owns per-change detail; tasks.md owns the
  map. Older narrative docs live in `docs/superpowers/{plans,specs}/`.
- A change's final "run `just ci`" validation task is a **pre-PR gate run once**
  when the implementation is complete — not something to run per-edit. Iterate
  with `just quick` (see the dev-loop policy above) and save `just ci` for the end.
- **Tooling is hermetic:** the `openspec` CLI is a pinned Nix build
  (`nix/openspec.nix`, `nix run .#openspec`), on PATH in `nix develop`; telemetry
  is off by construction. `just openspec <args>` is a passthrough;
  `just openspec-validate` (`openspec validate --all --strict`) runs in `just ci`.
- **`.hermes/plans/` is deprecated** in favor of `/opsx` — see
  `.hermes/plans/DEPRECATED.md`. Existing files are kept for history only.

## Conventions & gotchas

- **God-file ratchet (`test/file-size-ratchet.sh`, runs in `just lint`).**
  Source files are hard-capped at 3000 lines; the legacy oversized files
  (run.rs, config.rs, db.rs, agent.rs, chrome.rs, sandbox.rs, keymap.rs) are
  pinned at their recorded size in `test/file-size-ratchet.txt` and may only
  shrink. Don't grow them: put new feature/Section key handlers and helpers in
  a sibling module (e.g. `src/handlers/<area>.rs`) and call it from the loop.
  After shrinking a pinned file, run `test/file-size-ratchet.sh --update` to
  lock in the lower ceiling.
- **Coverage gate: `thegn-core` only, 95% lines.** I/O / subprocess seams
  (the `cov_ignore` regex in the justfile) are excluded and exercised by
  `test/smoke.sh` instead. The host and svc crates carry their own unit tests
  but aren't gated. New core logic needs unit tests.
- **Ignored `Result`s must be deliberate.** `let _ = …` / `.ok()` is the
  sanctioned pattern for best-effort work whose failure must never take down
  the compositor: DB cache/session persists (the DB is a cache; git is the
  source of truth), waker pulses, cleanup, channel sends to a possibly-gone
  consumer. Anywhere the ignore isn't obviously one of those, add a short
  `// best-effort: <why>` comment — and never swallow errors on the primary
  path of a user-invoked action (surface those via `model.status`, `msg`, or
  `tracing`).
- **This shell often runs _inside_ a live thegn.** Anything that opens the
  DB or spawns the host in tests/benches must isolate `XDG_STATE_HOME`
  (`just start`/`just bench` already do).
- **`.pre-commit-config.yaml` is a generated Nix store symlink** (devenv /
  git-hooks.nix) — edit `devenv.nix`, then re-enter `devenv shell` to
  regenerate. `git add` new files before `nix flake check`.
- Commit/push only when asked; branch off `main` first. Conventional commit
  style (`feat(scope):`, `fix(scope):`) matches the history.
- **Landing on `main` from a sandbox/worktree.** The canonical checkout's
  working tree is mounted **read-only** (protecting a live instance) but the
  shared `.git` (object + ref store) is **writable**. So `git checkout main &&
git merge` / `merge --ff-only` fail (they rewrite the read-only tree), while
  the object-DB fold succeeds. Use **`thegn land`** (one-shot: fold + gate +
  CAS-advance `refs/heads/main`, no target checkout) — or `thegn integrate` for
  the whole queue. A running instance on `main` then fast-forwards its own tree
  on the ref move (`git_watch`/`util::heal_main_checkout_worktree`). Don't
  hand-roll `git update-ref` to "merge to main" (it moves the ref but leaves the
  live tree stale). See `crates/thegn-core/src/merge_guard.rs`.
