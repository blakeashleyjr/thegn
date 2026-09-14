# Authenticate PR-agent authorship

THE-545 replaces PR-URL namespace guesses with current provider-reported author and authenticated viewer identities before own-PR automation. Organization PRs by the signed-in author become eligible; foreign PR authors in that user's namespace are held. Legacy author-less cache records remain readable and cannot authorize a handoff.

Impact: shared forge model and optional evidence operation, GitHub CLI/native read paths, PR queue, CI autofix and durable review handoff. Maintenance tracking: THE-629. Root registers delivery metadata.
