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

**`docs/ARCHITECTURE.md` is the single source** — crates and dependency
direction, the event loop, rendering/degradation chokepoints, platform code,
the provider-seam pattern, the capability catalog, config, keymap/help, state,
sandboxing — each with the gate that enforces it. Behavioural contracts are
`openspec/specs/<capability>/spec.md`; how-to-add recipes are
`docs/extending/`. The hard invariants, because every edit must respect them:

- **0% idle.** The loop blocks on `poll_input(None)`; only work-in-hand polls
  (8 ms batching, or a deferred frame's remainder — `idle_poll::poll_timeout`).
  Off-thread producers send on a channel **and pulse the `TerminalWaker`**.
  Never blocking I/O (git, DB, subprocess, D-Bus, network) on the loop or
  before the first frame.
- **Render decision is pure.** `render_plan::plan` → `Skip` / `Panes` / `Full`;
  pane output never recomposes chrome.
- **Degrade at the edges.** Compose in truecolor + Unicode; quantize once in
  `wire.rs::color_spec`, swap glyphs via `caps::active_glyphs()`. No color or
  glyph literal at a draw site.
- **`thegn-core` is substrate-free** (no tokio/termwiz/portable-pty/HTTP/forge
  SDK) and 95%-line covered.
- **Seams, not vendors.** Every backend is a provider seam (`thegn_core::seam`):
  object-safe trait, caps ⇔ optional ops, `kind` implemented-or-`reserved`,
  `Probe` in `thegn doctor`. Vendor CLIs (`gh`, `glab`, …) only inside their
  implementation files.
- **One capability catalog.** Control API, gRPC, CLI verbs, MCP tools and
  plugin host calls project `thegn_core::capability::CATALOG`.
- **git is the source of truth** for worktrees, the forge for PRs; SQLite is a
  cache + resurrection layer.

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
  / `thegn::hydrate=debug`. **No _sink_ is installed when `THEGN_LOG` is unset**
  — no file, no stderr layer, no per-frame work, no I/O. What IS always on is a
  minimal diagnostics layer holding a fixed-size in-memory WARN+ ring (plus the
  `thegn::panic` target) reused for crash reports and the debug bundle: it does
  **zero I/O until a crash report or bundle reads it**, adds no wake source, and
  its per-layer `LevelFilter::WARN` leaves every sub-WARN callsite a
  cached-interest check — the same order of cost as no subscriber at all, so
  instrumentation stays free at idle. See `thegn_core::diagnostics` +
  `log_trace::install`.
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
  thread, handed back over a channel). **That number is Linux-specific** —
  FSEvents registers in O(1), so on macOS the off-thread build is justified by
  its `git rev-parse` calls instead, not by watch registration. Don't "optimize"
  it back onto the loop after measuring on a Mac.
- **Thread QoS (`platform::qos`) is how off-loop work stays off the performance
  cores on Apple silicon.** The render/input loop declares `Interactive`; every
  worker off it declares `Utility` (user-visible, not blocking — model
  hydration) or `Background` (housekeeping — samplers, ticker, fs-watch
  registration). A no-op off macOS. New long-lived threads should declare a
  class; the default is `Interactive`, which for background work is wrong.

## Source map

`docs/ARCHITECTURE.md` §1 has the crate map. Entry points: `crates/thegn-host/src/main.rs`
(clap tree; subcommands in `src/cmd/`), `src/run.rs` (the event loop), `src/handlers/`
(channel-drain handlers), `crates/thegn-core/src/config.rs` + `config/config.toml.example`
(every key, documented), `openspec/specs/` (contracts), `docs/superpowers/{plans,specs}/`
(dated design records).

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
just test            # unit tests (nextest); the pre-push gate
just test-doc        # doctest pass — CI-only, see the note below
just smoke           # hermetic end-to-end CLI test
just lint            # clippy -D warnings + shellcheck + yamllint + taplo
just coverage        # cargo llvm-cov, gated at 95% lines on the core
just bench           # startup benchmarks (hyperfine; not part of ci)
just start name=dev  # run the host with an isolated XDG_STATE_HOME
just ci              # lint (fmt + ratchets) + deps-audit + build + cross/feature/msrv checks + test + coverage + smoke + term-check + nix-build (no e2e)
just ci-local        # ci + e2e
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
  instrumented recompile), cross/feature/MSRV checks, docs, term-check,
  nix-build, and `just test-doc`. e2e is in `just ci-local` only (opt-in in CI —
  see the e2e note below), so `just ci` is green-able on a clean checkout. Run
  `just coverage` locally on demand before a PR if you want the gate early.

