# Isolate maintenance regression fixtures

## Why

The full September 14 workspace gate exposed two classes of fixture defects.
THE-636's watchdog test used an absent worktree and default backend policy,
selecting the real source checkout and a container backend. THE-637's Git tests
assumed the user's initial branch and commit identity. These prevent trustworthy
private verification of existing behavior.

## What Changes

- Pin the watchdog fixture to a retained temporary worktree, private state and
  explicit native clean-shell behavior, without provider or login-profile work.
- Pin fixture Git branch names and commit identities, verify expected conflict
  setup, and retain temporary-directory ownership through assertion failures.
- Preserve actual production watchdog and Git behavior under the fixtures.

## Impact

Only verification fixtures change. There are no new product capabilities,
configuration options or production backend policy changes. Native Windows
execution of the existing POSIX clean-shell path remains unverified.
