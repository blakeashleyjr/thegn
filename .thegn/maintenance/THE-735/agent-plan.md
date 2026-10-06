# THE-735 plan

Scope: reuse thegn_core::git_operand at remaining svc mutation/plumbing sites (plumbing.rs, mod.rs add_worktree/remove_worktree/diff_files/log_commits_ref, cherry.rs, undo.rs) and host sites (integrate driver_merge/regenerate_merge, merge_ops push_target).
Not touched: submodule.rs (THE-391), stash.rs (operands are -m values or numeric stash@{n}; no option position).
Out of scope: newtypes, generation fencing, dash-ref support, --end-of-options.
Tests: refusal-before-spawn tests per module.
