# Sidebar

## ADDED Requirements

### Requirement: Persisted sidebar view state is tombstone-free and pruned

The sidebar's persisted view state SHALL live in the single global `ui_state`
scope `sidebar`, and boolean keys (`collapse:*`, `pin:*`) MUST be deleted —
never tombstoned with a `"0"` value — when they return to their default
state. Loading MUST sweep legacy tombstone rows and rewrite legacy sort-mode
spellings (`activity`) to the canonical value. Removing a workspace,
worktree, or folder MUST prune its `collapse:`/`pin:` keys by prefix so
`ui_state` never accumulates orphans.

#### Scenario: Unpinning deletes the key

- **WHEN** the user unpins a row
- **THEN** its `pin:` key is removed from `ui_state` rather than set to `"0"`

#### Scenario: Removing a workspace prunes its view keys

- **WHEN** a workspace is removed
- **THEN** every `collapse:`/`pin:` key under its slug (including folder and
  worktree variants) is deleted

### Requirement: Sidebar glyphs are capability-routed and degrade to ASCII

Every glyph the sidebar renders SHALL come from the capability-resolved glyph
table, and rendering under ASCII capabilities MUST produce pure 7-bit ASCII
output. Chrome glyphs MUST be Basic-Multilingual-Plane characters with
display width 1 (no astral-plane or emoji-presentation characters). The
merge-queue status vocabulary SHALL be a single shared mapping consumed by
every surface that renders it.

#### Scenario: ASCII terminal renders pure ASCII

- **WHEN** the sidebar renders under `Ascii` glyph capabilities with folders,
  terminals, badges and the detail line populated
- **THEN** every cell of the frame is 7-bit ASCII

#### Scenario: Sidebar and panel agree on merge-queue glyphs

- **WHEN** a branch's merge-queue status renders in the sidebar detail chip
  and the panel's queue section
- **THEN** both show the same glyph and hue for that status

### Requirement: The TERMINALS section visibility is configurable

The sidebar SHALL show the TERMINALS section banner by default even when no
terminals exist (with an actionable empty hint), and
`[ui] sidebar_terminals_section = "nonempty"` MUST hide the entire section
until a terminal exists.

#### Scenario: Empty section hides under nonempty

- **WHEN** `sidebar_terminals_section = "nonempty"` and no terminals exist
- **THEN** neither the TERMINALS banner nor its hint row renders

### Requirement: Rail mode preserves row-kind identity

The slim rail SHALL keep workspaces and terminals identifiable: workspace
rows show a bold initial, terminal rows show the activity-dot + initial
treatment worktrees get, and empty-hint rows render nothing.

#### Scenario: A terminal keeps its identity at rail width

- **WHEN** the sidebar is in rail mode with a terminal row
- **THEN** that row shows a dot cell and the terminal's first letter rather
  than a generic divider

### Requirement: Attention sort is available and churn-stable

The sidebar SHALL provide an Attention sort mode that orders worktrees within
a workspace by their attention rank. The persisted legacy value `activity`
MUST parse as Attention. Ordering MUST be hysteresis-stable: rows reorder
only on a tier or membership change, never from cache refreshes or timestamp
ticks; before the first hydration pass the mode MUST degrade to the manual
order.

#### Scenario: Saved activity mode migrates

- **WHEN** a session's persisted sort mode is the legacy string `activity`
- **THEN** it loads as the Attention sort mode

#### Scenario: Cache churn does not reshuffle

- **WHEN** the PR cache refreshes with no underlying state change
- **THEN** the displayed worktree order is unchanged

#### Scenario: Manual move under attention sort flips to manual

- **WHEN** the user manually reorders a worktree while Attention sort is active
- **THEN** the sort mode flips to Manual so the move is visible and persists

## MODIFIED Requirements

### Requirement: Workspace/worktree tree model

The sidebar SHALL render workspaces and, under each, their worktrees from a
host-side tree model, and selecting a worktree MUST switch to its tab within
the one running session rather than spawning or teleporting to another
session. Tabs (pages) MUST NOT appear in the sidebar — they live in the
tabbar. A dormant workspace's subtree SHALL be structurally identical to its
live rendering: same folders, sort order, pin behavior, and gutter alignment.

#### Scenario: Selecting a worktree switches tabs

- **WHEN** the user selects a worktree row
- **THEN** the session switches to that worktree's tab without spawning or
  teleporting to a separate session

#### Scenario: Tabs never render in the tree

- **WHEN** a worktree owns multiple tabs
- **THEN** no page child rows appear under it (tab switching lives in the
  tabbar)

#### Scenario: Dormant and live trees match

- **WHEN** the same workspace renders live and then dormant (parked)
- **THEN** the tree shape — folders, order, pinned rows — is identical

### Requirement: Worktrees default to stable creation order

Within a workspace, the manual arrangement SHALL be a stable creation-order
sequence with explicit, persisted manual reordering — and Manual SHALL be
the default display sort: the tree never reorders itself unless the user
picks a computed sort. Urgency still surfaces at the default through
activity dots, the statusbar needs-you chip, and the attention jump key.

#### Scenario: Default order without signals is creation order

- **WHEN** worktrees are listed with no attention signals and no manual
  reordering
- **THEN** they appear in stable creation order

#### Scenario: Attention signals alone do not reorder the default

- **WHEN** the sort mode is the (default) Manual and a worktree becomes
  blocked on the user
- **THEN** the displayed order is unchanged (the row's dot/chip reflect the
  urgency instead)

#### Scenario: Manual worktree reorder persists

- **WHEN** the user reorders worktrees
- **THEN** the new order persists across restarts

## REMOVED Requirements

### Requirement: Worktrees nest their tabs (pages) in the tree

**Reason**: Contradicts the shipped design — tabs live exclusively in the
tabbar (`sidebar.rs` module contract + the `tabs_never_appear_in_the_sidebar`
regression test). The home-worktree-as-sibling clause survives in the tree
model requirement's behavior.

**Migration**: None; the sidebar never rendered page rows in the current
architecture.

### Requirement: Attention sort is the default and is churn-stable

**Reason**: Superseded by "Attention sort is available and churn-stable" plus
the Manual default in "Worktrees default to stable creation order": a
release-stable explorer must not reshuffle itself by default.

**Migration**: Sessions without a persisted `sort_mode` now display Manual
order; explicitly persisted values (including legacy `activity`) keep
working unchanged.
