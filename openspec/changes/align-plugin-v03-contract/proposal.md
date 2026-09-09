# Align plugin API v0.3 documentation and host contract

Linear: THE-106 (Active, Plugin UI Platform)

## Why

The wire types have advanced to v0.3, including multi-row views, theme slots,
reserved `PanelSection`, and the independent `exec` scope. Documentation,
schema snapshots, loader negotiation, and runtime support statements must agree.
An enum variant is vocabulary, not proof that a host surface is wired.

## What Changes

- Establish one machine-readable support table for extension points, host verbs,
  plugin modes, and required surface capabilities/scopes.
- Align API version comments, developer/help docs, example manifests, generated
  JSON schema, loader negotiation, and `plugin check` output to v0.3.
- Document `exec` as independent of `write` and `git`, with `admin` implying all.
- Mark wired, separately implemented, and reserved extension points explicitly.
- Preserve v0.2 decode/negotiation compatibility and single-line view behavior.

PanelSection runtime is THE-108. Sidebar/theme/key-zone decisions are THE-107.
This change aligns truth; it does not implement either group.

## Impact

- Specs: `plugin-api` and `plugin-runtime`.
- Committed plugin schema and public plugin documentation/examples.
- No new extension runtime, render path, poll, or permission grant.
