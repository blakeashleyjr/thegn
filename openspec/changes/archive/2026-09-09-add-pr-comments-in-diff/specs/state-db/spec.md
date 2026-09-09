# State DB — complete PR review cache delta

## ADDED Requirements

### Requirement: The state DB caches only complete identity-bearing PR reviews

The state database SHALL store at most one complete PR review snapshot per
canonical worktree key in additive schema v63. A row SHALL carry worktree,
branch, PR number, head OID, fetched time, and one atomic payload containing the
PR-head diff plus complete conversation. Reads SHALL reject identity-mismatched
or malformed payloads. A partial or transient fetch SHALL NOT overwrite the last
complete row. This table is a best-effort cache; the forge remains authoritative.

#### Scenario: A complete review refresh succeeds

- **WHEN** both the PR diff and conversation are fetched for the same worktree, branch, PR, and head
- **THEN** one atomic cache row replaces the prior matching review snapshot

#### Scenario: A refresh is partial

- **WHEN** either the diff or conversation fetch fails after a complete row exists
- **THEN** the complete row remains unchanged and may be presented with stale/error status

#### Scenario: Cached identity no longer matches

- **WHEN** a cached row's branch, PR number, or head OID differs from the active review
- **THEN** the row is ignored or labeled stale and its feedback is not attached to the active diff

#### Scenario: A pre-v63 database migrates

- **WHEN** an older database containing unrelated state is opened by an authorized migrator
- **THEN** the `pr_review_cache` table is added idempotently, existing rows survive, and `user_version` advances through the normal migration ladder
