# Skills

## ADDED Requirements

### Requirement: thegn ships an embedded, versioned skill set

thegn SHALL embed a curated set of `SKILL.md` packages in the binary at build
time, each with validated frontmatter (`name`, `description`) and a prose body
teaching an agent a thegn workflow. The set is versioned with the binary and
requires no network, registry, or database. `thegn skills list` SHALL
enumerate the set and `thegn skills show <name>` SHALL print a skill's
content. Skill names MUST be path-safe single segments.

#### Scenario: The set is listable offline

- **WHEN** `thegn skills list` runs with no network and no prior setup
- **THEN** every embedded skill is listed with its description

### Requirement: Embedded skills cannot drift from the CLI

A unit test SHALL validate every embedded skill's frontmatter and check each
`thegn` command line cited in its body against the live clap definition
(placeholder arguments excepted), failing the build when a recipe names a
verb or flag that no longer parses.

#### Scenario: A renamed verb fails the gate

- **WHEN** a CLI verb cited by an embedded skill is renamed without updating
  the skill
- **THEN** the drift test fails naming the skill and the stale command

### Requirement: Sync installs into agent skill directories, marker-tagged

`thegn skills sync [--agent <kind>] [--remove]` SHALL install the embedded
set into supported agent CLIs' skill directories through per-vendor adapters
(vendor paths confined to implementation files; unimplemented kinds are
`reserved`), defaulting targets from the `[[agents]]` list. Every written
file MUST carry a thegn-managed marker, a content hash, and the shipping
version. Sync MUST be idempotent (current files are not rewritten), MUST
replace marked files on version change, MUST delete marked skills no longer
in the embedded set, and `--remove` MUST delete exactly the thegn-marked
files.

#### Scenario: Sync twice writes once

- **WHEN** `thegn skills sync` runs twice on the same thegn version
- **THEN** the second run writes nothing and reports the set current

#### Scenario: Deprecated managed skill is cleaned up

- **WHEN** a skill shipped by a previous version is absent from the current
  embedded set and sync runs
- **THEN** the marked directory for it is removed

### Requirement: User-authored and user-modified files are never clobbered

Sync MUST NOT overwrite or delete any file lacking the thegn marker, and MUST
treat a marked file whose content hash no longer matches as user-adopted:
skipped, preserved, and reported — never silently reverted.

#### Scenario: A hand-edited managed skill survives an upgrade

- **WHEN** the user edits a thegn-installed skill and a newer thegn syncs
- **THEN** the edited file is left intact and the divergence is reported

#### Scenario: A user's own skill at the same path is untouched

- **WHEN** an unmarked user-authored skill exists in a target directory and
  sync (or `--remove`) runs
- **THEN** the file is neither modified nor deleted

### Requirement: Sync is explicit, and auto-sync stays off the event loop

Skill installation MUST only occur on an explicit `skills sync` invocation or
when `[skills] auto_sync` is explicitly enabled, in which case it SHALL run
off the compositor event loop after the first frame, with failures surfaced
best-effort and never fatal. The `[skills]` table (`enabled`, `auto_sync`
default off, `exclude`) MUST be documented in `config/config.toml.example`,
and excluded skills MUST be withheld from sync (and removed if previously
managed-installed).

#### Scenario: Launch alone writes nothing

- **WHEN** thegn starts with default config
- **THEN** no agent directory is written

#### Scenario: Auto-sync never blocks the first frame

- **WHEN** `auto_sync = true` and thegn starts
- **THEN** sync runs off-thread after the first frame and a sync failure only
  produces a status message

### Requirement: doctor reports skill installation state

`thegn doctor` SHALL report, per supported agent kind: whether its skills
directory was found, and whether the thegn-managed set there is current,
stale, user-modified, or absent.

#### Scenario: Doctor shows a stale install

- **WHEN** an agent directory holds thegn-managed skills from an older
  version and `thegn doctor` runs
- **THEN** that agent kind is reported stale with the sync remedy named
