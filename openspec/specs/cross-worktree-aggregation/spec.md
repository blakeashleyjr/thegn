# cross-worktree-aggregation Specification

## Purpose

A read-only "Across" panel section that aggregates status, diffs, CI, and grep excerpts across every worktree of a workspace at once.

## Requirements

### Requirement: Heterogeneous results aggregate into one sorted, grouped excerpt stream

thegn SHALL provide a pure aggregation model that collects excerpts from
multiple worktrees and multiple source kinds (CI failure, dirty file, content
match) into one deterministically ordered stream, grouped by worktree. Each
excerpt MUST carry its owning worktree path (the jump target) and a display
label, its kind, and the source location (file + optional line) and text. The
ordering MUST be deterministic (stable across runs) so the surface does not
reshuffle between refreshes.

#### Scenario: Excerpts group by worktree deterministically

- **WHEN** excerpts from several worktrees and kinds are aggregated
- **THEN** they are ordered deterministically and grouped by worktree, so the
  same inputs always render in the same order

#### Scenario: Each excerpt knows its source

- **WHEN** an excerpt is produced from a CI failure, a dirty file, or a content
  match
- **THEN** it carries the owning worktree path and label and its file/line so
  the source is identifiable

### Requirement: The aggregation exposes a navigable rows view and jump targets

The model SHALL expose a flattened `rows` view interleaving per-worktree divider
rows (label + count) with excerpt rows, suitable for cursor navigation, and a
`jump_target` that resolves an excerpt row back to its owning worktree. It MUST
also expose per-kind summary counts.

#### Scenario: Rows interleave dividers and excerpts

- **WHEN** the rows view is requested for a non-empty aggregation
- **THEN** each worktree group is introduced by a divider row carrying its label
  and count, followed by that worktree's excerpt rows

#### Scenario: An excerpt row resolves to its worktree

- **WHEN** `jump_target` is asked for an excerpt row's index
- **THEN** it returns that excerpt's owning worktree path

#### Scenario: Empty aggregation is empty

- **WHEN** no excerpts have been added
- **THEN** the aggregation reports empty and its rows view is empty

### Requirement: A read-only cross-worktree section renders the stream

thegn SHALL render the aggregation as a read-only panel section in the Work
tab, showing each excerpt with its source (`worktree · file:line · text`) grouped
under per-worktree headers, across the panel's view widths. The section MUST be
populated off the event loop (from the cross-worktree CI cache) and MUST NOT
block the loop or require the active worktree to be the one an excerpt belongs
to.

#### Scenario: Section lists cross-worktree attention items

- **WHEN** one or more worktrees have failing CI in the cache
- **THEN** the cross-worktree section lists those failures grouped by worktree,
  each row naming its worktree and source

#### Scenario: Section is empty when nothing needs attention

- **WHEN** no worktree has an aggregated item
- **THEN** the section renders an empty/placeholder state without error
