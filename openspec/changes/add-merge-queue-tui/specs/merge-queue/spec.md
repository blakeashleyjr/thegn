# Merge Queue

## ADDED Requirements

### Requirement: The merge queue is manageable from the panel section

thegn SHALL let a user manage the merge queue from the Work ▸ Merge queue
panel section without leaving the TUI: enqueue the active worktree's branch or
every eligible branch, remove an entry, retry a blocked (deferred /
gate-failed / needs-human) entry, land a `ready` entry, clear landed entries,
and drain the queue. Every mutation MUST run off the event loop and report its
outcome to the user (toast or status line). The section MUST be reachable via a
default keybinding and the command palette, and its rows MUST be cursor-
addressable so per-row actions target the selected entry.

#### Scenario: Enqueueing from the section

- **WHEN** the user presses `a` (or `A`) in the Merge queue section
- **THEN** the active worktree's branch (or every eligible worktree branch) is
  recorded as `queued` and the row appears without waiting for a periodic
  refresh

#### Scenario: Retrying a blocked row

- **WHEN** the user presses `r` on a row whose status is `deferred`,
  `gate_failed`, or `needs_human`
- **THEN** the entry is re-queued (status reset to `queued`, failure details
  cleared) for the next drain

#### Scenario: Landing a ready row

- **WHEN** the user presses `l` on a row whose status is `ready`
- **THEN** the branch is landed through the same fold/gate/CAS core as
  `merge land` and the row records the outcome; on any other status the key
  explains itself instead of acting

### Requirement: An in-app drain streams live status transitions

thegn SHALL run the full agent-driven queue drain (the same driver as
`merge drain`, including headless-agent conflict handoff) from inside the TUI,
off the event loop. Every per-branch status transition SHALL be streamed back
to the loop and painted on the next frame by patching the panel's queue row in
place — the user MUST NOT have to wait for the periodic model refresh to see
`folding`, `agent_running`, or a settled state. The batch fold and the queue
drain SHALL be mutually exclusive (they advance the same target ref), and a
second dispatch while one is running MUST be refused with an explanation.

#### Scenario: Watching a drain live

- **WHEN** the user triggers a drain (section `D`, the `merge-drain` palette
  action, or a bound chord) with queued branches present
- **THEN** each branch's row transitions visibly through the driver's states as
  they happen, and a summary toast reports landed/ready/deferred/needs-human
  counts when the drain completes

#### Scenario: Drain refused while one is inflight

- **WHEN** the user triggers a drain or batch fold while either is already
  running
- **THEN** the dispatch is refused with an "already integrating" notice and no
  second driver starts

### Requirement: Queue state is visible outside the section

thegn SHALL surface merge-queue state ambiently: a statusbar chip that is
red when any entry is blocked (deferred / gate-failed / needs-human), amber
while the queue is working (folding / verifying / agent running), and quietly
dim when entries are merely queued or held at ready — silent only when the
queue is empty. The chip's overlay SHALL list the entries with per-row actions
(focus the entry's worktree; open the panel section). Worktree rows in the
sidebar SHALL carry a merge-queue status chip on their detail line. Settled
transitions SHALL be routed as notifications — `queue_landed` (info),
`queue_ready` (notice), and `queue_needs_human` (alert) — through the standard
routing rules so they toast, sound, and land in the inbox per the user's
configuration.

#### Scenario: An idle queue is still discoverable

- **WHEN** branches are queued but nothing is running or failing
- **THEN** the statusbar shows a quiet dim `MQ` chip (instead of nothing), and
  activating it opens the queue overlay

#### Scenario: The agent gives up

- **WHEN** a drain marks a branch `needs_human`
- **THEN** the statusbar chip turns red, the branch's sidebar row shows the ✋
  chip, and a `queue_needs_human` alert notification is recorded (toast/sound
  per routing rules)
