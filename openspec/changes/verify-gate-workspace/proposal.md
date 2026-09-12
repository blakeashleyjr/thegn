# Verify gate locking and linked-checkout identity

THE-597 closes unsafe fallback in the merge-queue roadmap (group T, item 758).
Reuse currently ignores lock failures and may recursively delete an unknown
checkout after Git preparation fails. Require nonblocking exclusive admission,
physical local identity and an exact commit instead. Failed admission is an
infrastructure hold and cannot advance the target or blame a branch.

This follows THE-589/THE-591/THE-595 outcome and selection guards. THE-588 owns
landed-worktree cleanup separately. THE-601 tracks existing unbounded subprocess
capture and missing gate deadlines; neither is claimed repaired here.
