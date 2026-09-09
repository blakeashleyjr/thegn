# Browser preview loop (accepted v1)

Linear: THE-13 (Done)

## Why

thegn needed a safe way to discover local development servers, show their
status, open them in the user's browser, and perform bounded diagnostic
fetches. It did not need to become a browser or import browser identity.

## What Changed

- Added preview configuration and deterministic discovery/selection of local
  preview targets from detected ports.
- Added preview status and external-browser open flows, plus optional placement
  through the existing terminal-pane/tool mechanisms.
- Added a credential-free, size- and timeout-bounded `preview.fetch` diagnostic
  seam.
- Kept browser engines, installed-browser profiles, cookies, history, snapshot
  rendering, and DOM automation out of process and out of scope.

`browser.drive` was not delivered by this change: its advertised endpoint still
returns `Unimplemented`. Truthful removal or a real implementation is tracked by
THE-103 (`remove-browser-drive-stub`).

## Impact

- Specs: new `browser-preview` capability.
- Code: preview config/discovery, host fetch seam, and preview-facing UI actions.
- Safety: no ambient credentials, no embedded engine, and no idle polling.
- Follow-up: THE-103 owns the separate public API/catalog truth gap.

## Archive status

The accepted THE-13 v1 scope is delivered. Earlier drafts that proposed a
snapshot provider and browser automation were rejected during architecture and
are intentionally absent from this delta.