**`just test` is nextest only — doctests are `just test-doc`, CI-only.** The doc
pass is a THIRD full-workspace compile (after clippy's and nextest's) and this
repo has no runnable doctests to show for it: all ~10 doc fences are
` ```text ` / ` ```ignore ` / ` ```sh ` — diagrams and shell recipes, not
assertions. It stays in `just ci` and the CI `doc` job, so a genuinely runnable
doctest added later is still gated; it is just no longer paid for on every push.

**A `PreToolUse` hook enforces this policy for AI agents** (`.claude/settings.json`
→ `test/heavy-guard.sh`): the full-workspace gates are refused with a pointer to
the scoped equivalent. Deliberate pre-push runs go through unchanged as
`THEGN_ALLOW_HEAVY=1 <command>`. The prose above kept losing to habit — several
worktrees each running a full compile is precisely what pins all cores and
drives the box into swap, so the policy is now mechanical. The git hooks are
outside the harness and unaffected either way.

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

**Disk hygiene — the build output is the biggest thing on the machine.** A
worktree-per-agent workflow keeps one populated `target/` per worktree, and an
audit (2026-08-26) found ~101 GiB across 8 copies of substantially the same
crate graph. Three things now hold it down, and they are defaults, not knobs:

- **`[profile.dev.package."*"] debug = 0`** — dependency debuginfo is off.
  Measured on this workspace: `target/` 4.04 → 3.05 GiB (−24.5%) for an
  unchanged build time, and it multiplies across every worktree at once.
  thegn's own crates keep `[profile.dev]`'s `line-tables-only`, so **our**
  backtraces still carry file:line; what is lost is file:line for frames inside
  a dependency (symbol names remain, and release builds were already
  `strip = true`). Don't "fix" this by dropping the override.
- **`[disk] idle_clean_days = 14` / `reclaim_on_low_disk = true`** — a worktree
  with nothing touched in it for two weeks has its `target/` reclaimed, and
  under real disk pressure (at/below `[stats] disk_free_critical`) the
  least-recently-touched `target/` dirs are evicted until free space is back
  above `disk_free_warn`. The active worktree, a running build, and (for the
  idle rule) uncommitted work are always exempt. Policy is pure and tested in
  `thegn_core::disk_reclaim`; it runs at the tail of the background disk scan.
  `[disk] warn_threshold_gb` is a `thegn disk` **reporting** threshold only — an
  absolute total is permanently red on a machine like this, so nothing behaves
  off it.
- **`just clean-aux`** — `just coverage`, `just check-cross` and `just doc` each
  leave a whole extra crate graph in `target/` (llvm-cov-target, the cross
  triples, `doc/`, `advisory-dbs/`: ~5 GiB here) that nothing ever reaps.
  `clean-aux` removes exactly those and keeps the warm `debug`/`release` build,
  unlike `just clean`.

**`[disk] shared_target_dir` is deliberately NOT the default.** Cargo holds an
exclusive flock on `target/<profile>/.cargo-lock` for the whole compile
(measured: a second `cargo build` blocks immediately and stays blocked), so one
shared target dir would serialize every dev-profile build/test/clippy across
every worktree — precisely the parallelism this tool exists for. It is a fine
opt-in for a single-worktree machine and a bad default here.

**One ceiling for everything thegn starts.** `[sandbox.limits] cpu_total` /
`memory_total` bound a shared `thegn.slice` that interactive panes join at
spawn (`sandbox_cpucap::wrap_pane_argv`) **and** the two background jobs join
via `wrap_background_argv`: the fold gate (`integrate.rs`) and the queues' agent
handoff (`agent_run.rs`). Those two used to escape every cap — they are spawned
straight from the thegn process, so the aggregate bounded the panes and then a
full test suite ran on top of it. `memory_total` is a `MemoryHigh` watermark,
not `MemoryMax`: over the line the slice is throttled and reclaimed rather than
OOM-killed, which is what keeps one greedy build from stalling the whole machine
in global direct reclaim. The wrap is fail-safe in both directions — an
unpublished policy (any unit test) or an unusable `systemd-run` runs the job
exactly as before, because a cap that breaks the gate would silently blame a
good branch, which is worse than no cap. `thegn doctor` prints what is actually
in effect.

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
  call it from the loop. (The old per-file size limit was removed; the
  preference stands. What IS enforced now are the **architecture ratchets** —
  shrink-only allowlists in `test/*-ratchet.txt` checked by `just lint` and
  `just test`: platform `#[cfg]` outside `platform/`, color/glyph literals
  outside the caps chokepoints, `gh` calls outside the forge impl,
  `async fn` in provider traits, ignored `Result`s, and a guard that the idle
  loop never polls. Pay debt down and delete the entry; never add one without a
  reason in the file. `just ratchet-update` regenerates after a burn-down.)
- **Remote CI is TEMPORARILY OFF — the pre-push hook is the only gate.**
  `.github/workflows/ci.yml` is dispatch-only (its `push`/`pull_request`
  triggers are commented out, with the reasoning above the `on:` key) because a
  push to main cost ~101 billable minutes and a PR cost that again before its
  merge. So `cargo test` + `clippy` + `smoke` on pre-push is what protects main
  right now — do not disable it, and do not assume a green push means coverage,
  cross-compilation, docs, deps-audit, nix-build, sandbox-e2e or openspec were
  checked. Run the suite on demand with `gh workflow run ci.yml --ref <branch>`
  (add `-f extras=true` for the macos/windows/e2e opt-ins). The macOS job is on
  the same `extras` gate as those two, not disabled outright — the OOM that
  hard-disabled it (pnpm building `openspec`, a tool that job never invokes) is
  fixed by `devShells.ci` plus the memory caps in `nix/openspec.nix`. It is
  still opt-in at 10x cost, and **has never completed a run**; even green it
  only proves `just build && just test`. Make it unconditional only once darwin
  is supported rather than best-effort. The other cheap wins still stand —
  self-hosted runners (note the fork-PR security caveat documented in the
  workflow), lean dev shells for the cheap jobs, and tiering
  coverage/nix-build/check-cross off every PR.
- **e2e (`just e2e`) is a local gate; in CI it is temporarily opt-in.** The CI
  job kept hitting its 30-minute timeout, and the committed baselines are stale
  (last recorded in `0f9c5a9a`; `1726a8e1` changed the UI without re-recording,
  and all 45 baselines are `__linux`, so none exist for darwin) — so it gated
  nothing while costing half an hour a push. Run it in CI with a workflow
  dispatch (`-f extras=true`; there is no `[ci-e2e]` commit-message trigger);
  locally it is unchanged and still the gate for anything that alters a frame.
  **On a Mac it hard-fails** rather than skipping — `--ci` treats a missing
  baseline as a failure — which is why `just ci` as a whole doesn't pass on
  darwin. Fix the timeout AND re-record (both platforms) before making it
  blocking again.
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
