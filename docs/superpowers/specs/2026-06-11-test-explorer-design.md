# Test Explorer & Status — Design

Date: 2026-06-11 · Status: approved (design Q&A)

Extends the IDE feature tiers design
(`2026-06-10-ide-feature-tiers-design.md`, Tier 1 → Test explorer and
test status). This deepens the existing `PanelTab::Tests` surface into a
performant, helpful test explorer backed by a general task/result substrate.

## Problem

thegn already has a minimal Tests panel: it detects one test command for the
focused worktree, runs it on `r`, parses a flat list of pass/fail/skip lines, and
shows a panel-local summary. That proves the UI seam, but it is not yet an IDE
explorer:

- no target tree before a run;
- no run-selected, run-file, run-package, or run-failed actions;
- no per-worktree sidebar/statusbar rollups;
- no jumpable failure locations;
- no shared task substrate for Problems, Timeline, or future DAP handoff;
- no persisted latest result across restarts or tab switches.

The goal is a Tests surface that answers six questions quickly:

1. What test command will run here?
2. What tests exist?
3. What is currently running?
4. What failed?
5. Where do I jump to fix it?
6. Can I rerun only what matters?

## Goals

- Build the feature on a general task/result substrate instead of a one-off test
  runner.
- Keep discovery lazy and cached so worktree switches stay cheap.
- Preserve the event-driven host invariant: no idle polling, no blocking command
  execution on the render/input loop.
- Prioritize Rust/cargo, JS/TS, Python/pytest, and Go.
- Persist latest useful test state, not full historical logs.
- Let Tests feed other Tier-1/Tier-2 surfaces: Problems, Search Everywhere,
  Timeline, and DAP.

## Non-goals

- No automatic test runs on every file save.
- No full historical test timeline in this slice; that belongs to AQ 533–534.
- No mandatory LSP or DAP.
- No in-place code editing; failures open through the existing editor/panel
  handoff.
- No perfect framework support on day one.
- No unbounded output persistence.

## Decisions (user-approved)

1. **Task engine first.** Tests become the first visible consumer of a generic
   task/result substrate. The same substrate later feeds Problems, Timeline,
   Search Everywhere, and DAP launch/debug flows.
2. **Lazy/cache discovery.** Worktree switches only perform cheap manifest sniffing.
   Individual test targets are collected when the Tests panel opens or the user
   explicitly refreshes, then cached per worktree.
3. **Latest-result persistence.** Persist latest rollups/results per worktree so
   sidebar/statusbar state survives tab switches and restarts. Full history is
   deferred.
4. **First ecosystems.** Prioritize Rust/cargo, JS/TS, Python/pytest, and Go.
5. **Inherited tasks + aliases.** The task substrate should ingest tasks from
   existing runners/manifests and expose semantic aliases (`dev`, `test`, `lint`,
   `build`, `up`, etc.) rather than requiring every command to be hand-copied
   into `[[tasks]]`.

## Architecture

### Core task model

`crates/thegn-core/src/config.rs` gains a future `[[tasks]]` model shaped like
`[[pins]]`, not a new command DSL:

```toml
[[tasks]]
name = "cargo test"
kind = "test"
command = "cargo"
args = ["test", "--workspace"]
cwd = "."
scope = "worktree"
matcher = "cargo-test"

[[tasks]]
name = "vitest"
kind = "test"
command = "npm"
args = ["run", "test", "--", "--run"]
matcher = "vitest"
```

A task is a normalized named command with command/args/cwd/env/scope plus
optional kind/matcher metadata. `kind = "test"` makes it eligible for the Tests
panel; other kinds (`build`, `lint`, `run`, `custom`) become
Problems/Timeline/Search Everywhere inputs later.

Task specs come from three layers, highest priority first:

1. **Explicit config:** `[[tasks]]` entries in layered thegn config.
2. **Static provider discovery:** parse existing runner/manifests such as
   `Justfile`, `Makefile`, `Taskfile.yml`, `package.json`, `Cargo.toml`,
   `go.mod`, `pyproject.toml`, `docker-compose.yml` / `compose.yaml`,
   `flake.nix`, and `Procfile`.
3. **Semantic aliases:** stable names like `test`, `dev`, `lint`, `fmt`, `build`,
   `check`, `serve`, `up`, `down`, `logs`, and `shell` resolved over explicit and
   discovered tasks.

