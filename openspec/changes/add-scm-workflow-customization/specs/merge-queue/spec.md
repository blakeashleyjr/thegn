# Merge Queue

## ADDED Requirements

### Requirement: The land strategy is configurable

thegn SHALL support `[merge_queue] land_strategy` = `merge` (default, today's
2-parent fold commit), `squash` (one single-parent commit carrying the merged
tree), or `rebase` (the branch's commits replayed one at a time in the object
database), overridable per workspace. Every strategy MUST preserve the
existing land guarantees: the target ref advances only by object-DB fold +
gate + CAS, never a working-tree merge; a branch already an ancestor of the
target is a no-op; a conflict at any step defers the whole branch (no partial
replays land). The land commit message SHALL be rendered from a configurable
`land_message` template using the shared prompt-template engine.

#### Scenario: Squash lands one commit

- **WHEN** a three-commit branch drains with `land_strategy = "squash"` and
  gates green
- **THEN** the target advances by exactly one commit whose tree equals the
  merge result and whose sole parent is the previous target tip

#### Scenario: Rebase replay stops on conflict

- **WHEN** a branch drains with `land_strategy = "rebase"` and its second
  commit conflicts with the target
- **THEN** no replayed commit lands, and the branch defers with the conflict
  recorded exactly as a conflicting merge fold would

#### Scenario: Default is unchanged behaviour

- **WHEN** `land_strategy` is unset
- **THEN** the fold produces the same 2-parent merge commits as before this
  change

### Requirement: thegn-created commits can be signed, and never prompt

When `[merge_queue] sign_commits = true`, fold/land commits SHALL be created
with `-S` (deferring key and format — GPG or SSH — to the user's git config
under the active identity). Signing MUST be non-interactive: the invocation
runs with terminal prompts disabled, and a signing failure SHALL be
classified as an infrastructure error that stops the drain with a clear
reason — it MUST NOT mark the branch `needs_human` and MUST NOT dispatch the
fixing agent. Independent of that key, the `snapshot_dirty` snapshot commit
SHALL honor `[git] override_gpg` like every other background history
operation, so an ambient `commit.gpgSign = true` can never hang a background
snapshot on a passphrase prompt.

#### Scenario: Signed fold commit

- **WHEN** a branch lands with `sign_commits = true` and a non-interactive
  signing setup
- **THEN** the created land commit carries a signature verifiable by
  `git verify-commit`

#### Scenario: Signing failure never blames the branch

- **WHEN** signing fails (agent locked, key missing, would-prompt)
- **THEN** the drain stops with a signing-infrastructure reason, the branch
  keeps its status, and no agent is dispatched

#### Scenario: Snapshot commit cannot hang

- **WHEN** `snapshot_dirty` snapshots a worktree in a repo whose git config
  sets `commit.gpgSign = true` and `[git] override_gpg = true`
- **THEN** the snapshot commit completes unsigned instead of waiting on a
  pinentry

### Requirement: Custom merge drivers and rerere participate in conflict folds

When a conflicted path in a fold is governed by a custom `.gitattributes`
`merge=<driver>` declaration that the object-DB fold did not honor, thegn
SHALL merge that branch through a throwaway-worktree real `git merge` (the
same machinery as lockfile regeneration) so the driver runs, feeding the
resulting tree back into the gated fold. When `[merge_queue] rerere = true`,
the reused gate worktree and the driver-merge worktree SHALL run with rerere
enabled against the repo's shared `rr-cache`, so a previously recorded
resolution auto-resolves on later drains — and an auto-resolved merge MUST
still pass the gate before landing. Clean folds MUST NOT pay any worktree or
attribute-check cost.

#### Scenario: Driver-governed conflict routes through a real merge

- **WHEN** a fold conflicts on a path whose merge is governed by a custom
  driver
- **THEN** the branch is merged in a throwaway worktree where git runs the
  driver, and the result is gated and landed through the normal fold + CAS

#### Scenario: A recurring conflict resolves itself the second time

- **WHEN** `rerere = true` and a drain hits a conflict identical to one
  resolved in an earlier drain
- **THEN** the recorded resolution applies, and the branch still runs the
  gate before landing
