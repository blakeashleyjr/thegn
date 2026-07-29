# merge-queue (delta)

## MODIFIED Requirements

### Requirement: Worktree branches can be assigned to the merge queue

Assigning a worktree SHALL enqueue its current branch against the target repo's
queue. When the worktree is on a **remote host** (a sprite) whose target repo
lives on another host, the destination of the row SHALL be selected by
`[merge_queue] remote_mode`:

- `route_to_host` — the row is written into the **target host's** queue via the
  host daemon's control plane, not the sprite's local DB.
- `push` — the row is written into the sprite's **local** queue (the sprite will
  drain its own clone and push).

For an on-host worktree the behavior is unchanged (local enqueue), regardless of
`remote_mode`.

#### Scenario: route-to-host enqueue reaches the host DB

- **WHEN** `merge add` runs in a sprite whose target repo is off-host and
  `remote_mode = route_to_host`, with a control endpoint + `MergeAdd`-scoped
  token injected
- **THEN** thegn POSTs the enqueue to the host daemon's `/v1/merge/add`, the row
  appears in the **host's** queue carrying the sprite's `location`, and the
  operator sees confirmation naming the host
- **AND** nothing is written to the sprite's local queue

#### Scenario: route-to-host with no reachable host defers with guidance

- **WHEN** `remote_mode = route_to_host` but no control endpoint/token is present
  or the host is unreachable
- **THEN** the enqueue fails with a clear message (how to provision the token /
  which host is unreachable) and does not silently fall back to a local row

#### Scenario: push-mode enqueue stays local

- **WHEN** `merge add` runs in a sprite with `remote_mode = push`
- **THEN** the branch is queued in the sprite's local queue for a local drain

## ADDED Requirements

### Requirement: Push mode lands the sprite's own clone and pushes to origin

When `[merge_queue] remote_mode = push`, draining on the sprite SHALL fold, gate,
and advance the sprite's **local** target branch — even if the target reads
off-host (the remote-target guard is bypassed) — and then SHALL `git push` the
advanced target to `origin`. A push failure SHALL defer the affected work with a
surfaced reason, never report a false success.

#### Scenario: a clean branch lands on origin via push

- **WHEN** a sprite with `remote_mode = push` drains a queued, conflict-free branch
- **THEN** its local target advances and is pushed to `origin`, so the host and
  other clones converge by fetching `origin`

#### Scenario: a rejected push does not report success

- **WHEN** the post-advance `git push` is rejected (e.g. non-fast-forward)
- **THEN** the drain reports the push failure with its reason and the branch is
  not marked landed upstream
