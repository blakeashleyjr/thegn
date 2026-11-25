# Skills — everyday thegn skill delta

## MODIFIED Requirements

### Requirement: Skills come from a bounded local registry

The system SHALL expose an offline registry containing the embedded `mq`,
`pipeline`, `supervise`, `lead`, and `thegn` packages plus valid packages
discovered from `[skills].user_dirs`. User packages MUST be immediate, regular,
non-symlink `<name>/SKILL.md` children; built-ins MUST win duplicate names; and
malformed, oversized, or unsafe packages MUST be skipped with diagnostics.

The embedded `thegn` package SHALL be gated `always`, target every harness with
a native project skill root (Claude, Codex, pi), and seed on create, startup,
and explicit seeding. It SHALL teach an agent how to inspect its worktree and
environment, how builds and tests reach the project's runner, how to land or
queue its branch, how to ask the human for attention, how to start a helper
agent, and what the sandbox refuses. Every fenced `thegn` command in it MUST
parse against the live CLI.

#### Scenario: A user package is discovered safely

- **WHEN** a configured directory contains a regular `review/SKILL.md` whose
  bounded frontmatter name is `review`
- **THEN** it appears in `thegn skills list` unless a built-in already owns the
  name

#### Scenario: A symlink is not followed

- **WHEN** a configured package directory or `SKILL.md` is a symlink
- **THEN** it is skipped and cannot escape the configured root

#### Scenario: A new worktree teaches its agent thegn

- **WHEN** a worktree is created with `[skills] enabled = true`
- **THEN** `.claude/skills/thegn/SKILL.md`, `.agents/skills/thegn/SKILL.md`, and
  `.pi/skills/thegn/SKILL.md` are seeded with thegn's managed marker
