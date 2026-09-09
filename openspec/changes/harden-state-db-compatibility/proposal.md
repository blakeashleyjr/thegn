# Harden shared-state migration authority and compatibility

Linear: THE-95, THE-96, THE-94

## Why

The shared state database currently fails open when a process has not installed
migration policy, while policy-aware clients use one build-wide schema version
instead of the minimum schema an operation needs. A schema change beneath a
running compositor can then be rendered as an unexplained empty sidebar. These
are three layers of one state-safety incident: authority, compatibility, and
truthful user feedback.

## What Changes

- Fail closed when the canonical shared DB is opened without an installed
  migration runtime, including through path aliases; keep fresh bootstrap and
  explicitly noncanonical temporary databases usable under explicit rules.
- Introduce explicit per-operation schema access requirements so a command can
  use compatible older tables without gaining migration authority or silently
  dropping safety checks.
- Propagate typed schema refusal into the compositor, preserve the last known
  model, and show a sticky actionable status with observed/build versions until
  a successful hydration.
- Add unit, command, and running-compositor regression coverage for the complete
  THE-95 → THE-96 → THE-94 chain.

## Non-goals

- Automatic downgrade or destructive migration.
- Allowing arbitrary libraries/tests to mutate the user's canonical DB.
- Treating a missing DB-backed safety check as successful command completion.
- A second database or per-command schema fork.
