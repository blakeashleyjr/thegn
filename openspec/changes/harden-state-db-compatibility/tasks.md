# Tasks

## 1. Canonical authority — THE-95

- [ ] 1.1 Replace the uninstalled-runtime early success with a typed refusal for
      an existing canonical shared DB before any DDL or version write.
- [ ] 1.2 Classify canonical-path aliases as shared state; preserve explicit
      bootstrap semantics and unrestricted initialization only for proven-distinct
      noncanonical temp DBs.
- [ ] 1.3 Add file-backed regression tests for uninstalled policy,
      `user_version` immutability, canonical-path aliases, fresh bootstrap,
      controller/client actors, schema pins, and temp DBs.

## 2. Per-operation compatibility — THE-96

- [ ] 2.1 Model minimum readable/writable schema requirements independently of
      migration authority.
- [ ] 2.2 Declare requirements at every shared-DB command/service entry point and
      reject undeclared access in tests.
- [ ] 2.3 Make `land` and other degradable commands report omitted DB-backed
      guards/side effects explicitly; never silently weaken success semantics.
- [ ] 2.4 Add compatibility-matrix tests across the supported schema window.
- [ ] 2.5 Exercise compatible and incompatible operations while a controller
      races for the migration lease; compatibility access must never migrate.

## 3. Runtime truthfulness — THE-94

- [ ] 3.1 Propagate typed schema refusal through hydration instead of discarding
      the error, while keeping generic DB failure distinct.
- [ ] 3.2 Preserve the prior model, disable incompatible mutations, and render a
      sticky actionable banner with on-disk/build versions and restart remedy.
- [ ] 3.3 Clear refusal state only after a successful hydration.
- [ ] 3.4 Add a running-compositor regression for a policy-bypassing schema
      advance and recovery.
- [ ] 3.5 Add startup coverage proving that a first hydration refusal renders
      explicit unavailability rather than a successful empty model.

## 4. Evidence

- [ ] 4.1 Update configuration, migration, and operator documentation.
- [ ] 4.2 Run focused state-db/host tests, OpenSpec strict validation, and the
      normal repository gate under isolated state.
