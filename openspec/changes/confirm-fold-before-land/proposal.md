# Confirm folds before recording a land

THE-589 repairs roadmap group T item 758 (merge queue). A speculative object-DB
fold currently reaches landed bookkeeping even when its gate fails and the target
ref never advances. This can trigger destructive lifecycle policy for unmerged work.

Separate prepared commits from successful CAS landings, preserve bounded gate
diagnostics, and report failed requests truthfully. Remove the CLI's unrelated
automatic expiry sweep. THE-588 independently secures explicit expiry cleanup;
both repairs block retrying THE-586 through the native merge queue.
