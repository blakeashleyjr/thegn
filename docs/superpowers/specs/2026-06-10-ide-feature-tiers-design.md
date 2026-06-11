# IDE-Inspired Feature Tiers — Design

Date: 2026-06-10 · Status: proposed

## Problem

superzej is now a native terminal worktree IDE, not just a multiplexer. The
current roadmap already contains many IDE-shaped pieces — git panels, files,
palette, notifications, editor handoff, pins, session layouts — but the work is
spread across groups and a few graphical-IDE affordances are missing entirely.

This design adds a cross-cutting tier model so the roadmap can answer two
questions cleanly:

1. Which graphical-IDE features make the **AI-free shell** feel complete?
2. Which deeper language/runtime features should wait until the shell surfaces
   they depend on are already stable?

The tier model is an overlay on the existing Phase 0–5 roadmap. It does not
replace the phase plan, renumber existing backlog items, or change the core
product invariant: the shell must remain useful without AI, and AI features stay
additive.

## Tier definitions

### Tier 1 — IDE parity for the AI-free shell

Tier 1 is the set of IDE affordances that make superzej feel like a complete
terminal-native development workspace even when no agent/proxy/LLM layer is
configured.

Tier 1 features should:

- work with plain repositories, worktrees, shells, tasks, and external editors;
- reuse existing chrome surfaces: sidebar, palette, right panel, statusbar, and
  pane layouts;
- degrade gracefully when optional integrations are absent;
- avoid new idle polling or blocking work on the host event loop.

Included features:

1. Full git management
2. Test explorer and test status
3. Problems / diagnostics panel
4. Run/task configurations
5. Search Everywhere palette
6. Agent/process attention routing

### Tier 2 — deep language/runtime tooling

Tier 2 adds language-server and runtime-debugger integration after Tier 1 has a
stable task/panel/palette foundation. These features can be powerful, but should
not make superzej an editor or require language tooling for the shell to boot.

Included features:

1. DAP debugging
2. LSP navigation, symbols, and references
3. Local worktree timeline/history
4. Layout/task templates

## Tier 1 — IDE parity for the shell

### Full git management

**Roadmap mapping:** Y (319–330), plus GitHub Z (331–340) where PR workflow is
already represented.

The goal is to make the right panel a complete git workbench, not only a diff
viewer. superzej already has per-worktree status/diff, PR tracking, checks,
review/approve/create/merge through `gh`, and `lazygit` as a fallback. Tier 1
makes the native path coherent:

- stage/unstage files, hunks, and eventually lines;
- commit, amend, sign, and hooks-aware commit flows;
- branch create/delete/rename/checkout with worktree awareness;
- merge/rebase/cherry-pick/revert affordances;
- conflict list and conflict-resolution UI;
- stash preview/apply/pop/drop;
- log/graph/blame views that feed review and history surfaces.

Architectural seam:

- `crates/superzej-svc/src/git.rs` remains the service trait boundary.
- Native reads prefer gix; mutating writes can delegate to CLI git first.
- UI lives in `crates/superzej-host/src/panel.rs` and
  `crates/superzej-host/src/chrome.rs` as panel drill-ins/actions.
- Background refresh stays in `crates/superzej-host/src/hydrate.rs`.

### Test explorer and test status

**Roadmap mapping:** new AQ items 516–518.

Tests are not yet a first-class roadmap concept. Tier 1 adds a test surface that
works for common project commands before any LSP/DAP integration exists:

- discover tests via configured commands/adapters;
- show a tree/list of test targets;
- run all tests, nearest test, file/module/package tests, and failed tests;
- surface pass/fail/running state per worktree;
- parse failure locations into jumpable file:line entries;
- optionally hand a selected test to DAP later.

Architectural seam:

- Add a future `PanelTab::Tests` following the existing `PanelData`/`PanelUi`
  pattern in `crates/superzej-host/src/panel.rs`.
- Test commands should use the same task registry as run configurations.
- Results hydrate off-thread and wake the host exactly like diff/check refresh.

### Problems / diagnostics panel

**Roadmap mapping:** new AQ item 519, later fed by LSP items 529–532.

Problems are compiler/linter/config/runtime diagnostics gathered into one panel:

- compiler errors from task output;
- linter output from configured diagnostics tasks;
- config/keybind validation problems;
- git/GitHub operation failures worth surfacing;
- later, LSP diagnostics.

