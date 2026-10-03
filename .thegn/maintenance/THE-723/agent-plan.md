# THE-723 plan

Scope: soft-apply cfg.default_folder in daemon worktree_create; session fork uses that RPC so it is covered.
Approach: pub(crate) cmd::wt::file_configured_default reusing check_folder_fileable + file_registered_worktree; warning logged.
Tests: wt.rs unit test.
Out of scope: optional folder request field.
