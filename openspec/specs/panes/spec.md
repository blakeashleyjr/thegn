# panes Specification

## Purpose

TBD - created by archiving change add-multiplexer-parity. Update Purpose after archive.

## Requirements

### Requirement: History search results identify their source

History search result rows SHALL identify the source pane using the current
display-safe worktree name, tab title, and pane title (falling back to the
pane's program name), and SHALL show how long before the search the line was
captured. The active scope SHALL filter which panes contribute without removing
available provenance from each result: pane- and tab-scoped rows carry the same
worktree, tab and pane parts as wider scopes, and no row SHALL expose an
internal pane id. History lines SHALL retain their capture time, stamped when
the line was written, so rows can show relative age; search SHALL NOT derive
timestamps from line order or invent time evidence: lines replayed by a daemon
reattach have an unknown capture time and their rows show no age. A row label
that does not fit SHALL shorten the worktree part before the tab or program.
The selected result's
preview SHALL show up to two surrounding lines before and after the match, with
the matched line highlighted. Identical lines from one pane SHALL group into a
single row that reports its occurrence count and resolves to the newest
occurrence.

#### Scenario: Worktree search distinguishes tabs and panes

- **WHEN** a worktree search returns lines from tabs with different titles or
  pane titles/programs
- **THEN** each result labels the actual worktree, tab, and pane source in that
  order, without exposing an internal pane id

#### Scenario: Narrower scopes retain full source provenance

- **WHEN** a pane- or tab-scoped search returns a result
- **THEN** its row identifies the owning worktree, tab, and pane title or
  program name

#### Scenario: Compact results retain time and nearby context

- **WHEN** a search result has a recorded capture time and nearby history
- **THEN** its compact row shows the relative age, and the selected result's
  preview shows up to two surrounding lines on either side with the matched line
  highlighted

#### Scenario: A pane out of the session tree has no invented provenance

- **WHEN** a pane history remains searchable after its owning tab leaves the
  live session tree
- **THEN** its row may be labelled by the pane's safe title or program name, but
  SHALL NOT invent a worktree or tab label

#### Scenario: Repeated lines group into one row

- **WHEN** the same pane printed one identical line six times
- **THEN** search shows a single row reporting six occurrences, and confirming it
  jumps to the newest of them

### Requirement: Pane splits are resizable from the keyboard

thegn SHALL provide keyboard resize actions (`resize-left`, `resize-right`,
`resize-up`, `resize-down`) that shift weight between the focused pane's
branch and its neighbor at the nearest ancestor split matching the direction,
by a fixed step, clamped so no pane's share reaches zero. A resize with no
matching-axis ancestor MUST be a harmless no-op with a statusbar hint. The
adjusted weights MUST persist through the existing tab-layout persistence and
survive resurrection.

#### Scenario: Growing the focused pane

- **WHEN** the focused pane sits left of a sibling in a row split and the user
  invokes `resize-right`
- **THEN** the focused pane's weight grows and the sibling's shrinks by the
  step, the frame repaints, and the new weights are persisted

#### Scenario: Resize cannot eliminate a pane

- **WHEN** the user repeats a resize action until the neighbor reaches the
  minimum share
- **THEN** further repeats leave the weights unchanged rather than collapsing
  the neighbor to zero cells

#### Scenario: No matching axis

- **WHEN** the focused tab holds a single pane and the user invokes a resize
  action
- **THEN** nothing changes and a statusbar hint explains there is nothing to
  resize

### Requirement: Panes swap positions from the keyboard

thegn SHALL provide swap actions (`swap-pane-left/right/up/down`) that
exchange the focused leaf with its spatial neighbor in that direction, where
the neighbor is resolved by the same geometry walk as the `Focus*` actions so
focus and swap always agree. The panes exchange tree positions (each adopts
the other slot's weight); focus follows the moved pane. Swapping toward an
edge with no neighbor MUST be a no-op.

#### Scenario: Swap with the right neighbor

- **WHEN** two panes sit side by side at 70/30 and the user invokes
  `swap-pane-right` from the left pane
- **THEN** the panes exchange positions, the moved pane now occupies the 30%
  slot, and focus stays on the moved pane

#### Scenario: Swap agrees with focus

- **WHEN** `focus-down` from a pane would land on pane B
- **THEN** `swap-pane-down` from that pane exchanges it with pane B, never a
  different pane

### Requirement: Pane borders drag-resize with the mouse

Pane frame borders SHALL act as mouse drag handles: pressing on a shared
border and dragging along the split axis shifts weight between the adjacent
branches continuously (same clamps as keyboard resize), committing one
persisted layout on release. Mouse events inside a pane's content rect MUST
remain governed by the existing pane mouse forwarding — chrome drag behavior
binds only to frame cells.

#### Scenario: Dragging a vertical border

- **WHEN** the user presses on the border between two side-by-side panes and
  drags right
- **THEN** the left pane widens live as the pointer moves and the final
  weights are persisted once on release

#### Scenario: Content clicks still reach the application

- **WHEN** a full-screen application has requested mouse reporting and the
  user clicks inside that pane's content
- **THEN** the click is forwarded to the application and no compositor drag
  begins

### Requirement: Panes rearrange by dragging their frames

Pressing and dragging on a pane's frame/title SHALL lift the pane and show a
live drop-target highlight as the pointer moves: hovering another pane's
center region targets a swap; hovering an edge band targets re-anchoring the
dragged pane as a new split on that side. Release commits the highlighted
operation through the same tree mutations as the keyboard; Esc or release
over a non-target cancels with no layout change. Drop-target resolution MUST
be a pure function of the pointer cell and the pane rects, and drag feedback
MUST render as chrome damage. Every drop outcome MUST also be reachable from
the keyboard (swap actions; splits + swaps compose to any re-anchor).

#### Scenario: Drop on a pane center swaps

- **WHEN** the user drags pane A's frame onto the center of pane B and
  releases
- **THEN** A and B exchange positions exactly as `swap-pane` would

#### Scenario: Drop on an edge re-anchors

- **WHEN** the user drags pane A onto the bottom edge band of pane B and
  releases
- **THEN** A is removed from its slot and B's slot becomes a column split with
  B above A

#### Scenario: Esc cancels a lift

- **WHEN** the user lifts a pane and presses Esc before releasing
- **THEN** the drag ends, the highlight clears, and the layout is unchanged

#### Scenario: Drag feedback never recomposes from pane output

- **WHEN** a lifted pane's process writes output mid-drag
- **THEN** the output renders through the normal pane damage path and the drag
  highlight repaints only on pointer motion or drag-state change

### Requirement: Zoom is remembered per tab

thegn SHALL keep the `Ctrl-Alt-z` zoom level (tiled, maximized, full-window)
on each worktree tab, and SHALL keep each tab's focused pane across tab,
worktree and workspace switches. Activating a tab MUST NOT move its focus
unless the remembered pane no longer exists. The chrome MUST follow the active
tab's level. A full-window tab whose focus moves to the sidebar or panel SHALL
show that chrome until focus returns to the center. The level is not
persisted: a restart comes back tiled.

#### Scenario: Returning to a zoomed worktree

- **WHEN** the user zooms the second pane of a split in worktree A, switches
  to worktree B, and switches back to A
- **THEN** A shows that same second pane zoomed, and B was shown with its own
  (unzoomed) layout

#### Scenario: A stale focus is repaired

- **WHEN** a tab is activated whose remembered focused pane is no longer in
  its tree
- **THEN** focus moves to the leftmost visible pane

#### Scenario: Keyboard navigation within a saved stack survives restoring tiled

- **WHEN** the user zooms a saved stack, uses Ctrl or Alt vertical navigation
  to focus a collapsed member, and restores tiled view
- **THEN** the original stack expands that same member and keyboard input
  targets the displayed pane, including stacks nested inside splits

### Requirement: Closing a pane preserves useful focus

Each tab SHALL keep a bounded, deduplicated, runtime-only history of the center
panes it has focused, most recent last. Every focus change that lands on a
center pane MUST record it. Closing the focused pane (by action or process exit)
SHALL focus the most recently focused pane still in that tab's tree, activating
it when it is a collapsed stack member. Closing an unfocused pane MUST leave
focus where it is. Closed pane ids MUST be removed from the history. When no
recorded pane survives, focus SHALL keep the existing first-pane fallback. The
history is not persisted.

#### Scenario: Closing the pane you just returned to

- **WHEN** a tab holds panes A, B and C, the user focuses A, then C, then B,
  and closes B
- **THEN** focus lands on C rather than the first pane in tree order

#### Scenario: Closing a background pane

- **WHEN** the user closes a pane that is not focused
- **THEN** focus stays on the same pane and the closed id is dropped from the
  history

#### Scenario: No remembered survivor

- **WHEN** the focused pane closes and no other pane in the history remains in
  the tree
- **THEN** focus falls back to the tree's first pane, as before

### Requirement: A zoomed tab renders as a pane stack

While a tab is maximized or full-window, thegn SHALL draw the focused pane
expanded and every other pane of the tab as a one-row title bar: panes before
it in tree order above it, and panes after it below. A left click on a bar
SHALL focus and expand that pane. When the center is too short to keep a
usable expanded pane under the bars, the bars SHALL be omitted. Vertical focus
moves SHALL step through the stack in its visual order; horizontal moves keep
the split geometry.

#### Scenario: Stack bars show the hidden panes

- **WHEN** the user zooms the middle pane of a three-pane tab
- **THEN** one collapsed title bar is drawn above the expanded pane and one
  below it

#### Scenario: Clicking a bar

- **WHEN** the user clicks a collapsed bar while zoomed
- **THEN** that pane becomes focused and expanded, and the tab stays zoomed

#### Scenario: A stack is too narrow to paint its bars

- **WHEN** a stack is laid out in fewer than two columns
- **THEN** collapsed bars are omitted from both rendering and hit-testing and
  the active member keeps the stack's full rect

#### Scenario: A stack bar is not displayed

- **WHEN** an app tab or loading splash replaces the worktree center, or an
  overlay covers the cell where a collapsed bar would otherwise be drawn
- **THEN** a click there MUST NOT activate the hidden stack member
- **AND** existing modal, drawer and pointer-capture routing retains priority

### Requirement: Paired stack geometry uses one layout walk

Rendering and mouse dispatch SHALL obtain pane frames and stack bars from one
shared layout walk. Callers needing only panes or bars MUST NOT allocate a
discarded vector for the unrequested output.

#### Scenario: A nested stack is rendered or hit-tested

- **WHEN** the caller needs both pane frames and collapsed bars
- **THEN** one layout walk supplies both and agrees with the pane-only and
  bar-only geometry for the same tree and rect

### Requirement: Mouse input targets the pane geometry as drawn

thegn SHALL resolve mouse hits against the pane tree as displayed, which is
the stack while zoomed, and never against hidden split geometry. Selection
anchors and drags, wheel scrolling, pane-app mouse forwarding, and border
gestures MUST use the same rects the renderer drew.

#### Scenario: Selecting text in a zoomed pane

- **WHEN** the user drag-selects inside a zoomed pane
- **THEN** the selection starts at the cell under the pointer in that pane

### Requirement: A starting pane shows what it is waiting on

A pane that exists but has produced no output yet SHALL show its startup
state instead of a blank cell: thegn's own lifecycle phase (`resolving
environment` → `starting sandbox` → `starting shell` → `waiting for shell
prompt`) with elapsed time, and while the prompt is pending the pane's live
descendant processes (argv[0] basename plus clipped args, with the process
age). The card MUST appear only after a short grace so instantly-ready panes
never flash one, MUST disappear on the pane's first output, and MUST retire
itself after a bounded time. Descendant sampling MUST stay off the event
loop, MUST stop as soon as output arrives, and MUST NOT run at all while no
pane is starting, and only for displayed panes. Once shown, a card MUST stay
visible across phase changes. Reattached sessions and program (non-shell)
panes MUST NOT show the shell-prompt card; pre-fork state MUST be keyed by
group, tab and leaf so a card never appears on another tab. Sampled argv
MUST be redacted (URL userinfo, query tokens, sensitive flag values) before
it is drawn.

#### Scenario: An instant spawn never flashes the card

- **WHEN** a pane produces its prompt within the grace window
- **THEN** no spawn-progress card is ever drawn for that pane

#### Scenario: The card retires on first output and at the bound

- **WHEN** the pane produces any output
- **THEN** the card disappears and its sampler stops
- **WHEN** the pane stays silent past the bound since its spawn was kicked
- **THEN** the card retires itself and the pane renders as an ordinary pane

#### Scenario: A recognized startup culprit gets a one-line hint

- **WHEN** a descendant's argv[0] matches the culprit table (direnv, nix,
  version managers, prompt frameworks, git, agents, ssh/mosh, sudo/askpass)
- **THEN** the card shows the table's one-line hint for it

#### Scenario: Sampled argv never leaks a secret

- **WHEN** a sampled descendant's argv contains a token-shaped value
- **THEN** the drawn line carries the redaction placeholder, never the value

#### Scenario: Platforms without /proc keep the phases

- **WHEN** the host does not expose a procfs descendant walk
- **THEN** the card shows the lifecycle phase alone and samples nothing

#### Scenario: Nothing waits, nothing polls

- **WHEN** no pane is between its spawn and its first output
- **THEN** no sampler thread exists and no descendant sampling occurs

### Requirement: Tabs reorder and panes move between tabs

thegn SHALL provide `move-tab-left` / `move-tab-right` (default
`Shift+Alt+←/→`) that swap the active tab one slot within its worktree strip
(no wrap; a move past either end is a no-op), and a drag on the tab strip that
moves the pressed tab to the chip it is released over. thegn SHALL provide
`break-pane-to-tab` (default `Ctrl+Alt+Shift+b`) that moves the focused pane
into a new tab right after the active one, and `move-pane-to-prev-tab` /
`move-pane-to-next-tab` (default `Ctrl+Alt+Shift+←/→`) that move the focused
pane beside the focused pane of the neighbouring tab. A tab's last pane MUST
NOT be moved out. A moved pane keeps its pane id, so its PTY and session stay
alive, and the resulting tab order and membership persist with the layout.

#### Scenario: Reordering a tab

- **WHEN** the second of three tabs is active and the user invokes
  `move-tab-right`
- **THEN** it becomes the third tab, stays active, and the order is still
  there after detach and restart

#### Scenario: Breaking a pane out

- **WHEN** a tab holds two panes and the user invokes `break-pane-to-tab`
  on one
- **THEN** a new tab right after the active one holds that pane, the process
  in it is the same one that was running, and that tab is active

#### Scenario: Moving a pane to the next tab

- **WHEN** the user invokes `move-pane-to-next-tab` on a pane of a split tab
- **THEN** the pane sits beside the next tab's focused pane with its process
  alive; at the last tab the action is a no-op

### Requirement: The panel lists the layout keys

When `[panel] layout_hints` is on (default), the right panel SHALL show a
"Layout keys" block in its blank tail space, below the section content and
above the pinned Lead footer, listing split, swap, resize, zoom, lock and the
tab / pane move chords as read from the live keymap. The block MUST drop its
lowest-priority rows and truncate text when space is short, MUST never cover
panel content, and MUST NOT take rows from the Lead footer.

#### Scenario: A rebind shows

- **WHEN** the user rebinds `zoom` to another chord
- **THEN** the block shows the new chord on its next frame

#### Scenario: Disabled

- **WHEN** `[panel] layout_hints = false`
- **THEN** no Layout keys block is drawn

### Requirement: Tab chips can use an agent's terminal title

When a tab has its default ordinal title, the tab strip SHALL display the
focused pane's cleaned terminal title when `[ui] tab_title_from_terminal` is
true (default). Custom tab titles SHALL take precedence. The cleanup SHALL
remove spinner prefixes and ignore titles that are only folder paths.

#### Scenario: A terminal title names a default tab

- **WHEN** a focused pane sets a terminal title for a tab with its default
  ordinal title
- **THEN** the tab chip shows the cleaned terminal title on the next redraw

#### Scenario: Custom and path-only titles are ignored

- **WHEN** a tab has a custom title, or its terminal title is only a folder path
- **THEN** the chip keeps the custom or default tab title, respectively

#### Scenario: Terminal title labels are disabled

- **WHEN** `[ui] tab_title_from_terminal` is false
- **THEN** the tab strip keeps the stored tab title
