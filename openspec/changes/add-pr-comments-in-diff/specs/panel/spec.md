# Panel

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
anchor lines: a thread whose path and line match a rendered new-side diff line
appears directly beneath that line with its author, resolved state, and
comment bodies. Unresolved threads MUST render by default; resolved threads
MUST be available behind a toggle. A thread whose anchor is absent from the
rendered diff (outdated, or anchored to a deleted line) MUST still be shown,
collected at the end of its file, never silently dropped. Thread rows MUST be
selectable, and replying from a thread row MUST reuse the same reply composer
as the Conversation tab. File rows in the Files tab file list MUST carry an
unresolved-thread count when nonzero.

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

#### Scenario: A review thread appears under its diff line

- **WHEN** the user expands a file in the Files tab that has an unresolved
  review thread anchored to a line present in the rendered diff
- **THEN** the thread renders inline directly beneath that diff line, with its
  author and comment bodies, and its row can be selected to reply

#### Scenario: An outdated thread is not dropped

- **WHEN** an unresolved thread's anchor line does not appear in the rendered
  diff
- **THEN** the thread is rendered in an outdated-feedback block at the end of
  its file rather than being omitted

#### Scenario: Resolved threads stay out of the way

- **WHEN** a file's threads are all resolved and the resolved toggle is off
- **THEN** no thread bodies render inline, and toggling resolved threads on
  renders them marked as resolved

## ADDED Requirements

### Requirement: A review thread can be handed to an agent

From a selected review-thread row in the PR view (Files or Conversation tab),
thegn SHALL offer a handoff action (`p` for the selected thread, `P` for all
unresolved threads) that formats the thread — pull request
context, `path:line`, the anchoring diff hunk, and every comment — as an agent
prompt. When the worktree's remembered agent pane is running, the handoff MUST
insert the prompt into that pane's input via bracketed paste without a
trailing newline, so the user reviews before submitting, and MUST target only
that worktree's own agent pane. When no agent pane is running but a headless
agent resolves from configuration, the handoff SHALL offer a confirm-gated
single-thread headless dispatch through the shared agent-task engine, run off
the event loop, carrying the PR task family's rules (the agent pushes with
`--force-with-lease` and never merges, approves, or resolves threads). With
neither available, the action MUST report why and do nothing — the view never
hard-depends on an agent.

Prompt text built from comment bodies MUST be sanitized before entering a PTY:
control bytes other than newline and tab MUST be stripped so an embedded
escape sequence cannot execute, and the bracketed-paste terminator cannot be
smuggled in a comment body.

#### Scenario: Thread pasted into the live agent pane

- **WHEN** the user presses the handoff key on an unresolved thread row while
  the worktree's agent pane is running
- **THEN** the formatted thread prompt is bracket-pasted into that agent
  pane's input without a trailing newline and focus moves to the pane

#### Scenario: Headless dispatch when no pane is running

- **WHEN** the user presses the handoff key with no agent pane running and a
  headless agent configured, and confirms
- **THEN** a single-thread review task is dispatched off the event loop in the
  PR's worktree, and its completion is reported in the status line

#### Scenario: No agent, no surprise

- **WHEN** the user presses the handoff key with no agent pane running and no
  headless agent configured
- **THEN** the status line explains what is missing and nothing else happens

#### Scenario: A hostile comment body cannot escape the paste

- **WHEN** a thread comment body contains terminal escape sequences or the
  bracketed-paste terminator
- **THEN** the pasted prompt reaches the pane with those bytes stripped or
  neutralized, as literal text only
