# External editor handoff (accepted outbound scope)

Linear: THE-17 (Done)

## Why

Users and remote clients need one reliable way to open a worktree, file, or
source location in their configured editor. The implementation needed to reuse
the editor resolver and safe daemon intent path rather than introduce editor-
specific commands.

## What Changed

- Extended the editor target model to include project roots as well as files
  with optional line/column positions.
- Added one provider seam used by the sidebar, diff/PR views, palette, and the
  public `editor.open` operation.
- Added strict request decoding, canonical containment, short-lived intents,
  off-loop planning, and existing external/pane placement behavior.

Inbound `thegn://` URLs, OS launcher registration, and IDE extensions were not
delivered or advertised. The decision and publication of that separate inbound
contract is THE-104 (`define-external-ide-inbound-boundary`).

## Impact

- Specs: `editor` and `ide-handoff`.
- Public API: `editor.open` is the canonical outbound operation on projected
  transports.
- No new auth table, bespoke socket, URL scheme, or CLI inbound door.

## Archive status

The bounded outbound scope accepted for THE-17 is delivered and archive-ready.
