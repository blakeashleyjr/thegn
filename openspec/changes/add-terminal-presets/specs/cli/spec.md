# CLI

## ADDED Requirements

### Requirement: `open --preset` applies a terminal preset on arrival

`thegn open <repo> --preset <name>` SHALL validate the preset name against the
configured `[[presets]]` (a miss lists candidates and exits 3, the `open`
resolution convention) and enqueue a launch-preset intent — carrying the
preset **name only** — after the focus intent in the DB `intents` mailbox. A
live compositor SHALL consume it claim-and-delete and apply the preset against
the focused workspace's active worktree; with no live instance the compositor
SHALL launch focused on that workspace and apply the preset after the first
frame (never before it). The flag without a live instance MUST NOT provision
sandboxes eagerly — panes resolve lazily exactly as interactive launches do.

#### Scenario: Remote preset launch into a running instance

- **WHEN** `thegn open myrepo --preset dev` runs while a compositor is running
- **THEN** the running instance focuses `myrepo` and applies the `dev` preset
  to its active worktree within approximately one model-refresh tick

#### Scenario: Headless start applies after first frame

- **WHEN** `thegn open myrepo --preset dev` runs with no live instance
- **THEN** the compositor launches on `myrepo`, renders its first frame, and
  then applies the preset

#### Scenario: Unknown preset

- **WHEN** `thegn open myrepo --preset nope` runs and no preset is named
  `nope`
- **THEN** the command lists the configured preset names and exits with code
  3, and no intent is enqueued
