# Rendering

## Purpose

superzej composites its entire UI (PTY panes plus in-process chrome) to the
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
