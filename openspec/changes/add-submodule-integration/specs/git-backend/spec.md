# Git Backend

## ADDED Requirements

### Requirement: Submodule state is a first-class read

thegn SHALL classify status and diff entries that are submodules (via a
cached parse of the checkout's `.gitmodules`) and SHALL expose a
`submodule_states` read on the `GitBackend` seam reporting, per submodule,
whether its recorded pointer moved and whether its own working tree is dirty
or carries untracked files. The read MUST run only when the checkout has a
`.gitmodules` (repos without submodules pay nothing), MUST batch into the
existing glyph read so the hot path stays one round-trip, and MUST degrade
independently (a failed submodule read never poisons the other glyph
fields).

#### Scenario: Repo without submodules pays nothing

- **WHEN** a worktree has no `.gitmodules`
- **THEN** no submodule read runs and the glyph scan's work shape is
  unchanged

#### Scenario: Dirty submodule is distinguishable

- **WHEN** a submodule's working tree is dirty but the superproject's own
  files are clean
- **THEN** the read reports submodule-dirty distinctly from file-dirty

### Requirement: Worktree and clone provisioning populates submodules

When `[git] submodules = "auto"` (the default) and the checkout has a
`.gitmodules`, thegn SHALL initialize submodules recursively after creating
a checkout: worktree creation runs `submodule update --init --recursive` in
the new worktree off the event loop, and workspace clones (local and the
remote provision script) recurse submodules. Initialization MUST be
non-fatal — failure leaves a valid checkout with a visible notice, never a
rollback — and in a repo whose trust class does not permit repo-driven
execution it MUST require consent naming the submodule URLs before any
network or checkout activity, since those URLs are repo-controlled input.
`"off"` restores today's behaviour entirely.

#### Scenario: New worktree starts populated

- **WHEN** a worktree is created in a repo with submodules under the default
  setting
- **THEN** the new checkout's submodules are initialized off-loop and the
  worktree is usable immediately, with progress surfaced

#### Scenario: Init failure is survivable

- **WHEN** submodule initialization fails (e.g. an unreachable URL)
- **THEN** the worktree remains registered and usable, with a "submodules
  not initialized" notice rather than an error rollback

#### Scenario: Untrusted repo asks first

- **WHEN** a worktree is created in a repo not trusted for repo-driven
  execution
- **THEN** initialization waits for consent that names the submodule URLs,
  and declining leaves the checkout uninitialized with the notice
