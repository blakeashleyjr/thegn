# Poll the open-PR feed once per repository TTL

The focused-worktree refresh currently starts a repository-wide open-PR list
request for every worktree switch. The feed is stored under the root checkout,
so requests within the configured PR interval are redundant. Share its polling
window by root checkout while retaining worktree-scoped PR detail refreshes.

Issue: THE-2775.
