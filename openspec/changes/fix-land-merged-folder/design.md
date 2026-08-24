# Design — fold-actor land files into the Merged folder

## The decision table (pure, `thegn_core::merge_lifecycle::decide`)

| event           | `off`                                                          | `move`        | `expire`      | `detach`             | `remove`            |
| --------------- | -------------------------------------------------------------- | ------------- | ------------- | -------------------- | ------------------- |
| `Enqueued`      | file "Merging" (all arms — `on_landed` is irrelevant pre-land) |
| `Failed`        | file "Needs attention" (all arms)                              |
| `Landed`        | Noop                                                           | file "Merged" | file "Merged" | remove (keep branch) | remove (del branch) |
| `LandedInPlace` | Unfile                                                         | file "Merged" | file "Merged" | **file "Merged"**    | **file "Merged"**   |
| `Dequeued`      | Unfile                                                         | Unfile        | Unfile        | Unfile               | Unfile              |

`LandedInPlace` is the only addition. Its two deliberate divergences from
`Landed`:

- **`remove`/`detach` degrade to filing, never removal.** `thegn land` is
  scripted from _inside_ the worktree being landed (CI, the fold-actor, a
  sandboxed agent whose cwd it is); deleting the caller's working directory
  out from under it is the failure mode the leave-in-place contract exists to
  prevent. A `remove`-mode user still gets their cleanup on the _queue_ path
  (`merge drain` / `merge land`), which is where they configured it.
- **`off` maps to `Unfile`, not Noop.** Today's `Dequeued` emission is what
  heals a worktree stranded in "Merging" after a fold-actor land; with
  `on_landed = "off"` the user has said "no Merged folder", so the stale
  "Merging" membership must still be cleared. The host-side guard (only
  lifecycle-managed folder names are ever left) carries over unchanged.

## Why not just emit `Landed` and special-case the host

The host executor (`thegn-host/src/merge_lifecycle.rs::apply`) is the I/O
half; teaching it "this Landed is a land-in-place" would smuggle policy into
the executor and leave the pure half untestable for the case that matters.
The event vocabulary is the policy surface — a fourth settled event is the
honest model, and it lands in the exhaustively-tested pure module (core 95%
line gate covers every arm of the table above).

## Why `thegn land` does not gain a queue row

Filing into "Merged" under `expire` raises the question of the TTL sweep.
`merge_sweep` collects candidates from `merge_queue` rows with status
`landed`; `thegn land` writes no rows ("no DB / queue side effects" is its
documented contract), so its filings are folder-only and never expire-swept.
Writing a synthetic `landed` row was considered and rejected: it would make a
non-queue command mutate queue state, surprise `merge list`, and put a
worktree on a deletion timer the user never queued. If uniform expiry is ever
wanted it should be its own proposal. (Open question below.)

## Event loop / rendering / schema

- No wake path: `cmd/land.rs` is a CLI process, off the loop by construction;
  a running instance picks the `folder_id` change up through the existing
  hydration/git-watch path exactly as queue filings do today.
- No damage-channel change; no SQLite schema change (`worktrees.folder_id`
  and `folders` exist); no new config key.
- Per-repo override already flows: `land.rs` resolves
  `cfg.repo_merge_queue(&root)`, so `[workspace.<slug>.merge_queue]`
  `merged_folder` / `on_landed` refinements are honored.

## Security

- **No new write surface.** The change swaps which existing best-effort DB
  write (`set_worktree_folder`) fires on a path that already performed one.
  No credentials, no network, no subprocess change.
- **Blast radius:** a wrong decision mis-files a sidebar row — cosmetic by
  design ("the DB is a cache; git refs are the source of truth"). The
  destructive arms (`RemoveWorktree`) are strictly _less_ reachable after
  this change: `LandedInPlace` can never return them, and the dirty-tree
  guard in `remove_landed` is untouched for the queue path.
- **Sandbox:** unchanged — `thegn land` already works from a read-only main
  checkout; folder filing goes to the shared DB, not the tree.

## Open questions

- Should `thegn land` optionally record a `landed` queue row (`--track`?) so
  `expire` sweeps land-in-place worktrees uniformly? Deferred — needs its own
  contract discussion (it changes `merge list` semantics).
- `merge land` on a `ready` row already emits `Landed`; under `remove` it
  deletes the worktree even when invoked _from_ that worktree via the CLI.
  Same cwd hazard, pre-existing, out of scope here — noted for the audit.
