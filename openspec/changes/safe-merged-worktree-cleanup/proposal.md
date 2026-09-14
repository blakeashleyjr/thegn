# Safe merged-worktree cleanup

THE-588 repairs automatic collection that trusted global queue rows as deletion
authority and reported refusals as successful collection. Ownership: Runtime
Security & Host Architecture. THE-589 separately prevents unsuccessful folds
from announcing a land or triggering unrelated expiry collection.

Require registered local linked-worktree identity, matching repository and branch,
actual target ancestry, preserved dirty/ignored files, and verified no-force
removal before reporting collection. Keep refused queue records for retry.
Explicit destructive worktree commands retain their existing user-selected
semantics; this change does not run cleanup on operator worktrees.

Delivery prerequisite: THE-586 supplies the missing reciprocal inventory for
the existing configuration-severity repair. This component does not claim that
prerequisite or either safety issue is landed.
