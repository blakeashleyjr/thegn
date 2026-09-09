# Design — remove browser.drive stub

## Decision

Remove the row instead of implementing an automation engine. Current preview
architecture owns discovery, external open, and bounded fetch; it has no
browser session, navigation history, DOM, or automation provider against which
`navigate`, `reload`, or `back` could be defined honestly.

## Removal set

The catalog row and `Verb` mapping, HTTP/gRPC/CLI/plugin/MCP projection entries,
wire schema remnants, generated API snapshots/docs, and tests must change in one
atomic implementation. Generic capability coverage must remain green after
removal.

## Reintroduction gate

A future change must name the provider/session lifecycle, authorization scope,
URL confinement, timeout/cancellation behavior, public response semantics, and
all claimed transport implementations before adding the id back.
