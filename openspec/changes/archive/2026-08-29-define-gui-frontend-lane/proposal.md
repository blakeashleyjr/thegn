# Native GUI frontend lane: publish the decision, build nothing

Linear: THE-40

## Why

“Native GUI” can mean an application wrapper, a GPU cell client, or a second
widget frontend. Those shapes have different architecture and cost. The
project needs a durable direction before it chooses a toolkit or expands a
runtime dependency closure.

The existing substrate makes a separate cell client plausible: the daemon
owns PTYs, attach supports observer and interactive subscribers, control-v1
publishes catalog-backed operations, and serve mode has pairing and
exact-origin CORS. It does not yet publish a complete client-facing binary
frame compatibility contract or a serializable layout/chrome model.

## What Changes

- Publish the dated THE-40 decision record: no GUI is built now.
- Preserve candidate 2, a separate GPU terminal-cell client of the daemon and
  control API, as the preferred future native lane.
- Keep native chrome gated on a stable, serializable view model and keep web
  access in the separate THE-39/remote-access lane.
- Record the one-catalog, 0%-idle, shell-independence, substrate-boundary, and
  existing-security-edge invariants.
- Define THE-40-F1, an observer cell-client contract/fixture spike, as the
  smallest follow-up.
- Archive this synchronized OpenSpec change as a completed decision record.

## Capabilities

### New Capabilities

<!-- None: this is an architecture decision, not shipped behavior. -->

### Modified Capabilities

- `architecture-gates`: records constraints for a future graphical frontend
  without changing the current gates or claiming an implementation.

## Impact

- **Roadmap:** J 127 is the adjacent optional web-terminal lane; THE-40 adds no
  native-GUI roadmap row, leaving any AP placement to a separate product
  audit.
- **Documentation:** adds
  `docs/superpowers/specs/2026-08-29-native-gui-frontend-lane-design.md` and
  archives this OpenSpec record.
- **THE-34 coordination:** adopts its per-connection filter and opt-in lag
  vocabulary; does not introduce another `events.subscribe` protocol or a
  competing state frame.
- **Follow-up:** THE-40-F1 publishes and fixture-tests the existing observer
  pane stream before a toolkit or GUI product is chosen.
- **No implementation:** no code, crate, dependency, route, catalog row,
  schema, config, database, migration, roadmap, test, or ratchet changes.

## Non-goals

- Building or prototyping any GUI, terminal wrapper, webview, or native-widget
  frontend.
- Designing a new event subscription, server-side layout, or chrome model.
- Editing `deny.toml`, crate-boundary tests, `docs/ARCHITECTURE.md`, or the
  roadmap. Those are future or separate implementation/product-audit work.
