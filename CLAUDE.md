# CLAUDE.md

Guidance for working in this repo. See `README.md` for the user-facing tour and
`tasks.md` for the roadmap / progress tracker.

## What this is

**thegn** (binary `thegn`, with a short `tg` alias) — a terminal-native git-worktree IDE that is its own terminal
multiplexer. One UI process, one session: each repo is a workspace, each git
**worktree** is a tab, and the chrome (sidebar tree, diff/PR panel, tabbar,
statusbar, pin strip) is rendered in-process. There is **no zellij and no WASM
plugins** — those were stripped (Phase 0, commit `bb2ecd4`); mentions in older
docs/comments are historical. The one IPC seam that DOES exist today is the
**pane daemon**: PTYs are owned by a background `thegn daemon` process (unix
socket, plus TCP for `thegn serve` thin clients), so sessions survive UI detach
and warm-reattach — `[daemon] enabled = false` restores fully in-process panes.
See `crates/thegn-host/src/daemon/`.

The **AI/agent layer was removed before the public alpha** (the old two-track
plan's LLM proxy, ACP/bouncer, and managed agent are gone; see `tasks.md`). The
product is the AI-free workspace shell. What remains agent-adjacent is strictly
generic: the `[[agents]]`/`[[tools]]` picker launches any configured CLI as a
plain command, and the two queues' agent handoff (`[merge_queue]` /
`[pr_queue]`) is an arbitrary subprocess hook — one shared, data-driven engine
(`thegn_core::agent_task` renders the prompt template and resolves the command;
`thegn-host/src/agent_run.rs` runs it). Both queues work with no agent
configured at all. If the AI track reopens, it must be additive — the shell
never hard-depends on it.

## Architecture

- **Cargo workspace.** The load-bearing crates:
  - `crates/thegn-core` — substrate-agnostic, testable domain logic: layered
    config, SQLite DB, keymap registry, theme, sandbox backends, activity
    state machine, `gh` wrapper. No tokio/termwiz deps.
  - `crates/thegn-svc` — service trait seams with graceful degradation:
    `GitBackend` (gix-native reads, CLI fallback + writes), GitHub (octocrab /
    `gh`), SSH (russh / `ssh`). Native gaps always fall back to subprocess.
  - `crates/thegn-host` — the compositor: tokio runtime, portable-pty panes
    through a pluggable `PaneEmulator` (vt100 today), termwiz `Surface`
    diff-flush rendering, in-process chrome, and the pane-daemon client/server.

  Support crates: `thegn-metrics` (system metrics collection), `thegn-media`
  (MPRIS/SMTC/mpv media control), `tg-kit` and the `gtui-*` family (UI/embed
  frameworks).

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

Run inside `nix develop` (rust toolchain + tools). Human-contributor
onboarding (prerequisites per platform, macOS notes) lives in
`CONTRIBUTING.md`.

**There is exactly ONE dev shell: `flake.nix`'s `devShells.default`.** Enter it
with `nix develop`, or `direnv allow` once (`.envrc` is a plain `use flake`).
Every CI job runs `nix develop --command just <gate>`, so the shell you develop
in is the shell that gates you — including the tiered git hooks and the
`treefmt`/`clippy` versions they run. **Don't add a second environment
definition.** There used to be a devenv one, and it cost:

- its own nixpkgs lock, which drifted ~6 weeks from `flake.lock` — so the
  `rustfmt` gating a commit (1.97.1) was a different build from the one
  `nix fmt` ran (1.96.1), on a repo whose whole formatter story is "one
  `treefmt.toml`".
- its own `languages.rust` toolchain with no `llvm-tools-preview` and no cross
  `rust-std`, so `just coverage` died with `failed to find llvm-tools-preview`
  and `just check-cross` with `can't find crate for 'core'` — in the shell
  `.envrc` put you in by default.
