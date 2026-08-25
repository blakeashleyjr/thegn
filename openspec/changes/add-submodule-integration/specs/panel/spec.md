# Panel

## ADDED Requirements

### Requirement: Submodule changes render as pointer moves

The changes list and diff surfaces SHALL flag submodule entries and render a
gitlink change as a pointer move (`<path> old → new` with a distinct glyph
from the caps table) rather than raw `Subproject commit` text lines, and a
gitlink change MUST NOT render as `+0/-0`. Drilling into a submodule row
SHALL show a bounded commit summary (`log --oneline old..new`, fetched off
the event loop) with a direction label, degrading to the bare SHAs when the
submodule checkout is missing or the range is not present locally — a render
MUST never trigger a fetch.

#### Scenario: Pointer move is legible

- **WHEN** a worktree's only change is a submodule pointer bump whose
  commits exist locally
- **THEN** the changes row shows the submodule flag with old → new, and the
  drilled view lists the moved commits with a direction label

#### Scenario: Missing history degrades

- **WHEN** the moved-to commit is not present in the local submodule
- **THEN** the drilled view shows the bare SHAs with a note, and no network
  fetch is attempted

### Requirement: Staging treats a submodule as atomic

Staging surfaces SHALL stage and unstage a submodule pointer change only as
a whole entry (index-level add/restore), and line-level staging MUST refuse
to split a gitlink hunk — a partial selection over a submodule entry is
rejected before any `git apply` is attempted.

#### Scenario: Whole-entry stage works

- **WHEN** the user stages a submodule row
- **THEN** the pointer change is staged via an index-level operation and the
  row moves to staged

#### Scenario: Line-splitting a gitlink is refused

- **WHEN** a line-level selection covers only part of a submodule pointer
  hunk
- **THEN** the operation is rejected with a clear message and the index is
  unchanged
