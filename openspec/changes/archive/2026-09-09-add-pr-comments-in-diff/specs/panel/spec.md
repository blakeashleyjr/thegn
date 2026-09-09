# Panel — PR review feedback delta

## MODIFIED Requirements

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

## ADDED Requirements

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
