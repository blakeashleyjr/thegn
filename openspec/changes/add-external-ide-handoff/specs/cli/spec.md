# CLI Specification (delta)

## ADDED Requirements

### Requirement: `open --file` reveals a file inside the workspace

`thegn open <repo> --file <path>[:<line>[:<col>]]` SHALL enqueue a
`reveal_file` intent (repo, path, optional line/column) instead of the plain
`focus_workspace` intent, using the same mailbox, resolution, exit-code, and
launch-fallback contract as `open <repo>`; with no live instance the intent
is enqueued before the compositor launches, so the reveal applies on first
model refresh. The `reveal_file` intent kind SHALL NOT be last-wins-merged
with `focus_workspace` intents. Path syntax with a trailing `:<line>` (and
optional `:<col>`) SHALL be parsed off the flag value; a non-numeric suffix
is part of the path.

#### Scenario: Reveal into a running instance

- **WHEN** `thegn open myrepo --file src/lib.rs:42` runs while a compositor
  holds the profile lock
- **THEN** a `reveal_file` intent is enqueued and the running instance
  focuses the workspace, selects the worktree containing the file, and opens
  it at line 42 within approximately one model-refresh tick

#### Scenario: Reveal with a cold launch

- **WHEN** the same command runs with no live instance
- **THEN** the intent is enqueued, the compositor launches on that workspace,
  and the reveal is claimed on its first model refresh

#### Scenario: Miss stays honest

- **WHEN** the path resolves inside no registered worktree of the repo
- **THEN** the running instance reports the miss in the status line and
  changes no focus beyond the workspace
