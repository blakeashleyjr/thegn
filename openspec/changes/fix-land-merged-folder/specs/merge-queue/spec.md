# Merge Queue

## ADDED Requirements

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
