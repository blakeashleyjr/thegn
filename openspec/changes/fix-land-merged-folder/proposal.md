# Fix: a fold-actor land files the worktree into the Merged folder

Linear: THE-63

## Why

The merge-queue → sidebar-folder lifecycle already exists and defaults ON
(`[merge_queue] organize_folders = true`): enqueue files a worktree into
"Merging", failure into "Needs attention", and a queue land into "Merged"
(`on_landed = "expire"`, the shipped default). But the **`thegn land`** path —
the blessed one-shot land, and the dominant landing gesture in the
sandbox/worktree workflow — deliberately emits `LifecycleEvent::Dequeued`
instead of `Landed` (`crates/thegn-host/src/cmd/land.rs:76-85`), because
`Landed` under `on_landed = remove/detach` would delete the worktree and
`thegn land`'s contract is leave-in-place. The side effect is exactly THE-63:
after a successful `thegn land`, the worktree is un-filed back to the
ungrouped repo root instead of moving to "Merged". The avoidance was
over-broad — under the default `move`/`expire` arms `Landed` is a plain
folder move, not a removal.

None of this behaviour is in the spec: `openspec/specs/merge-queue/spec.md`
has no lifecycle-folder requirement at all, so the contract that would have
caught the divergence was never written down.

## What Changes

- **New pure event `LifecycleEvent::LandedInPlace`** in
  `thegn_core::merge_lifecycle`: a land that keeps the worktree where it is.
  `decide` maps it to `FileInto(merged_folder)` under `move`/`expire` — and
  also under `remove`/`detach`, degrading the destructive arms to a
  non-destructive filing, because the caller has declared the worktree must
  stay (it is typically the caller's own cwd — a sandboxed agent runs
  `thegn land` from inside the worktree being landed). Under `off` it maps to
  `Unfile`, preserving today's cleanup of a stale "Merging" membership.
- **`thegn land` emits `LandedInPlace`** on `Landed` and `UpToDate` outcomes
  instead of `Dequeued`. The host-side guard is unchanged: only
  lifecycle-managed folders are ever left, a user-filed folder is never
  touched, and filing is best-effort (a DB hiccup never fails the land).
- **Spec the lifecycle** — the whole existing folder lifecycle (enqueue /
  failure / queue-land / dequeue, defaults, master toggle, per-repo override
  via `[workspace.<slug>.merge_queue]`, the home anchor, best-effort) is added
  to the merge-queue spec, plus the new fold-actor-land requirement.
- **No sweep coupling.** `thegn land` still writes no queue rows, so a
  worktree it files into "Merged" is _not_ a candidate for the `expire` TTL
  sweep (`merge_sweep` collects only `landed` queue rows). It stays filed
  until the user removes it. Called out in the spec so it is a contract, not
  an accident.

## Impact

- **tasks.md:** group **T** item 758 (agent-driven merge-queue driver — this
  is lifecycle polish on its shipped folder organization).
- **Capabilities:** `merge-queue` — ADDED requirements (lifecycle folders;
  fold-actor land filing). No other capability touched.
- **Code:** `crates/thegn-core/src/merge_lifecycle.rs` (new event + decide arm
  - exhaustive tests — pure logic, 95% core gate),
    `crates/thegn-host/src/cmd/land.rs` (event swap at the two success arms).
    No DB schema change, no new config key, no new wake path, no render change.
- **In-flight reconciliation:** `add-merge-queue-tui` adds _management/
  visibility_ requirements to the same capability — disjoint from these
  lifecycle requirements; either lands first cleanly.
  `add-cross-host-merge-queue` / `add-remote-enqueue-modes` change queue
  transport, not the folder lifecycle. No overlap with the sidebar changes:
  filing is a DB `folder_id` write consumed by the existing tree model.
