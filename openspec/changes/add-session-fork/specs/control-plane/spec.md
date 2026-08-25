# Control Plane

## ADDED Requirements

### Requirement: A live session forks into a new sibling session

The daemon SHALL fork a live session — capability `sessions.fork`
(`Verb::ForkSession`, same required scope as `sessions.open`, non-streaming
surfaces) — by opening a **new** session from the source's retained resolved
spawn recipe: same argv/cwd/env for raw-argv sessions, a freshly re-resolved
composition (command, sandbox, environment) for `agent:`-launched sessions,
with cwd/worktree overridable. The fork MUST be a new process with a new
session id and pid; the source session MUST be unaffected. The daemon SHALL
retain spawn recipes in memory only for the lifetime of the live session —
never persisted to the database or tombstones and never returned over the
API — so forking a dead session fails with a clear error naming
`sessions.open` as the alternative. Resource-cap wrapping MUST be re-applied
to the fork by the daemon regardless of how the source was capped.

#### Scenario: Fork re-runs the recipe

- **WHEN** a client forks a live session opened with argv `["npm","run","dev"]`
  in `/w/app`
- **THEN** a new session spawns running `npm run dev` in `/w/app` with a new
  session id and a different pid, and the source session keeps running
  untouched

#### Scenario: Agent launches re-resolve, never replay

- **WHEN** a session opened via an `agent:` launch is forked after the
  agent's credentials were rotated
- **THEN** the fork's environment is composed fresh at fork time (current
  config and credentials), not replayed from the source's spawn

#### Scenario: A dead session cannot fork

- **WHEN** a client forks a session that has exited
- **THEN** the daemon returns an error stating the session has exited and
  that `sessions.open` starts the command anew, and no process spawns

#### Scenario: Recipes never leak

- **WHEN** any control API response describes a session (listing, snapshot,
  fork result)
- **THEN** it never includes the retained env pairs, and the state database
  contains no spawn environment for any session

### Requirement: Forks carry lineage and optional scrollback context

A forked session's environment SHALL carry `THEGN_FORKED_FROM` (the source
session id) beside the standard identity variables, and `SessionInfo` SHALL
expose `forked_from` so listings and UIs can show lineage. When the fork
requests scrollback hand-off, the daemon SHALL write the source's retained
scrollback tail as plain text to an owner-only file under the per-profile
state dir, expose its path as `THEGN_FORK_SCROLLBACK` in the fork's
environment only, and best-effort delete it when the forked session exits.
The forked pane's screen MUST show only output the forked process itself
wrote — the source's output is never replayed into the new emulator.

#### Scenario: The fork can find its parent

- **WHEN** a forked session's process reads its environment
- **THEN** `THEGN_FORKED_FROM` names the source session, and
  `thegn session list --json` shows the fork's `forked_from`

#### Scenario: Scrollback rides a file, not the screen

- **WHEN** a fork is created with scrollback hand-off from a source with
  retained history
- **THEN** the fork's `THEGN_FORK_SCROLLBACK` names a 0600 file containing
  the source's scrollback tail, and the fork's terminal shows only the new
  process's own output

### Requirement: Fork placement and worktree fork compose existing flows

Forking from the CLI (`thegn session fork <id>`) or the UI (`fork-session`
action on the focused pane) SHALL place the fork through the existing adopt
intent — a running compositor grafts it as a split beside the source pane, or
a new tab on request; with no compositor attached the fork simply exists in
the daemon. A worktree fork SHALL first create a new worktree branched from
the source session's worktree via the existing worktree-creation path, then
fork the session with cwd and worktree remapped into it (relative cwd
preserved); a worktree-creation failure MUST leave the source session and
layout untouched, and a fork failure after worktree creation MUST report the
surviving worktree rather than deleting it. With the daemon disabled, fork
MUST degrade with a clear message that it requires the daemon.

#### Scenario: Fork lands beside its source

- **WHEN** the user invokes `fork-session` on a focused daemon-backed pane in
  a running compositor
- **THEN** the forked session is grafted as a sibling split beside the source
  pane

#### Scenario: Fork into a fresh worktree

- **WHEN** the user forks with worktree fork from a session whose cwd is
  `src/api` inside worktree `feat-x`
- **THEN** a new worktree is branched from `feat-x`, and the fork starts in
  the new worktree's `src/api`

#### Scenario: No daemon, clear answer

- **WHEN** `[daemon] enabled = false` and the user invokes fork
- **THEN** the action fails with a message naming the daemon requirement, and
  nothing spawns
