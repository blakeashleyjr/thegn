# THE-724 plan

Scope: IssuesOverlay::apply (config_issues.rs) becomes restrict-only. providers intersected with the globally enabled set; team_id/project_key/workspace_id/project_id accepted only when the global pin is empty or equal (else refused, diagnostic returned; Config::repo_issues logs a tracing warn). workspace_slug (URL display only) unchanged.
Tests: config_tests.rs next to repo_overlay_cannot_inject_tracker_accounts, a unit test in config_issues.rs, updated tests/repo_issues_overlay.rs (it asserted widening).
Out of scope: THE-725.