- a hand-duplicated copy of the packages/env/shellHook, which is how the mingw
  cross **C** toolchain (`CC_x86_64_pc_windows_gnu` &c.) came to be set only in
  devenv: `check-cross` passed locally and failed on CI, hidden by the nesting
  trap where a `nix develop` started _inside_ a devenv shell inherits devenv's
  env. (`704eee77`, now `mingwCrossEnv` in the flake.)

`flake.nix` is therefore the single source for the toolchain, the packages, the
env, and the hooks. Cross gates are still worth testing from a clean shell.

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

- **pre-commit** (cheap, no compile): treefmt + shellcheck + yamllint.
- **pre-push**: clippy + `cargo test` + smoke. **This is the single heavy gate**
  that must be green before code leaves the machine — rely on it, don't re-run
  full-workspace gates by hand while iterating.
- **CI-only** (`just ci`): coverage (`cargo llvm-cov` — the heaviest gate,
  instrumented recompile), cross-check, docs, e2e (still in `just ci`; opt-in in
  CI — see the e2e note below), nix-build. Run `just coverage` locally on demand
  before a PR if you want the gate early.

**Test precisely; keep full-workspace rebuilds to an absolute minimum.** A
full-workspace compile is the most expensive thing you can do on this box, so
while iterating:

- Run **one crate's** checks — `just quick <crate>` (typecheck/clippy on lib/bin).
- Run **the specific tests** you're touching, not the whole suite — e.g.
  `cargo nextest run -p <crate> <substring>` or `cargo test -p <crate> <module>::`.
  Reach for `cargo nextest run --workspace` / `just test` only right before push.
- `just test`, `just coverage`, `just ci` are **pre-push / pre-PR gates, not
  per-edit commands.** Let the pre-push hook be the thing that runs them.

The dev shell also **caps `CARGO_BUILD_JOBS`** (leaves ~2 cores free) and wires
**sccache** (`RUSTC_WRAPPER`, `CARGO_INCREMENTAL=0`) so cold worktrees / branch
switches reuse compiled crates instead of rebuilding from scratch.

