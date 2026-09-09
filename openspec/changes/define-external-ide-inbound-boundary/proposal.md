# Define the external IDE inbound boundary

Linear: THE-104 (Active, Client API & Remote Access)

## Why

THE-17 delivered outbound `editor.open`: thegn asks an editor to open a project
or file. It did not define the reverse direction in which an IDE or OS link asks
thegn to focus/reveal state. Draft `thegn://` and extension claims must not be
published until their ownership, authentication, and platform behavior are
decided.

## What Changes

- Publish the current support boundary: outbound `editor.open` is supported;
  inbound URL schemes, OS handlers, and vendor extensions are unsupported.
- Produce a decision record comparing paired control-API clients, a strict
  `thegn://open` handler, and no inbound integration.
- If inbound is adopted, specify it in a follow-on implementation delta before
  advertising it; this change defines the minimum security/compatibility gate.

## Impact

- Specs: `ide-handoff` boundary and future-adoption gate.
- No runtime, OS association, auth, catalog, or schema mutation is authorized by
  this decision change alone.
- Origin: deferred inbound scope from completed THE-17.
