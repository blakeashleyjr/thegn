# plugin-runtime — deltas

## MODIFIED Requirements

### Requirement: host.call is scope-checked and dispatched

A `host.call` request SHALL be checked against the plugin's declared `scopes`
using the capability catalog's `required_scope` before dispatch; a failing
check answers `RpcError` with code `denied`. The dispatchable set SHALL be
derived from the catalog — every row listing `Surface::Plugin` except
streaming rows — and dispatched generically through the same
capability-to-route spine `thegn api call` uses, off-loop over the daemon
control socket, so a newly routed catalog verb is callable by plugins with no
per-verb dispatch code. Capabilities outside the derived set answer code
`unsupported`; admin-scoped capabilities MUST remain unreachable by
construction (no admin row lists the plugin surface).

#### Scenario: An unscoped call is denied

- **WHEN** a plugin with `scopes = []` calls `{"cap": "sessions.list"}`
- **THEN** it receives an `RpcResponse` error with code `denied` and the
  audit log records the attempt

#### Scenario: A granted read call answers

- **WHEN** a plugin with the `read` scope calls `worktrees.list` while the
  daemon is up
- **THEN** it receives the worktree list as the `result`

#### Scenario: A newly routed verb needs no plugin-runtime change

- **WHEN** a catalog row listing `Surface::Plugin` gains its control route
- **THEN** a plugin with the required scope can `host.call` it immediately,
  with no new dispatch arm

#### Scenario: A granted git call performs the verb

- **WHEN** a plugin with the `git` scope calls `merge.add` for a worktree
- **THEN** the branch is enqueued exactly as via the HTTP surface, and a
  plugin holding only `read` receives `denied`

## ADDED Requirements

### Requirement: A resident plugin can subscribe to the control event feed

A resident plugin that declares an event-feed subscription SHALL receive
control feed events (activity, lease, session-list, exit, pairing) as
`on_event` notifications, gated by the `read` scope, delivered off-loop
through the plugin runtime's existing channel + waker path; pane byte streams
are never delivered this way. An undeclared or under-scoped plugin receives
nothing.

#### Scenario: A subscribed plugin sees an agent transition

- **WHEN** a `read`-scoped resident plugin has declared a feed subscription
  and a session's agent state changes
- **THEN** the plugin receives an `on_event` notification carrying the
  activity event, without any polling and without waking the idle render
  loop for a non-subscriber
