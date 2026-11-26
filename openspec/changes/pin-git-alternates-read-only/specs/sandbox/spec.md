# Sandbox

## ADDED Requirements

### Requirement: Git alternates are pinned and the object store is checked at launch

When `[sandbox.git]` keeps `<common>/objects` writable for a local linked
worktree, the sandbox SHALL bind `objects/info/alternates` and
`objects/info/http-alternates` read-only on bwrap, OCI and systemd, creating
each file empty (with its parent directory) when missing. Before the files are
created, the launch SHALL probe whether `objects/info/alternates` lists a
non-comment path, whether `objects/` is or directly holds a symlink, and
whether the worktree's `commondir` resolves outside the common dir its admin
dir lives in. A failing probe SHALL refuse a `sealed` or `sealed-tunnel`
launch before any spawn and SHALL log a warning for every other profile, which
then launches. The probe SHALL NOT run a git subprocess.

#### Scenario: A sandbox cannot write alternates

- **WHEN** a commit-only bwrap pane opens a linked worktree
- **THEN** both alternates paths are `--ro-bind` entries after the writable
  `objects/` bind

#### Scenario: Sealed refuses a borrowed object store

- **WHEN** a `sealed` pane opens a worktree whose `objects/info/alternates`
  names another object directory
- **THEN** the launch is refused with "sealed refused: this repository's
  object store borrows objects from elsewhere (objects/info/alternates); use a
  private copy"

#### Scenario: Standard warns and launches

- **WHEN** a `standard` pane opens the same worktree
- **THEN** thegn logs "alternates present: objects from <path> are shared with
  this sandbox" and the pane launches

#### Scenario: A redirected commondir is refused under sealed

- **WHEN** a `sealed` pane opens a worktree whose `commondir` points outside
  the repository that holds its admin dir
- **THEN** the launch is refused and the message names `commondir`
