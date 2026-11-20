# Prepare macOS OCI runtime behavior for evidence-based parity

Linear: THE-2099

## Why

The Apple `container` CLI and the Podman/Docker engines available on macOS are
not interchangeable with Linux Docker. Linux unit tests can validate generated
argv, but only an on-device run can establish whether `network none`, internal
networks, process limits, labels, and engine metadata behave as thegn assumes.
The existing spike is the decision input for the later runtime behavior change;
until those cases run, unsupported claims must remain refused or explicitly
unverified.

## What changes

- Add a private, collision-safe macOS evidence runner and hermetic self-test.
- Record the Apple and OCI runtime results needed by WS05-P0-02, P0-04, P0-05,
  and WS02-P3-08.
- Treat startup errors, missing positive controls, and timeouts as
  inconclusive. Only explicit network-denial results after successful controls
  may support the internal-network isolation decision.
- Keep runtime support decisions in a follow-up implementation change after
  the owner reviews the Mac evidence.

No sandbox enforcement is broadened by this change.
