# Skills

## ADDED Requirements

### Requirement: Skills come from a bounded local registry

The system SHALL expose an offline registry containing the embedded `mq`,
`pipeline`, and `supervise` packages plus valid packages discovered from
`[skills].user_dirs`. User packages MUST be immediate, regular, non-symlink
`<name>/SKILL.md` children; built-ins MUST win duplicate names; and malformed,
oversized, or unsafe packages MUST be skipped with diagnostics.

#### Scenario: A user package is discovered safely

- **WHEN** a configured directory contains a regular `review/SKILL.md` whose
  bounded frontmatter name is `review`
- **THEN** it appears in `thegn skills list` unless a built-in already owns the
  name

#### Scenario: A symlink is not followed

- **WHEN** a configured package directory or `SKILL.md` is a symlink
- **THEN** it is skipped and cannot escape the configured root

### Requirement: Skill documents have a strict bounded contract

Every document SHALL be at most 256 KiB and begin with flat frontmatter capped
at 32 lines/8 KiB. The only accepted keys SHALL be `name`, `description`,
`harnesses`, `gate`, and `when`; names MUST be path-safe and match the package
directory; harness, gate, and phase tokens MUST belong to their closed
catalogs. Invalid documents MUST be diagnosed rather than partially accepted.

#### Scenario: Unknown metadata is rejected

- **WHEN** a skill declares an unknown frontmatter key, harness, gate, or seed
  phase
- **THEN** parsing reports the source and line and the package is not seeded

### Requirement: Eligible skills seed only into the selected worktree

`thegn skills seed` and automatic create/startup seeding SHALL write eligible
packages only into the selected worktree's harness-native project directories
(`.claude/skills`, `.agents/skills`, or `.pi/skills`). Eligibility SHALL require
the configured harness, current phase, typed feature gate, and exclusion list
to permit the skill. Thegn MUST NOT write harness home directories.

#### Scenario: Automatic create seeding is enabled by default

- **WHEN** a worktree is created with default `[skills]` configuration
- **THEN** create-eligible skills are seeded for its configured harnesses

#### Scenario: Automatic seeding is disabled

- **WHEN** `[skills] enabled = false`
- **THEN** create/startup perform no skill writes, while an explicit
  `thegn skills seed --worktree <path>` remains available

### Requirement: Reconciliation preserves user-owned content

Seeded files SHALL carry a managed version/content-hash marker. Reconciliation
MAY create absent files and replace unchanged managed files, but MUST preserve
unmarked, malformed-marker, or locally modified files with a diagnostic.
Retired managed files MAY be removed only when unchanged and both registry
discovery and destination survey are complete.

#### Scenario: A local edit wins

- **WHEN** a user edits a previously seeded `SKILL.md`
- **THEN** a later seed preserves it and reports the conflict

#### Scenario: Incomplete discovery cannot delete

- **WHEN** a configured registry directory cannot be read
- **THEN** seeding may update known entries but removes no retired managed file

### Requirement: Skills are inspectable without mutation

`thegn skills list` SHALL return deterministic metadata and diagnostics, and
`thegn skills show <name>` SHALL print the canonical document. Neither command
may write skill files; `seed` SHALL report deterministic per-file results.

#### Scenario: Listing is stable

- **WHEN** `thegn skills list --json` runs repeatedly against unchanged config
- **THEN** it returns the same name-ordered registry and diagnostics
