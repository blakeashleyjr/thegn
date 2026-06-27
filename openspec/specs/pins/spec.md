# Pins

## Purpose

Pinned programs are daemon panes that live independently of tabs and visibility —
a top strip and tabbar chips host long-running helpers (proxy, monitors, scratch
shells). A supervisor owns their lifecycle, restarts them per policy on exit, and
resurrects them across restarts.

## Requirements

### Requirement: A supervisor owns daemon panes across tab and workspace switches

A `PinSupervisor` SHALL own pinned program panes independently of the active tab or workspace, MUST keep them alive across tab/workspace switches, and MUST resurrect them from persisted pin state on restart.

#### Scenario: Pin survives a workspace switch

- **WHEN** the user switches workspaces
- **THEN** running pins keep running, owned by the supervisor

#### Scenario: Pins resurrect on restart

- **WHEN** superzej restarts
- **THEN** previously running pins are resurrected from persisted pin state

### Requirement: Launch-or-focus with restart-on-exit policy

A pin keybind SHALL launch the pin if not running or focus it if running, and on PTY exit the supervisor MUST apply the pin's `on_exit` policy (never / always / on-failure).

#### Scenario: Toggle a pin

- **WHEN** the user invokes a pin's keybind and the pin is not running
- **THEN** the pin launches; invoking it again focuses the running pin

#### Scenario: Pin dies and restarts

- **WHEN** a pin with `on_exit = always` exits
- **THEN** the supervisor restarts it

### Requirement: Pins have scope and location

Each pin SHALL declare a scope (global or workspace) and a location (strip or float), and the supervisor MUST dedupe singletons by name on summon.

#### Scenario: Workspace-scoped pin

- **WHEN** a workspace-scoped pin is summoned
- **THEN** it is associated with the matching workspace session, not globally