Discovery should be safe and cheap: parse files, do not execute arbitrary project
commands on worktree switch, and run only after explicit user action. Provider
rows should remain visible alongside aliases, e.g. `cargo:test`, `npm:dev`,
`just:test`, `compose:up`, plus `Run test` / `Run dev` aliases.

When no configured test task exists, thegn resolves the `test` alias from the
provider registry. The default priority should prefer explicit config, then common
local test runners:

1. `just test`
2. `Cargo.toml` → `cargo test --workspace`
3. `go.mod` → `go test ./...`
4. `pyproject.toml` / pytest → `pytest`
5. `package.json` → vitest/jest/npm script detection
6. `nix flake check` when no narrower test command exists

### Host task runner

A host-side task runner owns task lifecycle and result delivery. It must follow
the same pattern as the existing test runner and hydration jobs:

- spawn expensive work off the host loop;
- send task events/results over a channel;
- pulse `TerminalWaker` after sending;
- tag async results with worktree/generation;
- drop or cache stale arrivals instead of painting them into the wrong active
  worktree.

Conceptual types:

```rust
TaskSpec        // configured or detected command
TaskRun         // one execution attempt
TaskEvent       // lifecycle/output event
TaskResult      // exit status, duration, bounded output, parsed facts
TestTarget      // stable identity for one test/package/file/module
TestResult      // latest status and optional failure location
TestRollup      // counts and freshness for chrome/sidebar/statusbar
```

The first implementation can live in `thegn-host` while the pure config/data
shapes live in `thegn-core`. A later plugin/API version can expose the same
vocabulary through `plugin_api.rs` `ProgramAdapter`, `DataSource`, and
`NotificationSource` concepts.

### Tests panel consumer

`crates/thegn-host/src/panel.rs` already has `PanelTab::Tests`; it remains the
UX home. The panel should stop directly owning a single command and instead ask
the task runner to discover or run test tasks.

The panel owns UI state:

- current test task;
- discovery state;
- target tree cursor/collapse/filter/scroll;
- latest results by target;
- running task ID, if any;
- selected failure location.

The tree should reuse the Files tab pattern: a flat visible-row list derived
from a hierarchical model, with collapsed subtrees skipped during rendering.

## UX contract

The Tests tab should read like an IDE explorer while staying terminal-native:

```text
TESTS
cargo test --workspace        last: 42 passed · 1 failed · 3 skipped

▾ workspace
  ▾ crates/thegn-core
    ✓ config::tests::loads_layered_config
    ✓ db::tests::migrates_v6_groups
    ✗ activity::tests::acks_quiet_state
      crates/thegn-core/src/activity.rs:123
      assertion failed: expected quiet, got active
  ▸ crates/thegn-host
  ○ crates/thegn-svc

r run selected   R run all   f failed   o open   d debug   u refresh
```

Panel actions:

- `r` — run selected test/package/file/group;
- `R` — run all tests for the worktree;
- `f` — run failed tests;
- `u` — refresh target discovery;
- `o` / `Enter` — expand/collapse a node or open the selected failure location;
- `e` — open selected failure in `$EDITOR`;
- `d` — future DAP handoff, hidden/disabled until DAP exists;
- `j/k` and arrows — move;
- `/` — filter tests;
- `Esc` — return focus to center.

Per-worktree rollup glyphs:

- `✓` latest known tests passed;
- `✗` latest known tests failed;
- `…` tests running;
- `○` no known result;
- `!` discovery or runner error.

The active worktree statusbar can show a compact summary:

```text
tests: 42✓ 1✗ running: cargo test
```

Search Everywhere later indexes task/test IDs so the palette can show:

- Run all tests;
- Run failed tests;
- Run nearest test;
- individual test targets;
- failure locations.

## Data flow

```text
explicit config / static providers / aliases
        │
        ▼
Normalized TaskSpec registry
        │
        ├── lazy discovery request ─────► background worker
        │                                  │
        │                                  ▼
        │                              TestTarget tree
        │                                  │
        ▼                                  ▼
Tests panel ── run selected/all/failed ─► TaskRunner
                                           │
                                           ▼
                                  stdout/stderr + exit
                                           │
                                           ▼
                              parser/matcher pipeline
                                           │
                    ┌──────────────────────┼──────────────────────┐
                    ▼                      ▼                      ▼
              Tests panel              Problems panel        sidebar/statusbar
```

