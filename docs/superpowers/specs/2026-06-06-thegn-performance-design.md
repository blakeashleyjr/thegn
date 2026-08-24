# thegn performance and jank fix design

Date: 2026-06-06

## Goal

Make the native host (`thegn`) feel usable in a real terminal: fast first paint,
no visible blank/flash during startup, responsive input while panes are chatty, and
no ghosting/stale cells when overlays or layouts change.

This is a focused first pass. It keeps the current single-loop host and the
`termwiz` + `vt100` stack, then fixes the worst launch/render/event-loop costs. A
larger tokio app/render split and a higher-fidelity emulator remain future work.

## Observed problems

1. **Slow/blank launch.** `thegn` enters raw mode and the alternate screen before
   it has painted anything, then performs session resurrection, DB reads, branch
   resolution, recent-repo reads, and `diff_files("HEAD")` synchronously. Large
   repos can leave the alternate screen blank or half-painted while this work runs.
2. **Heavy repaint path.** Every dirty frame composes the whole logical screen.
   Pane composition walks every cell, asks the emulator for an owned `GridCell`,
   allocates cell text, and emits multiple attribute changes for each cell.
3. **PTY flood starvation.** The loop drains the PTY channel until it is empty.
   A chatty pane can keep the loop busy feeding the emulator and delay both input
   polling and rendering. The reader uses an unbounded channel and allocates a new
   `Vec` per read.
4. **Logical ghosting.** The scratch `Surface` is reused across frames without an
   explicit logical-frame clear. If a region stops being painted, stale cells can
   remain in the scratch frame and get diffed onto the real terminal.
5. **Slow shell spawn.** Panes launch `$SHELL -l`. Login shells run heavier startup
   files and can trigger user autostart logic. That is too expensive for a pane in
   a compositor unless explicitly requested.

## Chosen approach

Apply surgical fixes to the current host before rewriting the architecture:

- paint a cheap first frame immediately;
- hydrate git/DB/diff data asynchronously after first paint;
- bound PTY drain work per tick;
- clear the logical scratch frame on every render while keeping physical
  `BufferedTerminal` diff-flush;
- reduce obvious compositor allocation/attribute churn;
- use a non-login shell by default, with an opt-in login override.

This should improve the current binary quickly and produce testable seams for the
future tokio render-task architecture.

## Architecture

The host remains a single state-owning event loop for this pass. The loop gains
three small concepts:

1. **Cheap initial model.** Startup builds a `FrameModel` from in-memory session
   data only: tabs, active tab, interim panel/status text, and the configured
   accent. It does not run git status, git diff, or DB recents before the first
   frame.
2. **Hydration events.** A background worker computes the expensive model data and
   sends a `HostEvent::Hydrated(FrameModel)` into the loop. Failure produces a
   status/panel message rather than blocking launch or crashing the UI.
3. **Work budget.** Each loop drains only a bounded amount of PTY data before it
   renders and polls input. Remaining queued data is handled on the next tick.

The first pass may keep `std::sync::mpsc` for minimal churn, but the event type and
budgeting should make a later `tokio::mpsc` conversion straightforward.

## Components

### Startup model

Add a helper such as `build_initial_model(session: &Session) -> FrameModel`.

It fills:

- `tabs` and `active_tab` from the resurrected session;
- empty sidebar or `Loading…`;
- panel lines like current tab/worktree and `hydrating git status…`;
- status text that makes the host look alive immediately.

`build_model` becomes the slower hydration function and must not run on the main
startup path before first paint.

### Hydration worker

Add a small event enum, for example:

```rust
enum HostEvent {
    Pane(PaneEvent),
    Model(FrameModel),
    ModelError(String),
}
```

The worker performs:

- branch lookup;
- recent repo DB read;
- diff summary lookup/computation.

The main loop applies a received `Model` by replacing the relevant `FrameModel`
fields and setting `dirty = true`. On error, it updates `model.status` and keeps the
pane active.

### PTY drain budget

Replace the unbounded `while try_recv()` drain with a helper that stops after a
small budget, such as:

