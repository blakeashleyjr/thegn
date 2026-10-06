# THE-391 plan

Scope: crates/thegn-svc/src/git/submodule.rs only. Replace per-path/per-tree ls-tree spawns
with chunked batched ls-tree (bounded argv chunks), budgets on path count/bytes/wall time,
all-or-error parsing (mode/oid/duplicate/unrequested). Tests: parser all-or-error, budget refusal.
Out of scope: cancellation plumbing (needs THE-368 substrate), has_custom_driver argv (other file).
