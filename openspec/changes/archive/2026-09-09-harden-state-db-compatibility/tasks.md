# Tasks

## 1. Canonical authority — THE-95

- [x] 1.1 Replace the uninstalled-runtime early success with a typed refusal for
      an existing canonical shared DB before any DDL or version write.
- [x] 1.2 Classify canonical-path aliases as shared state; preserve explicit
      bootstrap semantics and unrestricted initialization only for proven-distinct
      noncanonical temp DBs.
- [x] 1.3 Add file-backed regression tests for uninstalled policy,
      `user_version` immutability, canonical-path aliases, fresh bootstrap,
      controller/client actors, schema pins, and temp DBs.

## 2. Per-operation compatibility — THE-96

- [x] 2.1 Model minimum readable/writable schema requirements independently of
      migration authority.
- [x] 2.2 Make the full/current opener explicit, require every older-compatible
      access to use the closed operation catalog, and reject empty/undeclared
      compatibility entries in tests.
- [x] 2.3 Make `land` and other degradable commands report omitted DB-backed
      guards/side effects explicitly; never silently weaken success semantics.
- [x] 2.4 Add compatibility-matrix tests across the supported schema window.
- [x] 2.5 Exercise compatible and incompatible operations while a controller
      races for the migration lease; compatibility access must never migrate.

## 3. Runtime truthfulness — THE-94

- [x] 3.1 Propagate typed schema refusal through hydration instead of discarding
      the error, while keeping generic DB failure distinct.
- [x] 3.2 Preserve the prior model, disable incompatible mutations, and render a
      sticky actionable banner with on-disk/build versions and restart remedy.
- [x] 3.3 Clear refusal state only after a successful hydration.
- [x] 3.4 Pair a file-backed newer-schema typed-refusal regression with the
      compositor chokepoint regression for refusal after a good model and
      subsequent compatible recovery.
- [x] 3.5 Add startup coverage proving that a first hydration refusal renders
      explicit unavailability rather than a successful empty model.

## 4. Evidence

- [x] 4.1 Update configuration, migration, and operator documentation for the
      delivered THE-95 authority and THE-96 compatibility contracts.
- [x] 4.2 Update runtime-refusal operator documentation with THE-94's final UI.
- [x] 4.3 Run focused state-db and host regressions under isolated state.
- [x] 4.4 Run OpenSpec strict validation.
- [x] 4.5 Run the full normal repository gate under isolated state (7,841
      workspace tests passed in the final combined remediation gate).
