# Merge Queue

## ADDED Requirements

### Requirement: The ambient queue signal lives on the project row, not the bottom bar

thegn SHALL surface each repo's merge-queue state as a token on that
workspace's header row in the full sidebar: red with a count while any of the
repo's entries is blocked (deferred / gate-failed / gate-error / needs-human),
amber while the queue is working (folding / verifying / agent running), quietly
dim while entries are merely queued or held at ready, and absent when the
repo's queue is empty. The token MUST reflect the row's own repo — including
dormant workspaces — never the globally focused repo. Activating the token
SHALL open the merge-queue detail for that repo, and the activation MUST have
a keyboard equivalent. The token MUST yield to the workspace label under
width pressure (count first, then the token) rather than wrapping or
truncating the label, and its colors and glyphs MUST resolve through the
theme/caps chokepoints (no draw-site literals). In rail mode the token SHALL
degrade to an urgency tint on the workspace cell for the red and amber tiers
only.

The statusbar SHALL NOT show the merge-queue chip by default. An `mq` widget
id SHALL be available to the `[bars]` slots so the chip (with its overlay
activation) can be restored to any bar position by configuration.

#### Scenario: A background repo's blocked queue is visible

- **WHEN** a dormant workspace has a queue entry marked `needs_human` while
  the user works in another repo
- **THEN** that workspace's header row shows a red token with the blocked
  count, and no merge-queue chip appears in the default statusbar

#### Scenario: Activating the token opens that repo's queue

- **WHEN** the user activates the token on a workspace header (mouse or the
  keyboard equivalent)
- **THEN** the merge-queue detail opens scoped to that repo's entries

#### Scenario: An empty queue is silent

- **WHEN** a repo has no merge-queue entries
- **THEN** its header row shows no token and reads exactly as today

#### Scenario: The chip is restorable via bars config

- **WHEN** the user adds `"mq"` to a `[bars]` slot
- **THEN** the merge-queue chip renders in that slot with its red/amber/dim
  grammar and overlay activation, in addition to the project tokens

#### Scenario: A narrow sidebar never corrupts the header

- **WHEN** the sidebar is at its 12-column floor and a repo's queue is red
- **THEN** the workspace label stays legible and the token drops its count
  (or itself) rather than wrapping the row
