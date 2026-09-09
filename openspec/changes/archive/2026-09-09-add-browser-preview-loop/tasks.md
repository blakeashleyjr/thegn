# Tasks — browser preview loop (THE-13)

## Accepted v1 delivery

- [x] Add preview configuration with safe defaults and validation.
- [x] Derive preview targets from detected local ports/forwards without polling.
- [x] Expose preview status and external-browser open actions.
- [x] Reuse ordinary pane/tool placement for optional terminal preview tools.
- [x] Implement bounded, credential-free `preview.fetch` off the UI loop.
- [x] Test selection, invalid targets, response bounds, timeout/error paths, and
      zero-work disabled behavior.
- [x] Record the architecture exclusions: no engine, profile import, snapshot
      provider, or DOM automation.
- [x] Separate the unresolved `browser.drive` catalog stub into THE-103.
- [x] Reconcile this delta to the accepted v1 and validate it strictly.
