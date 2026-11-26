# Panel

## Purpose

The right panel is the in-process information surface for the focused worktree and
the developer's broader work. It is organized into switchable tabs and sections
(git changes, PR/CI, issues, jobs, tests, problems, and a cross-repo "My Work"
view), aggregates issues across multiple trackers, and hydrates its data off the
event loop so it never costs idle CPU.

## Requirements

### Requirement: Half-width commit history stays readable

The Git commits view at Half width SHALL use the full panel width for a
single-line-per-commit list containing the short hash, truncated subject,
author and age. The selected commit's changed files and line counts SHALL
render inline beneath the list. At Full width, the existing history and detail
columns SHALL remain available. Commit decorations with multiple refs SHALL
show the first ref and a count of the remaining refs. An empty stash whose
count is already known to be zero MUST NOT show a loading placeholder.

#### Scenario: Read commits at Half width

- **WHEN** the Git commits view is open at Half width
- **THEN** each history row stays on one line with its hash, subject, author and age, and the selected commit's files and line counts appear below

#### Scenario: Empty stash count is known

- **WHEN** the stash list is empty and the known stash count is zero
- **THEN** the view says `no stashes` instead of `loading stashes`

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

### Requirement: Issues list is team-scoped and opens a detail page

When the loaded issues span more than one team key (`THE` in `THE-12`), the Issues section SHALL show one team by default (the team of the issue linked to the active worktree, else the team with the most issues), MUST show the scope in the list, and `T` SHALL cycle each team and then all teams. `↵` SHALL open a detail page for the cursor issue that fetches the full issue from the tracker off the loop and shows its state, assignee, labels, priority, project, link, description and comments; linking the issue to the worktree MUST be its own key (`l`).

#### Scenario: Another team's issues are hidden by default

- **WHEN** the tracker returns issues from teams `THE` and another team, with more `THE` issues and no linked issue
- **THEN** the Issues section lists only the `THE` issues and labels the scope `[team THE]`

#### Scenario: Enter opens the fetched issue

- **WHEN** the user presses `↵` on an issue row
- **THEN** the panel shows the issue's cached fields at once, fetches the full issue off-loop, and shows its comments when the fetch lands, or says the fetch failed and keeps the cached fields

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
workflow happens inside `thegn` without a browser. The view MUST present the
PR's checks, conversation (comments + submitted reviews + review threads), and
unified diff, and MUST let the user act on the PR — merge, approve,
request-changes / comment reviews (each with a body), post a PR-level comment,
reply to a review thread, re-run failed checks, and post an inline review
comment anchored to a diff line. Opening the PR in the browser MUST remain
available (`o`) as an escape hatch.

The Files tab MUST render each expanded file's review threads inline at their
exact rendered new-side anchor lines. Unresolved threads MUST render by default;
resolved threads MUST be available through a view-local toggle. Missing,
outdated, deleted-side, and general feedback MUST remain visible in explicitly
unanchored buckets rather than being guessed onto a line or silently dropped.
Thread rows MUST be selectable for navigation, reply, IDE location, and agent
handoff. Files and panel review summaries MUST expose unresolved counts.

All GitHub writes MUST run off the event loop and, on completion, MUST trigger a
PR refresh that re-hydrates the panel cache and re-fetches the open view's data
so newly-posted comments/reviews become visible. The view's diff, conversation,
and complete identity-bearing review snapshot MUST load off the loop. Partial or
transient refresh failure MUST preserve the last matching complete snapshot and
render an honest stale/loading/unsupported status rather than crashing or
replacing known feedback with an authoritative empty result.

#### Scenario: Enter opens the PR view

- **WHEN** the `PR` section is focused for a worktree whose branch has an open PR and the user presses Enter
- **THEN** a full-screen PR view opens showing Overview / Checks / Conversation / Files tabs, and its review data loads asynchronously

#### Scenario: Post a comment from inside the app

- **WHEN** the user opens the composer in the PR view, types a body, and submits
- **THEN** the comment is posted via the forge seam off the loop, and after it lands the view re-fetches so the new comment appears

#### Scenario: Inline line comment

- **WHEN** the user expands a file in the Files tab, selects an added/context line, opens the composer, and submits a body
- **THEN** an inline review comment is posted on that new-side line, anchored to the PR head commit SHA

#### Scenario: A thread appears under its exact diff line

- **WHEN** an unresolved thread's path and line match a rendered new-side line in an expanded PR file
- **THEN** the thread renders directly beneath that line and its selectable row can reply, open the location, or be handed off

#### Scenario: Unanchored feedback is not dropped

- **WHEN** a thread is outdated, has a deleted/missing line, names an absent path, or has no path
- **THEN** it remains visible in an explicitly unanchored bucket and is never attached to a guessed line

#### Scenario: Resolved threads stay out of the way

- **WHEN** the resolved toggle is off
- **THEN** resolved thread bodies are hidden, and enabling the toggle renders them marked as resolved

#### Scenario: A refresh fails after a complete snapshot

- **WHEN** a transient refresh returns only part of the review data or fails
- **THEN** the last identity-matching complete snapshot remains visible with an honest status

#### Scenario: Browser escape hatch preserved

