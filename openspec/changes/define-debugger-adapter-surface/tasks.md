# Tasks — debugger adapter and DAP surface (THE-102)

## Decision

- [ ] Inventory debugger code, commands, docs, schemas, provider seams, plugin
      scopes, and reserved UI surfaces.
- [ ] Compare BugStalker-only, launch-adapter registry, native DAP, and plugin-
      hosted DAP options against security, portability, lifecycle, and UI cost.
- [ ] Decide adopt/defer/reject separately for launch adapters, DAP transport,
      and plugin debugger UI; publish rationale and support matrix.

## Conditional launch-adapter delivery

- [ ] If adopted, define trusted config schema, pure argv templates, platform/
      capability metadata, deterministic selection, and compatibility default.
- [ ] Implement CLI and doctor behavior with no UI-loop or shell execution.
- [ ] Add config trust, template, platform, resolution, run, attach, and compat
      tests plus documentation.

## Conditional DAP/provider delivery

- [ ] If adopted, define versioned lifecycle/wire, ownership, cancellation,
      bounds, failure recovery, scopes, and provider negotiation.
- [ ] Define host-rendered debugger surfaces, placement, actions, input ownership,
      cache/degradation, help context, and terminal safety.
- [ ] Add fake-adapter conformance, crash, timeout, malformed payload, permission,
      rendering-budget, and compatibility tests.

## Validation

- [ ] Ensure public docs/catalog/plugin schema state only adopted and delivered
      support; run strict OpenSpec and applicable full gates.
