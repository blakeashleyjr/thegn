# THE-634 headless review admission: independent adversarial review

Reviewed by the independent maintenance/protocol lane on September 14, 2026.
Candidate: `02876bcc` plus lossless-worktree revision `415a7e78` in
`/tmp/thegn-the545-20260914`.

## Verdict

Scoped source approval after revision. Compiled host regression execution remains
a required integration gate. This does not approve full THE-545 credential or
account-generation binding; that acceptance remains open.

The production UI handoff now captures the selected snapshot, queue policy and
checkout before scheduling off-loop preparation. Own-only admission verifies
persisted local execution before provider selection, matches PR number, branch,
head and selected worktree, and checks the entire selected URL against the fresh
proof's structured repository. A fresh proof precedes sandbox preparation and is
revalidated immediately before the actual agent-launch callback. Foreign,
unknown, stale and nonlocal authority does not reach preparation or launch.
Explicit broader policy and existing paste-only behavior retain their contracts.

The tests call the production execute helper with synthetic Git/DB/provider
fixtures. They cover positive launch, denial and ordering, stale selected data,
hostile URLs, actual SQL lookup failure, changes during preparation, unchanged
attempt/dispatch state and broader policy. They do not claim to execute an agent
under an immutable credential capability.

## Finding and required revision

Initial candidate authenticated the original `PathBuf` but launched using
`worktree_cache_key`, which converts non-UTF-8 paths lossily. A byte-invalid path
and a UTF-8 replacement-character sibling can be different checkouts with the
same launch string. Selected snapshot equality used the same lossy conversion,
so it did not prevent authenticating one checkout and launching in another.

The revision rejects non-UTF-8 worktree paths for own-only policy before the
outer worker opens its DB or resolves a forge and before the injected execute
helper accesses authority. A Unix fixture creates two distinct existing paths
sharing a lossy key and verifies refusal plus zero provider/preparation/launch
calls. It also invokes the outer worker to verify early refusal. Non-Unix source
does not claim native execution of that Unix byte-path fixture.

No remaining scoped source blocker was found. Independent `rustfmt --check` and
`git diff --check` passed after the revision. No Cargo or external provider/agent
operation was run during this review.
