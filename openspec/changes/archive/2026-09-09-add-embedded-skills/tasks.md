# Tasks — embedded worktree skills

## 1. Registry and validation

- [x] 1.1 Embed the `mq`, `pipeline`, and `supervise` packages and expose a
      deterministic core registry.
- [x] 1.2 Implement strict bounded frontmatter, package-name, harness, gate,
      and seed-phase validation with unit coverage.
- [x] 1.3 Discover bounded immediate user packages without following symlinks;
      keep built-ins authoritative and surface diagnostics.

## 2. Safe worktree seeding

- [x] 2.1 Implement marker/hash-aware pure reconciliation that preserves
      unmarked, malformed, modified, or incompletely-surveyed content.
- [x] 2.2 Map the closed harness catalog to project-local `.claude/skills`,
      `.agents/skills`, and `.pi/skills` destinations with path checks.
- [x] 2.3 Seed at existing worktree create/startup seams when enabled and use
      the same implementation for explicit seeding.

## 3. Surface and documentation

- [x] 3.1 Add `[skills] enabled`, `user_dirs`, and `exclude` validation and
      example configuration.
- [x] 3.2 Add `thegn skills list|show|seed`, JSON output, doctor visibility,
      user help, and command-drift tests for embedded prose.

## 4. Reconciliation gate

- [x] 4.1 Reconcile this change with the shipped worktree-seeding design and
      validate it with `openspec validate add-embedded-skills --strict`.