- **WHEN** the user presses `o` on the `PR` section or in the PR view
- **THEN** the PR opens in the system browser

### Requirement: Submodule changes render as pointer moves

Changes and drilled diffs SHALL identify submodules separately from files and
show old/new gitlink pointers plus forward, rewind, diverged, or unknown
direction. A bounded local commit summary MAY enrich the move when the objects
exist; absence SHALL degrade to pointers without fetching. Git numstat `-/-`
MUST NOT render as a zero-line edit.

#### Scenario: Offline pointer move stays useful

- **WHEN** the new gitlink object is unavailable locally
- **THEN** the panel shows old and new pointers and no network operation occurs

### Requirement: Panel staging treats gitlinks atomically

The staging UI SHALL offer whole-entry stage/unstage/restore for a submodule
and MUST NOT offer or submit a partial line patch.

#### Scenario: Whole-entry stage

- **WHEN** the user stages a submodule row
- **THEN** the recorded gitlink is staged atomically

### Requirement: Full-screen diffs separate local changes from PR-head feedback

The full-screen diff SHALL retain **Worktree** as its default source with local
and staging semantics unchanged. When a complete identity-matching review
snapshot is available, it SHALL offer an explicit **PR review** source that uses
the PR-head diff and renders exact inline threads plus outdated/general and
top-level feedback. Review feedback SHALL NOT be attached to local uncommitted
lines. Missing, stale, loading, and unsupported snapshots SHALL be labeled.

#### Scenario: The reviewer switches to PR review

- **WHEN** a matching complete review snapshot is available and the user switches the full-screen diff source
- **THEN** the view renders the PR-head diff with its anchored and unanchored review feedback

#### Scenario: The local worktree has drifted

- **WHEN** local uncommitted lines differ from the PR head
- **THEN** Worktree mode remains feedback-free and PR comments appear only in the separately labeled PR-review source

### Requirement: Review feedback can be handed to the active worktree's agent

From a selected review-thread row in the PR view, `p` SHALL format that thread
and `P` SHALL format all unresolved feedback with PR identity, location, diff
hunks, and comments. A recognized live agent pane in the active worktree SHALL
receive one bounded sanitized bracketed paste without a trailing newline, move
focus to that pane, and wait for human submission. The action SHALL NOT target a
focused arbitrary pane or another worktree.

With no live target, a configured headless agent/command SHALL require explicit
confirmation and dispatch the same selected or all-unresolved feedback through
the existing off-loop `PrReview` runner and sandbox/isolation-floor policy. With
no target, or on a fail-closed isolation-floor miss, the action SHALL report why
and do nothing. Remote text entering a PTY SHALL have terminal controls stripped
and embedded bracketed-paste terminators neutralized.

#### Scenario: Selected thread is pasted to a live agent

- **WHEN** the user presses `p` on a thread and a recognized agent pane exists in the active worktree
- **THEN** only that bounded thread prompt is pasted without a trailing newline and the pane waits for the user to submit

#### Scenario: All unresolved feedback is dispatched headlessly

- **WHEN** the user presses `P` with no live pane, a headless target resolves, and the user confirms
- **THEN** all unresolved feedback is dispatched off-loop as a `PrReview` task under the resolved isolation-floor policy

#### Scenario: No safe target exists

- **WHEN** neither target resolves or the headless isolation floor fails closed
- **THEN** status explains the missing or unsafe target and no agent launches

#### Scenario: Hostile remote text enters a live paste

- **WHEN** a comment contains terminal controls or an embedded bracketed-paste terminator
- **THEN** the controls are stripped and the marker is neutralized before one non-submitting paste is written

### Requirement: The panel separator drag grabs a two-column band

At the resting width the panel's width SHALL be resizable by dragging its
left separator with the mouse, persisting the settled width as the section's
memory and reporting it like the keyboard routes. The grab target SHALL be a
two-column band — the separator column plus the adjacent pane frame cell
beside it (the second vertical rule at that boundary) — with the extra cell
skipped whenever it is pane or drawer content (`hit_pane`); the separator
column itself always grabs. The divider SHALL hold its grab offset for the
whole drag, so it stays under the cursor instead of jumping to it on the
first sample. A press that never moves MUST change nothing: no width change,
no persist, and no width report — a bare click on the divider is a no-op.
`Esc` while the drag is in flight MUST cancel it, restoring the pre-drag
width and persisting nothing; Esc never half-applies.

#### Scenario: The band grabs the divider or the frame cell beside it

- **WHEN** the user presses on the pane frame cell adjacent to the panel
  separator while that cell is not pane or drawer content
- **THEN** the width drag grabs as if the divider itself had been pressed

#### Scenario: The extra cell yields to pane and drawer content

- **WHEN** the user presses on the band's extra cell in a row where a pane's
  or the drawer's content occupies it
- **THEN** the press reaches that content, not the width drag; only the
  separator column itself grabs there

#### Scenario: A click on the divider changes nothing

- **WHEN** the user presses the separator and releases without moving the
  pointer
- **THEN** no width is stored and the status line does not report a width

#### Scenario: Esc cancels a width drag

- **WHEN** the user presses `Esc` after dragging the separator to a new width
- **THEN** the panel returns to the width it had at the press and nothing is
  persisted
