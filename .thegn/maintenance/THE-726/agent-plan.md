# THE-726 plan

Scope: diff_watch.rs + hydrate.rs retarget. On the event that withdraws a live claim, the notify callback fires one off-loop rebuild (Reviver) that hands a fresh watcher over the existing watcher_tx adoption channel. Budget MAX_REVIVES=4 per binding with exponential sleep backoff on the worker thread; retarget cancels pending revives via a static epoch. No timers or polling on the loop; only fires on a live to withdrawn transition.
Tests: budget bound/backoff unit test; gitignore edit revives claim.
Out of scope: loop changes, config keys.
