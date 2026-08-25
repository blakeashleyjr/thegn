# Sidebar

## ADDED Requirements

### Requirement: Submodule-dirty state is indicated distinctly

Worktree rows SHALL indicate a dirty or pointer-moved submodule distinctly
from ordinary file dirtiness, behind a `[ui]` visibility toggle like the
existing status-icon toggles, with the glyph drawn from the caps table. The
indicator SHALL follow the existing glyph freshness model (active worktree
rescans, background rows TTL-cached) and MUST degrade independently — a
failed submodule read leaves the other glyphs intact and shows no indicator
rather than a stale lie being invented.

#### Scenario: Submodule-only dirtiness is visible

- **WHEN** a worktree's superproject files are clean but a submodule is
  dirty
- **THEN** the row shows the submodule indicator instead of (not in addition
  to) the plain dirty dot

#### Scenario: Toggle hides it

- **WHEN** the submodule indicator's `[ui]` toggle is off
- **THEN** rows render exactly as before this change