File changes mark cached discovery/results stale. They do not trigger automatic
collection or execution. Stale results remain visible and dimmed so the user does
not lose the last useful failure list.

## Framework matchers

Use a shared matcher interface rather than a monolithic parser:

```text
TestMatcher
├── discover(command, worktree) -> Vec<TestTarget>
├── parse_output(output) -> ParsedTestRun
└── failure_locations(output) -> Vec<FileLocation>
```

Initial matchers:

- **cargo**
  - fallback run: `cargo test --workspace`
  - discovery: `cargo test --workspace -- --list`
- **go test**
  - fallback run: `go test ./...`
  - discovery: package-aware `go test -list . ./...`
- **pytest**
  - fallback run: `pytest`
  - discovery: `pytest --collect-only -q`
- **JS/TS**
  - fallback run: configured script or detected npm/vitest/jest command
  - discovery: best-effort; prefer explicit `[[tasks]]` because JS package
    scripts vary heavily

Discovery is best-effort. If discovery fails, the Tests panel still supports
run-all, parses output, and builds an output-derived result tree.

Failure parsing captures:

```text
path
line
column optional
test target optional
message snippet
```

Only failing tests need jumpable locations in the first version.

## Performance model

1. **Cheap worktree switch.** Synchronous work is limited to cheap manifest sniffing
   or cached-state lookup.
2. **Lazy target discovery.** Expensive commands run only when the Tests panel
   opens, the user refreshes, or Search Everywhere asks for test targets and the
   cache is missing.
3. **No fs-watch test storms.** File changes mark tests stale; they do not run
   discovery or tests by default.
4. **Bounded output.** Capture enough output to parse summaries/failures, but do
   not persist unbounded logs.
5. **Generation tags.** Async discovery/run results carry worktree and generation.
   Late results update their own worktree cache but do not repaint the wrong
   active panel.
6. **Visible-row rendering.** Large trees render from cached visible indices;
   collapsed subtrees are not walked every frame.
7. **One runner per worktree by default.** Starting a new test task for a worktree
   should either cancel, supersede, or queue behind the existing run according to
   an explicit policy. The first version should prefer “supersede after confirm”
   for manual runs and “queue nothing automatically.”

## Persistence

The first version persists latest useful state only:

- discovered target cache;
- latest run summary;
- latest status per target;
- failure locations;
- stale/running/error markers.

This can be DB-backed for resurrection and per-worktree lookup, while high-churn
live-running state should remain in memory or in a lightweight snapshot model to
avoid WAL contention. Full historical runs and restore/compare belong to the
local timeline feature.

## Error handling

The panel should produce actionable states:

- **No task detected:** show “No test task detected” and suggest adding
  `[[tasks]]`.
- **Discovery failed:** keep run-all available; show the command and short error.
- **Command missing:** show command-not-found with task name.
- **Timeout/cancelled:** mark run stopped and preserve the previous latest result.
- **Huge output:** parse a bounded buffer and mark output as truncated.
- **Unknown parser:** show raw exit status and command summary without a target
  tree.
- **Stale result:** dim/mark stale, but keep jumps and failures visible.

## Roadmap mapping

| Feature                                | Roadmap                                  |
| -------------------------------------- | ---------------------------------------- |
| Test explorer tree                     | AQ 516                                   |
| Test status rollups                    | AQ 517                                   |
| Run/debug selected test                | AQ 518; DAP handoff later via AQ 525–528 |
| Named task registry                    | AQ 520                                   |
| Task lifecycle controls                | AQ 521                                   |
| Task output capture + problem matching | AQ 522                                   |
| Problems panel consumer                | AQ 519                                   |
| Search Everywhere provider aggregation | AQ 523                                   |
| Local timeline consumer                | AQ 533–534                               |

## Sequencing

1. **Task registry, providers, and aliases.** Add the core task model, static
   provider discovery, and deterministic alias resolution so existing runners are
   inherited before users write config.
2. **Task runner substrate.** Replace one-off test execution with a general
   off-thread task runner and bounded result events.
3. **Tests panel tree.** Convert the flat Tests panel into a lazy discovered tree
   with run-selected/run-all/run-failed actions.
4. **Parser/matcher upgrade.** Add framework matchers and jumpable failure
   locations.
5. **Rollups and cache.** Persist latest state and surface worktree rollups in the
   sidebar/statusbar.
6. **Palette integration.** Expose test/task actions and failure locations to
   Search Everywhere.
