# Sidebar

## ADDED Requirements

### Requirement: Header rows read in tiers

The full sidebar SHALL render its structural rows in visually distinct tiers:
workspace (and terminal-host) headers as the strongest tier, folder headers
as a clearly secondary tier, and worktree/terminal rows as the body tier. A
workspace header and a folder header MUST be distinguishable at a glance by
more than indentation alone, and the distinction MUST NOT rely on color
alone — it survives 16-color and mono quantization through weight and
layout. All styling MUST resolve through the theme slot / capability-glyph
chokepoints; no color or glyph literal at a draw site.

#### Scenario: A repo and its folder are told apart

- **WHEN** a workspace containing a "Merged" folder is rendered in the full
  sidebar
- **THEN** the workspace header and the folder header use visibly different
  emphasis (not merely different indent), with the folder subordinate

#### Scenario: The hierarchy survives a mono terminal

- **WHEN** the same tree renders with colors quantized to mono
- **THEN** workspace headers, folder headers and worktree rows remain
  distinguishable by weight and layout

### Requirement: Adjacent workspaces are visibly separated

The full sidebar SHALL lay out a separator gap between one workspace's
subtree and the next workspace header (and before the terminals region),
gated by `[ui] sidebar_dividers` (default on). The gap MUST be produced by
the same layout pass the renderer, hit-testing and scrolling share: it is not
a click target (a click over it resolves as empty space), the cursor never
rests on it, a drag-drop over it resolves to the same destination as the
run boundary it separates, and it counts toward scroll geometry so the
truncation indications stay truthful. Gaps MUST be suppressed in rail mode
and while the `/` filter is active. With `sidebar_dividers = false` the
layout MUST be identical to the ungapped form.

#### Scenario: Two repos no longer abut

- **WHEN** two workspaces render consecutively with `sidebar_dividers = true`
- **THEN** a blank separator row lies between the first workspace's last row
  and the second workspace's header

#### Scenario: The gap is interaction-transparent

- **WHEN** the user clicks on a separator gap, or releases a dragged worktree
  over it
- **THEN** the click selects nothing, and the drop lands exactly where a drop
  on the adjacent run boundary would land with dividers off

#### Scenario: Filtering stays dense

- **WHEN** the user types a `/` filter that matches rows in several
  workspaces
- **THEN** the filtered list renders without separator gaps

#### Scenario: Dividers can be turned off

- **WHEN** `[ui] sidebar_dividers = false`
- **THEN** no gaps are laid out and row geometry matches the pre-change
  layout exactly
