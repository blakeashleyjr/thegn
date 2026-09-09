# Remove the advertised browser.drive stub

Linear: THE-103 (Active, Client API & Remote Access)

## Why

`browser.drive` is projected as a public capability but every implementation
path returns `Unimplemented`. THE-13 explicitly did not deliver browser
automation. A discoverable operation that can never succeed is a misleading
API contract.

## What Changes

- Remove `browser.drive` from the capability catalog, verb mapping, transport
  routes/projections, schemas, and API/help output.
- Remove stub-specific tests and replace them with a gate that prevents a
  universally unimplemented operation from being advertised as supported.
- Preserve the implemented preview discovery/open/fetch surface.

Reintroducing browser automation later requires a new provider-backed OpenSpec
change with explicit commands, security boundaries, transports, and tests.

## Impact

- Specs: `capability-catalog`.
- Public API: removes a nonfunctional advertised operation; no working caller is
  broken because the operation has no success path.
- Origin: reconciles the outstanding API truth gap separated from THE-13.
