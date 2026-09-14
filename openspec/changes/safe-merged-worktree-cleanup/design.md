# Verified automatic collection

Queue rows select candidates, never grant filesystem authority. Resolve the
selected repository's actual common Git directory and configured target; reject
foreign, main, detached, locked, remote, unregistered, noncanonical and mismatched
paths before hooks or runtime teardown. Cache repository identity must agree.
Automatic cleanup uses `WorkspaceStore::worktree_record` for each exact registry
observation (THE-600). Missing rows remain distinct from malformed fields or
query failures, which refuse cleanup. The existing display list deliberately
retains its tolerant behavior and is not an admission source.
Cleanup also queries raw tenancy existence by both sandbox key and associated
worktree, retaining even released/malformed historical records. Dispatch guards
hold unknown/nonterminal states and pending worktree reassignment; only known
terminal rows without a matching pending reassignment permit cleanup. Errors
propagate, while ordinary placement/display status readers remain unchanged.
Pin open identity handles for the root, common directory, worktree, linked
metadata and `.git` file. Require the current branch tip and recorded landed
commit to remain ancestors of the current target.

All identity handles use the existing nofollow platform open, with nonblocking
Unix semantics and an explicit regular-file/directory type check before
constructing a same-file handle. No following `Handle::from_path` remains.
Windows uses a purpose-scoped directory opener with BACKUP_SEMANTICS and
OPEN_REPARSE_POINT, rejecting any returned reparse point; ordinary file opening
is unchanged. Unsupported directory handle or read-only lock semantics refuse
cleanup rather than falling back to an unsafe open; Windows runtime proof is
pending. Lock creation is exclusive, and an existing lock is opened nofollow.

Keep lexical UI reservations free of filesystem I/O. Every destroy worker also
takes a separate canonical physical-path claim; release uses its captured key
even when the path is deleted or a symlink changes. Automatic cleanup rechecks
identity, queue/cache records, and cleanliness after each lifecycle stage and
immediately before `git worktree remove` without force. It never recursively
purges leftovers. Status errors, submodules, hidden index flags, and configured
clean/process filters refuse cleanup conservatively. Hooks and fsmonitor are
disabled for internal Git probes; configured filter values are never logged.

Automatic cleanup does not mutate any source, victim or target ref. THE-596
tracks future atomic direct-ref-type proof: Git 2.54.0 accepts a same-OID symbolic
target even in an OID verify/delete transaction with `--no-deref`. The private
falsification `/tmp/thegn-audit/ref-semantics.PR32KV/probe.sh` (SHA256
`05ab36837b66ce0bbe412f66a83daef15fbcffbd90865004a2571f5e684600e4`)
demonstrated this with no live repository access. A prepare/ack protocol checking
types under both Git ref locks may be a future solution; it is not implemented.

After verified physical removal, requested branch deletion instead installs the
fixed `CleanupHold::BranchRetained` marker in the landed row's `error_detail`
through a full-row null-safe CAS. All other fields, including timestamps, raw
nullable location, attempts and original result, remain unchanged. Generic
`del_worktree` cache pruning preserves this exact landed hold; explicit queue
dismissal clears it. Background sweeps skip held rows without repeatedly probing
the absent path; forced sweeps report the hold, never fresh collection/clearing.
Normal requeue/retry clears the marker. Failed bookkeeping does not claim that
a persistent hold was installed and requires manual reconciliation.
Requested detach (keep branch) can complete its ordinary queue deletion.

Git has bounded output and a 15-second completion/pipe deadline. At most two
outstanding command budgets retain ownership across child and pipe workers. A
single bounded reaper owns delayed waits; unknown reap states retain capacity
rather than admit unlimited children. Worker creation is fallible and partial
setup transfers child ownership safely. Numeric process-group signals are only
issued while `try_wait` confirms an unreaped running child. OS spawn/filesystem
operations are not interruptible by this deadline.

Limitations: cooperative locks cannot exclude unrelated Git processes or an
external editor. An ignored file created after the final check may race Git's
own removal. No automatic branch ref mutation remains.
This is conservative automatic cleanup, not atomic filesystem isolation against
host writers. No force fallback or arbitrary recursive deletion narrows the
remaining race without claiming to eliminate it.

Only verified physical removal enters `collected`. Dirty, refused, and partial
bookkeeping outcomes remain distinct, with bounded terminal-safe diagnostics.
Expiry-mode UI clearing must not eagerly drop retained retry records. A failed
repository identity lookup refuses the clear operation.
`cleared_rows` comes only from successful bookkeeping writes, never from noticing
that a row is now absent; concurrent revocation cannot inflate the count.

THE-593 adds exact selected-row eligibility across probes and compare-and-delete
manual clearing: missing, retried, reassigned or refinalized records revoke the
old selection. Duplicate selections cannot inflate cleared counts. Actual land
callers carry the commit through `apply_landed`; generic Landed/UpToDate events
without a matching recorded result cannot trigger destructive cleanup.

This is comparison of the full observed row values, not a durable generation.
A same-value ABA (another writer changes a row and then restores every observed
value, including timestamps, before revalidation) is not excluded. Detecting that
history requires a separately implemented durable generation; these guards make
no such claim. Changed result OIDs revoke eligibility even when every other row
value is unchanged, for both physical cleanup and asynchronous panel clearing.

THE-594 settles the environment once and uses a dedicated conservative local
callback, never the ambient-reloading explicit teardown routine. Worktree and
workspace selections are checked with error-preserving DB reads. Active
projection/sync/bridge/session registries, any persisted session or tenancy,
active dispatch, managed placement/data/VPN/provider settings, unresolved envs,
and enabled `auto` or non-bwrap/non-host backends refuse automatic collection.
Any discoverable OCI runtime or uncertain/beyond-bound PATH probe also refuses;
no container inspection or name-derived deletion is attempted. This is a
temporary compatibility restriction: runtime-attached/managed worktrees need
explicit cleanup until verified owned teardown handles exist. Even stale
persisted sessions and `auto` with no running container deliberately refuse.
Absence from current PATH/known registries does not prove that no historical
runtime ever existed. Other processes can create resources between observations;
process-local claims are not a global runtime ownership boundary.

Private tests use temporary Git repositories, in-memory queue databases, private
XDG state/config, and a thread-local test-only Git-global-config override applied
after the production Git environment scrub. Pipe workers never construct Git
commands. OCI discovery receives a private test search directory; dedicated
cases verify conservative detection without executing any candidate. This
exercises actual production argv and removal without touching
live sessions/configuration. Tests and native delivery gates remain pending.

## THE-594 acceptance fixtures

Private tests insert owned data-only entries into the real agent projection/provider-sync registries and invoke `remove_landed_with_config`. A path-scoped thread-local observer refuses the explicit `teardown_runtime` entry before ambient lookups if automatic cleanup accidentally reaches it. This counts entry into the current synchronous teardown boundary, not individual provider commands or future foreign threads. Actual pre/post hook marker commands prove resource refusal occurs before hooks and that the same landed selection succeeds after fixture custody is released. Registry RAII restores previous entries on assertion unwind. All helpers compile only under cfg(test); runtime cleanup policy and scheduling remain unchanged.
