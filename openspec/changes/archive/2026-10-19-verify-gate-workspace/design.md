# Design

The host platform seam walks absolute directory components with retained
descriptors and no-follow/nonblocking opens. Root/current-user ancestors must
not be group/other writable, except root-owned sticky traversal roots such as
`/tmp`. State created at/below the selected state root belongs to the current
user. Identity files must be bounded regular single-link current-user files;
unknown types, writable sharing and alias components refuse. A nonblocking
advisory lock is held across preparation, setup, gate execution and cleanup.
Contention immediately holds with a retry diagnostic, never waits indefinitely
or silently falls back to unlocked reuse.

Repository root and Git-directory association, per-worktree administration,
gitfile, common-directory backlink and detached HEAD are checked before work.
The exact requested commit is retained. Rechecks after preparation, setup and
gate execution must still match it; a green exit cannot override changed
identity. There is no recursive worktree purge, global prune or forced creation.
Stale registrations and failed checkouts remain for explicit operator inspection.
Existing ignored artifacts in valid reused checkouts remain warm.

Every gate-owned Git invocation disables replacement objects with
GIT_NO_REPLACE_OBJECTS=1, including preparation, fresh-index materialization,
verification and cleanup. Configured setup/gate commands inherit the same Git
view. No replacement ref or user configuration is edited: the pinned OID denotes
its original object, not a mutable replacement with a different tree at the same
reported HEAD. Private regressions cover fresh/reused/throwaway gates and inherited
Git commands while retaining replacement refs unchanged. Operator commands remain
trusted and can explicitly override their environment; this is not a sandbox.

Before any checkout/materialization, bounded read-only Git probes reject enabled
or unknown sparse-checkout/cone/sparse-index settings. Reused indexes contain ordinary cached
entries: skip-worktree, assume-unchanged, unmerged and unknown tags refuse without
repair. The checks repeat at execution boundaries. A same-OID force checkout is
not proof of content identity: Git can preserve stale skip-worktree bytes while
reporting success. Private regressions preserve the raw index, HEAD, refs and
tracked bytes on refusal.

Ordinary H tags and same-OID checkout are also insufficient when fsmonitor or
cached stat data hides stale content, even with strict stat overrides. After
checkout, each attempt creates an absent private index in a unique owned directory
under the pinned gate parent. Git read-tree populates that index from the exact
commit; checkout-index --all --force materializes from fresh entries with no
inherited stat cache. GIT_INDEX_FILE is set only on those two commands. The real
index is never replaced; normal preceding checkout retains its usual index update.
Fsmonitor is disabled per command on materialization and preparation Git calls.
No global or repository configuration is rewritten.

The new materialization commands use explicit Child ownership and blocking waits
with null stdin/stdout/stderr while retaining the entire Workspace/index lease.
They do not use the read-only deadline runner. A successful wait, including a
nonzero exit, releases direct-child ownership; errors are static infrastructure
diagnostics. An unknown wait result first poisons further materialization in the
process, then retains the child, Workspace and private-index guard until process
exit. Both reuse and throwaway admission check poison. Already admitted concurrent
operations are not cancelled or claimed atomic. Failure retains the private index
directory; success removes only the revalidated regular index and empty owned
directory, never recursively. Unexpected files or replacement prevent cleanup.

Gate materialization preserves operator-configured clean/smudge/process filters,
including unused global LFS configuration, under existing trusted Git/hooks/setup
authority. Used/unused/required-failure regressions distinguish filter execution
from read-only admission; cleanup's THE-588 no-filter policy remains unchanged.
Built-in attribute/EOL conversion and authorized filter/setup transformations
remain supported, not a bitwise immutable worktree claim. Submodule/gitlink
contents are not recursively materialized, fetched or verified; configured setup
retains responsibility for those dependencies as before.

Fresh materialization rewrites tracked mtimes on every attempt, potentially
reducing incremental build reuse. Ignored artifacts, untracked notes and the
configured target cache remain preserved. This cost is explicit, not a promise
that preserving the cache directory preserves all warm-build behavior.

Fixed probes share THE-588's extracted private host capture runner with cleanup:
one two-slot process/pipe budget, 15-second completion deadline, 2 MiB per output
pipe, and retained ownership for unfinished readers/writers/reaping. Cleanup
retains its existing Git/env/policy wrapper and refusal mapping. Gate probes have
their own read-only Git wrapper; no lifecycle/cleanup policy dependency is added.
The extraction preserves worker/reaper failure and inherited-pipe tests. This
runner does not cover configured setup/gate commands or make OS spawn/filesystem
operations interruptible. Config-probe errors are static to avoid echoing values.

Gitfile paths must be absolute and canonical. Common-directory mappings may use
only a leading relative parent chain from already pinned admin ancestors (the
ordinary `../..` mapping), followed by normal components, or an absolute path
without parent traversal. Empty/dot components, absolute parents and any parent
after a supplied normal component refuse. No lexical collapse may skip a symlink
that Git would interpret physically. Root's private read-only Git probe proved
that `symlink/..` can otherwise make the validator and Git select different paths;
the linked-worktree regression checks refusal before any checkout mutation.

Throwaway mode allocates a unique owned parent and an absent child. Automatic
recursive TempDir cleanup is relinquished before Git creates content. Only a
still-verified child is removed through the selected repository's worktree
operation; only an empty verified parent is removed afterward. Unknown state is
retained rather than guessed to be disposable.

## Limits

Descriptor pins and repeated checks are not a filesystem lease against an
arbitrary malicious same-UID actor. Advisory locking serializes cooperating
gate launchers. Git hooks/configuration and the configured setup/gate commands
retain existing operator authority; this is not a sandbox or THE-225 repair.
Successful direct-child waits are not supervision of detached filter descendants.
Materialization has no execution deadline. Configured setup/gate Command output
capture remains unbounded and has no execution deadline
(THE-601); returned tails are presentation bounds, not memory/cancellation bounds.

Verified gate filesystem admission is Unix-only. Windows refuses nonempty local
gates as infrastructure rather than pretending to have safe reuse or cleanup;
an empty configured gate remains a no-op. This explicit availability limitation
requires a future verified Windows implementation, not an unsafe fallback.
