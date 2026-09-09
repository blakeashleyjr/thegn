# Integrate Git submodules across reads, views, lifecycle, and merge reporting

Linear: THE-32

## Why

Submodules are gitlink entries, not ordinary files or ordinary directories.
Treating them as text hunks permits invalid partial staging; ignoring their
checkout state leaves new worktrees unusable; and reporting only a dirty dot or
two raw SHAs hides actionable state.

## Delivered design

- Core parses `.gitmodules` with path-escape rejection and models gitlink
  state, pointer direction/summary, atomic patch selection, and conflicts.
- The service Git adapter discovers state only for repositories with submodule
  metadata, using bounded local Git reads that never fetch.
- Change/diff views render pointer moves and local commit summaries; partial
  line staging is rejected and whole-entry stage/restore remains available.
- Sidebar rendering has a separate, configurable submodule indicator.
- `[git] submodules = "auto" | "off"` governs a shared post-checkout
  initializer for CLI, UI, clone, and remote/provider lifecycle paths.
  Initialization is metadata-aware, explicitly trust-gated, non-fatal, and
  surfaced.
- Workspace creation performs the ordinary clone first, then the same
  trust-gated initialization. It does not rely on `git clone
--recurse-submodules` as a separate policy path.
- Merge conflicts name the submodule path and both pointer SHAs and are not
  routed through text conflict drivers.

## Non-goals

- Fetching submodule history merely to enrich a view.
- Automatically choosing one side of a gitlink conflict.
- Exposing a new external capability or descending the file explorer into a
  nested repository.

## Evidence

Implemented in core `submodule`/`patch`, the service Git submodule adapter,
host lifecycle/clone/remote paths, panel/sidebar views, and merge reporting,
with fixture/unit coverage in the THE-32 commit series.
