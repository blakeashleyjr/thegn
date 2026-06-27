# Event Loop

## Purpose

superzej is a single process, single session compositor whose central event loop
must consume ~0% CPU when idle while still reacting immediately to PTY output,
input, filesystem changes, and background hydration. This is achieved by blocking
on terminal input with no timeout and waking the loop explicitly from off-thread
producers.

## Requirements

### Requirement: Block with no polling timeout

The event loop SHALL block on termwiz `poll_input(None)` with no tick and no timeout, and MUST NOT introduce a polling interval to detect background work.

#### Scenario: Idle process consumes no CPU

- **WHEN** there is no input, no PTY output, and no pending background work
- **THEN** the loop remains blocked and the process consumes ~0% CPU

### Requirement: Off-thread producers wake via the TerminalWaker

Every off-thread producer (PTY reader threads, model hydration, config/diff fs-watchers, the refresh ticker) SHALL deliver its result on a tokio mpsc channel AND pulse the TerminalWaker, and the loop MUST drain its channels on wake and re-render only when state is dirty.

#### Scenario: Background result wakes the loop

- **WHEN** a background task (e.g. model hydration) finishes and sends on its
  channel
- **THEN** it pulses the `TerminalWaker`, the loop unblocks, drains the channel,
  marks the relevant damage, and renders only the resulting change

#### Scenario: Waker pulse without dirty state

- **WHEN** the loop wakes but no damage channel is dirty after draining
- **THEN** the render decision is `Skip` and no frame is flushed

### Requirement: No blocking I/O on the loop

Blocking I/O — git, DB, or subprocess calls — SHALL NOT run on the event loop and MUST instead run off-thread and hand results back over a channel.

#### Scenario: Expensive setup runs off-thread

- **WHEN** an expensive operation is needed (e.g. recursive inotify registration
  on a large worktree, ~1s)
- **THEN** it is performed on a background thread and the result is delivered to
  the loop over a channel, never blocking the loop
