# Sidebar

## ADDED Requirements

### Requirement: Member workspaces group under collapsible program headers

The sidebar SHALL render each program as a header row (name, member count,
and a tier-granular attention rollup of its members) with its member
workspaces nested beneath it, while unprojected workspaces render exactly as
today after the program groups. Program headers SHALL be collapsible (state
persisted tombstone-free and pruned when the program is deleted) and
manually orderable with exact-order persistence; member workspaces keep
their own manual order within the group. Header glyphs MUST route through
the capability glyph table, and header/model changes render through the
`Full` damage channel without touching the pane-output path.

#### Scenario: Projected and unprojected workspaces coexist

- **WHEN** two workspaces belong to program `shop` and one workspace is
  unprojected
- **THEN** the two render nested under a `shop` header row and the third
  renders as an ordinary top-level workspace row

#### Scenario: Collapse persists and is pruned

- **WHEN** the user collapses a program header, restarts thegn, and later
  deletes the program
- **THEN** the collapse state is restored after the restart and its
  persisted key is removed when the program is deleted

#### Scenario: Header rollup reflects the most urgent member

- **WHEN** one member workspace's worktree becomes blocked on the user
- **THEN** the program header's attention indication reflects that tier
  while equal-tier programs keep their manual order
