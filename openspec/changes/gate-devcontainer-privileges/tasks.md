# Tasks

## WS07-P0-05

- [x] Add privilege request and origin types.
- [x] Share engine socket paths with the Compose guard without changing its
      protected path order or classes.
- [x] Refuse engine socket and thegn control/state directory bind mounts,
      excluding them from generic mount approval.
- [x] Document the refusal and add regression coverage.

## WS07-P0-04

- [ ] Inspect merged configuration and created-container state using the shared
      privilege request types.

## Validation

- [ ] Run focused `thegn-core` tests and `openspec validate --all --strict`.