Architectural seam:

- Add a future `PanelTab::Problems` beside Diff/Files/PR/Checks.
- Use `crates/superzej-core/src/plugin_api.rs` alert/data-source concepts as the
  vocabulary, even before external plugin transports are wired.
- Avoid turning diagnostics into a polling subsystem; diagnostics are produced by
  task completions, file-watch-triggered refreshes, or explicit user actions.

### Run/task configurations

**Roadmap mapping:** new AQ items 520–522, related to worktree templates (54),
pins (E), actions (M), and session/layout groups.

Graphical IDEs usually have run configurations. superzej's terminal-native
version should be a small, explicit task registry:

- named task with command, args, cwd, env, scope, and optional matcher;
- launch in current pane, split, tab, drawer, or pin;
- stop/restart/rerun controls;
- task output capture for Problems and Tests;
- pre/post hooks later through the event bus.

Architectural seam:

- `crates/superzej-core/src/config.rs` already has rich program-like config in
  `[[pins]]`; `[[tasks]]` should reuse that shape rather than inventing a new
  command DSL.
- Launching should reuse pure `LaunchSpec` composition from
  `crates/superzej-host/src/agent.rs` and pane spawning from the host.
- Palette entries should be generated from the task registry.

### Search Everywhere palette

**Roadmap mapping:** M (161–170) plus AQ item 523.

The existing command palette is the seed. Tier 1 turns it into a universal
navigation/action surface:

- actions and keybind-backed commands;
- workspaces/worktrees/tabs/panes;
- files and recent files;
- ripgrep/content results;
- tasks and pins;
- git branches/commits/PRs/issues where available;
- problems/test failures;
- later, LSP symbols/references.

Architectural seam:

- `crates/superzej-host/src/palette.rs` already provides fuzzy matching,
  frecency, rendering, and item dispatch.
- `crates/superzej-host/src/keymap.rs` should become the source for action rows
  instead of duplicating palette command entries.
- `crates/superzej-core/src/plugin_api.rs` already names `PaletteAction` as an
  extension point for future external providers.

### Agent/process attention routing

**Roadmap mapping:** AI (419–430), S (243–258), T (259), and AQ item 524.

Herdr-style attention state is useful, but superzej should generalize it beyond
agents so plain shell tasks and pinned processes can also ask for attention:

- worktree is active/running;
- worktree went quiet after activity;
- task failed;
- process exited non-zero;
- command awaits input/approval where detectable;
- PR checks failed or changed;
- user has not viewed the result yet.

Architectural seam:

- `crates/superzej-core/src/activity.rs` already provides the lightweight
  `none → active → quiet → acked` worktree FSM.
- Sidebar rollups live in `crates/superzej-host/src/sidebar.rs` and statusbar
  widgets in `crates/superzej-host/src/chrome.rs`.
- Notifications should grow through AI group items 420/421/430 and the plugin
  API `NotificationSource` surface, without requiring the AI proxy.

## Tier 2 — deep language/runtime tooling

### DAP debugging

**Roadmap mapping:** new AQ items 525–528.

DAP support should be a runtime/debug surface, not an editor replacement:

- launch and attach configurations;
- breakpoints;
- continue/pause/step over/step into/step out;
- threads and call stack;
- variables and watch expressions;
- debug console/output;
- run selected test under debug after Tier 1 test/task support exists.

Architectural seam:

- A future DAP client belongs in `crates/superzej-svc`, next to git/GitHub/SSH
  service boundaries.
- Debug views belong in right-panel tabs/drill-ins.
- Launch configurations should share the Tier 1 task registry.
- Debug actions should route through `keymap.rs` and the palette.

### LSP navigation, symbols, and references

**Roadmap mapping:** new AQ items 529–532, related to AG (405–410) and AF
(395–404).

LSP should provide navigation/context while `$EDITOR` remains the editing tool:

- go to definition/declaration/implementation;
- find references;
- document and workspace symbols;
- hover/signature/code-action preview;
- diagnostics feeding the Problems panel;
- symbols feeding Search Everywhere.

Architectural seam:

- A future LSP client should share a JSON-RPC-over-stdio substrate with DAP in
  `crates/superzej-svc`.
- Palette symbol providers live behind the Search Everywhere source model.
- Diagnostics flow into the Problems panel and attention routing.
- Opening/editing still hands off to `$EDITOR` through existing editor/pane
  paths.

