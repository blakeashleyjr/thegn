# wt-exact-branch REPORT (THE-1973 first slice)

Verdict: implemented in 11a697328 (source-only, not compiled).
Files: crates/thegn-host/src/cmd/wt.rs (New variant `exact_branch` flag, `exact_branch_name` fn, derivation arm in `new`), docs/help/workspaces-and-worktrees.md (one paragraph).
Tests (cmd::wt): exact_branch_keeps_name_verbatim, exact_branch_refuses_missing_invalid_or_existing, exact_branch_flag_constraints.
Acceptance: requires name (clap `requires`) met; conflicts --program/--from-issue met; validation reuses thegn_core::git_operand::branch_name met; existing branch refused (plain path dedupes with -N and never attaches, so refusal is stricter) met; path derivation untouched met.
Unverified: compile; clap `requires = "name"` on the positional.
Not done: `--batch <file>`; THE-1973 stays open. Merge after wt-prune.
