# Panel

## Purpose

The right panel is the in-process information surface for the focused worktree and
the developer's broader work. It is organized into switchable tabs and sections
(git changes, PR/CI, issues, jobs, tests, problems, and a cross-repo "My Work"
view), aggregates issues across multiple trackers, and hydrates its data off the
event loop so it never costs idle CPU.

## Requirements

### Requirement: Tabbed panel with switchable sections

The right panel SHALL present switchable tabs (e.g. Git / Work / System) each containing sections, and switching a tab or section MUST NOT recompute chrome geometry.

#### Scenario: Switch panel tab

- **WHEN** the user switches the panel tab
- **THEN** the panel renders that tab's sections without resizing the panel or
  other chrome

### Requirement: Issues aggregate across multiple trackers

The panel SHALL aggregate issues from all active tracker providers (the `IssueRouter` fans out and concatenates), and a failing provider MUST contribute an empty result rather than breaking the others.

#### Scenario: Two trackers configured

- **WHEN** two issue providers are active and one errors
- **THEN** the Issues section still shows the working provider's issues and logs
  the failure

### Requirement: Cross-repo "My Work" surface

The panel SHALL provide a cross-repo `Mine` section listing actionable work (assigned issues, review-requested PRs, high-priority unread notifications); selecting a row MUST jump to its linked worktree, or offer to create one from the issue when none is linked.

#### Scenario: Row links to an existing worktree

- **WHEN** the user selects a My Work row whose issue is linked to a worktree
- **THEN** the session switches to that worktree

#### Scenario: Row has no worktree yet

- **WHEN** the selected row's issue has no linked worktree
- **THEN** the panel offers to create a worktree from the issue

### Requirement: Panel data hydrates off the event loop

Panel data refresh SHALL run off the loop (background worker → channel → `TerminalWaker` pulse) with no polling timeout, preserving the ~0% idle invariant.

#### Scenario: Background refresh

- **WHEN** panel data is refreshed
- **THEN** the work runs off-loop and wakes the loop only when new data is ready

### Requirement: Content tabs with in-panel preview and auto-expand

The panel SHALL expose content tabs (e.g. DIFF, FILES, PR, CHECKS, TESTS) and render file/diff previews in-panel (syntect highlighting, no pager subprocess); drilling into a diff or preview MAY auto-expand the panel and MUST retract it on exit.

#### Scenario: Open a file preview in-panel

- **WHEN** the user opens a file from the FILES tab
- **THEN** it renders in the panel via in-process highlighting, without spawning a
  pager subprocess

#### Scenario: Auto-expand on drill

- **WHEN** the user drills into a diff or preview
- **THEN** the panel widens for reading and retracts when the user exits the
  drilled view
