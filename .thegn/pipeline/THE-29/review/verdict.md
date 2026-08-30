# THE-29 Security/Test/Bug Review

PASS

The branch was checked after `git merge main` reported `Already up to date`.
The full `git diff main...HEAD` and all THE-29 lane documents were reviewed,
including every coder `Unverified` section and the approved architect-review
follow-ups.

Review fixes committed separately:

- `b809df1b fix(the-29): harden fork handoff and cwd remapping (review)`
  - resolves native fork launch failures before creating scrollback handoff
    files;
  - rejects symlink/non-directory handoff paths and refuses to overwrite an
    existing file/link with exclusive creation;
  - rejects lexical `..` traversal when remapping a cwd into a fork worktree;
  - adds regression tests for the failure paths.

Security/bug checks found no remaining blocking issue. Raw fork argv/env stays
in daemon memory and is re-capped; identity variables are re-applied; native
commands come from the closed harness registry with validated session IDs; the
lineage cache is credential-free and additive; and fork errors propagate from
the user-invoked control path. Cache persistence and teardown cleanup remain
explicitly best-effort as documented by the lane.

Verification:

- Focused fork/path/cleanup suite: 5/5 passed.
- Core harness/fork/migration scope: 52/52 passed.
- Service control schema: 1/1 passed.
- Host quick gate passed.
- Host clippy with tests and `-D warnings` passed.
- Core, host, and service platform ratchets passed; ignored-result ratchet
  clean (320 pinned).
- `treefmt --no-cache --allow-missing-formatter` passed with 0 files changed;
  `cargo fmt --all -- --check` and `git diff --check` passed.
- `openspec validate --all --strict` could not run because `openspec` is not
  installed. Socket-bind CLI smoke coverage remains environment-restricted,
  as recorded by the architect review.
- No e2e, full CI, migration, live binary, or normal state DB was used;
  fork tests used temporary state where needed.

## Snapshots

None. No frame-affecting changes were made.
