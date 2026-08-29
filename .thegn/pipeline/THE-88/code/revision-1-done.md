verdict: done

## Commits

- 13801e95 fix(the-88): preserve control response status
- 60f27209 fix(the-88): preserve wait and DB failures

## Changes

- Typed control HTTP status errors; only wait HTTP 404 becomes `gone: true`.
- Transport, control, malformed-response, and wake-time DB failures propagate
  as retryable command errors (exit 2).
- Missing dispatch rows alone return empty report/artifact facts.
- Added isolated dispatch classification and DB-error tests.

## Verified

- `just quick thegn-host`
- Targeted nextest: `wait_only_treats_a_control_404_as_a_gone_wake`
- Targeted nextest: `wake_response_and_db_read_errors_are_not_timeouts_or_missing_rows`
- `cargo fmt --all -- --check` and `git diff --check`

## Unverified

- Full workspace gates (`just test`, `just lint`, `just ci`, coverage).
- E2E and cross-platform checks.
