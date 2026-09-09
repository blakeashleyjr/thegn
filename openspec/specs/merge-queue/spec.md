# Merge Queue

## Purpose

The merge queue lets a user assign worktree branches to a per-repo queue that
drains serially, folding each branch onto the target tip in the object database,
test-gating it, and atomically CAS-advancing the target ref (never a working-tree
merge) for clean branches. Conflicts and gate failures can be handed to a
headless CLI agent that fixes the branch in its own worktree, while thegn
always performs the land itself so object-DB coherence and the merge guard hold.
The whole queue is drivable from a `merge` CLI namespace.

## Requirements

### Requirement: Worktree branches can be assigned to the merge queue

thegn SHALL let a user assign worktree branches to a per-repo merge queue,
both explicitly (one or more named worktrees) and in bulk (every eligible
worktree branch), and SHALL let them list, remove, and clear queue entries. An
assigned branch MUST be recorded with a `queued` status keyed by its worktree
path, so the queue survives across invocations and is visible in the panel. Each
row MUST also record the worktree's location descriptor (the host it lives on),
so the queue can attribute a row to a host without resolving it against the local
filesystem.

#### Scenario: Explicitly assigning a worktree queues its branch

- **WHEN** a user runs `merge add <worktree>`
- **THEN** that worktree's current branch is recorded in the queue as `queued`
  against the repo's target branch, together with the worktree's location

#### Scenario: Assigning all eligible branches

- **WHEN** a user runs `merge add --all` in a repo
- **THEN** every eligible worktree branch (excluding the target branch and, absent
  `snapshot_dirty`, dirty worktrees) is queued

#### Scenario: Removing and clearing entries

- **WHEN** a user runs `merge rm <worktree>` or `merge clear`
- **THEN** the named entry (or every entry for the repo) is removed from the queue

#### Scenario: A queued worktree on another host is still attributed to its repo

- **WHEN** the queue lists rows for a repo and a queued worktree lives on another
  host (so it cannot be resolved by a local `git worktree list`)
- **THEN** that row is still attributed to its repo by its recorded `repo_path`
  and included in the repo's queue, instead of being silently dropped

### Requirement: The queue drains branches one at a time and auto-lands the clean ones

thegn SHALL drain the queue serially, one branch at a time, oldest-queued
first. For each branch it SHALL fold the branch onto the repo's current target
tip in the object database and test-gate the result; a branch that merges clean
and passes the gate SHALL be landed by an atomic compare-and-swap of the target
ref when `auto_land` is on, or held at a `ready` status when it is off. The
target ref MUST only advance through the object-DB fold + CAS (never a
working-tree merge), and the main checkout SHALL be fast-forwarded without
clobbering uncommitted work.

#### Scenario: A clean branch lands automatically

- **WHEN** the driver drains a queued branch that folds clean and gates green
  with `auto_land = true`
- **THEN** the target ref is CAS-advanced to include the branch and the row is
  marked `landed`

#### Scenario: auto_land off holds at ready

- **WHEN** the same branch is drained with `auto_land = false`
- **THEN** the target ref is not advanced and the row is marked `ready` for a
  later explicit land

#### Scenario: An already-merged branch is a no-op

- **WHEN** a queued branch's tip is already an ancestor of the target
- **THEN** the driver records it as landed without creating a redundant merge

### Requirement: Conflicts and gate failures are handed to a headless agent

When `conflict_handoff` is `"agent"` and an agent is configured — either as an
`agent_command` template or as the name of a configured `[[agents]]` entry — the
driver SHALL dispatch a headless CLI agent to fix a branch that has a textual
merge conflict or fails the test gate, running the agent in that branch's own
worktree with a task prompt describing the conflict paths or the gate output.
The prompt SHALL be rendered from a configurable template for that failure kind,
defaulting to thegn's built-in instructions when the user has not supplied one.
After the agent finishes, the driver SHALL re-attempt the fold; it SHALL retry up
to `agent_max_attempts` and mark the branch `needs_human` if it still cannot
land. The agent MUST NOT be relied on to merge into the target — thegn performs
the land itself, so the object-DB coherence guarantee and the merge guard hold.
Each agent invocation SHALL be bounded by `agent_timeout_secs`.

#### Scenario: The agent resolves a conflict and the branch lands

- **WHEN** a queued branch conflicts with the target and the agent resolves it in
  the worktree
- **THEN** the driver's re-attempt folds the branch clean and lands it