### Local worktree timeline/history

**Roadmap mapping:** new AQ items 533–534, related to I (114/116), AD (381),
and AN (481–488).

A timeline answers “what happened in this worktree?” across git, files, tasks,
agents, and diagnostics:

- worktree created/selected;
- task started/exited;
- files changed;
- tests failed/passed;
- git commits/checkouts/rebases;
- PR checks changed;
- agent/process became active/quiet;
- local snapshots or restore points later.

Architectural seam:

- File-watch events already exist in hydration/diff refresh machinery.
- A future central event log in AN (481) should be the persistence substrate.
- Git history rides GitBackend log/blame work from full git management.
- The UI can start as a right-panel view or palette-filterable history list.

### Layout/task templates

**Roadmap mapping:** D (54), G (89/94/95/99/100), AM (480), and AQ item 535.

Templates should compose existing concepts:

- serialized center layout;
- tabs/stacks/splits;
- task set;
- pins;
- sandbox/container preset;
- optional per-worktree defaults.

Architectural seam:

- `crates/superzej-host/src/center.rs` already owns serializable `CenterTree`.
- `crates/superzej-host/src/session.rs` and `crates/superzej-core/src/db.rs`
  already persist worktree/tab state.
- Task templates depend on the Tier 1 task registry.
- The native `CenterTree` model is the target; do not extend legacy zellij KDL
  layout machinery.

## Roadmap mapping summary

| Tier | Feature | Existing roadmap | New roadmap |
| --- | --- | --- | --- |
| 1 | Full git management | Y 319–330, Z 331–340 | none |
| 1 | Test explorer/status | none | AQ 516–518 |
| 1 | Problems/diagnostics | O 188, AO 490 adjacent | AQ 519 |
| 1 | Run/task configurations | E, G, M, D 54 adjacent | AQ 520–522 |
| 1 | Search Everywhere | M 161–170, AF 395–404 | AQ 523 |
| 1 | Attention routing | AI 419–430, S 256, T 259 | AQ 524 |
| 2 | DAP debugging | none | AQ 525–528 |
| 2 | LSP navigation/symbols | AF/AG adjacent | AQ 529–532 |
| 2 | Local timeline/history | I 114/116, AD 381, AN 481–488 | AQ 533–534 |
| 2 | Layout/task templates | D 54, G 89/94/95/99, AM 480 | AQ 535 |

## Sequencing

1. **Foundation first:** task registry and palette/action unification.
2. **Tier 1 shell panels:** Problems and Tests follow the existing right-panel
   pattern.
3. **Git completion:** expand native git management in parallel with panels.
4. **Attention routing:** aggregate activity/task/problem/check signals into
   sidebar/statusbar/palette jump flows.
5. **Tier 2 substrate:** add shared JSON-RPC-over-stdio service substrate for
   DAP/LSP only after task/panel/palette surfaces exist.
6. **Timeline and templates:** compose the event log, task registry, and
   serialized layout model.

## Non-goals

- Do not make superzej a text editor; keep `$EDITOR` handoff central.
- Do not require LSP, DAP, or AI for the shell to be useful.
- Do not add idle render ticks, background polling loops, or blocking I/O on the
  host loop.
- Do not split the roadmap into a new phase taxonomy; tiers are an overlay on
  Phase 0–5.
- Do not reintroduce zellij/KDL layout machinery for new native template work.

## Testing / verification expectations

This document is planning-only. Implementation plans should add tests per slice:

- Git management: service trait tests and smoke coverage around safe CLI git
  writes in isolated repos.
- Tests/Problems panels: pure parser/state tests, panel navigation tests, and
  smoke flows for jumpable file:line diagnostics.
- Tasks: config parsing, command resolution, lifecycle transitions, and no loop
  blocking.
- Palette: provider ordering/frecency tests and action dispatch tests.
- Attention routing: FSM tests with injected snapshots/time and sidebar/status
  rendering tests.
- DAP/LSP: protocol fixture tests around the shared JSON-RPC client substrate.
- Timeline/templates: DB migration/idempotence tests and `CenterTree` serde
  round-trips.

All implementation work must preserve the project invariants from `CLAUDE.md`:
0% idle CPU, damage-tracked rendering, no blocking work on the host loop, and an
AI-free shell whose AI features are strictly additive.
