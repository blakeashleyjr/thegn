# Keep static CLI output independent of configured startup

## Why

THE-611: config path/schema, embedded API catalog/schema/coverage and completion registration currently pass through migration, profile reroot and effective config loading. An unread FIFO can block static output; a simple schema query can migrate legacy state or create a profile.

## What Changes

Classify the borrowed parsed command exhaustively and return static output before migration/reroot. Reuse the existing formatting helpers and retain the configured path for every other action. Add actual parser coverage and bounded private CLI fixtures for hostile config, startup canaries, aliases and closed stdout.

## Impact

Roadmap A.6 (shared CLI front doors), O.185–189 (configuration). No new commands, settings, capability, database schema, UI, or external action. THE-505/592 checked configuration admission, THE-607 worker loading and THE-612 source-inspection routing remain separate. Dynamic TAB completion is unchanged.