- max chunks per tick;
- max bytes per tick;
- or a short elapsed-time budget.

If the budget is exhausted, leave `dirty = true` and continue to render/input. This
keeps output floods from monopolizing the loop.

A bounded channel can be introduced in the same pass if it stays simple. If not,
coalescing/bounded channels are the next incremental step after the drain budget.

### Logical frame clearing

Before composing panes/chrome/palette into `scratch`, clear the logical frame:

- apply `Change::ClearScreen(theme_color(theme::BG0))` or an equivalent full-frame
  fill to the scratch `Surface`;
- then compose panes, chrome, and overlays;
- continue using `buf.draw_from_screen(&scratch, 0, 0)` and `buf.flush()` so the
  physical terminal still receives only the diff.

The key rule: clear the logical frame, never clear-and-redraw the real terminal.

### Compositor hot-path cleanup

Keep the current `PaneEmulator` trait for this pass, but reduce needless work:

- avoid allocating a new `String` for blank cells;
- cache/current-style tracking so consecutive cells with identical style do not
  emit duplicate fg/bg/intensity/italic/underline changes;
- write row text in runs where practical.

A larger API change (`row_cells` borrowing from the emulator, or direct vt100 row
rendering) is deferred unless the surgical cleanup is insufficient.

### Shell startup

Use a non-login shell by default. The preferred behavior is:

- if `THEGN_LOGIN_SHELL=1`, spawn `$SHELL -l`;
- otherwise spawn `$SHELL -i` when the shell supports it, or plain `$SHELL` if the
  shell path is unknown.

This avoids expensive login startup for normal panes while preserving an escape
hatch for users who rely on login-shell behavior.

## Data flow

1. `main` enters terminal mode and reads screen size.
2. Session is resurrected/seeded.
3. The loop starts with a cheap initial `FrameModel`.
4. The first visible frame is rendered immediately.
5. A hydration worker computes branch/sidebar/diff data and sends a model event.
6. The loop interleaves bounded PTY draining, model events, rendering, and input.
7. Pane output always advances the emulator; only visible output dirties the frame.

## Error handling

- Terminal setup or render flush errors still return `anyhow::Result` and restore
  cooked mode/alternate screen as the existing host does.
- Hydration errors are non-fatal and visible in status text.
- PTY read errors become pane exit events.
- PTY write errors remain fatal for the action that attempted the write, because a
  focused pane that cannot receive input is no longer usable.
- Background workers must not panic the UI thread; their errors are converted to
  model/status events.

## Testing

Add tests for the behavior that can be verified headlessly:

1. **Initial model is cheap.** A unit test verifies `build_initial_model` uses only
   session data and produces interim panel/status text.
2. **Hydration replacement.** A pure helper or event-application test verifies a
   hydrated model updates tabs/sidebar/panel/status and marks the frame dirty.
3. **PTY drain budget.** A unit test feeds more chunks than the configured budget
   and verifies the helper stops early and reports pending work.
4. **Logical frame clear.** A surface test renders content, then renders a smaller
   or empty state after a logical clear and verifies stale text is gone.
5. **Shell argv.** Unit tests cover default non-login shell argv and
   `THEGN_LOGIN_SHELL=1` login override.
6. **Regression gates.** Run `just fmt-check`, `just build`, `just test`,
   `nix develop --command just lint`, `just coverage`, and `just smoke`.

## Non-goals

- Replacing `vt100` with another emulator.
- Implementing image protocol passthrough.
- Completing mouse selection, drawers, or worktree pickers.
- Full tokio app/render task split.
- Persisting scrollback.

Those are valid future work, but they are not required to fix the current launch
slowness, flashing, and jank.

## Success criteria

- First frame appears before git diff/status hydration completes.
- A chatty pane cannot make keyboard input feel stuck for multiple frames.
- Toggling overlays/layouts does not leave stale text behind.
- Pane startup no longer pays login-shell cost unless explicitly requested.
- Existing CLI/smoke behavior remains green.
