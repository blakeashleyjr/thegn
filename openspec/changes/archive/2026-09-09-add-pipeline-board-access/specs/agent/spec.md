# Agent

## ADDED Requirements

### Requirement: The pipeline board has a door that does not depend on tab position

The agent-pipeline board SHALL be reachable by a single named action
(`open-pipeline-board`) that opens the system monitor directly on the board,
independently of where the board sits in the monitor's tab order.

The action MUST be a first-class action — a keymap variant with a round-tripping
id, an `ActionSpec` carrying a label, a hint and search keywords, a palette
entry, and a help page that both claims the id and mentions it in prose — so it
is rebindable, discoverable and gated like every other action.

Its default chord MUST be deliverable by a legacy-encoding terminal: it SHALL
NOT be a `Ctrl`+letter chord whose control code collides with an existing
control character, nor a `Ctrl`+digit.

Invoking it while the monitor already shows the board SHALL close the monitor;
invoking it while the monitor shows another tab SHALL move to the board rather
than closing. When the board is not present on this machine — no roster row and
no configured pipeline — the action SHALL say so rather than landing the user on
an unrelated tab with no explanation.

#### Scenario: The board is reachable when no digit indexes it

- **WHEN** every monitor tab family is present, so the board sits past the
  ninth visible tab and no digit key selects it
- **THEN** the `open-pipeline-board` action still opens the monitor on the board

#### Scenario: The same door closes what it opened

- **WHEN** the action is invoked while the monitor is already showing the board
- **THEN** the monitor closes

#### Scenario: An open monitor jumps rather than closing

- **WHEN** the action is invoked while the monitor is open on another tab
- **THEN** the monitor moves to the board and stays open

#### Scenario: No pipeline is honest about it

- **WHEN** the action is invoked with an empty roster and no configured stages
- **THEN** the board is not shown and the user is told why

### Requirement: The monitor hands back the keys it does not own

The system-monitor modal SHALL treat a key it does not implement as **not
handled** and let the global keymap have it, rather than consuming it silently.

Chords in the `Alt`/`Super` layer (including `Ctrl Alt …`) belong to the
compositor and MUST be handed back, so the chord that opened the monitor can
close it. The global key-lock chord MUST reach the key lock rather than closing
the monitor. `Ctrl-C` remains a close, and a plain `Ctrl` chord the monitor does
not implement remains consumed — the modal owns the keyboard except where it
explicitly does not.

#### Scenario: The opening chord toggles the modal shut

- **WHEN** an `Alt`-layer chord is delivered to the open monitor
- **THEN** the monitor reports the key as unhandled and the global keymap runs
  the action bound to it

#### Scenario: Key lock is not a close

- **WHEN** the key-lock chord is pressed while the monitor is open
- **THEN** the monitor stays open and the key lock toggles

### Requirement: The monitor reopens on the tab it was left on

The tab shown when the monitor closes SHALL be recorded as the tab the next open
lands on. Every path that moves the tab — cycling, the tab digits, and a direct
jump to a named tab — MUST record it, and the recording MUST reach the same
persistence path the monitor's other remembered preferences use.

#### Scenario: A tab switch is remembered

- **WHEN** the user switches the monitor to another tab and closes it
- **THEN** the next open shows that tab

### Requirement: A dispatch records when it was dispatched, in milliseconds

A roster row's dispatch timestamp SHALL be stored in the unit its column
declares (milliseconds). A row written now MUST read back as seconds old on the
board and MUST NOT read as blocked since the epoch on the sidebar.

#### Scenario: A fresh row is not two decades old

- **WHEN** a dispatch is recorded and the board renders it immediately
- **THEN** its age reads in seconds

### Requirement: The monitor is findable from the palette by any of its tabs

The system-monitor action's search keywords SHALL name every tab family it can
show, so a palette query for a family — containers, the container engines, the
pipeline board — finds the monitor.

#### Scenario: A tab name finds the monitor

- **WHEN** the command palette is queried for a monitor tab family by name
- **THEN** the system-monitor action is among the results
