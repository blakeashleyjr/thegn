# State DB — deltas

## ADDED Requirements

### Requirement: CI job logs are cached bounded and pre-redacted

The store SHALL carry a `ci_log_cache` table keyed by (worktree, run id, job
id) holding redacted log text, a truncation flag, and `fetched_at`, with the
schema `user_version` bumped for its introduction. Rows MUST only ever contain
text that has passed the CI redaction chokepoint; retention MUST evict the
oldest runs' rows per worktree beyond the configured `[ci] log_cache_runs`
count; and the table remains a cache — deleting it loses nothing the CI
provider cannot restore, and a terminal run's row is authoritative enough to
skip re-fetching (finished-job logs are immutable upstream).

#### Scenario: A cached log round-trips

- **WHEN** a failing job's redacted log tail is written and later read back
- **THEN** the text, truncation flag, and fetched-at stamp are returned
  unchanged, and no re-fetch is triggered for that terminal run

#### Scenario: Dropping the cache is safe

- **WHEN** `ci_log_cache` rows are deleted
- **THEN** thegn behaves as on first run for those logs — surfaces report
  not-cached and the next failure repopulates the table
