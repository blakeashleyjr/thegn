# Design

## Why a separate pure module

Ordering rules were duplicated across three call sites (`move_worktree_group`,
`sidebar_mouse::worktree_run`, `run::sidebar_worktree_order`) and each had
drifted into its own flat, folder-blind notion of "the run". Folding them into
one pure function over `&[SidebarRow]` — the rows the renderer actually
painted — means the keyboard and the mouse cannot disagree about where a move
lands, and the rules are testable without a terminal, a DB, or a session. This
is the same shape as `render_plan::plan`.

## Run membership comes from the row tree, not `pin_key`

A filed worktree's `pin_key` embeds its folder (`{slug}/{branch}/folder:{id}`),
so membership could be parsed out of it. Reading the tree instead — a depth-2
worktree belongs to the folder header above it — matches exactly the
containment `build_rows` emits, and it survives `apply_pins`, which reorders
whole blocks and therefore never separates a folder's children from their
header. It also avoids a second place that has to know the key format.

`runs()` deliberately includes rows that are currently **invisible** (filed
into a collapsed folder). A persisted order has to account for every member;
dropping the hidden ones would leave them with stale positions that interleave
on the next reload.

## Whole-order writes, not swaps

`set_workspace_order` already exists because a two-position swap leans on a
normalize pass to heal NULL/tied values, and can seed a sequence that differs
from what the tree is showing. Worktrees had exactly the same hazard, plus a
worse one: the mouse drove a _bounded step loop_ of swaps, so a drop that
couldn't converge (which every cross-folder drop couldn't) left the run
partially reordered when the loop hit `max_steps`.

`Plan.order` therefore carries the workspace's entire new sequence and
`set_worktree_order` writes it in one transaction. A drop is one decision and
one write.

`worktrees.position` is a table-wide sequence, so a per-workspace rewrite can
tie with another workspace's values. That is harmless — `worktrees()` is
grouped by `repo_path` before order is ever compared — and is called out in
the store doc comment.

## Live vs dormant

A loaded workspace renders from `session.worktrees` slot order; a dormant one
renders from the DB list. Keying the model on **worktree paths** rather than
session group indices makes both work from the same plan: the model's DB list
is re-sorted optimistically (dormant), and the session's slots for that
workspace are permuted in place (live). The active group is tracked by name
across the permutation — the previous pairwise index fixup only expressed a
swap, not a general permutation.

## Crossing semantics

Up off a run's head lands at the **end** of the previous run; down off the tail
lands at the **head** of the next. That is the reading that makes a single
repeated keypress walk a worktree continuously down the tree, and it mirrors
what a drag between the same two rows would do.

Collapsed folders are skipped rather than entered. Entering one would move a
row into a container the user cannot see, and the row would appear to vanish —
the same reason `home` is anchored.

## What stayed

`file_worktree_path` / `unfile_worktree_path` still own header drops and the
`f` key: they resolve folders **by name** and create one on demand (with a
synthetic negative id reconciled by a deferred refresh), which the ordering
model has no business knowing about. The drop path now simply follows them
with a move-to-end-of-run so the row lands somewhere predictable instead of
wherever its stale position happened to sort.