#### Scenario: The agent cannot fix it within the attempt budget

- **WHEN** the agent fails to make the branch landable within `agent_max_attempts`
- **THEN** the branch is marked `needs_human` and the target is left unchanged

#### Scenario: Agent handoff disabled defers instead

- **WHEN** `conflict_handoff` is not `"agent"` or no agent is configured and a
  branch conflicts or fails the gate
- **THEN** the branch is left `deferred` / `gate_failed` with its reason recorded,
  and no agent is run

#### Scenario: A custom prompt template replaces the built-in instructions

- **WHEN** a user configures a prompt template for the conflict or gate-failure
  kind and the driver dispatches the agent for that kind
- **THEN** the agent receives the user's rendered template instead of thegn's
  built-in prompt

#### Scenario: No configured template preserves today's prompt

- **WHEN** no prompt template is configured for a failure kind
- **THEN** the agent receives thegn's built-in prompt for that kind, unchanged

### Requirement: The merge queue is driven from the CLI

thegn SHALL expose a `merge` command namespace (`add`, `list`, `rm`, `clear`,
`drain`, `land`) that assigns and drains the queue programmatically, honoring the
`--json` output convention. The batch fold-everything path SHALL remain available
as the `integrate` command.

#### Scenario: Draining from the CLI reports outcomes

- **WHEN** a user runs `merge drain`
- **THEN** each branch's outcome (landed / ready / deferred / needs a human) is
  reported, and `--json` emits a machine-readable summary

#### Scenario: Landing a ready branch

- **WHEN** a user runs `merge land <worktree>` for a branch held at `ready`
- **THEN** the branch is folded and CAS-advanced into the target

### Requirement: Prompt and command templates are validated before use

thegn SHALL validate agent prompt and command templates against the variables
available for their task kind, and SHALL report an unknown placeholder as a
configuration error rather than expanding it to nothing. Command templates MUST
use bare placeholders, because every substituted value is shell-quoted when the
command line is composed; a placeholder written inside quotes in a command
template SHALL be reported as a configuration error, since quoting it a second
time would deliver the value with literal quote characters attached.

#### Scenario: An unknown placeholder is rejected

- **WHEN** a configured prompt template references a variable that its task kind
  does not provide
- **THEN** configuration validation reports the unknown placeholder and names the
  variables that kind does provide

#### Scenario: A double-quoted placeholder is rejected

- **WHEN** a configured command template wraps a placeholder in quotes
- **THEN** configuration validation reports it, because the value is already
  shell-quoted during substitution

### Requirement: Submodule pointer conflicts are explicit

A gitlink fold conflict SHALL be classified as a submodule pointer conflict,
name the path and both competing SHAs in operator/agent-facing output, and MUST
NOT be sent through text conflict drivers or automatic rerere resolution.

#### Scenario: Two pointer updates conflict

- **WHEN** queue folding finds different gitlinks for the same submodule path
- **THEN** the disposition reports `submodule pointer conflict` with both SHAs
  and requires an explicit resolution

### Requirement: Cross-host branch tips are fetched into the target store before folding

The merge queue folds each branch into the target repo's object store. When a
queued branch's worktree lives on a different host from the target repo, its tip
commit exists only in that branch host's own object store and is therefore absent
from the target. thegn SHALL make the tip present in the target store before
folding, by creating a git bundle of the branch on its own host, transferring it
to the target host, and fetching it under a synthetic ref
(`refs/thegn/mq/<branch>`) that the fold then merges. A branch that already
shares the target store (local, or the same host as the target) SHALL be folded
directly from `refs/heads/<branch>` with no transfer. If the branch host is
unreachable or the transfer fails, the branch SHALL be deferred with the reason
recorded and MUST NOT be silently dropped, so a transient failure is retried on
the next drain.

#### Scenario: An off-host branch lands via a fetched tip

- **WHEN** the driver drains a queued branch whose worktree is on another host and
  the host is reachable
- **THEN** the branch's tip is bundle-fetched into the target store under
  `refs/thegn/mq/<branch>`, folded onto the target, and landed like a same-host
  branch

#### Scenario: A same-host branch needs no transfer

- **WHEN** the driver drains a queued branch whose worktree shares the target's
  object store
- **THEN** it is folded directly from `refs/heads/<branch>` with no bundle/fetch

#### Scenario: An unreachable branch host defers the row

- **WHEN** a queued branch's host is unreachable or its tip cannot be fetched in
- **THEN** the row is marked `deferred` with the reason, is not dropped, and is
  retried on a later drain

