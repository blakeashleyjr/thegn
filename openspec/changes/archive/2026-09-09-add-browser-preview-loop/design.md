# Design — browser preview loop (accepted v1)

## Decisions

1. A preview is a URL derived from the existing forward/port model, not a new
   browser-session model.
2. Opening a preview delegates to the existing external-open or ordinary
   terminal-pane/tool seams. thegn never embeds a browser engine.
3. `preview.fetch` is diagnostic only. It carries no cookies or ambient browser
   state, has strict response-size and timeout bounds, and executes off the UI
   loop.
4. Discovery reacts to existing state changes; it adds no timer or idle poll.
5. Browser profiles, keychain access, snapshot rasterization, and DOM control
   are excluded.
6. The pre-existing `browser.drive` catalog stub is not evidence of delivered
   automation. THE-103 must either remove the advertised row or implement an
   honest contract.

## Delivered seams

- Core owns configuration, target selection, and bounded request planning.
- Host owns network I/O and delivers results through the normal async/waker
  path.
- UI surfaces consume preview state and existing placement primitives; they do
  not create a parallel pane lifecycle.

## Verification boundary

THE-13 is complete when discovery, selection, external open, and bounded fetch
remain covered by unit/integration tests and strict OpenSpec validation passes.
No browser automation or raster output is asserted by this change.
