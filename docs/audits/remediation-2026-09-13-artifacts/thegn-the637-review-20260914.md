# THE637 service Git fixture review

The final no-fail-fast workspace run reported two actual service Git failures.
Each reproduced in the existing compiled service binary with no HOME, private
XDG/TMPDIR roots, GIT_CONFIG_GLOBAL=/dev/null and GIT_CONFIG_NOSYSTEM=1. No shared
Git config or application process was changed. Exact receipts:
`/tmp/thegn-svc-git-fixture-reproduction-20260914.json`.

`ahead_behind_counts_divergence_and_is_none_without_upstream` seeded a bare remote
with default HEAD master, pushed main only, and cloned an unborn master branch.
The fixture silently relied on developer init.defaultBranch=main.
`merge_state_detects_a_live_merge_and_clears_after_abort` bypassed its setup
helper for the deliberate merge, omitting the helper's committer identity. Git
returned128 before attempting the merge; the fixture treated any failure as the
expected conflict and subsequently found no MERGE_HEAD.

Primary approved the bounded tests-only fix before edits. The bare init now
explicitly selects main. The deliberate merge gets the same four private
author/committer environment fields as the existing helper and must return
exit1, with captured diagnostics if setup fails differently. Both fixtures now
use owned TempDir guards, preserving cleanup on assertion unwind without deleting
predictable paths. Stale helper comments about requiring global default main were
corrected. Production Git implementation and global environment policy are
unchanged. Formatting and diff checks pass; independent review and rebuilt
execution are pending at this report checkpoint.