### Requirement: The drain is anchored to the target repo's host

Because the fold, test-gate, and CAS-advance all operate in the target repo's
object store and working tree, the drain SHALL run on the host where the target
repo lives. When a drain, land, or integrate command is invoked for a repo whose
target store lives on another host, thegn SHALL decline to fold in place and
SHALL tell the user which host to run the drain on, rather than attempting a
partial fold or failing obscurely.

#### Scenario: Draining a remote-target queue guides to the target host

- **WHEN** a user runs `merge drain`, `merge land`, `land`, or `integrate` for a
  repo whose target branch lives on another host
- **THEN** thegn declines to fold locally and reports the host on which the drain
  must be run (where off-host branch tips are fetched in automatically)

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

### Requirement: The queue lifecycle organizes worktrees into sidebar folders

When `[merge_queue] organize_folders` is on (the default), thegn SHALL file a
branch's worktree into a sidebar folder as the branch moves through the queue:
into `queued_folder` (default "Merging") on enqueue, into `failed_folder`
(default "Needs attention") when it cannot land (conflict, red gate, agent
gave up), and — per `on_landed` — into `merged_folder` (default "Merged") when
it lands under the `move`/`expire` arms, with `expire` additionally letting
the sweep collect the worktree after `merged_ttl_secs`. A plain dequeue
(`merge rm` / `merge clear` or the in-app remove) SHALL return the worktree to
the ungrouped repo root, but MUST only clear membership of a folder the
lifecycle itself manages — a folder the user filed the worktree into by hand
is never touched. Folder bookkeeping MUST be best-effort (a DB failure never
fails a merge), MUST never file or remove the home/main checkout, and MUST
honor the per-repo `[workspace.<slug>.merge_queue]` overlay. With
`organize_folders = false` the whole lifecycle SHALL be inert.

#### Scenario: Enqueue files into the queued folder

- **WHEN** a worktree branch is enqueued with `organize_folders = true`
- **THEN** its worktree is filed into the `queued_folder` sidebar folder
  (created if absent) under its own workspace

#### Scenario: A queue land files into the merged folder

- **WHEN** a drained branch lands cleanly with `on_landed = "move"` or
  `"expire"`
- **THEN** the worktree is re-filed from the queued folder into
  `merged_folder`, and under `expire` it remains a sweep candidate once
  `merged_ttl_secs` elapses

#### Scenario: Dequeue leaves user folders alone

- **WHEN** a queued worktree that the user had hand-filed into their own
  folder is removed from the queue
- **THEN** its folder membership is unchanged; only lifecycle-managed folder
  memberships (queued / failed / merged) are cleared by a dequeue

#### Scenario: The master toggle disables everything

- **WHEN** `organize_folders = false` and a branch is enqueued, lands, or
  fails
- **THEN** no folder is created and no worktree is filed or un-filed

### Requirement: A fold-actor land files the worktree into the merged folder

A successful `thegn land` (including the already-in-target no-op) SHALL file
the landed worktree into `merged_folder` when `on_landed` is `move` or
`expire`, and — because `thegn land`'s contract is leave-in-place, it is
routinely invoked from inside the worktree being landed — SHALL also file
rather than remove under the destructive `remove`/`detach` arms. It MUST NOT
remove the worktree or delete the branch under any `on_landed` value. With
`on_landed = "off"` it SHALL clear a lifecycle-managed folder membership
(the stranded-in-"Merging" cleanup) and otherwise leave the worktree where it
is. Because `thegn land` records no queue row, a worktree it files MUST NOT
become an expiry-sweep candidate.

#### Scenario: thegn land moves the worktree to Merged

- **WHEN** `thegn land` CAS-advances the target with the default config
  (`organize_folders = true`, `on_landed = "expire"`)
- **THEN** the worktree is filed into `merged_folder` — not returned to the
  ungrouped repo root — and the worktree directory and branch are left in
  place

#### Scenario: Destructive on_landed degrades to filing for a land-in-place

- **WHEN** `thegn land` lands a branch with `on_landed = "remove"`
- **THEN** the worktree is filed into `merged_folder` and neither the
  worktree nor the branch is deleted

#### Scenario: A land-in-place is never swept

- **WHEN** a worktree filed into `merged_folder` by `thegn land` passes
  `merged_ttl_secs` in age
- **THEN** the expiry sweep does not collect it, because only queue-recorded
  `landed` rows are sweep candidates
