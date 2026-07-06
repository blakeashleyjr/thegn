# Panel

## ADDED Requirements

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