**Merge-queue gate cost.** `thegn merge`'s fold gate runs `[merge_queue]
gate_command` per fold. By default it now reuses a stable per-repo worktree +
`CARGO_TARGET_DIR` under `$XDG_STATE_HOME/thegn/gate/` (`gate_reuse_worktree`), so
folds warm-rebuild instead of cold-compiling from scratch. Keep `gate_command`
**lean** (e.g. `just test`, not `just lint && just test`) — pre-push already
covered clippy/test before the branch was enqueued.

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
- **The old `.hermes/plans/` narrative docs were removed** in favor of `/opsx`
  (openspec). Historical design docs still live in `docs/superpowers/{plans,specs}/`.

## Conventions & gotchas

- **Keep god-files from growing (guidance, no longer enforced).** The legacy
  oversized files (run.rs, config.rs, db.rs, agent.rs, chrome.rs, sandbox.rs,
  keymap.rs) are already large; don't add to them. Put new feature/Section key
  handlers and helpers in a sibling module (e.g. `src/handlers/<area>.rs`) and
  call it from the loop. (The size ratchet that used to enforce this was
  removed; the preference stands.)
- **Remote CI is TEMPORARILY OFF — the pre-push hook is the only gate.**
  `.github/workflows/ci.yml` is dispatch-only (its `push`/`pull_request`
  triggers are commented out, with the reasoning above the `on:` key) because a
  push to main cost ~101 billable minutes and a PR cost that again before its
  merge. So `cargo test` + `clippy` + `smoke` on pre-push is what protects main
  right now — do not disable it, and do not assume a green push means coverage,
  cross-compilation, docs, deps-audit, nix-build, sandbox-e2e or openspec were
  checked. Run the suite on demand with `gh workflow run ci.yml --ref <branch>`
  (add `-f extras=true` for the windows/e2e opt-ins). The macOS job is disabled
  outright: 10x cost, and it OOMs building openspec before it compiles anything.
  Re-enable only after the cheap wins in that comment — self-hosted runners
  (note the fork-PR security caveat documented in the workflow), lean dev shells
  for the cheap jobs, and tiering coverage/nix-build/check-cross off every PR.
- **e2e (`just e2e`) is a local gate; in CI it is temporarily opt-in.** The CI
  job kept hitting its 30-minute timeout, and the committed baselines are stale
  (last recorded in `0f9c5a9a`; `1726a8e1` changed the UI without re-recording,
  and no darwin baselines exist) — so it gated nothing while costing half an
  hour a push. Run it in CI with `[ci-e2e]` in a commit message or a workflow
  dispatch; locally it is unchanged and still the gate for anything that alters
  a frame. Fix the timeout AND re-record before making it blocking again.
  muse drives the built binary in a
  PTY under the `THEGN_E2E=1` determinism freeze (`src/e2e_freeze.rs`) and
  diffs snapshots against `test/muse/snapshots/`. A UI change that alters a
  frame must re-record with `just e2e-update` (review the diff); a failing
  case leaves `e2e-results/<case>/` (final screen, diffs, trace) — read it
  before retrying. New volatile chrome (clocks, counters, spinners) must be
  pinned in `e2e_freeze` or the snapshots flap. To check a change by hand,
  drive thegn with `muse session`. **Read `docs/testing-with-muse.md`
  before writing or editing a spec** — it lists the traps (panel focus,
  section digits + Enter, the 3-state sidebar toggle, chip overflow, ESC
  spacing) that otherwise cost a run each; `extensions/skills/tui-check/`
  is the agent recipe.
- **Help ratchet (`crates/thegn-host/src/help/ratchet_tests.rs`, runs in
  `just test`).** Every `ACTION_SPECS` action id must be claimed by a
  `docs/help/` page's `actions:` frontmatter (F1 opens the in-app help;
  pages are embedded via `include_str!` in `src/help/pages.rs`). New
  actions/keybinds/zones/panel-sections need a help-page update in the same
  change. **A second ratchet requires the page to actually mention what it
  claims** (by chord, id, or a distinctive label word) — claiming an id is
  cheap, and was how the corpus drifted while coverage read ~100%. Three
  pinned-debt allowlists, all shrink-only and all regenerated by
  `just help-ratchet-update`: `test/help-ratchet.txt` (unclaimed actions),
  `test/help-prose-ratchet.txt` (claimed but unwritten), and
  `test/help-context-ratchet.txt` (unclaimed panel contexts). The keybindings
  and config-reference pages are generated at runtime — never hand-write them;
  the keybindings page is built from `keymap_merge::collect`, the same fold
  `thegn keys list` prints, and a test asserts every bindable action appears.
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
- **`.pre-commit-config.yaml` is a generated Nix store symlink** (git-hooks.nix,
  via the `preCommit` block in `flake.nix`) — edit `flake.nix`, then re-enter
  `nix develop` to regenerate. The hooks are tiered: pre-commit stays cheap
  (treefmt + shellcheck + yamllint), pre-push carries the correctness gates
  (clippy, `just test`, `just smoke`). `git add` new files before
  `nix flake check`.
- Commit/push only when asked; branch off `main` first. Conventional commit
  style (`feat(scope):`, `fix(scope):`) matches the history.
- **Landing on `main` from a sandbox/worktree.** The canonical checkout's
  working tree is mounted **read-only** (protecting a live instance) but the
  shared `.git` (object + ref store) is **writable**. So `git checkout main &&
git merge` / `merge --ff-only` fail (they rewrite the read-only tree), while
  the object-DB fold succeeds. Use **`thegn land`** (one-shot: fold + gate +
  CAS-advance `refs/heads/main`, no target checkout) — or `thegn integrate` for
  the whole queue. The land itself
  fast-forwards every worktree holding the target (`util::resync_branch_checkouts`)
  and reports any it had to leave alone; a running instance additionally
  self-heals on the ref move (`git_watch`/`util::heal_main_checkout_worktree`). Don't
  hand-roll `git update-ref` to "merge to main" (it moves the ref but leaves the
  live tree stale). See `crates/thegn-core/src/merge_guard.rs`.
