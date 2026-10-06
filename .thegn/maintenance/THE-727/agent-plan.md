# THE-727 plan

Scope: IssuesOverlay::apply in config_issues.rs applies linear.team_id / jira.project_key / kaneo ws+project pins per matching [[issue_accounts]] entry, restrict-only (account pin is the ceiling, kaneo joint ceiling via shared narrow_kaneo). Refusals carry the account name. Legacy sub-table refusals are dropped in accounts mode.
Tests: unit tests in config_issues.rs. Out of scope: workspace_slug, THE-725.
