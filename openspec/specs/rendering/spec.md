# Rendering

## Purpose

thegn composites its entire UI (PTY panes plus in-process chrome) to the
terminal through a damage-region compositor. Rendering must produce the cheapest
correct frame for any given change so that streaming pane output never forces a
full chrome recompose and an idle wake paints nothing. The frame decision is a
pure function so it can be exhaustively unit-tested and locked as a regression
gate.

## Requirements

### Requirement: Pure render-decision function

The render decision SHALL be computed by a pure function (`render_plan::plan`) from the three damage channels (`full`, `chrome`, `dirty_panes`), MUST NOT perform I/O or mutate state, and MUST return exactly one of `Skip`, `Panes`, or `Full`.

#### Scenario: Idle wake paints nothing

- **WHEN** the loop wakes with no damage on any channel
- **THEN** `plan()` returns `Skip` and no frame is composed or flushed

#### Scenario: Pane output does not recompose chrome

- **WHEN** only `dirty_panes` is set (PTY content changed, no chrome/geometry change)
- **THEN** `plan()` returns `Panes`, recomposing and bounded-diffing only the
  changed panes (via `Surface::diff_region`) and never recomposing chrome

#### Scenario: Chrome or geometry change forces a full frame

- **WHEN** any of the `chrome`, overlay, or `full`/geometry channels is set
- **THEN** `plan()` returns `Full`, running `render_tab` and a whole-screen
  `diff_screens`

### Requirement: Separable chrome and panes composition

`render_tab` SHALL compose center panes (`render_panes`) and chrome (`draw_chrome`) separately so that either MUST be able to repaint without the other.

#### Scenario: Chrome repaint leaves panes intact

- **WHEN** a chrome-only change occurs (e.g. statusbar update)
- **THEN** chrome is recomposed without requiring a recomposition of pane content
  from the emulator

### Requirement: Render-decision invariants are CI-enforced

The Skip/Panes/Full work-shape SHALL be covered by exhaustive unit tests that run in `just ci`, and a change that reintroduces a full recompose on pane-only output MUST fail those tests.

#### Scenario: Regression gate

- **WHEN** `cargo test` runs as part of `just ci`
- **THEN** the render-plan invariant tests execute and a violation of the
  Skip/Panes/Full mapping causes CI to fail

### Requirement: Chrome geometry is stable across tab and workspace switches

Chrome geometry SHALL be recomputed (`layout::compute`) only on startup, sidebar toggle, panel toggle, and terminal resize; tab/workspace switches, palette navigation, new/close tab, split/focus, and model hydration MUST reuse the current `ChromeLayout` so the panel width and tab-label region do not shift.

#### Scenario: Tab switch does not recompute geometry

- **WHEN** the user switches tabs or workspaces
- **THEN** the current `ChromeLayout` is reused and the right panel keeps its
  configured width

#### Scenario: Only toggles and resize recompute

- **WHEN** the sidebar or panel is toggled, or the terminal is resized
- **THEN** chrome geometry is recomputed

### Requirement: Tabbar background and label regions are separated

The tabbar SHALL fill its full width as background while drawing labels only within the center content rectangle (`tabbar_content()`, aligned to the center pane), so labels never flash in the sidebar-owned far-left columns.

#### Scenario: Labels align to center with sidebar visible

- **WHEN** the sidebar is visible
- **THEN** the tabbar background fills full width while tab labels draw within the
  center content region rather than the far-left columns

### Requirement: Dormant launch shows a splash without forking a PTY

On a genuine first launch or fresh workspace the center SHALL start dormant — a splash is drawn and no center PTY is forked until the first key or center click — and whenever no visible leaf has a live emulator the splash MUST replace the empty center while chrome still draws. Benchmark mode MAY bypass dormancy.

#### Scenario: Fresh launch is dormant

- **WHEN** thegn launches a fresh workspace
- **THEN** a splash is drawn and no center PTY is forked until the first input

#### Scenario: No live pane shows the splash

- **WHEN** no visible leaf has a live emulator
- **THEN** the splash replaces the empty center while chrome continues to draw

### Requirement: No stale cells survive a geometry change

When computed geometry differs from the previous frame, or a terminal resize is observed, the host SHALL force a full repaint (explicit screen clear + front-buffer diff) so that no cell from the previous geometry survives.

#### Scenario: Resize forces a full repaint

- **WHEN** a resize is observed (even a coalesced A→B→A drag that lands on the
  prior size)
- **THEN** the frame is fully repainted rather than diffed against stale contents
