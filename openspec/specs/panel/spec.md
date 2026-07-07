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

### Requirement: Full in-app PR workflow view

The panel SHALL open a full-screen PR view when the user activates (Enter) the
`PR` section for a worktree that has a pull request, so the complete review
workflow happens inside `szhost` without a browser. The view MUST present the
PR's checks, conversation (comments + submitted reviews + review threads), and
unified diff, and MUST let the user act on the PR — merge, approve,
request-changes / comment reviews (each with a body), post a PR-level comment,
reply to a review thread, re-run failed checks, and post an inline review
comment anchored to a diff line. Opening the PR in the browser MUST remain
available (`o`) as an escape hatch.

All GitHub writes MUST run off the event loop and, on completion, MUST trigger a
PR refresh that re-hydrates the panel cache and re-fetches the open view's data
so newly-posted comments/reviews become visible. The view's diff and
conversation MUST load off the loop (never blocking it) and MUST degrade
gracefully — a failed or unauthenticated fetch leaves that pane empty/"loading"
rather than crashing the compositor.

#### Scenario: Enter opens the PR view

- **WHEN** the `PR` section is focused for a worktree whose branch has an open PR
  and the user presses Enter
- **THEN** a full-screen PR view opens showing Overview / Checks / Conversation /
  Files tabs, and its diff + conversation load asynchronously

#### Scenario: Post a comment from inside the app

- **WHEN** the user opens the composer in the PR view, types a body, and submits
- **THEN** the comment is posted via `gh` off the loop, and after it lands the
  view re-fetches so the new comment appears in the Conversation tab

#### Scenario: Inline line comment

- **WHEN** the user expands a file in the Files tab, selects an added/context
  line, opens the composer, and submits a body
- **THEN** an inline review comment is posted on that new-side line, anchored to
  the PR head commit SHA

#### Scenario: Browser escape hatch preserved

- **WHEN** the user presses `o` on the `PR` section (or in the PR view)
- **THEN** the PR opens in the system browser as before
