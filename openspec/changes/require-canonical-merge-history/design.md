# THE-606: canonical history for automatic local merge operations

Status: source implementation in progress; no runtime gates or merge approval.

The exact-commit gate does not repair a fold already computed from replacement
objects. An ancestry check can also be forged by legacy graft metadata, including
an ambient `GIT_GRAFT_FILE` outside the repository's default graft path.

Add a host-only `CanonicalHistory` observation shared by candidate preparation,
fold publication and automatic cleanup. It pins the originally admitted root,
Git entry, administration and common directories, and refuses replacement refs,
grafts, shallow history, unsupported ambient history overrides, unknown metadata,
and unsupported nonlocal transports. Fixed Git probes reuse THE-597's bounded
capture; path identities reuse its no-follow platform handles. It never repairs
or deletes user refs, configuration, metadata or shallow boundaries.

Revalidate before snapshots, after merge/filter/regeneration and gate callbacks,
before publishing no-op or failed-gate/blame evidence, and immediately before
target advancement or physical worktree removal. History failures are
infrastructure holds/refusals, never conflict or bad-branch results. Retain
existing queue evidence and explicit-cleanup behavior.

This observes mutations; it is not atomic with external Git/filesystem writers.
Another same-UID actor temporarily installing then removing alternate history
during a command is outside this guarantee. No global Git backend behavior,
remote proof, filesystem snapshot or privileged isolation is introduced.

Registry authority uses the existing strict point decoder (`worktree_record`),
not the legacy location resolver's missing/error-to-local fallback. Missing rows
allow an explicitly addressed verified local repository; decoded empty/`local`
are the only admitted location values. Unknown/nonlocal values and open/query/
decode errors hold. The original location observation is retained and rechecked.
Selected-fold preparation reuses its already opened DB through a synchronous
`Rc<Db>` and transfers the original root history token to the fold adapter.
All selected source tokens are retained before any snapshot hook executes.
No object operation opens a new registry connection.

A source token is retained through that source's own final snapshot write; a
prior callback cannot replace a still-pending source and have it readmitted.
After a successful snapshot, batch fold inputs are immutable object IDs in the
retained root object store, not mutable source checkout locations. Completed
source tokens are therefore not claimed to remain leased through target CAS.
Final queue persistence and any later cleanup perform their independent
THE-591/THE-600 observation checks. This avoids quadratic all-source checks at
every object operation without asserting protection against an external writer.

Windows automatic local folds (including disabled/empty gates) are unsupported
until equivalent platform identity pins exist. Tests exercise explicit
infrastructure refusal and unchanged refs/index/worktree, not ignored tests.
This is a deliberate compatibility reduction; explicit manual Git operations
are unchanged. Unix private fixture roots canonicalize the temporary base so
Darwin's `/var` alias is not mistaken for an admitted symlink path.

Each original-history revalidation currently runs two fixed Git path queries
and one replacement-ref query plus metadata/registry checks. Before/after
callback checks prioritize proof over throughput; focused timing must be
reported. This can materially increase process count for large folds. Future
optimization must preserve original mapping and error semantics, rather than
silently skipping revalidation or claiming these probes are free.

Dependencies: THE-597 shared probe and platform identity implementation; the
THE-588/THE-600 cleanup and THE-591/THE-595 outcome/snapshot contracts remain.

Required private tests cover replacement refs (loose and packed), alternate
graft environment, ordinary graft metadata, shallow metadata, malformed/symlink
paths, initial refusal, post-callback refusal, unchanged refs/index/worktree and
queue evidence, and explicit unsupported platform/transport outcomes.
