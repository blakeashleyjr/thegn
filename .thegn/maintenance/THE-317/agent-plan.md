# THE-317 plan

Scope: linear.rs update_issue status path. Resolve issue(id){team{states}} via GraphQL variables, pick the single state of target type in that team; none or multiple => explicit error (ambiguity lists names). Verify returned state type. Out of scope: per-team cache (extra query per status write only), name-based statuses (IssuePatch is type-based; a new name field is an API change).
Tests: pure pick_team_state tests + updated fake-server tests.
