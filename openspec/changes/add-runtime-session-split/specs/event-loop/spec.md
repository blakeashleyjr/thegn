# Event Loop

## ADDED Requirements

### Requirement: Session ownership is delegated through a handle

The compositor loop SHALL route every structural mutation of the session —
splitting/closing panes, moving focus, adding/closing/switching tabs and groups,
opening a pane — through a `SessionHandle` seam rather than mutating the session
inline. The seam MUST have a local implementation that is behavior-identical to
today's in-loop session and a socket-backed implementation that forwards
mutations to the daemon; the loop MUST NOT know which one it holds. Handle
mutations MUST NOT block the loop: a socket-backed mutation is dispatched off the
loop and its result lands as a delta frame plus a `TerminalWaker` pulse, exactly
as pane bytes and git operations already do.

#### Scenario: A structural mutation goes through the handle

- **WHEN** the loop handles a split/close/focus/tab action
- **THEN** it calls the `SessionHandle` rather than editing the session tree
  directly, and the local implementation produces the same result as before

#### Scenario: A remote mutation never blocks the loop

- **WHEN** the handle is socket-backed and the loop issues a layout mutation
- **THEN** the loop does not await the socket inline; the mutation is dispatched
  off-loop and its applied result arrives as a layout-delta frame with a waker
  pulse, and the loop re-renders only on that wake

#### Scenario: Rendering from a streamed model preserves the render decision

- **WHEN** a socket-backed client receives a layout delta
- **THEN** the frame is classified as a full chrome repaint, pane-only output
  stays an incremental pane frame, and an idle wake still resolves to Skip — the
  render-decision invariants are unchanged by the split
