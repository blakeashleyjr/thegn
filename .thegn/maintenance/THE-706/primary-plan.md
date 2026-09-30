# THE-706 — primary greenlight (with a scope cut)

**Greenlit, reduced.** The investigation in
`.thegn/pipeline/THE-706/maintenance-investigate/692.md` is accurate and did the
reader audit I asked for. But it found ~12 consumer sites, and implementing all
of them in one lane is how a lane sprawls and lands half-verified. Where this
document and the investigation disagree, this document wins.

## What I verified myself

I spot-checked the two load-bearing decision claims rather than taking them:

- **`crates/thegn-host/src/cmd/wt.rs:559`** — confirmed. `rm` resolves its
  target with `w.worktree == target_path || w.branch == target`, so
  `thegn wt rm <branch>` can select a worktree by a branch it no longer holds.
  Note the very next arm already falls back to git with the comment "the DB is a
  cache; git is the source of truth" — the intent is present, the first match
  arm just doesn't honour it. This is the most dangerous site found and it is
  **in scope**.
- **`crates/thegn-host/src/attention_status.rs:126`** — confirmed, and it is
  **not this issue**. `is_home: wt.branch == "home"` uses the branch string as
  worktree _identity_. Reconciling it against git makes it worse, not better: a
  home worktree sitting on `main` would correctly reconcile to `main` and then
  stop being home. That is a distinct bug with a distinct fix (use the stable
  row/tab identity), and I am filing it separately. **Out of scope — do not
  touch `attention_status.rs`.**

I also accept, without re-deriving it, the investigation's judgement that the
**merge queue is a separate durable contract** (`merge_sweep.rs`,
`merge_cleanup.rs` already prove live git identity). Leave it alone. Replacing a
persisted outcome identity with a mutable display field would be a regression.

## In scope

1. **The snapshot helper — build this first and test it hardest.** A pure host
   helper joining one batched `GitBackend::worktrees(root)` / `git worktree
list --porcelain` result to registry rows by normalized path, with explicit
   representation for detached HEAD, unborn, and unavailable. Do not hand-parse
   porcelain at each caller. Unit-test the mapping: multiple worktrees on
   different branches, detached HEAD, a changed checkout, a path that no longer
   exists, and two rows whose stale branch values collide.

2. **Hydration feeds every row from the snapshot**, dormant and gated rows
   included — that gap is the whole visible symptom.

3. **The decision paths**, in this priority order:
   - `cmd/wt.rs` `rm` target resolution (highest risk);
   - PR-close cleanup (`hydrate.rs` ~4061–4117);
   - agent/tool/dispatch branch context (`daemon/agent_open.rs`,
     `cmd/session.rs`);
   - control `worktrees.list` (`daemon/service.rs`) — externally visible, so a
     consumer can decide on it.

4. **`BranchRow.held_by` + the branch menu.** This is where the user-visible
   value is and what THE-697 needs. Keep `is_head` meaning exactly what it means
   today (this panel's worktree). A branch held by a _sibling_ worktree must
   either not offer delete or name the holder. The race itself stays THE-697's.

## Out of scope — do not do these

- `attention_status.rs` (separate bug, filed separately — see above).
- Merge queue / sweep / `merge_cleanup` (separate durable contract).
- `cmd/disk.rs`, `measure/disk.rs`, `palette.rs`, `automation_runtime.rs`.
  These are display-only labels and events; they cannot select or delete a
  worktree. Once the snapshot exists they are a trivial follow-up. Leaving them
  is honest debt; doing them here is what makes the diff unreviewable.
- `thegn list` (`cmd/list.rs`) — already git-first. **Preserve it unchanged**;
  it is the positive example.
- Any schema change, migration, watcher, background thread, or new wake source.

## The open question you asked me to settle

> whether every displayed/API projection should return unknown on a failed git
> snapshot, or use the stored branch strictly as a labeled display fallback

**One rule, applied by kind:**

- **Display** may fall back to the stored value when the snapshot is
  unavailable. A slightly stale label beats a blank sidebar.
- **Decisions fail closed.** A decision path that cannot get a live answer must
  refuse or report unknown. It must never reach for the stored value — that is
  the entire defect this issue describes.

If a site is ambiguous, treat it as a decision. Getting that wrong in the
cautious direction costs a refusal; getting it wrong the other way deletes the
wrong worktree.

## Hard constraints

- **No git on the event loop.** `git` is subprocess I/O. It runs on the existing
  off-loop hydration worker or an existing command/daemon worker. If the natural
  place turns out to be on-loop, **stop and tell me** — that is a design
  question, not something to solve with a blocking call. The repo's 0%-idle
  contract is not negotiable for a branch label.
- **No new watcher.** One batched call on an existing path. If you find yourself
  adding a thread, you have left the plan.
- **Test fixtures:** plain `git` in a temp dir. Never shell out to
  `thegn wt new` — a known bug makes that mutate the _real_ repo. Every fixture
  commit needs `-c commit.gpgsign=false`, or it hangs on a GPG prompt inherited
  from global config.
- **Never open or migrate the live DB** from your shell.

## If you run short

Ship items 1, 2 and 4 complete and tested, and report exactly which of item 3's
four sites you did. A lane that lands a correct snapshot helper plus the
held-branch fix is worth more than one that touches twelve files and compiles in
none of them. Say where you stopped; do not quietly narrow and call it done.

## Verification you owe me

- `cargo check -p thegn-host --all-targets` clean (attempt it; if `nix develop`
  fails in the sandbox, report that verbatim and stop).
- The new tests pass, and at least one of them **fails** against the current
  behaviour — a test that passes before and after is not encoding this bug.
- `git diff main...HEAD --diff-filter=D --name-only` prints nothing.
