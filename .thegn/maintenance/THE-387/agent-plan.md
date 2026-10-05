# THE-387 plan
Scope: pure validators `thegn_core::git_operand` (branch/tag/remote/revision/bisect term, check-ref-format semantics, leading dash refused) and guards at the thegn-svc mutation sites (branch.rs, commit.rs, bisect.rs, rebase.rs). Refuse before spawn.
Out of scope: typed operand newtypes across all APIs, generation-fenced confirmation identity, plumbing.rs/mod.rs raw sites, dash-named refs supported via plumbing.
Tests: core unit tests; svc test creating refs/heads/--help.