7. **Future DAP handoff.** Let selected test targets become debug launch inputs
   once AQ 525–528 exists.

## Testing / verification expectations

Implementation plans should include:

- pure config tests for `[[tasks]]` parsing/defaults;
- static provider discovery tests for Justfile, package.json, Cargo.toml,
  docker-compose/compose files, pyproject.toml, go.mod, and flake.nix;
- alias-resolution tests for `dev`, `test`, `lint`, `build`, `up`, and explicit
  config overriding discovered tasks;
- fixture parser tests for cargo, go test, pytest, jest, and vitest output;
- discovery command selection tests;
- target-tree build/collapse/filter tests;
- generation/stale-result tests;
- task runner lifecycle tests for pass/fail/cancel/truncate;
- panel navigation tests for run-selected, run-all, run-failed, open failure;
- sidebar/statusbar rollup rendering tests;
- DB/cache migration and resurrection tests if latest results are DB-backed;
- smoke fixture repo with one passing and one failing test, asserting the failure
  is jumpable by file:line.

All code must preserve the project invariants: 0% idle CPU, damage-tracked
rendering, no blocking I/O on the host loop, and an AI-free shell whose AI
features are strictly additive.

## Implemented: multi-ecosystem ingestion + resource discipline

The Test Explorer shipped well beyond the original cargo/go/pytest/jest/vitest
text matchers. See the matching design notes in
`2026-06-10-ide-feature-tiers-design.md`.

### Ingestion modes (`crates/thegn-host/src/testkit/`)

- **Text** (`panel::parse_test_output`) — cargo, go, pytest, jest/vitest, swift,
  ctest; the fragile baseline.
- **JSON** (`testkit/json.rs`) — libtest/nextest, dart/flutter, rspec, and the
  `nix flake show` enumerator. Precise pass/fail/skip + `file:line` from the tool.
- **Report** (`testkit/report.rs`) — JUnit XML (Maven/Gradle/sbt/PHP) and TRX
  (.NET), parsed from disk after the run via `TestTask.report_glob`.

`TestTask.ingestion` selects the parser in `task::parse_task_outcome`.

### Ecosystems detected (`task::detect_fallback`, lowest match wins last)

cargo (→ nextest when installed), go, pytest, dart/flutter, swift, elixir, ctest,
Maven, Gradle, sbt, .NET, erlang, zig, ruby (rspec/minitest), php, d, npm
(vitest/jest), and **Nix flake checks** as the lowest-priority fallback so a
polyglot repo still uses its language runner.

### Resource discipline (the hard constraints)

- **Never auto-runs.** Discovery is lazy (panel open / `u`); runs are explicit
  (`r`/`R`/`f` or the Search-Everywhere rows). The fs-watcher only ever
  `mark_stale`s — `TestPanelState::mark_stale` is the sole file-change effect.
- **Never pins a CPU.** Every run/discovery child is wrapped by
  `task::wrap_capped` in a `systemd-run --user --scope` (`CPUQuota`/`MemoryMax`/
  `Nice`) or `nice`/`ionice` fallback; knobs in `[limits]`
  (`test_cpu_quota`/`test_mem_max`/`test_nice`/`test_max_parallel`).
- **Single-flight + real cancellation.** A per-worktree slot registry kills the
  prior run's process group (`killpg`) when a newer run supersedes it.
- **Bounded + concurrency-capped.** Combined output capped at 256 KB read
  deadlock-free on threads; a global semaphore bounds concurrent jobs.

### Depth shipped

- Jump-to-failure opens `$EDITOR +line file` in the drawer (`o`/`e`/`Enter`).
- `d` produces a runner-specific DAP launch descriptor (handoff for AQ 525–528).
- Search Everywhere exposes "Run all tests" / "Run failed tests".

### Deferred follow-ups

- **Live per-test streaming** — the runner currently delivers one batched
  result; converting `run_capped` to emit incremental `TestEvent`s (tests flip
  green/red live) is a clean next step on the same channel.
- **Per-test timing + flaky detection** — add `duration_ms` to `TestNode` and
  populate from the JSON modes (libtest `exec_time`, dart `time`).
- **Inline assertion diff rendering** — currently the failure message surfaces on
  the status line; rendering it (and `left == right` diffs) under the node is next.
- **Opt-in watch** — a per-worktree toggle that, when on, `mark_stale`s on change
  and (optionally) enqueues one capped single-flight run; default stays off.
